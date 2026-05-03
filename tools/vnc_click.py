"""Send a couple of left-clicks via raw RFB and exit. Used to drive
the input pipeline live-test for folkui-demo: clicks land inside the
Draug-authored sysmon panel at (40,40)-(360,200).
"""

from __future__ import annotations
import argparse
import socket
import struct
import sys
import time


def rfb_handshake(sock: socket.socket) -> tuple[int, int]:
    server_version = sock.recv(12)
    if not server_version.startswith(b"RFB"):
        raise RuntimeError(f"unexpected greeting: {server_version!r}")
    sock.sendall(b"RFB 003.008\n")
    n = sock.recv(1)[0]
    if n == 0:
        reason_len = struct.unpack(">I", sock.recv(4))[0]
        raise RuntimeError(f"server rejected: {sock.recv(reason_len)!r}")
    sec_types = sock.recv(n)
    if 1 not in sec_types:
        raise RuntimeError(f"no None security; offered={list(sec_types)}")
    sock.sendall(bytes([1]))
    if struct.unpack(">I", sock.recv(4))[0] != 0:
        raise RuntimeError("security failed")
    sock.sendall(b"\x01")
    init = sock.recv(24)
    width, height = struct.unpack(">HH", init[:4])
    name_len = struct.unpack(">I", init[20:24])[0]
    if name_len:
        sock.recv(name_len)
    return width, height


def pointer_event(sock: socket.socket, x: int, y: int, buttons: int = 0) -> None:
    sock.sendall(struct.pack(">BBHH", 5, buttons & 0xFF, x, y))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=5901)
    ap.add_argument("--clicks", type=int, default=3)
    ap.add_argument("--x", type=int, default=200, help="click X (default panel center)")
    ap.add_argument("--y", type=int, default=120, help="click Y")
    args = ap.parse_args()

    sock = socket.create_connection((args.host, args.port), timeout=10.0)
    try:
        width, height = rfb_handshake(sock)
        print(f"RFB connected: {width}x{height}")
        sock.settimeout(0.05)

        # The guest mouse driver is PS/2 (relative). VNC PointerEvent
        # gives absolute coords, which QEMU converts to relative
        # deltas — but a lone "park" event at (x, y) is too big a
        # jump and gets clamped/lost. Drag the cursor toward the
        # target with many small steps so the deltas accumulate
        # cleanly into the guest's cursor position.
        steps = 40
        for i in range(steps):
            t = (i + 1) / steps
            xi = int(args.x * t)
            yi = int(args.y * t)
            pointer_event(sock, xi, yi, 0)
            time.sleep(0.02)

        # Park then click.
        pointer_event(sock, args.x, args.y, 0)
        time.sleep(0.3)

        for i in range(args.clicks):
            print(f"click {i+1}/{args.clicks} at ({args.x}, {args.y})")
            pointer_event(sock, args.x, args.y, 1)  # left down
            time.sleep(0.1)
            pointer_event(sock, args.x, args.y, 0)  # left up
            time.sleep(0.4)
        return 0
    finally:
        sock.close()


if __name__ == "__main__":
    sys.exit(main())
