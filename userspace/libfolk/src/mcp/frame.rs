//! COBS Framing + CRC-16 + Layer 4 Transport Header
//!
//! Wire format:
//!   [COBS-encoded { FrameHeader(8) + Postcard payload + CRC-16(2) }] [0x00 sentinel]
//!
//! FrameHeader is always the first 8 bytes of the decoded payload:
//!   session_id: u32 LE — zombie session killer
//!   seq_id:     u32 LE — monotonic, for ACK/NACK correlation

use serde::{Serialize, de::DeserializeOwned};
use crate::mcp::types::FrameHeader;

/// Maximum encoded frame size (header + payload + CRC + COBS overhead + sentinel)
pub const MAX_FRAME_SIZE: usize = 4096;

/// CRC-16/CCITT-FALSE
const CRC16_POLY: u16 = 0x1021;

pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ CRC16_POLY;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Encode: FrameHeader + message → COBS frame with CRC.
/// Returns bytes written to out_buf (ends with 0x00 sentinel).
pub fn encode<T: Serialize>(header: &FrameHeader, msg: &T, out_buf: &mut [u8]) -> Option<usize> {
    // Serialize header + payload into temp buffer
    let mut raw = [0u8; 3840];
    let mut pos = 0;

    // Header: session_id(4 LE) + seq_id(4 LE) — fixed format, no Postcard varint
    let sid = header.session_id.to_le_bytes();
    let seq = header.seq_id.to_le_bytes();
    raw[0..4].copy_from_slice(&sid);
    raw[4..8].copy_from_slice(&seq);
    pos = 8;

    // Payload: Postcard-serialized message
    let payload = postcard::to_slice(msg, &mut raw[pos..]).ok()?;
    pos += payload.len();

    // CRC-16 over header + payload
    let crc = crc16(&raw[..pos]);
    if pos + 2 > raw.len() { return None; }
    raw[pos] = (crc & 0xFF) as u8;
    raw[pos + 1] = (crc >> 8) as u8;
    pos += 2;

    // COBS encode
    let cobs_len = cobs_encode(&raw[..pos], out_buf)?;
    if cobs_len >= out_buf.len() { return None; }
    out_buf[cobs_len] = 0x00;
    Some(cobs_len + 1)
}

/// Decode: COBS frame → (FrameHeader, deserialized message).
/// `frame` should NOT include the trailing 0x00 sentinel.
pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Option<(FrameHeader, T)> {
    if frame.len() > 8192 { return None; }
    let mut decoded = [0u8; 8192];
    let decoded_len = cobs_decode(frame, &mut decoded)?;

    // Minimum: 8 (header) + 1 (payload) + 2 (CRC)
    if decoded_len < 11 { return None; }

    // Verify CRC
    let payload_len = decoded_len - 2;
    let received_crc = (decoded[payload_len] as u16) | ((decoded[payload_len + 1] as u16) << 8);
    if crc16(&decoded[..payload_len]) != received_crc { return None; }

    // Parse header (fixed 8 bytes LE)
    let session_id = u32::from_le_bytes([decoded[0], decoded[1], decoded[2], decoded[3]]);
    let seq_id = u32::from_le_bytes([decoded[4], decoded[5], decoded[6], decoded[7]]);
    let header = FrameHeader { session_id, seq_id };

    // Deserialize payload (after header)
    let msg: T = postcard::from_bytes(&decoded[8..payload_len]).ok()?;
    Some((header, msg))
}

/// Encode a small ACK/NACK frame (no Postcard, just header + 1 byte type tag).
/// Used for fast transport-level acknowledgments.
pub fn encode_ack(header: &FrameHeader, out_buf: &mut [u8]) -> Option<usize> {
    // Minimal frame: header(8) + type_tag(1) + CRC(2) → COBS → sentinel
    let mut raw = [0u8; 16];
    raw[0..4].copy_from_slice(&header.session_id.to_le_bytes());
    raw[4..8].copy_from_slice(&header.seq_id.to_le_bytes());
    // We still use Postcard for the ACK enum so the decoder works uniformly
    // McpResponse::Ack is a simple enum variant
    let ack = crate::mcp::types::McpResponse::Ack;
    let payload = postcard::to_slice(&ack, &mut raw[8..]).ok()?;
    let pos = 8 + payload.len();
    let crc = crc16(&raw[..pos]);
    raw[pos] = (crc & 0xFF) as u8;
    raw[pos + 1] = (crc >> 8) as u8;
    let total = pos + 2;
    let cobs_len = cobs_encode(&raw[..total], out_buf)?;
    if cobs_len >= out_buf.len() { return None; }
    out_buf[cobs_len] = 0x00;
    Some(cobs_len + 1)
}

// ── COBS ────────────────────────────────────────────────────────────────

fn cobs_encode(input: &[u8], out: &mut [u8]) -> Option<usize> {
    if input.is_empty() { return Some(0); }
    let mut read_idx = 0;
    let mut write_idx = 1;
    let mut code_idx = 0;
    let mut code: u8 = 1;
    while read_idx < input.len() {
        if write_idx >= out.len() { return None; }
        if input[read_idx] == 0x00 {
            out[code_idx] = code;
            code = 1;
            code_idx = write_idx;
            write_idx += 1;
        } else {
            out[write_idx] = input[read_idx];
            write_idx += 1;
            code += 1;
            if code == 0xFF {
                out[code_idx] = code;
                code = 1;
                code_idx = write_idx;
                write_idx += 1;
            }
        }
        read_idx += 1;
    }
    if write_idx >= out.len() { return None; }
    out[code_idx] = code;
    Some(write_idx)
}

fn cobs_decode(input: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut read_idx = 0;
    let mut write_idx = 0;
    while read_idx < input.len() {
        let code = input[read_idx];
        if code == 0 { return None; }
        read_idx += 1;
        for _ in 1..code {
            if read_idx >= input.len() || write_idx >= out.len() { return None; }
            out[write_idx] = input[read_idx];
            read_idx += 1;
            write_idx += 1;
        }
        if code < 0xFF && read_idx < input.len() {
            if write_idx >= out.len() { return None; }
            out[write_idx] = 0x00;
            write_idx += 1;
        }
    }
    Some(write_idx)
}

/// Streaming frame accumulator
pub struct FrameAccumulator<const N: usize> {
    buf: [u8; N],
    pos: usize,
}

impl<const N: usize> FrameAccumulator<N> {
    pub const fn new() -> Self { Self { buf: [0; N], pos: 0 } }
    pub fn push(&mut self, byte: u8) -> Option<&[u8]> {
        if byte == 0x00 {
            if self.pos > 0 { return Some(&self.buf[..self.pos]); }
            return None;
        }
        if self.pos < N { self.buf[self.pos] = byte; self.pos += 1; }
        None
    }
    pub fn reset(&mut self) { self.pos = 0; }
}
