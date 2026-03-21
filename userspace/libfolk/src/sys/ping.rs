//! Network ping syscall wrapper

use crate::syscall::{syscall1, SYS_PING};

/// Send an ICMP echo request to an IPv4 address.
/// Returns 0 on success.
pub fn ping(a: u8, b: u8, c: u8, d: u8) -> u64 {
    let packed = (a as u64) | ((b as u64) << 8) | ((c as u64) << 16) | ((d as u64) << 24);
    unsafe { syscall1(SYS_PING, packed) }
}
