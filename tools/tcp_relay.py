#!/usr/bin/env python3
"""
Fase B fallback: bridge QEMU guest's SLIRP network to the Pi daemon.

Folkering OS inside QEMU uses user-mode networking (SLIRP). Outbound
connections go through the host's socket stack, but in practice
arbitrary LAN destinations (192.168.x.x) either get blocked or
dropped — Folkering's entire networking has historically pointed at
the SLIRP gateway (10.0.2.2, which maps to host loopback).

This relay listens on the host and forwards each accepted TCP
connection to the real target. draug-streamer connects to the host
address via hostfwd (or directly to 10.0.2.2:PORT for SLIRP default
gateway); we pipe bytes to the Pi daemon at 192.168.68.72:14712.

Usage:
    py -3 tools/tcp_relay.py [LISTEN_PORT] [PI_HOST] [PI_PORT]
Defaults:
    LISTEN_PORT = 14712  (so host's own nc/curl works too)
    PI_HOST     = 192.168.68.72
    PI_PORT     = 14712

Pair with QEMU `-netdev user,hostfwd=tcp::14712-:14712` if you want
the guest to reach the relay via the gateway. Simpler: change
draug-streamer's DAEMON_IP to 10.0.2.2 so the guest hits the host
directly via the SLIRP gateway.
"""

import socket
import sys
import threading

def pipe(src: socket.socket, dst: socket.socket, tag: str) -> None:
    """Forward bytes from src → dst. Exits silently on EOF or error.
    Does NOT shut down dst — let the other-direction thread handle
    its own lifecycle. Closing one half while the other is mid-transfer
    has been observed to kill the streaming session (SLIRP reports
    FIN_WAIT_2 and the guest sees PeerClosed)."""
    try:
        while True:
            chunk = src.recv(8192)
            if not chunk:
                break
            dst.sendall(chunk)
    except OSError as e:
        print(f"[{tag}] stream error: {e}", flush=True)

def handle(client: socket.socket, client_addr, pi_host: str, pi_port: int) -> None:
    print(f"[relay] {client_addr} connected; dialing {pi_host}:{pi_port}", flush=True)
    try:
        upstream = socket.create_connection((pi_host, pi_port), timeout=5.0)
    except OSError as e:
        print(f"[relay] upstream connect failed: {e}", flush=True)
        client.close()
        return
    # Important: create_connection's `timeout` applies to the socket
    # for all subsequent operations. Clear it so recv() blocks
    # indefinitely — the streaming protocol can have arbitrary gaps
    # between frames (Folkering guest's scheduler may take seconds
    # to produce the next sample) and a timeout here tears down the
    # session mid-stream.
    upstream.settimeout(None)
    client.settimeout(None)
    # Tee bytes each direction.
    t1 = threading.Thread(target=pipe, args=(client, upstream, "c->s"), daemon=True)
    t2 = threading.Thread(target=pipe, args=(upstream, client, "s->c"), daemon=True)
    t1.start(); t2.start()
    t1.join(); t2.join()
    try: client.close()
    except OSError: pass
    try: upstream.close()
    except OSError: pass
    print(f"[relay] {client_addr} done")

def main() -> None:
    listen_port = int(sys.argv[1]) if len(sys.argv) > 1 else 14712
    pi_host = sys.argv[2] if len(sys.argv) > 2 else "192.168.68.72"
    pi_port = int(sys.argv[3]) if len(sys.argv) > 3 else 14712

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    # Bind to all interfaces so both host (direct) and guest (via
    # SLIRP hostfwd) can reach us.
    srv.bind(("0.0.0.0", listen_port))
    srv.listen(4)
    print(f"[relay] listening 0.0.0.0:{listen_port} -> {pi_host}:{pi_port}")

    while True:
        client, addr = srv.accept()
        threading.Thread(target=handle, args=(client, addr, pi_host, pi_port), daemon=True).start()

if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\n[relay] stopped")
