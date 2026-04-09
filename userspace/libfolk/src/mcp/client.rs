//! MCP Client — Layer 4 Transport with Session Multiplexing
//!
//! Features:
//! - Session ID: random u32 generated at boot, rejects zombie proxy data
//! - Sequence IDs: monotonic counter for ACK/NACK correlation
//! - Auto-ACK: sends ACK for every valid received frame
//! - WASM chunking: reassembles multi-chunk WASM binaries

use crate::mcp::types::*;
use crate::mcp::frame;

/// Session state — initialized once at boot
static mut SESSION_ID: u32 = 0;
static mut SEQ_COUNTER: u32 = 0;

/// WASM reassembly buffer
static mut WASM_CHUNKS_RECEIVED: u16 = 0;
static mut WASM_CHUNKS_TOTAL: u16 = 0;
static mut WASM_ASSEMBLY_BUF: [u8; 65536] = [0u8; 65536]; // 64KB max WASM
static mut WASM_ASSEMBLY_LEN: usize = 0;

/// Static receive buffer (avoids mmap per-poll)
static mut POLL_BUF: [u8; 16384] = [0u8; 16384];

/// Initialize the MCP session. Call once at compositor boot.
/// Uses RTC + uptime as entropy source for session_id.
/// Activates COM2 async RX polling for incoming MCP frames.
pub fn init_session() {
    let rtc = crate::sys::get_rtc_packed();
    let uptime = crate::sys::uptime();
    let sid = (rtc as u32) ^ ((uptime as u32).wrapping_mul(2654435761));
    unsafe { SESSION_ID = sid; }
    activate_rx(); // Start background COM2 RX draining
}

fn next_seq() -> u32 {
    unsafe { SEQ_COUNTER += 1; SEQ_COUNTER }
}

fn header(seq: u32) -> FrameHeader {
    FrameHeader { session_id: unsafe { SESSION_ID }, seq_id: seq }
}

/// Initialize COM2 async RX polling. Call once after init_session().
/// This activates background RX draining without sending any data.
pub fn activate_rx() {
    // Send empty data to activate async mode (resets ring + enables polling)
    crate::sys::com2_async_send(&[]);
}

/// Send an MCP response with transport header.
/// Uses raw COM2 TX — does NOT reset the async RX ring buffer.
pub fn send(msg: &McpResponse) -> Option<u32> {
    let seq = next_seq();
    let mut wire_buf = [0u8; frame::MAX_FRAME_SIZE];
    let len = frame::encode(&header(seq), msg, &mut wire_buf)?;
    crate::sys::com2_write_raw(&wire_buf[..len]);
    Some(seq)
}

/// Send ACK for a received frame — uses raw COM2 TX (does NOT reset async RX).
fn send_ack(received_seq: u32) {
    let mut wire_buf = [0u8; 64];
    let hdr = header(received_seq);
    if let Some(len) = frame::encode(&hdr, &McpResponse::Ack, &mut wire_buf) {
        crate::sys::com2_write_raw(&wire_buf[..len]);
    }
}

/// Send NACK for a received frame.
fn send_nack(received_seq: u32, reason: u8) {
    let mut wire_buf = [0u8; 64];
    let hdr = header(received_seq);
    if let Some(len) = frame::encode(&hdr, &McpResponse::Nack { reason }, &mut wire_buf) {
        crate::sys::com2_write_raw(&wire_buf[..len]);
    }
}

/// Send a chat request to the LLM proxy.
pub fn send_chat(prompt: &str) -> Option<u32> {
    let mut prompt_vec = heapless::Vec::<u8, MAX_PROMPT_LEN>::new();
    let bytes = prompt.as_bytes();
    let copy_len = bytes.len().min(MAX_PROMPT_LEN);
    prompt_vec.extend_from_slice(&bytes[..copy_len]).ok();
    send(&McpResponse::ChatRequest { prompt: prompt_vec })
}

/// Send a time sync request.
pub fn send_time_sync() -> bool {
    send(&McpResponse::TimeSyncRequest).is_some()
}

/// Send a WASM generation request.
pub fn send_wasm_gen(description: &str) -> bool {
    let mut desc = heapless::String::<256>::new();
    let _ = desc.push_str(&description[..description.len().min(255)]);
    send(&McpResponse::WasmGenRequest { description: desc }).is_some()
}

/// Get current session ID (for logging/debugging).
pub fn session_id() -> u32 {
    unsafe { SESSION_ID }
}

/// Poll for a complete MCP frame from the proxy (non-blocking).
/// Validates session_id, sends ACK, handles WASM chunk reassembly.
pub fn poll() -> Option<McpRequest> {
    let frame_len = crate::sys::com2_async_poll()?;
    let buf = unsafe { &mut POLL_BUF };
    let read_len = crate::sys::com2_async_read(buf);
    if read_len == 0 || frame_len == 0 { return None; }

    let safe_len = frame_len.min(read_len).min(buf.len());

    // Try to decode — if it fails, send NACK
    let (hdr, msg) = match frame::decode::<McpRequest>(&buf[..safe_len]) {
        Some(result) => result,
        None => {
            // Decode failed (CRC mismatch or parse error) — send NACK
            // We don't know the seq_id, so use 0
            send_nack(0, crate::mcp::types::nack::PARSE_ERROR);
            return None;
        }
    };

    // Session ID validation — reject zombie proxy data
    let my_sid = unsafe { SESSION_ID };
    if my_sid != 0 && hdr.session_id != 0 && hdr.session_id != my_sid {
        send_nack(hdr.seq_id, crate::mcp::types::nack::SESSION_MISMATCH);
        return None;
    }

    // Valid frame — send ACK (unless it's an ACK itself, to avoid infinite ACK loop)
    match &msg {
        McpRequest::Ack | McpRequest::Nack { .. } => {} // Don't ACK an ACK
        _ => send_ack(hdr.seq_id),
    }

    // Handle WASM chunk reassembly transparently
    match &msg {
        McpRequest::WasmChunk { total_chunks, chunk_index, data } => {
            unsafe {
                if *chunk_index == 0 {
                    WASM_CHUNKS_TOTAL = *total_chunks;
                    WASM_CHUNKS_RECEIVED = 0;
                    WASM_ASSEMBLY_LEN = 0;
                }
                let offset = WASM_ASSEMBLY_LEN;
                let copy_len = data.len().min(WASM_ASSEMBLY_BUF.len() - offset);
                WASM_ASSEMBLY_BUF[offset..offset + copy_len].copy_from_slice(&data[..copy_len]);
                WASM_ASSEMBLY_LEN += copy_len;
                WASM_CHUNKS_RECEIVED += 1;

                if WASM_CHUNKS_RECEIVED >= WASM_CHUNKS_TOTAL {
                    return Some(msg);
                }
            }
            return None; // Wait for more chunks
        }
        _ => {}
    }

    Some(msg)
}

/// Check if WASM assembly is complete.
pub fn wasm_assembly_complete() -> bool {
    unsafe { WASM_CHUNKS_TOTAL > 0 && WASM_CHUNKS_RECEIVED >= WASM_CHUNKS_TOTAL }
}

/// Get the assembled WASM binary.
pub fn wasm_assembly_data() -> &'static [u8] {
    unsafe { &WASM_ASSEMBLY_BUF[..WASM_ASSEMBLY_LEN] }
}

/// Reset WASM assembly state.
pub fn wasm_assembly_reset() {
    unsafe {
        WASM_CHUNKS_TOTAL = 0;
        WASM_CHUNKS_RECEIVED = 0;
        WASM_ASSEMBLY_LEN = 0;
    }
}
