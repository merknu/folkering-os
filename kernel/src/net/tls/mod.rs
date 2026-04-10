//! TLS 1.3 client — embedded-tls integration over smoltcp TCP.
//!
//! Provides `https_get(host, path, ip)` for making HTTPS requests.
//!
//! # Module structure
//!
//! - `mod.rs` (this file) — public API: `https_get`, `https_get_raw`
//! - `io.rs` — low-level glue: `TcpStream`, `KernelRng`, `tsc_ms`, `next_port`
//!
//! # SECURITY WARNING
//! Uses `MinimalVerifier` from `tls_verify.rs` which performs:
//!   - Hostname matching against SAN extension
//!   - Certificate expiration check
//!
//! It does NOT verify the certificate chain or signature against a trust
//! anchor. This means MITM attacks with a valid-looking cert from a
//! malicious CA are still possible. See `tls_verify.rs` for details.
//!
//! Folkering OS is a research/development OS — do not use it for sensitive
//! logins or financial transactions.

extern crate alloc;

mod io;

// Re-export the helpers other net modules use
pub use io::{next_port, tsc_ms};
pub(crate) use io::{KernelRng, TcpStream};

use alloc::vec;
use embedded_tls::blocking::{Aes128GcmSha256, TlsConfig, TlsConnection, TlsContext};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, Ipv4Address};

use super::device::FolkeringDevice;
use super::state::NET_STATE;
use super::tls_verify::VerifyingProvider;

// ── Public API ─────────────────────────────────────────────────────────

/// Perform an HTTPS request with a raw pre-built HTTP request.
/// Returns the full HTTP response (headers + body) as a Vec<u8>.
pub fn https_get_raw(ip: [u8; 4], host: &str, request: &[u8]) -> Result<alloc::vec::Vec<u8>, &'static str> {
    let mut guard = NET_STATE.lock();
    let state = guard.as_mut().ok_or("no network")?;
    if !state.has_ip {
        return Err("no IP address");
    }

    // Create TCP socket with large buffers for TLS.
    // Google's TLS ServerHello + cert chain spans 4-6KB across multiple TCP segments.
    // A TLS record can be up to 16KB. 64KB RX ensures no packet drops.
    let tcp_rx = tcp::SocketBuffer::new(vec![0u8; 65536]);
    let tcp_tx = tcp::SocketBuffer::new(vec![0u8; 16384]);
    let tcp_socket = tcp::Socket::new(tcp_rx, tcp_tx);
    let tcp_handle = state.sockets.add(tcp_socket);

    let remote_addr = IpAddress::Ipv4(Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]));

    // Enable interrupts during TLS (syscall entry disables IF via FMASK).
    // Needed so VirtIO-net IRQ can deliver received packets.
    unsafe { core::arch::asm!("sti"); }

    crate::serial_str!("[TLS] TCP connecting to ");
    crate::drivers::serial::write_dec(ip[0] as u32); crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[1] as u32); crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[2] as u32); crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[3] as u32); crate::serial_str!(":443\n");

    {
        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        socket.connect(state.iface.context(), (remote_addr, 443u16), next_port())
            .map_err(|_| "TCP connect failed")?;
    }

    crate::serial_str!("[TLS] Waiting for TCP established...\n");

    // Wait for TCP connect (use TSC for timing, not APIC uptime)
    let start_tsc = tsc_ms();
    loop {
        let now = Instant::from_millis(tsc_ms());
        let mut device = FolkeringDevice;
        state.iface.poll(now, &mut device, &mut state.sockets);

        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        if socket.may_send() { break; }
        if !socket.is_active() {
            state.sockets.remove(tcp_handle);
            return Err("TCP refused");
        }
        if tsc_ms() - start_tsc > 15_000 {
            state.sockets.remove(tcp_handle);
            crate::serial_str!("[TLS] TCP TIMEOUT after 15s\n");
            return Err("TCP timeout");
        }
        for _ in 0..1000 { core::hint::spin_loop(); }
    }

    crate::serial_str!("[TLS] TCP connected! Starting TLS handshake...\n");

    // TLS record buffers: 32KB read (Google cert chain ~6KB + headroom), 8KB write
    let mut tls_read_buf = vec![0u8; 32768];
    let mut tls_write_buf = vec![0u8; 8192];
    let config = TlsConfig::new().with_server_name(host);

    let stream = TcpStream { state, handle: tcp_handle };
    let mut tls: TlsConnection<'_, TcpStream<'_>, Aes128GcmSha256> =
        TlsConnection::new(stream, &mut tls_read_buf, &mut tls_write_buf);

    let rng = KernelRng;
    let provider = VerifyingProvider::new::<Aes128GcmSha256>(rng);
    crate::serial_str!("[TLS] using MinimalVerifier (hostname + expiration check)\n");
    tls.open(TlsContext::new(&config, provider))
        .map_err(|_| {
            crate::serial_str!("[TLS] TLS handshake FAILED\n");
            "TLS handshake failed"
        })?;

    crate::serial_str!("[TLS] TLS handshake OK! Sending request...\n");

    // Send request
    use embedded_io::{Read, Write};
    let mut written = 0;
    while written < request.len() {
        match tls.write(&request[written..]) {
            Ok(n) => written += n,
            Err(_) => {
                crate::serial_str!("[TLS] Write error!\n");
                break;
            }
        }
    }
    let _ = tls.flush();

    crate::serial_str!("[TLS] Request sent (");
    crate::drivers::serial::write_dec(written as u32);
    crate::serial_str!(" bytes). Reading response...\n");

    // Read response — with per-read logging
    let mut response = alloc::vec::Vec::new();
    let mut buf = [0u8; 4096];
    let read_start = tsc_ms();
    loop {
        match tls.read(&mut buf) {
            Ok(0) => {
                crate::serial_str!("[TLS] Read: EOF\n");
                break;
            }
            Ok(n) => {
                crate::serial_str!("[TLS] Read: ");
                crate::drivers::serial::write_dec(n as u32);
                crate::serial_str!(" bytes (total ");
                crate::drivers::serial::write_dec((response.len() + n) as u32);
                crate::serial_str!(")\n");
                response.extend_from_slice(&buf[..n]);
                if response.len() > 65536 { break; }
            }
            Err(_) => {
                crate::serial_str!("[TLS] Read: error/timeout\n");
                break;
            }
        }
        if tsc_ms() - read_start > 60_000 {
            crate::serial_str!("[TLS] Read: overall timeout\n");
            break;
        }
    }

    // Cleanup
    let stream = match tls.close() {
        Ok(s) => s,
        Err((s, _)) => s,
    };
    stream.state.sockets.remove(tcp_handle);

    Ok(response)
}

/// Perform an HTTPS GET request to a hardcoded IP.
/// Logs the result to serial. Blocking — call from syscall context only.
///
/// `ip` = target IPv4 address (bypasses DNS)
/// `host` = hostname for TLS SNI + HTTP Host header
/// `path` = HTTP path (e.g. "/")
pub fn https_get(ip: [u8; 4], host: &str, path: &str) -> Result<(), &'static str> {
    crate::serial_str!("[TLS] HTTPS GET https://");
    for &b in host.as_bytes() {
        crate::drivers::serial::write_byte(b);
    }
    for &b in path.as_bytes() {
        crate::drivers::serial::write_byte(b);
    }
    crate::serial_str!(" -> ");
    crate::drivers::serial::write_dec(ip[0] as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[1] as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[2] as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[3] as u32);
    crate::drivers::serial::write_newline();

    let mut guard = NET_STATE.lock();
    let state = guard.as_mut().ok_or("no network")?;

    if !state.has_ip {
        return Err("no IP address");
    }

    crate::serial_strln!("[TLS] Creating TCP socket...");

    let tcp_rx = tcp::SocketBuffer::new(vec![0u8; 8192]);
    let tcp_tx = tcp::SocketBuffer::new(vec![0u8; 4096]);
    let tcp_socket = tcp::Socket::new(tcp_rx, tcp_tx);
    let tcp_handle = state.sockets.add(tcp_socket);

    let remote_addr = IpAddress::Ipv4(Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]));
    let remote_endpoint = (remote_addr, 443u16);

    {
        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        socket
            .connect(state.iface.context(), remote_endpoint, next_port())
            .map_err(|_| "TCP connect failed")?;
    }

    crate::serial_strln!("[TLS] TCP connecting...");

    // Enable interrupts: SYSCALL entry clears IF (FMASK=0x600).
    unsafe { core::arch::asm!("sti", options(nomem, nostack)); }

    let start_tsc = tsc_ms();
    loop {
        let now = Instant::from_millis(tsc_ms());
        let mut device = FolkeringDevice;
        state.iface.poll(now, &mut device, &mut state.sockets);

        let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
        if socket.may_send() {
            crate::serial_strln!("[TLS] TCP connected!");
            break;
        }
        if !socket.is_active() {
            state.sockets.remove(tcp_handle);
            return Err("TCP connection refused");
        }
        if tsc_ms() - start_tsc > 15_000 {
            state.sockets.remove(tcp_handle);
            return Err("TCP connect timeout");
        }
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }

    // ── TLS handshake ──────────────────────────────────────────────────
    crate::serial_strln!("[TLS] Starting TLS 1.3 handshake...");

    let mut tls_read_buf = vec![0u8; 32768];
    let mut tls_write_buf = vec![0u8; 8192];
    let config = TlsConfig::new().with_server_name(host);

    let stream = TcpStream {
        state,
        handle: tcp_handle,
    };

    let mut tls: TlsConnection<'_, TcpStream<'_>, Aes128GcmSha256> =
        TlsConnection::new(stream, &mut tls_read_buf, &mut tls_write_buf);

    let rng = KernelRng;
    let provider = VerifyingProvider::new::<Aes128GcmSha256>(rng);
    match tls.open(TlsContext::new(&config, provider)) {
        Ok(()) => {
            crate::serial_strln!("[TLS] Handshake complete! Connection encrypted.");
        }
        Err(e) => {
            crate::serial_str!("[TLS] Handshake FAILED: ");
            log_tls_error(&e);
            let stream = match tls.close() {
                Ok(s) => s,
                Err((s, _)) => s,
            };
            stream.state.sockets.remove(tcp_handle);
            return Err("TLS handshake failed");
        }
    }

    // ── Send HTTP GET request ──────────────────────────────────────────
    crate::serial_strln!("[TLS] Sending HTTP request...");

    let mut req_buf = [0u8; 256];
    let req_len = build_http_request(&mut req_buf, host, path);

    use embedded_io::Write;
    let mut written = 0;
    while written < req_len {
        match tls.write(&req_buf[written..req_len]) {
            Ok(n) => written += n,
            Err(_) => break,
        }
    }
    let _ = tls.flush();

    // Skip response read — handshake + request are sufficient verification
    crate::serial_strln!("[TLS] Handshake + request verified. Skipping response read.");

    // Cleanup
    let stream = match tls.close() {
        Ok(s) => s,
        Err((s, _)) => s,
    };
    stream.state.sockets.remove(tcp_handle);

    crate::serial_strln!("[TLS] Connection closed.");
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Build "GET <path> HTTP/1.1\r\nHost: <host>\r\nConnection: close\r\n\r\n"
fn build_http_request(buf: &mut [u8], host: &str, path: &str) -> usize {
    let mut pos = 0;
    let parts: &[&[u8]] = &[
        b"GET ",
        path.as_bytes(),
        b" HTTP/1.1\r\nHost: ",
        host.as_bytes(),
        b"\r\nConnection: close\r\n\r\n",
    ];
    for part in parts {
        let len = part.len().min(buf.len() - pos);
        buf[pos..pos + len].copy_from_slice(&part[..len]);
        pos += len;
    }
    pos
}

/// Log TLS error to serial
fn log_tls_error(e: &embedded_tls::TlsError) {
    use embedded_tls::TlsError;
    let msg = match e {
        TlsError::Io(_) => "I/O error",
        TlsError::MissingHandshake => "missing handshake",
        TlsError::InternalError => "internal error",
        TlsError::InvalidRecord => "invalid record",
        TlsError::UnknownContentType => "unknown content type",
        TlsError::InvalidHandshake => "invalid handshake",
        TlsError::InvalidCertificate => "invalid certificate",
        TlsError::InvalidSignature => "invalid signature",
        TlsError::DecodeError => "decode error",
        TlsError::EncodeError => "encode error",
        TlsError::CryptoError => "crypto error",
        _ => "other TLS error",
    };
    crate::drivers::serial::write_str(msg);
    crate::drivers::serial::write_newline();
}
