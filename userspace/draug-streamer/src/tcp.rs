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
    /// The kernel's `tcp_connect_async(ip, port)` is idempotent on
    /// `(ip, port, owner)`: the first call allocates a slot and
    /// kicks off the SYN; subsequent calls with the same destination
    /// return the same slot id (promoting `Connecting → Connected`
    /// when `may_send()` goes true, or `EAGAIN` while still
    /// handshaking, or `u64::MAX` on failure). We loop on it until
    /// the returned value is the real slot id, yielding the CPU on
    /// each `EAGAIN` so the scheduler stays responsive while the
    /// handshake completes.
    pub fn connect(ip: [u8; 4], port: u16) -> Result<Self, TcpError> {
        let initial = tcp_connect_async(ip, port);
        if initial == u64::MAX {
            return Err(TcpError::ConnectFailed);
        }
        if initial == TCP_EAGAIN || initial == INVALID_SLOT {
            return Err(TcpError::ConnectFailed);
        }
        // Re-poll until the slot is promoted to Connected. Because
        // the kernel matches on (ip, port) + owner we'll always get
        // the same slot id back — no risk of shadow-allocating a
        // second connection to the same destination.
        loop {
            match tcp_connect_async(ip, port) {
                TCP_EAGAIN => yield_cpu(),
                v if v == u64::MAX => return Err(TcpError::ConnectFailed),
                slot => return Ok(TcpSession { slot }),
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
