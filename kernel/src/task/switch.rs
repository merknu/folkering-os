//! Task Context Switching
//!
//! Low-level context switching implementation for x86_64.
//! Saves/restores CPU registers and switches page tables.

use super::task::{Context, Task, current_task, set_current_task, get_task};
use super::TaskId;
use alloc::sync::Arc;
use spin::Mutex;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::PhysFrame;
use x86_64::PhysAddr;

/// Switch from current task to target task
///
/// # Safety
/// - Must be called with interrupts disabled
/// - Current task must be in valid state
/// - Target task must exist and be runnable
///
/// # Performance Target
/// <500 CPU cycles
pub unsafe fn switch_to(target_id: TaskId) {
    use super::task::get_current_task;

    let current_id = get_current_task();

    // Check if this is the first switch (from kernel context, no task)
    let current = if current_id == 0 {
        // First switch from kernel - no current task to save
        None
    } else if current_id == target_id {
        // Don't switch to ourselves
        return;
    } else {
        // Normal switch - get current task
        get_task(current_id)
    };

    crate::serial_println!("[SWITCH] Getting target task");
    let target = match get_task(target_id) {
        Some(task) => task,
        None => {
            crate::serial_println!("WARN: Attempted to switch to non-existent task {}", target_id);
            return;
        }
    };

    crate::serial_println!("[SWITCH] Getting context ptrs");
    // Save current task's context pointer (if we have a current task)
    let current_ctx_ptr = if let Some(ref current_task) = current {
        let current_locked = current_task.lock();
        &current_locked.context as *const Context as usize
    } else {
        0 // No current task (first switch from kernel)
    };

    // Get target task's context pointer
    let target_ctx_ptr = {
        let target_locked = target.lock();
        &target_locked.context as *const Context as usize
    };

    crate::serial_println!("[SWITCH] Switching page table");
    // Switch page table to target's address space
    {
        let target_locked = target.lock();
        let target_cr3 = get_page_table_phys_addr(target_locked.page_table.as_ref());

        // Only switch if different
        let current_cr3 = Cr3::read().0.start_address().as_u64();
        if current_cr3 != target_cr3 {
            Cr3::write(
                PhysFrame::containing_address(PhysAddr::new(target_cr3)),
                Cr3Flags::empty(),
            );
        }
    }

    crate::serial_println!("[SWITCH] Updating current task");
    // Update current task pointer
    set_current_task(target_id);

    crate::serial_println!("[SWITCH] Calling switch_context");
    // Perform actual register switch (assembly)
    if current_ctx_ptr == 0 {
        // First switch from kernel - just restore new task, don't save
        crate::serial_println!("[SWITCH] First switch - restoring task context");
        restore_context_only(target_ctx_ptr);
    } else {
        // Normal switch - save current, restore new
        switch_context(current_ctx_ptr, target_ctx_ptr);
    }
}

/// Get physical address of page table
///
/// For now, returns a dummy value since we're using kernel page table
fn get_page_table_phys_addr(_page_table: &crate::memory::PageTable) -> u64 {
    // TODO: Extract actual physical address from page table
    // For now, return current CR3 (kernel page table)
    Cr3::read().0.start_address().as_u64()
}

/// Assembly context switch
///
/// Saves current CPU state to old_ctx, restores from new_ctx.
///
/// # Arguments
/// - `old_ctx`: Pointer to Context structure to save to
/// - `new_ctx`: Pointer to Context structure to restore from
///
/// # Assembly Implementation
/// Saves/restores all callee-saved registers plus RSP, RBP, RFLAGS, RIP.
/// Uses R10/R11 as temporary registers to preserve pointer arguments.
#[unsafe(naked)]
extern "C" fn switch_context(_old_ctx: usize, _new_ctx: usize) {
    core::arch::naked_asm!(
        // Arguments: RDI = old_ctx pointer, RSI = new_ctx pointer
        // Immediately move to temp registers to preserve original RDI/RSI behavior
        "mov r10, rdi",           // R10 = old_ctx pointer
        "mov r11, rsi",           // R11 = new_ctx pointer

        // Save current task's context to old_ctx (pointed by R10)
        "mov [r10 + 0],  rsp",    // Save RSP
        "mov [r10 + 8],  rbp",    // Save RBP
        "mov [r10 + 16], rax",    // Save RAX
        "mov [r10 + 24], rbx",    // Save RBX
        "mov [r10 + 32], rcx",    // Save RCX
        "mov [r10 + 40], rdx",    // Save RDX
        "mov [r10 + 48], rsi",    // Save RSI (original value from argument)
        "mov [r10 + 56], rdi",    // Save RDI (original value from argument)
        "mov [r10 + 64], r8",     // Save R8
        "mov [r10 + 72], r9",     // Save R9
        // Note: R10 and R11 are caller-saved, so we save whatever was passed
        "mov [r10 + 80], r10",    // Save R10 (contains old_ctx pointer)
        "mov [r10 + 88], r11",    // Save R11 (contains new_ctx pointer)
        "mov [r10 + 96], r12",    // Save R12
        "mov [r10 + 104], r13",   // Save R13
        "mov [r10 + 112], r14",   // Save R14
        "mov [r10 + 120], r15",   // Save R15

        // Save return address as RIP
        "mov rax, [rsp]",
        "mov [r10 + 128], rax",   // Save RIP

        // Save RFLAGS
        "pushfq",
        "pop rax",
        "mov [r10 + 136], rax",   // Save RFLAGS

        // Save segment registers
        "mov ax, cs",
        "mov [r10 + 144], rax",   // Save CS
        "mov ax, ss",
        "mov [r10 + 152], rax",   // Save SS

        // Restore new task's context from new_ctx (pointed by R11)
        "mov rsp, [r11 + 0]",     // Restore RSP
        "mov rbp, [r11 + 8]",     // Restore RBP
        "mov rax, [r11 + 16]",    // Restore RAX
        "mov rbx, [r11 + 24]",    // Restore RBX
        "mov rcx, [r11 + 32]",    // Restore RCX
        "mov rdx, [r11 + 40]",    // Restore RDX
        "mov rsi, [r11 + 48]",    // Restore RSI
        "mov rdi, [r11 + 56]",    // Restore RDI
        "mov r8,  [r11 + 64]",    // Restore R8
        "mov r9,  [r11 + 72]",    // Restore R9
        "mov r10, [r11 + 80]",    // Restore R10
        // R11 restored last since we're using it
        "mov r12, [r11 + 96]",    // Restore R12
        "mov r13, [r11 + 104]",   // Restore R13
        "mov r14, [r11 + 112]",   // Restore R14
        "mov r15, [r11 + 120]",   // Restore R15

        // Restore RFLAGS
        "mov rax, [r11 + 136]",
        "push rax",
        "popfq",

        // Restore RIP by jumping to it
        "mov rax, [r11 + 128]",
        "mov [rsp], rax",         // Overwrite return address

        // Finally restore R11
        "mov r11, [r11 + 88]",    // Restore R11

        // Return will jump to restored RIP
        "ret"
    );
}

/// Restore task context without saving (for first switch from kernel to user)
///
/// Uses IRETQ to properly transition from CPL 0 (kernel) to CPL 3 (user).
///
/// # Arguments
/// - `new_ctx`: Pointer to Context structure to restore from
#[unsafe(naked)]
extern "C" fn restore_context_only(_new_ctx: usize) {
    core::arch::naked_asm!(
        // Argument: RDI = new_ctx pointer
        "mov r11, rdi",           // R11 = new_ctx pointer

        // Build IRETQ frame on stack (required for CPL 0->3 transition)
        // IRETQ pops: SS, RSP, RFLAGS, CS, RIP
        "mov rax, [r11 + 152]",   // SS
        "push rax",
        "mov rax, [r11 + 0]",     // RSP
        "push rax",
        "mov rax, [r11 + 136]",   // RFLAGS
        "push rax",
        "mov rax, [r11 + 144]",   // CS
        "push rax",
        "mov rax, [r11 + 128]",   // RIP
        "push rax",

        // Restore general-purpose registers
        "mov rax, [r11 + 16]",    // Restore RAX
        "mov rbx, [r11 + 24]",    // Restore RBX
        "mov rcx, [r11 + 32]",    // Restore RCX
        "mov rdx, [r11 + 40]",    // Restore RDX
        "mov rsi, [r11 + 48]",    // Restore RSI
        "mov rdi, [r11 + 56]",    // Restore RDI
        "mov rbp, [r11 + 8]",     // Restore RBP
        "mov r8,  [r11 + 64]",    // Restore R8
        "mov r9,  [r11 + 72]",    // Restore R9
        "mov r10, [r11 + 80]",    // Restore R10
        "mov r12, [r11 + 96]",    // Restore R12
        "mov r13, [r11 + 104]",   // Restore R13
        "mov r14, [r11 + 112]",   // Restore R14
        "mov r15, [r11 + 120]",   // Restore R15

        // Finally restore R11
        "mov r11, [r11 + 88]",    // Restore R11

        // IRETQ will pop SS, RSP, RFLAGS, CS, RIP and switch to user mode
        "iretq"
    );
}

/// Initialize a new task's context
///
/// Sets up initial CPU state for a task that hasn't run yet.
///
/// # Arguments
/// - `entry_point`: Virtual address of task's entry point
/// - `stack_top`: Virtual address of task's stack top
///
/// # Returns
/// Initialized Context structure
pub fn init_context(entry_point: u64, stack_top: u64) -> Context {
    Context {
        rsp: stack_top,
        rbp: stack_top,
        rax: 0,
        rbx: 0,
        rcx: 0,
        rdx: 0,
        rsi: 0,
        rdi: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        rip: entry_point,
        rflags: 0x202,  // IF=1 (interrupts enabled), reserved bit 1 always set
        cs: 0x08,       // Kernel code segment (will be 0x1B for user)
        ss: 0x10,       // Kernel data segment (will be 0x23 for user)
    }
}

/// Create initial context for userspace task
///
/// Sets up context with user-mode segments.
pub fn init_user_context(entry_point: u64, stack_top: u64) -> Context {
    Context {
        rsp: stack_top,
        rbp: stack_top,
        rax: 0,
        rbx: 0,
        rcx: 0,
        rdx: 0,
        rsi: 0,
        rdi: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        rip: entry_point,
        rflags: 0x202,  // IF=1 (interrupts enabled)
        cs: 0x1B,       // User code segment (0x18 | RPL=3)
        ss: 0x23,       // User data segment (0x20 | RPL=3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_size() {
        // Context should be 160 bytes (20 * 8 bytes)
        assert_eq!(core::mem::size_of::<Context>(), 160);
    }

    #[test]
    fn test_init_context() {
        let ctx = init_context(0x400000, 0x7FFFFFFFF000);
        assert_eq!(ctx.rip, 0x400000);
        assert_eq!(ctx.rsp, 0x7FFFFFFFF000);
        assert_eq!(ctx.rflags & 0x200, 0x200); // IF flag set
    }

    #[test]
    fn test_init_user_context() {
        let ctx = init_user_context(0x400000, 0x7FFFFFFFF000);
        assert_eq!(ctx.cs, 0x1B); // User code segment
        assert_eq!(ctx.ss, 0x23); // User data segment
    }
}
