//! System information syscalls
//!
//! Functions for querying system state.

use crate::syscall::{syscall0, syscall2, SYS_TASK_LIST, SYS_UPTIME, SYS_TASK_LIST_DETAILED};

/// Get the number of tasks in the system
///
/// # Returns
/// The number of tasks in the system
pub fn task_list() -> u32 {
    unsafe { syscall0(SYS_TASK_LIST) as u32 }
}

/// Fill a buffer with detailed task info
///
/// Buffer format per task (32 bytes):
///   [task_id: u32][state: u32][name: [u8; 16]][cpu_time_ms: u64]
///
/// # Arguments
/// * `buf` - Mutable slice to fill with task data (must be count * 32 bytes)
///
/// # Returns
/// The number of tasks written to the buffer
pub fn task_list_detailed(buf: &mut [u8]) -> u32 {
    unsafe { syscall2(SYS_TASK_LIST_DETAILED, buf.as_mut_ptr() as u64, buf.len() as u64) as u32 }
}

/// Get the system uptime in milliseconds
pub fn uptime() -> u64 {
    unsafe { syscall0(SYS_UPTIME) }
}
