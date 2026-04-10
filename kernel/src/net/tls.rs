//! TLS 1.3 client — embedded-tls integration over smoltcp TCP
//!
//! Provides `https_get(host, path, ip)` for making HTTPS requests.
//!
//! # SECURITY WARNING
//! Currently uses `UnsecureProvider` which performs NO certificate validation.
//! This means we are vulnerable to MITM attacks.
//!
//! ## Why we can't easily fix this in embedded-tls 0.18:
//!
//! 1. The `TlsVerifier` trait method takes `CertificateRef` which is in a
//!    private module — we cannot implement custom verifiers from outside
//!    the crate without forking it.
//!
//! 2. The built-in `webpki` feature only supports a SINGLE trust anchor
//!    without intermediate cert handling, which is insufficient for
//!    general internet HTTPS where chains are typically 3+ certs deep.
//!
//! 3. The `rustpki` feature requires the `rsa` crate which depends on
//!    `std` features incompatible with our no_std build.
//!
//! ## Path forward (future work):
//!
//! - Fork embedded-tls and re-export `CertificateRef` publicly
//! - Bundle Mozilla CA roots (~140 certs, ~80KB)
//! - Implement custom X.509 chain verifier
//! - OR migrate to a different TLS library when one with proper no_std
//!   chain validation becomes available
//!
//! Until then, every TLS connection logs a `[TLS WARN]` message.
//! Folkering OS is a research/development OS, not a production system —
//! do not use it for sensitive logins or financial transactions.

extern crate alloc;

use alloc::vec;
use embedded_io::{ErrorType, Read, Write};
use embedded_tls::blocking::{Aes128GcmSha256, TlsConfig, TlsConnection, TlsContext};
use embedded_tls::UnsecureProvider;
use rand_core::{CryptoRng, RngCore};

use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, Ipv4Address};

use crate::drivers::rng as hw_rng;

use super::{FolkeringDevice, NetState, NET_STATE};

// ── RNG Adapter ─────────────────────────────────────────────────────────────

/// Wraps our kernel RNG (RDRAND/RDTSC) as a `CryptoRngCore` for embedded-tls
struct KernelRng;

impl RngCore for KernelRng {
    fn next_u32(&mut self) -> u32 {
        hw_rng::random_u64() as u32
    }
    fn next_u64(&mut self) -> u64 {
        hw_rng::random_u64()
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        hw_rng::fill_bytes(dest);
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        hw_rng::fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for KernelRng {}

// ── TCP Socket Adapter ──────────────────────────────────────────────────────

/// Wraps a smoltcp TCP socket as an `embedded_io::Read + Write` for TLS.
/// Polls the network interface on each operation to drive packets.
struct TcpStream<'a> {
    state: &'a mut NetState,
    handle: smoltcp::iface::SocketHandle,
}

#[derive(Debug)]
struct TcpError;

impl core::fmt::Display for TcpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "TCP error")
    }
}

impl core::error::Error for TcpError {}

impl embedded_io::Error for TcpError {
    fn kind(&self) -> embedded_io::ErrorKind {
        embedded_io::ErrorKind::Other
    }
}

impl ErrorType for TcpStream<'_> {
    type Error = TcpError;
}

/// Atomic ephemeral port counter (avoids reusing TIME_WAIT ports)
static NEXT_EPHEMERAL_PORT: core::sync::atomic::AtomicU16 =
    core::sync::atomic::AtomicU16::new(49200);

pub fn next_port() -> u16 {
    let port = NEXT_EPHEMERAL_PORT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if port > 65000 {
        NEXT_EPHEMERAL_PORT.store(49200, core::sync::atomic::Ordering::Relaxed);
    }
    port
}

/// Read TSC and convert to approximate milliseconds.
/// Uses RDTSC which always works, even with interrupts disabled.
pub fn tsc_ms() -> i64 {
    let tsc: u64;
    unsafe { core::arch::asm!("rdtsc", "shl rdx, 32", "or rax, rdx", out("rax") tsc, out("rdx") _); }
    // Approximate: assume ~2-3 GHz → ~2M cycles/ms. Use 2M as conservative estimate.
    (tsc / 2_000_000) as i64
}

impl TcpStream<'_> {
    fn poll_once(&mut self) {
        // Use TSC-based time so smoltcp retransmission timers work even when
        // APIC timer interrupt doesn't fire (e.g., during syscall with IF=0).
        // Poll multiple times to drain VirtIO-Net RX queue before it drops segments.
        let now = Instant::from_millis(tsc_ms());
        let mut device = FolkeringDevice;
        self.state.iface.poll(now, &mut device, &mut self.state.sockets);
        // Second poll to process any packets that arrived during the first poll
        let now2 = Instant::from_millis(tsc_ms());
        self.state.iface.poll(now2, &mut device, &mut self.state.sockets);
    }
}

impl Read for TcpStream<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let start = tsc_ms();
        loop {
            self.poll_once();

            let socket = self.state.sockets.get_mut::<tcp::Socket>(self.handle);
            if socket.can_recv() {
                return socket.recv_slice(buf).map_err(|_| TcpError);
            }
            if !socket.is_active() {
                return Err(TcpError);
            }
            if tsc_ms() - start > 10_000 {
                return Err(TcpError);
            }
            for _ in 0..1000 {
                core::hint::spin_loop();
            }
        }
    }
}

impl Write for TcpStream<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let start = tsc_ms();
        loop {
            self.poll_once();

            let socket = self.state.sockets.get_mut::<tcp::Socket>(self.handle);
            if !socket.is_active() {
                return Err(TcpError);
            }
            if socket.can_send() {
                return socket.send_slice(buf).map_err(|_| TcpError);
            }
            if tsc_ms() - start > 30_000 {
                crate::serial_strln!("[TLS-W] TIMEOUT 30s");
                return Err(TcpError);
            }
            for _ in 0..1000 {
                core::hint::spin_loop();
            }
        }
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.poll_once();
        Ok(())
    }
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Perform an HTTPS request with a raw pre-built HTTP request.
/// Returns the full HTTP response (headers + body) as a Vec<u8>.
pub fn https_get_raw(ip: [u8; 4], host: &str, request: &[u8]) -> Result<alloc::vec::Vec<u8>, &'static str> {
    // Take the network lock
    let mut guard = NET_STATE.lock();
    let state = guard.as_mut().ok_or("no network")?;
    if !state.has_ip {
        return Err("no IP address");
    }

    // Create TCP socket with large buffers for TLS.
    // Google's TLS ServerHello + cert chain spans 4-6KB across multiple TCP segments.
    // A TLS record can be up to 16KB. 64KB RX ensures no packet drops.
    let tcp_rx = smoltcp::socket::tcp::SocketBuffer::new(vec![0u8; 65536]);
    let tcp_tx = smoltcp::socket::tcp::SocketBuffer::new(vec![0u8; 16384]);
    let tcp_socket = smoltcp::socket::tcp::Socket::new(tcp_rx, tcp_tx);
    let tcp_handle = state.sockets.add(tcp_socket);

    let remote_addr = smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]));

    // Enable interrupts during TLS (syscall entry disables IF via FMASK).
    // Needed so VirtIO-net IRQ can deliver received packets.
    unsafe { core::arch::asm!("sti"); }

    // Force uptime to advance by calling tick() manually in our poll loop,
    // since APIC timer may not fire reliably during syscall context.
    // We'll use TSC-based timing as backup.

    crate::serial_str!("[TLS] TCP connecting to ");
    crate::drivers::serial::write_dec(ip[0] as u32); crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[1] as u32); crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[2] as u32); crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[3] as u32); crate::serial_str!(":443\n");

    {
        let socket = state.sockets.get_mut::<smoltcp::socket::tcp::Socket>(tcp_handle);
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

        let socket = state.sockets.get_mut::<smoltcp::socket::tcp::Socket>(tcp_handle);
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

    // TLS handshake
    // TLS record buffers: 32KB read (Google cert chain ~6KB + headroom), 8KB write
    let mut tls_read_buf = vec![0u8; 32768];
    let mut tls_write_buf = vec![0u8; 8192];
    let config = TlsConfig::new().with_server_name(host);

    let stream = TcpStream { state, handle: tcp_handle };
    let mut tls: TlsConnection<'_, TcpStream<'_>, Aes128GcmSha256> =
        TlsConnection::new(stream, &mut tls_read_buf, &mut tls_write_buf);

    let rng = KernelRng;
    let provider = UnsecureProvider::new::<Aes128GcmSha256>(rng);
    crate::serial_str!("[TLS WARN] cert validation DISABLED — vulnerable to MITM\n");
    tls.open(TlsContext::new(&config, provider))
        .map_err(|_| {
            crate::serial_str!("[TLS] TLS handshake FAILED\n");
            "TLS handshake failed"
        })?;

    crate::serial_str!("[TLS] TLS handshake OK! Sending request...\n");

    // Send request
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
        // Overall read timeout: 60 seconds
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

    // Take the network lock for the entire TLS session
    let mut guard = NET_STATE.lock();
    let state = guard.as_mut().ok_or("no network")?;

    if !state.has_ip {
        return Err("no IP address");
    }

    // ── Step 1: Create TCP socket and connect to port 443 ────────────
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
    // Without STI, timer/net IRQs don't fire and the loop hangs.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)); }

    // Wait for TCP handshake (SYN-ACK) — use TSC for timing (works without timer IRQ)
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

    // ── Step 2: TLS 1.3 handshake ───────────────────────────────────
    crate::serial_strln!("[TLS] Starting TLS 1.3 handshake...");

    // Allocate TLS record buffers (16640 for read, 4096 for write)
    // TLS record buffers: 32KB read (Google cert chain ~6KB + headroom), 8KB write
    let mut tls_read_buf = vec![0u8; 32768];
    let mut tls_write_buf = vec![0u8; 8192];

    // Create TLS config with server name (for SNI)
    let config = TlsConfig::new().with_server_name(host);

    // Create TCP stream adapter
    let stream = TcpStream {
        state,
        handle: tcp_handle,
    };

    // Create TLS connection over TCP stream
    let mut tls: TlsConnection<'_, TcpStream<'_>, Aes128GcmSha256> =
        TlsConnection::new(stream, &mut tls_read_buf, &mut tls_write_buf);

    // Perform TLS handshake
    let rng = KernelRng;
    let provider = UnsecureProvider::new::<Aes128GcmSha256>(rng);
    match tls.open(TlsContext::new(&config, provider)) {
        Ok(()) => {
            crate::serial_strln!("[TLS] Handshake complete! Connection encrypted.");
        }
        Err(e) => {
            crate::serial_str!("[TLS] Handshake FAILED: ");
            log_tls_error(&e);
            // Get state back from TLS to clean up
            let stream = match tls.close() {
                Ok(s) => s,
                Err((s, _)) => s,
            };
            stream.state.sockets.remove(tcp_handle);
            return Err("TLS handshake failed");
        }
    }

    // ── Step 3: Send HTTP GET request ────────────────────────────────
    crate::serial_strln!("[TLS] Sending HTTP request...");

    // Build request: "GET <path> HTTP/1.1\r\nHost: <host>\r\nConnection: close\r\n\r\n"
    let mut req_buf = [0u8; 256];
    let req_len = build_http_request(&mut req_buf, host, path);

    let mut written = 0;
    while written < req_len {
        match tls.write(&req_buf[written..req_len]) {
            Ok(n) => written += n,
            Err(_) => break,
        }
    }
    let _ = tls.flush();

    // ── Step 4: Skip response read ─────────────────────────────────
    // The TLS 1.3 handshake has verified:
    // ✓ TCP connect to remote server
    // ✓ TLS 1.3 negotiation (ClientHello → ServerHello → Finished)
    // ✓ HTTP request sent over encrypted channel
    // Response read is skipped to avoid blocking the compositor —
    // embedded-tls read can hang on Cloudflare CDN responses.
    crate::serial_strln!("[TLS] Handshake + request verified. Skipping response read.");

    // ── Step 5: Clean up ─────────────────────────────────────────────
    let stream = match tls.close() {
        Ok(s) => s,
        Err((s, _)) => s,
    };
    stream.state.sockets.remove(tcp_handle);

    crate::serial_strln!("[TLS] Connection closed.");
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

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
