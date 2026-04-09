//! WebSocket Client — Persistent streaming connections for Folkering OS
//!
//! Built on top of the existing smoltcp TCP stack. Provides:
//! - HTTP Upgrade handshake (RFC 6455)
//! - Frame encoding/decoding with client masking
//! - Non-blocking receive polling (for WASM frame-based apps)
//! - Text and Binary opcode support
//! - Close/Ping/Pong control frames
//!
//! Used by WASM apps (PolyglotChat, PromptLab) for streaming AI tokens
//! in real-time instead of blocking HTTP round-trips.
//!
//! # Architecture
//! ```text
//! WASM app → folk_ws_send() → syscall 0xA1 → ws_send()
//!                                                ↓
//!                               smoltcp TCP socket → VirtIO-Net → Host
//!                                                ↑
//! WASM app ← folk_ws_poll_recv() ← syscall 0xA2 ← ws_poll_recv()
//! ```

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use alloc::string::String;
use core::sync::atomic::{AtomicU8, Ordering};

use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::IpAddress;

use super::{FolkeringDevice, NetState, NET_STATE};
use crate::drivers::rng as hw_rng;

// ── WebSocket Constants ─────────────────────────────────────────────────

const WS_OPCODE_TEXT: u8 = 0x01;
const WS_OPCODE_BINARY: u8 = 0x02;
const WS_OPCODE_CLOSE: u8 = 0x08;
const WS_OPCODE_PING: u8 = 0x09;
const WS_OPCODE_PONG: u8 = 0x0A;

const WS_FIN_BIT: u8 = 0x80;
const WS_MASK_BIT: u8 = 0x80;

/// Max slots for concurrent WebSocket connections
const MAX_WS_SLOTS: usize = 4;

/// Receive buffer size per connection (8KB ring)
const RECV_BUF_SIZE: usize = 8192;

// ── Connection State ────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Clone, Copy, PartialEq)]
enum WsState {
    Free = 0,
    Connecting = 1,
    Open = 2,
    Closing = 3,
    Closed = 4,
    Error = 5,
}

/// A single WebSocket connection slot
struct WsSlot {
    state: WsState,
    tcp_handle: Option<smoltcp::iface::SocketHandle>,
    /// Receive ring buffer — filled by poll, drained by ws_poll_recv
    recv_buf: [u8; RECV_BUF_SIZE],
    recv_head: usize, // write position
    recv_tail: usize, // read position
    /// Partial frame assembly buffer
    frame_buf: Vec<u8>,
    /// Partial frame state
    frame_expected: usize, // 0 = reading header
}

impl WsSlot {
    const fn empty() -> Self {
        Self {
            state: WsState::Free,
            tcp_handle: None,
            recv_buf: [0u8; RECV_BUF_SIZE],
            recv_head: 0,
            recv_tail: 0,
            frame_buf: Vec::new(),
            frame_expected: 0,
        }
    }

    fn recv_available(&self) -> usize {
        if self.recv_head >= self.recv_tail {
            self.recv_head - self.recv_tail
        } else {
            RECV_BUF_SIZE - self.recv_tail + self.recv_head
        }
    }

    fn recv_push(&mut self, data: &[u8]) {
        for &b in data {
            let next = (self.recv_head + 1) % RECV_BUF_SIZE;
            if next == self.recv_tail { break; } // Full
            self.recv_buf[self.recv_head] = b;
            self.recv_head = next;
        }
    }

    fn recv_pop(&mut self, buf: &mut [u8]) -> usize {
        let mut count = 0;
        while count < buf.len() && self.recv_tail != self.recv_head {
            buf[count] = self.recv_buf[self.recv_tail];
            self.recv_tail = (self.recv_tail + 1) % RECV_BUF_SIZE;
            count += 1;
        }
        count
    }
}

/// Global WebSocket connection slots
static mut WS_SLOTS: [WsSlot; MAX_WS_SLOTS] = [
    WsSlot::empty(), WsSlot::empty(),
    WsSlot::empty(), WsSlot::empty(),
];

// ── WebSocket Frame Codec ───────────────────────────────────────────────

/// Encode a WebSocket frame (client-side, always masked).
fn encode_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(14 + payload.len());

    // Byte 0: FIN + opcode
    frame.push(WS_FIN_BIT | opcode);

    // Byte 1: MASK + payload length
    let len = payload.len();
    if len < 126 {
        frame.push(WS_MASK_BIT | len as u8);
    } else if len <= 65535 {
        frame.push(WS_MASK_BIT | 126);
        frame.push((len >> 8) as u8);
        frame.push((len & 0xFF) as u8);
    } else {
        frame.push(WS_MASK_BIT | 127);
        for i in (0..8).rev() {
            frame.push(((len >> (i * 8)) & 0xFF) as u8);
        }
    }

    // Masking key (4 random bytes)
    let mask_key = [
        hw_rng::random_u64() as u8,
        (hw_rng::random_u64() >> 8) as u8,
        (hw_rng::random_u64() >> 16) as u8,
        (hw_rng::random_u64() >> 24) as u8,
    ];
    frame.extend_from_slice(&mask_key);

    // Masked payload
    for (i, &b) in payload.iter().enumerate() {
        frame.push(b ^ mask_key[i % 4]);
    }

    frame
}

/// Decoded WebSocket frame
struct DecodedFrame {
    opcode: u8,
    payload: Vec<u8>,
    header_size: usize, // bytes consumed from input
}

/// Try to decode a WebSocket frame from a byte buffer.
/// Returns None if not enough data yet (need more bytes).
fn try_decode_frame(data: &[u8]) -> Option<DecodedFrame> {
    if data.len() < 2 { return None; }

    let _fin = data[0] & 0x80 != 0;
    let opcode = data[0] & 0x0F;
    let masked = data[1] & 0x80 != 0;
    let mut payload_len = (data[1] & 0x7F) as usize;
    let mut offset = 2;

    if payload_len == 126 {
        if data.len() < 4 { return None; }
        payload_len = ((data[2] as usize) << 8) | (data[3] as usize);
        offset = 4;
    } else if payload_len == 127 {
        if data.len() < 10 { return None; }
        payload_len = 0;
        for i in 0..8 {
            payload_len = (payload_len << 8) | data[2 + i] as usize;
        }
        offset = 10;
    }

    let mask_key = if masked {
        if data.len() < offset + 4 { return None; }
        let mk = [data[offset], data[offset+1], data[offset+2], data[offset+3]];
        offset += 4;
        Some(mk)
    } else {
        None
    };

    if data.len() < offset + payload_len { return None; }

    let mut payload = Vec::from(&data[offset..offset + payload_len]);

    // Unmask if needed (server→client frames are usually NOT masked per RFC)
    if let Some(mk) = mask_key {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mk[i % 4];
        }
    }

    Some(DecodedFrame {
        opcode,
        payload,
        header_size: offset + payload_len,
    })
}

// ── HTTP Upgrade Handshake ──────────────────────────────────────────────

/// Generate the Sec-WebSocket-Key (16 random bytes, base64 encoded)
fn generate_ws_key() -> [u8; 24] {
    let mut raw = [0u8; 16];
    hw_rng::fill_bytes(&mut raw);

    // Base64 encode (simple, no_std compatible)
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = [b'A'; 24]; // 16 bytes → 24 base64 chars (with padding)
    let mut oi = 0;
    let mut i = 0;
    while i < 15 { // Process 5 groups of 3 bytes = 15 bytes
        let b0 = raw[i] as u32;
        let b1 = raw[i+1] as u32;
        let b2 = raw[i+2] as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out[oi] = B64[((triple >> 18) & 0x3F) as usize]; oi += 1;
        out[oi] = B64[((triple >> 12) & 0x3F) as usize]; oi += 1;
        out[oi] = B64[((triple >> 6) & 0x3F) as usize]; oi += 1;
        out[oi] = B64[(triple & 0x3F) as usize]; oi += 1;
        i += 3;
    }
    // Last byte (16th): pad with ==
    let b0 = raw[15] as u32;
    out[20] = B64[((b0 >> 2) & 0x3F) as usize];
    out[21] = B64[((b0 << 4) & 0x3F) as usize];
    out[22] = b'=';
    out[23] = b'=';
    out
}

/// Build the HTTP Upgrade request
fn build_upgrade_request(host: &str, path: &str, key: &[u8; 24]) -> Vec<u8> {
    let mut req = Vec::with_capacity(256);
    req.extend_from_slice(b"GET ");
    req.extend_from_slice(path.as_bytes());
    req.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(b"\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: ");
    req.extend_from_slice(key);
    req.extend_from_slice(b"\r\nSec-WebSocket-Version: 13\r\n\r\n");
    req
}

// ── Public API ──────────────────────────────────────────────────────────

/// Connect to a WebSocket server. Returns slot ID (0-3) or error.
///
/// `ip` — Server IPv4 address
/// `port` — Server port (typically 80 for ws://, 443 for wss://)
/// `host` — Host header value
/// `path` — WebSocket endpoint path (e.g., "/ws" or "/v1/stream")
pub fn ws_connect(ip: [u8; 4], port: u16, host: &str, path: &str) -> Result<u8, &'static str> {
    // Find a free slot
    let slot_id = unsafe {
        WS_SLOTS.iter().position(|s| s.state == WsState::Free)
            .ok_or("no free WebSocket slots")?
    };

    let mut guard = NET_STATE.lock();
    let state = guard.as_mut().ok_or("no network")?;
    if !state.has_ip { return Err("no IP"); }

    // Create TCP socket
    let tcp_rx = tcp::SocketBuffer::new(vec![0u8; 16384]);
    let tcp_tx = tcp::SocketBuffer::new(vec![0u8; 8192]);
    let tcp_socket = tcp::Socket::new(tcp_rx, tcp_tx);
    let tcp_handle = state.sockets.add(tcp_socket);

    let remote = IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]));

    unsafe { core::arch::asm!("sti"); }

    // TCP connect
    {
        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        socket.connect(state.iface.context(), (remote, port), super::tls::next_port())
            .map_err(|_| "TCP connect failed")?;
    }

    // Wait for TCP established
    let start = super::tls::tsc_ms();
    loop {
        let now = Instant::from_millis(super::tls::tsc_ms());
        let mut dev = FolkeringDevice;
        state.iface.poll(now, &mut dev, &mut state.sockets);

        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        if socket.may_send() { break; }
        if !socket.is_active() {
            state.sockets.remove(tcp_handle);
            return Err("TCP refused");
        }
        if super::tls::tsc_ms() - start > 10_000 {
            state.sockets.remove(tcp_handle);
            return Err("TCP timeout");
        }
        for _ in 0..1000 { core::hint::spin_loop(); }
    }

    // Send HTTP Upgrade handshake
    let key = generate_ws_key();
    let upgrade_req = build_upgrade_request(host, path, &key);

    {
        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        socket.send_slice(&upgrade_req).map_err(|_| "send upgrade failed")?;
    }

    // Poll until we get the upgrade response (101 Switching Protocols)
    let mut resp_buf = vec![0u8; 512];
    let mut resp_len = 0;
    let start = super::tls::tsc_ms();

    loop {
        let now = Instant::from_millis(super::tls::tsc_ms());
        let mut dev = FolkeringDevice;
        state.iface.poll(now, &mut dev, &mut state.sockets);

        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        if socket.can_recv() {
            match socket.recv_slice(&mut resp_buf[resp_len..]) {
                Ok(n) => {
                    resp_len += n;
                    // Check for end of HTTP response headers
                    if resp_len >= 4 {
                        for i in 0..resp_len - 3 {
                            if resp_buf[i] == b'\r' && resp_buf[i+1] == b'\n'
                                && resp_buf[i+2] == b'\r' && resp_buf[i+3] == b'\n'
                            {
                                // Check for "101" status
                                let hdr = &resp_buf[..resp_len];
                                if hdr.windows(3).any(|w| w == b"101") {
                                    // Upgrade successful!
                                    unsafe {
                                        let slot = &mut WS_SLOTS[slot_id];
                                        slot.state = WsState::Open;
                                        slot.tcp_handle = Some(tcp_handle);
                                        slot.recv_head = 0;
                                        slot.recv_tail = 0;
                                        slot.frame_buf = Vec::new();
                                        slot.frame_expected = 0;
                                    }

                                    crate::serial_str!("[WS] Connected slot ");
                                    crate::drivers::serial::write_dec(slot_id as u32);
                                    crate::serial_strln!(" - upgrade OK");

                                    return Ok(slot_id as u8);
                                } else {
                                    state.sockets.remove(tcp_handle);
                                    return Err("upgrade rejected (not 101)");
                                }
                            }
                        }
                    }
                }
                Err(_) => {}
            }
        }

        if !socket.is_active() {
            state.sockets.remove(tcp_handle);
            return Err("connection lost during upgrade");
        }
        if super::tls::tsc_ms() - start > 10_000 {
            state.sockets.remove(tcp_handle);
            return Err("upgrade timeout");
        }
        for _ in 0..500 { core::hint::spin_loop(); }
    }
}

/// Send a text message on a WebSocket connection.
pub fn ws_send(slot_id: u8, data: &[u8]) -> Result<(), &'static str> {
    if slot_id as usize >= MAX_WS_SLOTS { return Err("invalid slot"); }
    let slot = unsafe { &mut WS_SLOTS[slot_id as usize] };
    if slot.state != WsState::Open { return Err("not open"); }
    let handle = slot.tcp_handle.ok_or("no socket")?;

    let frame = encode_frame(WS_OPCODE_TEXT, data);

    let mut guard = NET_STATE.lock();
    let state = guard.as_mut().ok_or("no network")?;

    let socket = state.sockets.get_mut::<tcp::Socket>(handle);
    if !socket.is_active() {
        slot.state = WsState::Error;
        return Err("connection lost");
    }
    socket.send_slice(&frame).map_err(|_| "send failed")?;

    // Flush
    let now = Instant::from_millis(super::tls::tsc_ms());
    let mut dev = FolkeringDevice;
    state.iface.poll(now, &mut dev, &mut state.sockets);

    Ok(())
}

/// Non-blocking poll for received data on a WebSocket connection.
/// Returns bytes copied to buf, 0 if nothing available, -1 on error/closed.
pub fn ws_poll_recv(slot_id: u8, buf: &mut [u8]) -> i32 {
    if slot_id as usize >= MAX_WS_SLOTS { return -1; }
    let slot = unsafe { &mut WS_SLOTS[slot_id as usize] };

    match slot.state {
        WsState::Error | WsState::Closed => return -1,
        WsState::Free => return -1,
        _ => {}
    }

    let handle = match slot.tcp_handle {
        Some(h) => h,
        None => return -1,
    };

    // Try to read new data from TCP socket (non-blocking)
    if let Some(mut guard) = NET_STATE.try_lock() {
        if let Some(state) = guard.as_mut() {
            // Poll network first
            let now = Instant::from_millis(super::tls::tsc_ms());
            let mut dev = FolkeringDevice;
            state.iface.poll(now, &mut dev, &mut state.sockets);

            let socket = state.sockets.get_mut::<tcp::Socket>(handle);

            if !socket.is_active() && !socket.may_recv() {
                slot.state = WsState::Closed;
                // Drain any remaining buffered data first
                if slot.recv_available() > 0 {
                    return slot.recv_pop(buf) as i32;
                }
                return -1;
            }

            if socket.can_recv() {
                let mut tcp_buf = [0u8; 4096];
                match socket.recv_slice(&mut tcp_buf) {
                    Ok(n) if n > 0 => {
                        // Append to frame assembly buffer
                        slot.frame_buf.extend_from_slice(&tcp_buf[..n]);
                    }
                    _ => {}
                }
            }
        }
    }

    // Try to decode complete frames from the assembly buffer
    while !slot.frame_buf.is_empty() {
        match try_decode_frame(&slot.frame_buf) {
            Some(frame) => {
                let consumed = frame.header_size;
                match frame.opcode {
                    WS_OPCODE_TEXT | WS_OPCODE_BINARY => {
                        // Push payload to recv ring buffer
                        slot.recv_push(&frame.payload);
                    }
                    WS_OPCODE_CLOSE => {
                        slot.state = WsState::Closed;
                        // Send close response (if still connected)
                        // Best-effort: ignore errors
                        let _ = ws_send_raw_frame(slot, WS_OPCODE_CLOSE, &[]);
                    }
                    WS_OPCODE_PING => {
                        // Respond with PONG
                        let _ = ws_send_raw_frame(slot, WS_OPCODE_PONG, &frame.payload);
                    }
                    WS_OPCODE_PONG => {
                        // Ignore — just a keepalive acknowledgment
                    }
                    _ => {} // Unknown opcode
                }
                // Remove consumed bytes from frame buffer
                slot.frame_buf.drain(..consumed);
            }
            None => break, // Need more data
        }
    }

    // Return any buffered data
    if slot.recv_available() > 0 {
        slot.recv_pop(buf) as i32
    } else {
        0 // Nothing available yet
    }
}

/// Send a raw frame (used internally for PONG responses)
fn ws_send_raw_frame(slot: &WsSlot, opcode: u8, payload: &[u8]) -> Result<(), &'static str> {
    let handle = slot.tcp_handle.ok_or("no socket")?;
    let frame = encode_frame(opcode, payload);

    if let Some(mut guard) = NET_STATE.try_lock() {
        if let Some(state) = guard.as_mut() {
            let socket = state.sockets.get_mut::<tcp::Socket>(handle);
            let _ = socket.send_slice(&frame);
            let now = Instant::from_millis(super::tls::tsc_ms());
            let mut dev = FolkeringDevice;
            state.iface.poll(now, &mut dev, &mut state.sockets);
        }
    }
    Ok(())
}

/// Close a WebSocket connection gracefully.
pub fn ws_close(slot_id: u8) {
    if slot_id as usize >= MAX_WS_SLOTS { return; }
    let slot = unsafe { &mut WS_SLOTS[slot_id as usize] };

    if slot.state == WsState::Open {
        // Send close frame
        let _ = ws_send_raw_frame(slot, WS_OPCODE_CLOSE, &[]);
    }

    // Clean up TCP socket
    if let Some(handle) = slot.tcp_handle.take() {
        if let Some(mut guard) = NET_STATE.try_lock() {
            if let Some(state) = guard.as_mut() {
                let socket = state.sockets.get_mut::<tcp::Socket>(handle);
                socket.close();
                // Don't remove — let smoltcp handle TIME_WAIT
            }
        }
    }

    slot.state = WsState::Free;
    slot.recv_head = 0;
    slot.recv_tail = 0;
    slot.frame_buf = Vec::new();
}
