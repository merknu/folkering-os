"""
Probe Issue #15: drives the OS via the same VNC PointerEvent path a real
TigerVNC viewer would. Boots Folkering, opens an RFB session over TCP, sends
N pointer events and measures how long it takes each one to surface as a
`[M]` marker on the serial log.

Why a barebones RFB client instead of vncdotool: we want millisecond-accurate
timing per event and a synchronous send/wait loop. Twisted's deferred queue
made that awkward in vncdotool. The handshake here is RFB 3.8 with security
type "None" (QEMU's default for unauthenticated VNC).
"""

from __future__ import annotations
import argparse
import socket
import struct
import sys
import time
from pathlib import Path


def rfb_handshake(sock: socket.socket) -> tuple[int, int]:
    # 1. Read server version, reply with our version (RFB 003.008).
    server_version = sock.recv(12)
    if not server_version.startswith(b"RFB"):
        raise RuntimeError(f"unexpected server greeting: {server_version!r}")
    sock.sendall(b"RFB 003.008\n")

    # 2. Server lists security types. With RFB 3.8 it's: u8 count + count*u8.
    n = sock.recv(1)
    if not n:
        raise RuntimeError("server closed during security negotiation")
    n_types = n[0]
    if n_types == 0:
        # Connection rejected, server sends reason string.
        reason_len = struct.unpack(">I", sock.recv(4))[0]
        reason = sock.recv(reason_len).decode("utf-8", "replace")
        raise RuntimeError(f"server rejected handshake: {reason}")
    sec_types = sock.recv(n_types)
    if 1 not in sec_types:
        raise RuntimeError(f"no `None` security; offered={list(sec_types)}")
    sock.sendall(bytes([1]))

    # 3. Security result: 4-byte big-endian, 0 = OK.
    result = struct.unpack(">I", sock.recv(4))[0]
    if result != 0:
        raise RuntimeError(f"security failed code={result}")

    # 4. ClientInit (shared=1 so we don't kick out other viewers).
    sock.sendall(b"\x01")

    # 5. ServerInit: width(u16) height(u16) pixel-format(16B) name-len(u32) name.
    init = sock.recv(24)
    if len(init) < 24:
        raise RuntimeError("short ServerInit")
    width, height = struct.unpack(">HH", init[:4])
    name_len = struct.unpack(">I", init[20:24])[0]
    if name_len:
        sock.recv(name_len)
    return width, height


def pointer_event(sock: socket.socket, x: int, y: int, buttons: int = 0) -> None:
    # Message-type 5 = PointerEvent: u8 type | u8 button-mask | u16 x | u16 y.
    sock.sendall(struct.pack(">BBHH", 5, buttons & 0xFF, x, y))


def count_m_markers(serial_path: Path) -> int:
    try:
        return serial_path.read_text(encoding="utf-8", errors="replace").count("[M]")
    except FileNotFoundError:
        return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=5901)
    ap.add_argument("--serial", required=True,
                    help="Path to QEMU serial log to count [M] markers in")
    ap.add_argument("--count", type=int, default=30, help="number of pointer events")
    ap.add_argument("--interval-ms", type=int, default=100,
                    help="sleep between sends")
    ap.add_argument("--per-event-timeout-ms", type=int, default=5000,
                    help="max wait per event before declaring drop")
    args = ap.parse_args()

    serial = Path(args.serial)
    if not serial.exists():
        print(f"serial log {serial} does not exist", file=sys.stderr)
        return 2

    print(f"connecting to VNC {args.host}:{args.port} ...")
    sock = socket.create_connection((args.host, args.port), timeout=10.0)
    sock.settimeout(10.0)
    try:
        width, height = rfb_handshake(sock)
        print(f"RFB connected: {width}x{height}")

        # Switch to a non-blocking-ish socket so we can send fast and not be
        # blocked on incoming framebuffer-update messages we ignore.
        sock.settimeout(0.05)

        baseline = count_m_markers(serial)
        print(f"baseline [M] markers: {baseline}")

        latencies = []
        dropped = 0
        for i in range(args.count):
            # Diagonal sweep across the screen; clamp to fb bounds.
            progress = i / max(1, args.count - 1)
            x = int(50 + progress * (width - 100))
            y = int(50 + progress * (height - 100))

            pre = count_m_markers(serial)
            t0 = time.perf_counter()
            pointer_event(sock, x, y, buttons=0)

            deadline = t0 + (args.per_event_timeout_ms / 1000.0)
            arrived = False
            while time.perf_counter() < deadline:
                if count_m_markers(serial) > pre:
                    arrived = True
                    break
                time.sleep(0.005)
            elapsed_ms = (time.perf_counter() - t0) * 1000.0

            if arrived:
                latencies.append(elapsed_ms)
                tag = "OK" if elapsed_ms < 1000 else "SLOW"
            else:
                dropped += 1
                tag = "DROP"
            print(f"  ev[{i:02d}]  ({x:>4},{y:>4})  {elapsed_ms:7.1f}ms  {tag}")

            time.sleep(args.interval_ms / 1000.0)

        print()
        if latencies:
            n = len(latencies)
            avg = sum(latencies) / n
            mx = max(latencies)
            mn = min(latencies)
            slow = sum(1 for ms in latencies if ms > 1000)
            print(f"delivered: {n}/{args.count}   "
                  f"avg={avg:.1f}ms  min={mn:.1f}ms  max={mx:.1f}ms")
            print(f"events >1s (freeze symptom): {slow}/{n}")
            if mx > 3000:
                print("FREEZE REPRODUCES — at least one event >3s (Issue #15 territory)")
        else:
            print("no events delivered at all")
        if dropped:
            print(f"dropped (no marker within {args.per_event_timeout_ms}ms): {dropped}")
        return 0
    finally:
        sock.close()


if __name__ == "__main__":
    sys.exit(main())
