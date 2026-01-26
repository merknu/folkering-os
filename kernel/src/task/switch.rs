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

    crate::serial_println!("[SWITCH] switch_to(target_id={})", target_id);

    let current_id = get_current_task();

    // Check if this is the first switch (from kernel context, no task)
    let current = if current_id == 0 {
        // First switch from kernel - no current task to save
        None
    } else if current_id == target_id {
        // Don't switch to ourselves, but update context pointer for syscalls
        let task = match get_task(target_id) {
            Some(t) => t,
            None => return,
        };
        let ctx_ptr = {
            let locked = task.lock();
            &locked.context as *const Context as usize
        };
        crate::arch::x86_64::syscall::set_current_context_ptr(ctx_ptr as *mut Context);
        return;
    } else {
        // Normal switch - get current task
        get_task(current_id)
    };

    let target = match get_task(target_id) {
        Some(task) => task,
        None => {
            return;
        }
    };

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
        let ctx_ptr = &target_locked.context as *const Context as usize;
        crate::serial_println!("[SWITCH] Target task {} context pointer: {:#x}", target_id, ctx_ptr);

        // DEBUG: Check if this looks like a valid heap address
        if ctx_ptr < 0xFFFF_FF00_0000_0000 {
            crate::serial_println!("[SWITCH] WARNING: Context pointer {:#x} looks like kernel data/code, not heap!", ctx_ptr);
        }
        ctx_ptr
    };

    // Switch to target task's page table if it has one
    {
        let target_locked = target.lock();
        if target_locked.page_table_phys != 0 {
            crate::serial_println!("[SWITCH] Switching to page table {:#x}", target_locked.page_table_phys);
            unsafe {
                crate::memory::paging::switch_page_table(target_locked.page_table_phys);
            }
        }
    }

    // Update current task pointer
    set_current_task(target_id);

    // Update current context pointer for fast syscall access
    crate::serial_println!("[SWITCH] Setting CURRENT_CONTEXT_PTR to {:#x} for task {}", target_ctx_ptr, target_id);
    crate::arch::x86_64::syscall::set_current_context_ptr(target_ctx_ptr as *mut Context);

    // Perform actual register switch (assembly)
    if current_ctx_ptr == 0 {
        // First switch from kernel - just restore new task, don't save
        crate::serial_println!("[SWITCH] First switch from kernel, calling restore_context_only()");
        restore_context_only(target_ctx_ptr);
    } else {
        // Normal switch - save current, restore new
        crate::serial_println!("[SWITCH] Normal switch, calling switch_context()");
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

/// Debug helper: print context before restore (using bypass functions)
#[inline(never)]
pub fn debug_context_before_restore(ctx_ptr: usize) {
    let ctx = unsafe { &*(ctx_ptr as *const Context) };
    crate::drivers::serial::write_str("[RESTORE] RIP=");
    crate::drivers::serial::write_hex(ctx.rip);
    crate::drivers::serial::write_str(", RSP=");
    crate::drivers::serial::write_hex(ctx.rsp);
    crate::drivers::serial::write_str(", CS=");
    crate::drivers::serial::write_hex(ctx.cs);
    crate::drivers::serial::write_str(", SS=");
    crate::drivers::serial::write_hex(ctx.ss);
    crate::drivers::serial::write_str(", RFLAGS=");
    crate::drivers::serial::write_hex(ctx.rflags);
    crate::drivers::serial::write_newline();

    // CRITICAL: Verify CS and SS are correct (not swapped!)
    if ctx.cs != 0x23 {
        crate::drivers::serial::write_str("[RESTORE] ERROR: CS=");
        crate::drivers::serial::write_hex(ctx.cs);
        crate::drivers::serial::write_str(" should be 0x23!\n");
    }
    if ctx.ss != 0x1B {
        crate::drivers::serial::write_str("[RESTORE] ERROR: SS=");
        crate::drivers::serial::write_hex(ctx.ss);
        crate::drivers::serial::write_str(" should be 0x1B!\n");
    }
}

/// Restore task context without saving (for first switch from kernel to user)
///
/// Uses IRETQ to properly transition from CPL 0 (kernel) to CPL 3 (user).
///
/// # Arguments
/// - `new_ctx`: Pointer to Context structure to restore from
///
/// # Safety
/// Must be called with interrupts disabled and valid context pointer
#[unsafe(naked)]
pub unsafe extern "C" fn restore_context_only(_new_ctx: usize) {
    core::arch::naked_asm!(
        // Argument: RDI = new_ctx pointer

        // Save new_ctx to R11 first (so we can call debug function)
        "mov r11, rdi",           // R11 = new_ctx pointer

        // Call debug function (RDI still has ctx_ptr)
        // NOTE: R11 is caller-saved, so we must save it across the call!
        "push r11",
        "call {debug_fn}",
        "pop r11",                // Restore R11 after call

        // R11 now has new_ctx pointer again
        // NOTE: MSR restore removed - caused format! crashes at boot

        // Build IRETQ frame on stack (required for CPL 0->3 transition)
        // IRETQ pops: SS, RSP, RFLAGS, CS, RIP
        "mov qword ptr [{debug_marker}], 0x1111",   // Marker: starting frame build
        "mov rax, [r11 + 152]",   // SS
        "mov qword ptr [{debug_marker}], 0x2222",   // Marker: loaded SS
        "push rax",
        "mov qword ptr [{debug_marker}], 0x3333",   // Marker: pushed SS
        "mov rax, [r11 + 0]",     // RSP
        "push rax",
        "mov qword ptr [{debug_marker}], 0x4444",   // Marker: pushed RSP
        "mov rax, [r11 + 136]",   // RFLAGS
        "push rax",
        "mov qword ptr [{debug_marker}], 0x5555",   // Marker: pushed RFLAGS
        "mov rax, [r11 + 144]",   // CS
        "push rax",
        "mov qword ptr [{debug_marker}], 0x6666",   // Marker: pushed CS
        "mov rax, [r11 + 128]",   // RIP
        "push rax",
        "mov qword ptr [{debug_marker}], 0xDEAD",   // Marker: frame complete

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

        // Can't use push/pop here - would corrupt IRETQ frame!
        // IRETQ will pop RIP, CS, RFLAGS, RSP, SS from stack and switch to user mode
        "iretq",

        debug_fn = sym debug_before_iretq_fn,
        debug_marker = sym crate::arch::x86_64::syscall::DEBUG_MARKER,
    );
}

/// Debug function called before IRETQ (using bypass functions)
#[no_mangle]
extern "C" fn debug_before_iretq_fn(ctx_ptr: usize) {
    let ctx = unsafe { &*(ctx_ptr as *const Context) };
    crate::drivers::serial::write_str("[PRE-IRETQ] RIP=");
    crate::drivers::serial::write_hex(ctx.rip);
    crate::drivers::serial::write_str(", RSP=");
    crate::drivers::serial::write_hex(ctx.rsp);
    crate::drivers::serial::write_str(", CS=");
    crate::drivers::serial::write_hex(ctx.cs);
    crate::drivers::serial::write_str(", SS=");
    crate::drivers::serial::write_hex(ctx.ss);
    crate::drivers::serial::write_str(", RFLAGS=");
    crate::drivers::serial::write_hex(ctx.rflags);
    crate::drivers::serial::write_newline();

    let marker = crate::arch::x86_64::syscall::get_debug_marker();
    crate::drivers::serial::write_str("[PRE-IRETQ] DEBUG_MARKER before user mode: ");
    crate::drivers::serial::write_hex(marker);
    crate::drivers::serial::write_newline();
    crate::drivers::serial::write_str("[PRE-IRETQ] About to IRETQ to user mode...\n");

    // Verify CS/SS are correct before IRETQ
    if ctx.cs != 0x23 || ctx.ss != 0x1B {
        crate::drivers::serial::write_str("[PRE-IRETQ] CRITICAL: CS/SS corrupted before IRETQ!\n");
        crate::drivers::serial::write_str("  CS should be 0x23, got ");
        crate::drivers::serial::write_hex(ctx.cs);
        crate::drivers::serial::write_str("\n  SS should be 0x1B, got ");
        crate::drivers::serial::write_hex(ctx.ss);
        crate::drivers::serial::write_newline();
    }
}

/// Debug function called after IRETQ frame is built to dump stack
/// Arguments: RDI = current RSP (top of IRETQ frame)
#[no_mangle]
extern "C" fn debug_switch_iretq_frame(frame_rsp: usize) {
    unsafe {
        let frame = frame_rsp as *const u64;
        let rip = *frame.add(0);
        let cs = *frame.add(1);
        let rflags = *frame.add(2);
        let rsp = *frame.add(3);
        let ss = *frame.add(4);
        crate::serial_println!("[IRETQ-FRAME] RSP={:#x}", frame_rsp);
        crate::serial_println!("[IRETQ-FRAME] [RSP+0]  RIP:    {:#x}", rip);
        crate::serial_println!("[IRETQ-FRAME] [RSP+8]  CS:     {:#x}", cs);
        crate::serial_println!("[IRETQ-FRAME] [RSP+16] RFLAGS: {:#x}", rflags);
        crate::serial_println!("[IRETQ-FRAME] [RSP+24] RSP:    {:#x}", rsp);
        crate::serial_println!("[IRETQ-FRAME] [RSP+32] SS:     {:#x}", ss);
    }
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
        cs: 0x23,       // User code segment (0x20 | RPL=3)
        ss: 0x1B,       // User data segment (0x18 | RPL=3)
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
        assert_eq!(ctx.cs, 0x23); // User code segment
        assert_eq!(ctx.ss, 0x1B); // User data segment
    }
}
