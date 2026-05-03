"""Single-shot VNC cursor probe. Sends a long sequence of deliberate
small relative motions, then a click, and we read the serial for what
the compositor thinks the cursor coords were on the click edge."""

from __future__ import annotations
import socket
import struct
import sys
import time


def rfb_handshake(sock):
    server_version = sock.recv(12)
    sock.sendall(b"RFB 003.008\n")
    n = sock.recv(1)[0]
    sec_types = sock.recv(n)
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


def pe(sock, x, y, b=0):
    sock.sendall(struct.pack(">BBHH", 5, b & 0xFF, x, y))


def main():
    target_x = int(sys.argv[1]) if len(sys.argv) > 1 else 200
    target_y = int(sys.argv[2]) if len(sys.argv) > 2 else 200

    sock = socket.create_connection(("127.0.0.1", 5901), timeout=10.0)
    w, h = rfb_handshake(sock)
    print(f"connected {w}x{h}")
    sock.settimeout(0.05)

    # 1) Hard floor: 200 events stepping toward (-1, -1) so QEMU's
    # internal vnc pointer ends at (0,0) and the guest's accumulator
    # has been driven to its 0-clamp.
    cur_x, cur_y = w - 1, h - 1
    pe(sock, cur_x, cur_y, 0)
    time.sleep(0.05)
    for _ in range(200):
        cur_x = max(0, cur_x - 20)
        cur_y = max(0, cur_y - 20)
        pe(sock, cur_x, cur_y, 0)
        time.sleep(0.02)
    # Park, give the guest a beat to drain the kernel ring.
    pe(sock, 0, 0, 0)
    time.sleep(1.0)

    # 2) Walk to target with small steps (PS/2 won't accept a giant
    # single delta).
    steps = 80
    for i in range(1, steps + 1):
        ix = target_x * i // steps
        iy = target_y * i // steps
        pe(sock, ix, iy, 0)
        time.sleep(0.02)
    time.sleep(0.5)

    # 3) Click
    print(f"clicking at ({target_x},{target_y})")
    pe(sock, target_x, target_y, 1)
    time.sleep(0.1)
    pe(sock, target_x, target_y, 0)
    time.sleep(0.5)


if __name__ == "__main__":
    main()
