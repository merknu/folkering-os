//! Low-level I/O glue for TLS: TCP socket adapter, RNG, ephemeral ports.
//!
//! This module isolates the embedded-tls plumbing from the high-level
//! TLS connection logic in `mod.rs`. The `TcpStream` adapter wraps a
//! smoltcp TCP socket as `embedded_io::Read + Write` so embedded-tls
//! can drive the TLS handshake over it.

extern crate alloc;

use embedded_io::{ErrorType, Read, Write};
use rand_core::{CryptoRng, RngCore};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;

use crate::drivers::rng as hw_rng;
use super::super::device::FolkeringDevice;
use super::super::state::NetState;

// ── RNG Adapter ────────────────────────────────────────────────────────

/// Wraps our kernel RNG (RDRAND/RDTSC) as a `CryptoRngCore` for embedded-tls.
pub(crate) struct KernelRng;

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

// ── Ephemeral port allocation ──────────────────────────────────────────

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

// ── TSC-based timing ───────────────────────────────────────────────────

/// Read TSC and convert to approximate milliseconds.
/// Uses RDTSC which always works, even with interrupts disabled.
pub fn tsc_ms() -> i64 {
    let tsc: u64;
    unsafe { core::arch::asm!("rdtsc", "shl rdx, 32", "or rax, rdx", out("rax") tsc, out("rdx") _); }
    // Approximate: assume ~2 GHz → ~2M cycles/ms.
    (tsc / 2_000_000) as i64
}

// ── TCP Stream Adapter ─────────────────────────────────────────────────

/// Wraps a smoltcp TCP socket as an `embedded_io::Read + Write` for TLS.
/// Polls the network interface on each operation to drive packets.
pub(crate) struct TcpStream<'a> {
    pub(crate) state: &'a mut NetState,
    pub(crate) handle: smoltcp::iface::SocketHandle,
}

#[derive(Debug)]
pub(crate) struct TcpError;

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

impl TcpStream<'_> {
    fn poll_once(&mut self) {
        // Use TSC-based time so smoltcp retransmission timers work even when
        // APIC timer interrupt doesn't fire (e.g., during syscall with IF=0).
        // Poll multiple times to drain VirtIO-Net RX queue before it drops segments.
        let now = Instant::from_millis(tsc_ms());
        let mut device = FolkeringDevice;
        self.state.iface.poll(now, &mut device, &mut self.state.sockets);
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
