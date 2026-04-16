//! TCP session wrapper — synchronous-looking send/recv over Folkering's
//! non-blocking TCP syscalls.
//!
//! The kernel's `tcp_*_async` APIs return `TCP_EAGAIN` when the socket
//! buffer would block. For a streaming client that runs a simple
//! linear protocol (send CODE, send EXEC, wait for RESULT, repeat) we
//! don't need the full state-machine gymnastics that the compositor's
//! `draug_async.rs` uses for 60 Hz UI integration. Instead we wrap
//! each syscall in a spin-yield loop: retry on EAGAIN after a
//! `yield_cpu()`, return the final (possibly partial) byte count, and
//! let the caller compose `send_all` / `recv_exact` from those
//! primitives.
//!
//! This keeps the streaming-client code linear and readable while
//! still cooperating with the scheduler.

use libfolk::sys::{
    tcp_close_async, tcp_connect_async, tcp_poll_recv, tcp_send_async, yield_cpu, TCP_EAGAIN,
};

/// Zero-length slice to probe slot state via `tcp_send_async`.
/// The kernel's `syscall_tcp_send` auto-promotes a `Connecting` slot
/// to `Connected` once the handshake completes; sending 0 bytes is
/// a side-effect-free way to drive that promotion.
const EMPTY: &[u8] = &[];

#[derive(Debug)]
pub enum TcpError {
    /// `tcp_connect_async` returned `u64::MAX` — kernel out of slots
    /// or the target couldn't be reached synchronously.
    ConnectFailed,
    /// `tcp_send_async` returned `u64::MAX` after spin-yield loop.
    SendFailed,
    /// `tcp_poll_recv` returned `u64::MAX` during read.
    RecvFailed,
    /// Peer closed the connection before enough bytes arrived to
    /// satisfy a `recv_exact` call.
    PeerClosed,
}

/// Open TCP session. The slot is valid for `send_all` / `recv_exact`
/// once the kernel reports the connection is established; both
/// primitives spin-yield on `TCP_EAGAIN` so the caller doesn't have
/// to tell apart "still connecting" from "buffer full".
pub struct TcpSession {
    slot: u64,
}

const INVALID_SLOT: u64 = 0xFFFF;

impl TcpSession {
    /// Open a TCP session, **blocking until the handshake completes**.
    ///
    /// Connection flow:
    ///   1. `tcp_connect_async(ip, port)` allocates a slot and kicks
    ///      off the SYN. Returns the slot id (state = Connecting).
    ///   2. We drive the handshake to completion via
    ///      `tcp_send_async(slot, &[])`. Empty-slice sends are
    ///      side-effect-free and return `0` once the kernel has
    ///      promoted the slot to Connected (or `TCP_EAGAIN` while
    ///      still handshaking, or `u64::MAX` on failure).
    ///
    /// Why not loop on `tcp_connect_async`? The kernel's
    /// `syscall_tcp_connect` only matches *Connecting* slots in its
    /// re-poll path — once a slot is promoted to Connected, a
    /// subsequent call allocates a brand-new slot (and socket) and
    /// kicks off a second handshake. That means `tcp_poll_recv` on
    /// the original slot never sees any data because the daemon's
    /// bytes arrive on the new, unrelated connection. Using
    /// `tcp_send_async` as the probe sidesteps that quirk.
    pub fn connect(ip: [u8; 4], port: u16) -> Result<Self, TcpError> {
        let slot = tcp_connect_async(ip, port);
        if slot == u64::MAX {
            return Err(TcpError::ConnectFailed);
        }
        if slot == TCP_EAGAIN || slot == INVALID_SLOT {
            return Err(TcpError::ConnectFailed);
        }
        // Drive the handshake to completion. `tcp_send_async` with a
        // zero-length buffer auto-promotes Connecting → Connected
        // once `socket.may_send()` is true — the first non-EAGAIN,
        // non-MAX return value means we're fully connected.
        loop {
            match tcp_send_async(slot, EMPTY) {
                TCP_EAGAIN => yield_cpu(),
                v if v == u64::MAX => return Err(TcpError::ConnectFailed),
                _ => return Ok(TcpSession { slot }),
            }
        }
    }

    /// Send every byte in `data`. Spins on `TCP_EAGAIN` via
    /// `yield_cpu`. Returns on error or after the last byte is
    /// accepted by the kernel.
    pub fn send_all(&mut self, data: &[u8]) -> Result<(), TcpError> {
        let mut sent = 0usize;
        while sent < data.len() {
            let remaining = &data[sent..];
            match tcp_send_async(self.slot, remaining) {
                TCP_EAGAIN => yield_cpu(),
                v if v == u64::MAX => return Err(TcpError::SendFailed),
                n => sent += n as usize,
            }
        }
        Ok(())
    }

    /// Read exactly `buf.len()` bytes. Spins on `TCP_EAGAIN`. Returns
    /// `PeerClosed` if the peer closes before the buffer is filled.
    pub fn recv_exact(&mut self, buf: &mut [u8]) -> Result<(), TcpError> {
        let mut read = 0usize;
        while read < buf.len() {
            let tail = &mut buf[read..];
            match tcp_poll_recv(self.slot, tail) {
                TCP_EAGAIN => yield_cpu(),
                v if v == u64::MAX => return Err(TcpError::RecvFailed),
                0 => return Err(TcpError::PeerClosed),
                n => read += n as usize,
            }
        }
        Ok(())
    }
}

impl Drop for TcpSession {
    fn drop(&mut self) {
        // Fire-and-forget close. The kernel handles the TCP FIN
        // handshake — we don't wait for it to complete.
        tcp_close_async(self.slot);
    }
}
