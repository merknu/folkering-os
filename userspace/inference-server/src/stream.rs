//! `TokenRing` — shared-memory streaming buffer between inference server and compositor.
//!
//! ULTRA 37: AtomicU32 for write_idx and status (cross-task shared memory).
//! ULTRA 40: LINEAR 16KB buffer — no wrapping, write_idx grows monotonically.

/// Token streaming ring buffer.
#[repr(C)]
pub struct TokenRing {
    /// Bytes written so far (inference: Release, compositor: Acquire)
    pub write_idx: core::sync::atomic::AtomicU32,
    /// 0 = generating, 1 = done, 2 = error
    pub status: core::sync::atomic::AtomicU32,
    /// Tool feedback: 0=none, 1=paused(waiting), 2=result_ready
    pub tool_state: core::sync::atomic::AtomicU32,
    /// Byte length of tool result written to data[write_idx..]
    pub tool_result_len: core::sync::atomic::AtomicU32,
    /// UTF-8 text data, linear (no wrapping)
    pub data: [u8; 16368],
}
// Total: 16 + 16368 = 16384 bytes = 4 pages

/// Maximum writable data in TokenRing (ULTRA 48: prevent overflow)
pub const RING_DATA_MAX: usize = 16368;

/// Return the length of the longest valid UTF-8 prefix (ULTRA 47).
#[allow(dead_code)]
pub fn valid_utf8_prefix_len(data: &[u8]) -> usize {
    let mut len = data.len();
    while len > 0 {
        if core::str::from_utf8(&data[..len]).is_ok() {
            return len;
        }
        len -= 1;
    }
    0
}
