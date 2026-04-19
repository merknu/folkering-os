//! WASM Streaming Service — wire protocol.
//!
//! Framing:
//! ```text
//!   +------+-----------------+----------------+
//!   | type |  length (LE u32)|    payload     |
//!   |  1B  |       4B        |  `length` B    |
//!   +------+-----------------+----------------+
//! ```
//!
//! Multi-byte integers in the payload are little-endian (matches
//! both x86_64 and AArch64 native ordering, no byte-swapping either
//! side of the link). Strings use a u8 length prefix — long enough
//! for helper symbol names, short enough to keep the parser branch-
//! less.
//!
//! Frame types:
//!
//! | Code | Name   | Direction      | Meaning                              |
//! |------|--------|----------------|--------------------------------------|
//! | 0x01 | HELLO  | server→client  | mem_base + helpers (post-connect)    |
//! | 0x02 | CODE   | client→server  | A64 bytes to mmap(PROT_EXEC)         |
//! | 0x03 | DATA   | client→server  | u32 offset, payload to mem_buffer    |
//! | 0x04 | EXEC   | client→server  | invoke current code, result as i32   |
//! | 0x05 | RESULT | server→client  | LE i32 exit code of last EXEC        |
//! | 0x06 | ERROR  | server→client  | u32 code + UTF-8 reason              |
//! | 0x07 | BYE    | either         | close cleanly                         |
//!
//! This crate is `std`-only (runs on Linux on both ends today; Pi side
//! will migrate to Folkering DAQ once that's ready). Single connection
//! at a time — per-connection state, no concurrency yet.

pub mod auth;

use std::io::{Read, Result as IoResult, Write};

// ── Frame types ─────────────────────────────────────────────────────

pub const FRAME_HELLO: u8 = 0x01;
pub const FRAME_CODE: u8 = 0x02;
pub const FRAME_DATA: u8 = 0x03;
pub const FRAME_EXEC: u8 = 0x04;
pub const FRAME_RESULT: u8 = 0x05;
pub const FRAME_ERROR: u8 = 0x06;
pub const FRAME_BYE: u8 = 0x07;

// Hard cap on a single frame payload — above this we reject, since
// the JIT programs we expect are measured in tens of KiB and the
// largest weight buffers we expect are a few MiB. 8 MiB covers our
// 256→512→256 ablation MLP (~1.05 MiB) with headroom for future
// transformer weight sets.
pub const MAX_FRAME_PAYLOAD: usize = 8 * 1024 * 1024;

pub const DEFAULT_PORT: u16 = 14712;

// ── Frame I/O ───────────────────────────────────────────────────────

/// Read one frame from `r`. Returns the frame type byte and the
/// payload. Blocks until the full frame arrives or the peer closes.
pub fn read_frame<R: Read>(r: &mut R) -> IoResult<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let ty = header[0];
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame payload too large: {len} bytes"),
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }
    Ok((ty, payload))
}

pub fn write_frame<W: Write>(w: &mut W, ty: u8, payload: &[u8]) -> IoResult<()> {
    let mut header = [0u8; 5];
    header[0] = ty;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&header)?;
    w.write_all(payload)?;
    Ok(())
}

// ── HELLO payload ───────────────────────────────────────────────────

/// One callable helper the server exposes by absolute address. The
/// client bakes these into JIT programs via MOVZ+MOVK+BLR.
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

pub fn serialize_hello(h: &Hello) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 4 + 4 + h.helpers.len() * 24);
    out.extend_from_slice(&h.mem_base.to_le_bytes());
    out.extend_from_slice(&h.mem_size.to_le_bytes());
    out.extend_from_slice(&(h.helpers.len() as u32).to_le_bytes());
    for help in &h.helpers {
        assert!(help.name.len() <= u8::MAX as usize, "helper name too long");
        out.push(help.name.len() as u8);
        out.extend_from_slice(help.name.as_bytes());
        out.extend_from_slice(&help.addr.to_le_bytes());
    }
    out
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
        let name = std::str::from_utf8(&buf[p..p + name_len])
            .map_err(|_| ProtoError::InvalidUtf8)?
            .to_string();
        p += name_len;
        let addr = read_u64(buf, &mut p)?;
        helpers.push(Helper { name, addr });
    }
    Ok(Hello { mem_base, mem_size, helpers })
}

// ── DATA payload ────────────────────────────────────────────────────

/// Build a DATA frame payload: u32 offset followed by the bytes to
/// write into the server's linear-memory buffer.
pub fn serialize_data(offset: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(bytes);
    out
}

pub fn parse_data(buf: &[u8]) -> Result<(u32, &[u8]), ProtoError> {
    if buf.len() < 4 {
        return Err(ProtoError::Truncated);
    }
    let offset = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    Ok((offset, &buf[4..]))
}

// ── RESULT payload ──────────────────────────────────────────────────

pub fn serialize_result(rv: i32) -> Vec<u8> {
    rv.to_le_bytes().to_vec()
}

pub fn parse_result(buf: &[u8]) -> Result<i32, ProtoError> {
    if buf.len() != 4 {
        return Err(ProtoError::BadLength);
    }
    Ok(i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

// ── ERROR payload ───────────────────────────────────────────────────

pub fn serialize_error(code: u32, reason: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + reason.len());
    out.extend_from_slice(&code.to_le_bytes());
    out.extend_from_slice(reason.as_bytes());
    out
}

pub fn parse_error(buf: &[u8]) -> Result<(u32, String), ProtoError> {
    if buf.len() < 4 {
        return Err(ProtoError::Truncated);
    }
    let code = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let reason = String::from_utf8(buf[4..].to_vec()).map_err(|_| ProtoError::InvalidUtf8)?;
    Ok((code, reason))
}

// ── Helpers ────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ProtoError {
    Truncated,
    BadLength,
    InvalidUtf8,
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoError::Truncated => write!(f, "frame truncated"),
            ProtoError::BadLength => write!(f, "frame length mismatch"),
            ProtoError::InvalidUtf8 => write!(f, "invalid UTF-8 in string field"),
        }
    }
}

impl std::error::Error for ProtoError {}

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

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, FRAME_CODE, b"\x01\x02\x03").unwrap();
        let mut cursor = &buf[..];
        let (ty, payload) = read_frame(&mut cursor).unwrap();
        assert_eq!(ty, FRAME_CODE);
        assert_eq!(payload, b"\x01\x02\x03");
    }

    #[test]
    fn hello_roundtrip() {
        let h = Hello {
            mem_base: 0x1234_5678_9abc_def0,
            mem_size: 0x1_0000,
            helpers: vec![
                Helper { name: "add5".into(), addr: 0xDEAD_BEEF },
                Helper { name: "mul2".into(), addr: 0xCAFE_BABE_1234 },
            ],
        };
        let bytes = serialize_hello(&h);
        let back = parse_hello(&bytes).unwrap();
        assert_eq!(back.mem_base, h.mem_base);
        assert_eq!(back.mem_size, h.mem_size);
        assert_eq!(back.helpers.len(), 2);
        assert_eq!(back.helpers[0].name, "add5");
        assert_eq!(back.helpers[0].addr, 0xDEAD_BEEF);
        assert_eq!(back.helpers[1].name, "mul2");
    }

    #[test]
    fn data_roundtrip() {
        let payload = serialize_data(16, &[0xDE, 0xAD, 0xBE, 0xEF]);
        let (off, body) = parse_data(&payload).unwrap();
        assert_eq!(off, 16);
        assert_eq!(body, &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn result_roundtrip() {
        assert_eq!(parse_result(&serialize_result(-42)).unwrap(), -42);
        assert_eq!(parse_result(&serialize_result(42)).unwrap(), 42);
    }

    #[test]
    fn parse_hello_rejects_truncation() {
        assert!(parse_hello(&[0u8; 7]).is_err());
    }

    #[test]
    fn oversize_frame_rejected() {
        // Craft a header claiming 2 MiB.
        let mut buf = vec![FRAME_CODE];
        buf.extend_from_slice(&(2u32 * 1024 * 1024).to_le_bytes());
        let mut cursor = &buf[..];
        assert!(read_frame(&mut cursor).is_err());
    }
}
