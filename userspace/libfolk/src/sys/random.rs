//! Hardware RNG syscall wrapper

use crate::syscall::{syscall2, SYS_GET_RANDOM};

/// Fill a buffer with random bytes (RDRAND or RDTSC fallback).
/// Returns true on success.
pub fn fill_bytes(buf: &mut [u8]) -> bool {
    if buf.is_empty() {
        return true;
    }
    let result = unsafe { syscall2(SYS_GET_RANDOM, buf.as_mut_ptr() as u64, buf.len() as u64) };
    result == 0
}

/// Get a single random u64
pub fn random_u64() -> u64 {
    let mut buf = [0u8; 8];
    fill_bytes(&mut buf);
    u64::from_le_bytes(buf)
}

/// Get a single random u32
pub fn random_u32() -> u32 {
    let mut buf = [0u8; 4];
    fill_bytes(&mut buf);
    u32::from_le_bytes(buf)
}
