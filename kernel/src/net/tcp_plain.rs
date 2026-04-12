//! Plain (un-encrypted) TCP request helper.
//!
//! Mirrors the shape of `tls::https_get_raw` — creates a smoltcp TCP
//! socket, connects to the remote, writes a pre-built request, reads
//! the response — but without the TLS 1.3 handshake in the middle.
//!
//! Primary consumer is the Fase 5 `sys_fbp_request` syscall which
//! talks to the loopback `folkering-proxy` running on the host. The
//! proxy listens on a plain TCP socket at `10.0.2.2:14711` (the
//! QEMU user-mode networking default for "host's 127.0.0.1").
//!
//! This module deliberately duplicates a bit of the TLS module's
//! setup loop instead of factoring it out — the existing net stack
//! has enough subtle interrupt / polling requirements that a shared
//! abstraction would likely leak details into both call sites.

extern crate alloc;

use alloc::vec;
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, Ipv4Address};

use super::device::FolkeringDevice;
use super::state::NET_STATE;
use super::tls::{next_port, tsc_ms};

/// Send a raw byte request over plain TCP and return the full
/// response as a `Vec<u8>`. No framing is applied — it's up to the
/// caller to interpret the reply.
///
/// `request` is written in full before any reads begin.
///
/// Timeouts:
///   - TCP connect: 15 s
///   - Total read time: 30 s
///
/// `max_response` caps how much of the response we'll buffer; once
/// reached, reads stop and any already-buffered bytes are returned.
///
/// `read_timeout_tsc_ms` caps total read time. Phase 13.4: different
/// callers want very different budgets — KHunt wants to fail fast
/// (~20-30 s wall clock) so the refactor loop can start, while an
/// LLM generate call may legitimately wait 5-8 minutes for a cold
/// cloud model. Pass 0 to use the default of 900_000 tsc_ms.
pub fn tcp_request(
    ip: [u8; 4],
    port: u16,
    request: &[u8],
    max_response: usize,
) -> Result<alloc::vec::Vec<u8>, &'static str> {
    tcp_request_with_timeout(ip, port, request, max_response, 0)
}

pub fn tcp_request_with_timeout(
    ip: [u8; 4],
    port: u16,
    request: &[u8],
    max_response: usize,
    read_timeout_tsc_ms: u64,
) -> Result<alloc::vec::Vec<u8>, &'static str> {
    let read_budget = if read_timeout_tsc_ms == 0 { 900_000 } else { read_timeout_tsc_ms };
    let mut guard = NET_STATE.lock();
    let state = guard.as_mut().ok_or("no network")?;
    if !state.has_ip {
        return Err("no IP address");
    }

    // Reasonable per-socket buffers for small FBP payloads.
    // 64 KB RX is plenty for the mock extractor (~500 B) and leaves
    // headroom for chromiumoxide-extracted trees once those arrive.
    let tcp_rx = tcp::SocketBuffer::new(vec![0u8; 65536]);
    let tcp_tx = tcp::SocketBuffer::new(vec![0u8; 8192]);
    let tcp_socket = tcp::Socket::new(tcp_rx, tcp_tx);
    let tcp_handle = state.sockets.add(tcp_socket);

    let remote_addr = IpAddress::Ipv4(Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]));

    // Syscall entry clears IF via FMASK=0x600; we need interrupts
    // enabled so the VirtIO-net IRQ can deliver received packets
    // into smoltcp during the blocking wait loops below.
    unsafe { core::arch::asm!("sti"); }

    crate::serial_str!("[TCP] plain connect to ");
    crate::drivers::serial::write_dec(ip[0] as u32); crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[1] as u32); crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[2] as u32); crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[3] as u32); crate::serial_str!(":");
    crate::drivers::serial::write_dec(port as u32); crate::serial_str!("\n");

    {
        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        socket
            .connect(state.iface.context(), (remote_addr, port), next_port())
            .map_err(|_| "TCP connect failed")?;
    }

    // ── Wait for TCP established ───────────────────────────────────
    let start_tsc = tsc_ms();
    loop {
        let now = Instant::from_millis(tsc_ms());
        let mut device = FolkeringDevice;
        state.iface.poll(now, &mut device, &mut state.sockets);
        super::tcp_shell::poll(state); // keep shell responsive during blocking calls

        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        if socket.may_send() { break; }
        if !socket.is_active() {
            state.sockets.remove(tcp_handle);
            return Err("TCP refused");
        }
        if tsc_ms() - start_tsc > 15_000 {
            state.sockets.remove(tcp_handle);
            crate::serial_str!("[TCP] connect TIMEOUT\n");
            return Err("TCP connect timeout");
        }
        for _ in 0..1000 { core::hint::spin_loop(); }
    }

    crate::serial_str!("[TCP] connected, sending ");
    crate::drivers::serial::write_dec(request.len() as u32);
    crate::serial_str!(" bytes\n");

    // ── Send request ───────────────────────────────────────────────
    let mut sent = 0usize;
    while sent < request.len() {
        let now = Instant::from_millis(tsc_ms());
        let mut device = FolkeringDevice;
        state.iface.poll(now, &mut device, &mut state.sockets);
        super::tcp_shell::poll(state);

        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        if socket.can_send() {
            match socket.send_slice(&request[sent..]) {
                Ok(n) => sent += n,
                Err(_) => {
                    state.sockets.remove(tcp_handle);
                    return Err("TCP send error");
                }
            }
        }
        if !socket.is_active() {
            break;
        }
        for _ in 0..100 { core::hint::spin_loop(); }
    }

    // Force one more poll so the last TX packet hits the NIC
    {
        let now = Instant::from_millis(tsc_ms());
        let mut device = FolkeringDevice;
        state.iface.poll(now, &mut device, &mut state.sockets);
        super::tcp_shell::poll(state);
    }

    crate::serial_str!("[TCP] request sent, reading response\n");

    // ── Read response ──────────────────────────────────────────────
    let mut response: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let read_start = tsc_ms();
    loop {
        let now = Instant::from_millis(tsc_ms());
        let mut device = FolkeringDevice;
        state.iface.poll(now, &mut device, &mut state.sockets);
        super::tcp_shell::poll(state);

        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        if socket.can_recv() {
            let got = socket.recv(|buf| {
                let take = buf.len().min(max_response.saturating_sub(response.len()));
                response.extend_from_slice(&buf[..take]);
                (take, ())
            });
            match got {
                Ok(()) => {}
                Err(_) => break,
            }
            if response.len() >= max_response {
                break;
            }
        }

        if !socket.may_recv() && !socket.can_recv() {
            // Peer closed and we have drained everything
            break;
        }

        // Phase 13: tsc_ms() assumes 2 GHz but modern CPUs run at
        // 3-5 GHz, so every "1 ms of tsc_ms" is actually ~0.4-0.7 ms
        // of wall clock. The budget is per-caller (read_budget).
        if tsc_ms() - read_start > read_budget as i64 {
            crate::serial_str!("[TCP] read TIMEOUT\n");
            break;
        }

        for _ in 0..1000 { core::hint::spin_loop(); }
    }

    // Drop the socket back into the stack
    state.sockets.remove(tcp_handle);

    crate::serial_str!("[TCP] done, ");
    crate::drivers::serial::write_dec(response.len() as u32);
    crate::serial_str!(" bytes\n");

    Ok(response)
}
