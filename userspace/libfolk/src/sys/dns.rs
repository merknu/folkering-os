//! DNS resolution syscall wrapper

use crate::syscall::{syscall2, SYS_NET_LOOKUP};

/// Resolve a domain name to an IPv4 address.
/// Returns Some((a, b, c, d)) on success, None on failure.
pub fn lookup(name: &str) -> Option<(u8, u8, u8, u8)> {
    let result = unsafe { syscall2(SYS_NET_LOOKUP, name.as_ptr() as u64, name.len() as u64) };
    if result == 0 {
        return None;
    }
    Some((
        (result & 0xFF) as u8,
        ((result >> 8) & 0xFF) as u8,
        ((result >> 16) & 0xFF) as u8,
        ((result >> 24) & 0xFF) as u8,
    ))
}
