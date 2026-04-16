//! WASM Streaming Service — no_std wire-protocol helpers.
//!
//! Ports the pure byte-manipulation parts of `a64-streamer::lib`
//! (see `tools/a64-streamer/src/lib.rs` for the canonical spec).
//! The upstream crate's framed-I/O wrappers depend on `std::io::{Read,
//! Write}`, which aren't available under Folkering's no_std userspace
//! — so this module works directly on byte slices and `alloc::Vec<u8>`,
//! leaving the actual TCP reads/writes to the async-TCP state machine
//! in `tcp.rs`.
//!
//! Framing:
//! ```text
//!   +------+-----------------+----------------+
//!   | type |  length (LE u32)|    payload     |
//!   |  1B  |       4B        |  `length` B    |
//!   +------+-----------------+----------------+
//! ```

use alloc::string::String;
use alloc::vec::Vec;

// ── Frame types (match a64-streamer) ────────────────────────────────

pub const FRAME_HELLO: u8 = 0x01;
pub const FRAME_CODE: u8 = 0x02;
pub const FRAME_DATA: u8 = 0x03;
pub const FRAME_EXEC: u8 = 0x04;
pub const FRAME_RESULT: u8 = 0x05;
pub const FRAME_ERROR: u8 = 0x06;
pub const FRAME_BYE: u8 = 0x07;

pub const FRAME_HEADER_LEN: usize = 5; // type(1) + length(4 LE)

/// Write a frame header + payload into a fresh Vec, ready to hand
/// to `tcp_send_async`.
pub fn build_frame(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    out.push(ty);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Parse a frame header (5 bytes) from the front of a buffer.
/// Returns (type, payload_len). Caller is responsible for checking
/// that the full payload has been received before calling
/// [`take_frame`].
pub fn peek_header(buf: &[u8]) -> Option<(u8, usize)> {
    if buf.len() < FRAME_HEADER_LEN {
        return None;
    }
    let ty = buf[0];
    let len = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    Some((ty, len))
}

/// Given a buffer that contains at least one complete frame, return
/// the frame's (type, payload) and the byte count consumed. Returns
/// `None` if the buffer is incomplete.
pub fn take_frame(buf: &[u8]) -> Option<(u8, &[u8], usize)> {
    let (ty, len) = peek_header(buf)?;
    let total = FRAME_HEADER_LEN + len;
    if buf.len() < total {
        return None;
    }
    let payload = &buf[FRAME_HEADER_LEN..total];
    Some((ty, payload, total))
}

// ── HELLO ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Helper {
    pub name: String,
    pub addr: u64,
}

#[derive(Debug, Clone)]
pub struct Hello {
    pub mem_base: u64,
    pub mem_size: u32,
    pub helpers: Vec<Helper>,
}

pub fn parse_hello(buf: &[u8]) -> Result<Hello, ProtoError> {
    let mut p = 0usize;
    let mem_base = read_u64(buf, &mut p)?;
    let mem_size = read_u32(buf, &mut p)?;
    let n = read_u32(buf, &mut p)? as usize;
    let mut helpers = Vec::with_capacity(n);
    for _ in 0..n {
        let name_len = read_u8(buf, &mut p)? as usize;
        if p + name_len > buf.len() {
            return Err(ProtoError::Truncated);
        }
        let name_bytes = &buf[p..p + name_len];
        let name = core::str::from_utf8(name_bytes)
            .map_err(|_| ProtoError::InvalidUtf8)?
            .into();
        p += name_len;
        let addr = read_u64(buf, &mut p)?;
        helpers.push(Helper { name, addr });
    }
    Ok(Hello { mem_base, mem_size, helpers })
}

impl Hello {
    /// Look up a helper by name. Used to get e.g. `helper_add_five`'s
    /// address for baking into a MOVZ+MOVK+BLR chain.
    pub fn helper(&self, name: &str) -> Option<u64> {
        self.helpers.iter().find(|h| h.name == name).map(|h| h.addr)
    }
}

// ── DATA ────────────────────────────────────────────────────────────

/// Build a DATA frame payload: u32 offset followed by the bytes to
/// write into the server's linear-memory buffer.
pub fn build_data_payload(offset: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

// ── RESULT ──────────────────────────────────────────────────────────

pub fn parse_result(buf: &[u8]) -> Result<i32, ProtoError> {
    if buf.len() != 4 {
        return Err(ProtoError::BadLength);
    }
    Ok(i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

// ── Helpers ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoError {
    Truncated,
    BadLength,
    InvalidUtf8,
}

fn read_u8(buf: &[u8], p: &mut usize) -> Result<u8, ProtoError> {
    if *p >= buf.len() {
        return Err(ProtoError::Truncated);
    }
    let v = buf[*p];
    *p += 1;
    Ok(v)
}

fn read_u32(buf: &[u8], p: &mut usize) -> Result<u32, ProtoError> {
    if *p + 4 > buf.len() {
        return Err(ProtoError::Truncated);
    }
    let v = u32::from_le_bytes([buf[*p], buf[*p + 1], buf[*p + 2], buf[*p + 3]]);
    *p += 4;
    Ok(v)
}

fn read_u64(buf: &[u8], p: &mut usize) -> Result<u64, ProtoError> {
    if *p + 8 > buf.len() {
        return Err(ProtoError::Truncated);
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[*p..*p + 8]);
    *p += 8;
    Ok(u64::from_le_bytes(b))
}
