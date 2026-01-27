//! Task management syscalls
//!
//! Functions for controlling the current task's execution.

use crate::syscall::{syscall0, syscall1, syscall2, SYS_EXIT, SYS_YIELD, SYS_GET_PID, SYS_SPAWN};

/// Exit the current task with the given exit code
///
/// This function never returns.
pub fn exit(code: u64) -> ! {
    unsafe { syscall1(SYS_EXIT, code) };
    // Should never reach here, but just in case
    loop {
        core::hint::spin_loop();
    }
}

/// Voluntarily yield the CPU to other tasks
///
/// This allows the scheduler to run other tasks. The current task
/// will be resumed later when the scheduler selects it again.
pub fn yield_cpu() {
    unsafe { syscall0(SYS_YIELD) };
}

/// Get the current task's process ID
pub fn get_pid() -> u32 {
    unsafe { syscall0(SYS_GET_PID) as u32 }
}

/// Spawn a new task from an ELF binary
///
/// # Arguments
/// * `binary` - The ELF binary data
///
/// # Returns
/// * `Some(task_id)` - The new task's ID on success
/// * `None` - On failure
pub fn spawn(binary: &[u8]) -> Option<u32> {
    let ptr = binary.as_ptr() as u64;
    let len = binary.len() as u64;
    let ret = unsafe { syscall2(SYS_SPAWN, ptr, len) };
    if ret == u64::MAX {
        None
    } else {
        Some(ret as u32)
    }
}
