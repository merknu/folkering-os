//! Real-time clock syscall wrapper

use crate::syscall::{syscall0, SYS_GET_TIME};

/// Get current Unix timestamp (seconds since 1970-01-01 00:00:00 UTC)
pub fn unix_timestamp() -> u64 {
    unsafe { syscall0(SYS_GET_TIME) }
}
