//! a64-stream-relay — host-side TCP bridge.
//!
//! Replaces `tools/tcp_relay.py`. Same job: listen on a host TCP
//! port (so a Folkering OS guest behind SLIRP can reach it via the
//! 10.0.2.2 gateway) and forward every byte both ways to the real
//! Pi-side a64-stream-daemon.
//!
//! Two bugs the Python version hit are designed-out here:
//!
//!   1. NO `shutdown(SHUT_WR)` from either pipe thread. When one
//!      side of the bridge closed its write half, SLIRP reported
//!      `FIN_WAIT_2` and the guest saw `PeerClosed` after 2
//!      samples. Each thread just exits when its `read` sees EOF
//!      or an error — the OS reaps the sockets when both pipes
//!      are done.
//!
//!   2. NO inherited connect-timeout leaking into recv. Python's
//!      `socket.create_connection(timeout=5.0)` sets the socket's
//!      timeout for *all* subsequent operations; we were seeing
//!      `timed out` mid-stream after 5 s of idle. Rust's `connect`
//!      doesn't set a lingering timeout, and we explicitly clear
//!      any read/write timeouts before the pipes start.
//!
//! Usage:
//!     a64-stream-relay [LISTEN_ADDR] [TARGET_ADDR]
//! Defaults:
//!     LISTEN_ADDR = 0.0.0.0:14712
//!     TARGET_ADDR = 192.168.68.72:14712

use std::env;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::process;
use std::thread;

const DEFAULT_LISTEN: &str = "0.0.0.0:14712";
const DEFAULT_TARGET: &str = "192.168.68.72:14712";

fn pipe<R: Read, W: Write>(mut src: R, mut dst: W, tag: &'static str) {
    let mut buf = [0u8; 8192];
    loop {
        match src.read(&mut buf) {
            Ok(0) => break, // EOF — peer closed the read direction
            Ok(n) => {
                if let Err(e) = dst.write_all(&buf[..n]) {
                    // Write error often means the other side closed.
                    // Log once and exit so the other pipe thread can
                    // also unwind.
                    if e.kind() != ErrorKind::BrokenPipe {
                        eprintln!("[{tag}] write failed: {e}");
                    }
                    break;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == ErrorKind::ConnectionReset
                || e.kind() == ErrorKind::ConnectionAborted => break,
            Err(e) => {
                eprintln!("[{tag}] read failed: {e}");
                break;
            }
        }
    }
}

fn handle(client: TcpStream, client_peer: SocketAddr, target: SocketAddr) {
    eprintln!("[relay] {client_peer} connected; dialing {target}");
    let upstream = match TcpStream::connect(target) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[relay] upstream connect to {target} failed: {e}");
            return;
        }
    };

    // Paranoid but cheap — clear any lingering timeouts. `connect`
    // itself doesn't set them in std::net::TcpStream (unlike Python's
    // create_connection), but make the contract explicit.
    let _ = upstream.set_read_timeout(None);
    let _ = upstream.set_write_timeout(None);
    let _ = client.set_read_timeout(None);
    let _ = client.set_write_timeout(None);
    // TCP_NODELAY shaves a few ms off the streaming round-trip —
    // matters when we're doing 60 Hz sensor pumping and each frame
    // is a tiny packet (DATA = 9 B, EXEC = 5 B).
    let _ = client.set_nodelay(true);
    let _ = upstream.set_nodelay(true);

    // Two half-duplex pipes. Each gets its own clone of the sockets
    // so both threads can own read+write halves.
    let client_rd = match client.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[relay] client clone failed: {e}");
            return;
        }
    };
    let upstream_rd = match upstream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[relay] upstream clone failed: {e}");
            return;
        }
    };

    // client_rd reads from guest, upstream writes to Pi.
    let t_c2s = thread::spawn(move || pipe(client_rd, upstream, "c->s"));
    // upstream_rd reads from Pi, client writes to guest.
    let t_s2c = thread::spawn(move || pipe(upstream_rd, client, "s->c"));

    let _ = t_c2s.join();
    let _ = t_s2c.join();
    eprintln!("[relay] {client_peer} done");
}

fn resolve(addr: &str) -> SocketAddr {
    addr.to_socket_addrs()
        .unwrap_or_else(|e| {
            eprintln!("[relay] bad address {addr}: {e}");
            process::exit(2);
        })
        .next()
        .unwrap_or_else(|| {
            eprintln!("[relay] no addresses resolved for {addr}");
            process::exit(2);
        })
}

fn main() {
    let mut args = env::args().skip(1);
    let listen_arg = args.next().unwrap_or_else(|| DEFAULT_LISTEN.into());
    let target_arg = args.next().unwrap_or_else(|| DEFAULT_TARGET.into());
    let target_addr = resolve(&target_arg);

    let listener = match TcpListener::bind(&listen_arg) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[relay] bind {listen_arg} failed: {e}");
            process::exit(1);
        }
    };
    eprintln!("[relay] listening on {listen_arg} → {target_addr}");

    loop {
        let (client, peer) = match listener.accept() {
            Ok(x) => x,
            Err(e) => {
                eprintln!("[relay] accept failed: {e}");
                continue;
            }
        };
        thread::spawn(move || handle(client, peer, target_addr));
    }
}
