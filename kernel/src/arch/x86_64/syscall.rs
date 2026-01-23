//! System Call Interface
//!
//! Fast syscall entry using SYSCALL/SYSRET instructions (AMD64).

use x86_64::registers::model_specific::{Efer, EferFlags, LStar, Star};
use x86_64::structures::gdt::SegmentSelector;
use x86_64::{VirtAddr, PrivilegeLevel};

/// Syscall numbers
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syscall {
    /// Send IPC message
    IpcSend = 0,
    /// Receive IPC message
    IpcReceive = 1,
    /// Reply to IPC message
    IpcReply = 2,
    /// Create shared memory
    ShmemCreate = 3,
    /// Map shared memory
    ShmemMap = 4,
    /// Spawn new task
    Spawn = 5,
    /// Exit current task
    Exit = 6,
    /// Yield CPU
    Yield = 7,
}

/// Initialize syscall support
pub fn init() {
    // Enable SYSCALL/SYSRET extensions
    unsafe {
        Efer::update(|flags| {
            flags.insert(EferFlags::SYSTEM_CALL_EXTENSIONS);
        });
    }

    // Set syscall handler entry point
    let entry_addr = syscall_entry as u64;
    LStar::write(VirtAddr::new(entry_addr));

    // Configure STAR MSR for SYSCALL/SYSRET
    // STAR[47:32] = kernel CS for SYSCALL
    // STAR[63:48] = base for SYSRET (SYSRET adds +16 for CS, +8 for SS)
    let kernel_cs = super::gdt::kernel_code_selector();

    let star_value: u64 =
        ((kernel_cs.0 as u64) << 32) |  // Kernel CS for SYSCALL
        ((kernel_cs.0 as u64) << 48);   // Base for SYSRET

    unsafe {
        use x86_64::registers::model_specific::Msr;
        let mut star = Msr::new(0xC0000081); // IA32_STAR
        star.write(star_value);
    }
}

/// Syscall entry point
///
/// Simplified version without GS base switching for MVP.
/// Uses current stack (kernel stack from task structure).
///
/// # Register Convention (x86-64 SYSCALL)
/// - RAX: syscall number
/// - RDI: arg1
/// - RSI: arg2
/// - RDX: arg3
/// - R10: arg4 (RCX is used by SYSCALL for return address)
/// - R8:  arg5
/// - R9:  arg6
/// - RCX: saved RIP (by SYSCALL instruction)
/// - R11: saved RFLAGS (by SYSCALL instruction)
#[unsafe(naked)]
extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        // Save callee-saved registers (caller-saved are preserved by C ABI)
        "push rcx",         // Save return RIP
        "push r11",         // Save return RFLAGS
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",

        // Rearrange arguments for C ABI (System V AMD64):
        // C ABI expects: RDI, RSI, RDX, RCX, R8, R9
        // We have: RAX=syscall#, RDI=arg1, RSI=arg2, RDX=arg3, R10=arg4, R8=arg5, R9=arg6
        // Need to shift: syscall# → RDI, arg1 → RSI, arg2 → RDX, arg3 → RCX, arg4 → R8, arg5 → R9

        // Save RAX (syscall number) temporarily
        "push rax",

        // Shift arguments: arg6→arg7, arg5→arg6, arg4→arg5, arg3→arg4, arg2→arg3, arg1→arg2
        "mov r9, r8",       // arg5 → arg6
        "mov r8, r10",      // arg4 → arg5
        "mov rcx, rdx",     // arg3 → arg4
        "mov rdx, rsi",     // arg2 → arg3
        "mov rsi, rdi",     // arg1 → arg2

        // Pop syscall number into RDI (first argument)
        "pop rdi",

        // Call Rust syscall handler
        // Signature: fn(syscall_num: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64, arg6: u64) -> u64
        "call {handler}",

        // Return value in RAX (preserved automatically)

        // Restore callee-saved registers
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "pop r11",          // Restore RFLAGS
        "pop rcx",          // Restore RIP

        // Return to userspace
        "sysretq",

        handler = sym syscall_handler
    );
}

/// Syscall handler (called from assembly)
#[no_mangle]
extern "C" fn syscall_handler(
    syscall_num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    arg6: u64,
) -> u64 {
    match syscall_num {
        0 => syscall_ipc_send(arg1, arg2, arg3),
        1 => syscall_ipc_receive(arg1),
        2 => syscall_ipc_reply(arg1, arg2),
        3 => syscall_shmem_create(arg1),
        4 => syscall_shmem_map(arg1, arg2),
        5 => syscall_spawn(arg1, arg2),
        6 => syscall_exit(arg1),
        7 => syscall_yield(),
        _ => {
            crate::serial_println!("Invalid syscall: {}", syscall_num);
            u64::MAX // Return error
        }
    }
}

// ===== Syscall Implementations =====

fn syscall_ipc_send(target: u64, msg_ptr: u64, _flags: u64) -> u64 {
    use crate::ipc::{IpcMessage, ipc_send};

    // 1. Validate pointer
    if msg_ptr == 0 {
        return u64::MAX; // EINVAL
    }

    // 2. Copy message from userspace
    let msg = unsafe {
        // TODO: Validate that msg_ptr is in userspace
        // For now, trust the pointer
        core::ptr::read(msg_ptr as *const IpcMessage)
    };

    // 3. Call kernel IPC send
    let target_id = target as u32;
    match ipc_send(target_id, &msg) {
        Ok(reply) => {
            // Copy reply back to userspace
            unsafe {
                core::ptr::write(msg_ptr as *mut IpcMessage, reply);
            }
            0 // Success
        }
        Err(err) => {
            // Convert errno to u64
            err as u64
        }
    }
}

fn syscall_ipc_receive(msg_ptr: u64) -> u64 {
    use crate::ipc::{IpcMessage, ipc_receive};

    // 1. Validate pointer
    if msg_ptr == 0 {
        return u64::MAX; // EINVAL
    }

    // 2. Call kernel IPC receive (blocking)
    match ipc_receive() {
        Ok(msg) => {
            // Copy message to userspace
            unsafe {
                core::ptr::write(msg_ptr as *mut IpcMessage, msg);
            }
            0 // Success
        }
        Err(err) => err as u64,
    }
}

fn syscall_ipc_reply(request_ptr: u64, reply_payload_ptr: u64) -> u64 {
    use crate::ipc::{IpcMessage, ipc_reply};

    // 1. Validate pointers
    if request_ptr == 0 || reply_payload_ptr == 0 {
        return u64::MAX; // EINVAL
    }

    // 2. Copy request and reply payload from userspace
    let request = unsafe {
        core::ptr::read(request_ptr as *const IpcMessage)
    };

    let reply_payload = unsafe {
        core::ptr::read(reply_payload_ptr as *const [u64; 4])
    };

    // 3. Call kernel IPC reply
    match ipc_reply(&request, reply_payload) {
        Ok(()) => 0, // Success
        Err(err) => err as u64,
    }
}

fn syscall_shmem_create(size: u64) -> u64 {
    use crate::ipc::shared_memory::{shmem_create, ShmemPerms};

    // 1. Validate size (must be page-aligned and reasonable)
    if size == 0 || size > 1024 * 1024 * 1024 {
        // Max 1GB
        return u64::MAX; // EINVAL
    }

    // 2. Create shared memory region
    match shmem_create(size as usize, ShmemPerms::ReadWrite) {
        Ok(shmem_id) => shmem_id.get() as u64,
        Err(_) => u64::MAX, // Error
    }
}

fn syscall_shmem_map(shmem_id: u64, virt_addr: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_map;
    use core::num::NonZeroU32;

    // 1. Validate shmem_id and address
    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX, // EINVAL
    };

    if virt_addr == 0 {
        return u64::MAX; // EINVAL
    }

    // 2. Map shared memory into task's address space
    match shmem_map(id, virt_addr as usize) {
        Ok(()) => 0, // Success
        Err(_) => u64::MAX, // Error
    }
}

fn syscall_spawn(binary_ptr: u64, binary_len: u64) -> u64 {
    use crate::task::spawn;

    // 1. Validate parameters
    if binary_ptr == 0 || binary_len == 0 || binary_len > 100 * 1024 * 1024 {
        // Max 100MB binary
        return u64::MAX; // EINVAL
    }

    // 2. Create slice from userspace pointer
    let binary = unsafe {
        core::slice::from_raw_parts(binary_ptr as *const u8, binary_len as usize)
    };

    // 3. Spawn new task
    match spawn(binary, &[]) {
        Ok(task_id) => task_id as u64,
        Err(_) => u64::MAX, // Error
    }
}

fn syscall_exit(exit_code: u64) -> u64 {
    // TODO: Implement task exit
    crate::serial_println!("syscall: exit(code={})", exit_code);
    // Mark task as exited and never return
    loop {
        x86_64::instructions::hlt();
    }
}

fn syscall_yield() -> u64 {
    // Yield CPU to scheduler
    // TODO: Implement scheduler and call yield_cpu() here
    // For now, this is a no-op that successfully returns to user mode

    0 // Success
}
