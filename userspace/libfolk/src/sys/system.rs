//! System information syscalls
//!
//! Functions for querying system state.

use crate::syscall::{syscall0, SYS_TASK_LIST, SYS_UPTIME};

/// Print a list of all tasks to the console
///
/// # Returns
/// The number of tasks in the system
pub fn task_list() -> u32 {
    unsafe { syscall0(SYS_TASK_LIST) as u32 }
}

/// Get the system uptime in milliseconds
pub fn uptime() -> u64 {
    unsafe { syscall0(SYS_UPTIME) }
}
