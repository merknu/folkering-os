"""Drive the folkui-demo calculator over VNC. Boots assumed to be done.
Sends pointer drags + clicks to a sequence of (x,y) pairs and prints
serial-side `[FOLKUI-DEMO] click #N` lines that confirm the input
pipeline + hit_test resolved each click to the intended button id.

Calculator window: x=100 y=60, w=260 h=320. Buttons in 4 rows × 4 cols
per the markup in `userspace/folkui-demo/src/main.rs`. Centers below
are picked by hand from the layout (see comment below) — the script's
job is to *verify* hit_test agrees, not recompute the layout itself.

Usage:
  py -3.12 tools/vnc_calc_demo.py --port 5901 --serial tools/calc_serial.log
"""

from __future__ import annotations
import argparse
import socket
import struct
import sys
import time
from pathlib import Path


# Window: (100,60)..(360,380). VBox padding=12 → inner (112,72)..(348,368).
# Display row ≈ 24 px tall; spacing=6 between rows; 4 rows × 4 cols of
# buttons fill the rest. Approx button centers (verified empirically):
ROW_Y = [133, 201, 269, 337]
COL_X = [139, 197, 255, 313]
BUTTONS = {
    "btn_7": (COL_X[0], ROW_Y[0]),
    "btn_8": (COL_X[1], ROW_Y[0]),
    "btn_9": (COL_X[2], ROW_Y[0]),
    "btn_div": (COL_X[3], ROW_Y[0]),
    "btn_4": (COL_X[0], ROW_Y[1]),
    "btn_5": (COL_X[1], ROW_Y[1]),
    "btn_6": (COL_X[2], ROW_Y[1]),
    "btn_mul": (COL_X[3], ROW_Y[1]),
    "btn_1": (COL_X[0], ROW_Y[2]),
    "btn_2": (COL_X[1], ROW_Y[2]),
    "btn_3": (COL_X[2], ROW_Y[2]),
    "btn_sub": (COL_X[3], ROW_Y[2]),
    "btn_0": (COL_X[0], ROW_Y[3]),
    "btn_clear": (COL_X[1], ROW_Y[3]),
    "btn_eq": (COL_X[2], ROW_Y[3]),
    "btn_add": (COL_X[3], ROW_Y[3]),
}


def rfb_handshake(sock):
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

    # Advertise POINTER_TYPE_CHANGE (-257). Without this, QEMU's VNC
    # server sticks the connection in relative-deltas mode and pointer
    # positions go to PS/2 instead of any registered virtio-tablet —
    # we get button events but no `EV_ABS`. With the pseudo-encoding
    # advertised, QEMU flips this connection to absolute mode and our
    # PointerEvent (x, y) lands as ABS_X/ABS_Y on virtio-tablet's
    # eventq. Encoding list also includes Raw (0) so the server has
    # at least one real framebuffer encoding to fall back on; we
    # never request a framebuffer update so it doesn't actually
    # matter, but RFB 3.8 expects at least one.
    encs = [0, -257]  # Raw, PointerTypeChange
    msg = struct.pack(">BBH", 2, 0, len(encs))
    for e in encs:
        msg += struct.pack(">i", e)
    sock.sendall(msg)
    return width, height


def pointer_event(sock, x, y, buttons=0):
    sock.sendall(struct.pack(">BBHH", 5, buttons & 0xFF, x, y))


def drag_to(sock, last_xy, target_xy, steps=20, step_ms=15):
    """Walk the cursor toward target with small accumulating relative
    deltas. PS/2 mouse won't move on a single huge jump — QEMU clamps
    the relative delta — so we feather it out."""
    lx, ly = last_xy
    tx, ty = target_xy
    for i in range(1, steps + 1):
        ix = lx + (tx - lx) * i // steps
        iy = ly + (ty - ly) * i // steps
        pointer_event(sock, ix, iy, 0)
        time.sleep(step_ms / 1000.0)


def click_at(sock, x, y, hold_ms=80, settle_ms=200):
    pointer_event(sock, x, y, 1)  # left down
    time.sleep(hold_ms / 1000.0)
    pointer_event(sock, x, y, 0)  # left up
    time.sleep(settle_ms / 1000.0)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=5901)
    ap.add_argument("--serial", default=None,
                    help="Path to QEMU serial log; we tail it after the run")
    ap.add_argument("--sequence", default="5,add,3,eq",
                    help="Comma-separated calculator buttons (digits or "
                         "add/sub/mul/div/eq/clear)")
    # Folkering OS framebuffer is the virtio-gpu display (1280x800 on
    # Proxmox VM 800 by default). VNC, however, may advertise a
    # different resolution to the client (Proxmox/QEMU still reports
    # the legacy 1024x768 even after virtio-gpu has resized). Click
    # targets in BUTTONS are in OS-framebuffer pixels, so we must scale
    # them to whatever the VNC server reports as the desktop size.
    ap.add_argument("--fb-w", type=int, default=1280, help="OS framebuffer width")
    ap.add_argument("--fb-h", type=int, default=800,  help="OS framebuffer height")
    args = ap.parse_args()

    seq = args.sequence.split(",")
    targets = []
    for s in seq:
        s = s.strip()
        if s.isdigit() and len(s) == 1:
            targets.append(f"btn_{s}")
        elif s in ("add", "sub", "mul", "div", "eq", "clear"):
            targets.append(f"btn_{s}")
        else:
            raise SystemExit(f"unknown button: {s!r}")
    print(f"plan: {' '.join(targets)}")

    sock = socket.create_connection((args.host, args.port), timeout=10.0)
    try:
        w, h = rfb_handshake(sock)
        print(f"RFB connected: {w}x{h}  (OS fb {args.fb_w}x{args.fb_h})")
        sock.settimeout(0.05)

        # Scale OS-framebuffer-space targets into VNC-pixel-space.
        def to_vnc(p):
            tx, ty = p
            vx = tx * w // args.fb_w
            vy = ty * h // args.fb_h
            return (vx, vy)

        # PS/2 mouse only sees relative deltas. Force the guest's
        # cursor to the top-left by sweeping the VNC pointer all the
        # way from (w-1, h-1) to a large negative origin. QEMU clamps
        # the relative delta into the real frame, so 60 steps of
        # ~-30 in each axis is more than enough to floor any starting
        # position. Then drive forward from a known origin.
        for i in range(60):
            tx = (w - 1) - (w + 200) * i // 60
            ty = (h - 1) - (h + 200) * i // 60
            pointer_event(sock, max(tx, 0), max(ty, 0), 0)
            time.sleep(0.01)
        cur = (1, 1)
        pointer_event(sock, *cur, 0)
        time.sleep(0.5)

        for label in targets:
            fb_tgt = BUTTONS[label]
            tgt = to_vnc(fb_tgt)
            print(f"  -> {label} @ fb {fb_tgt} -> vnc {tgt}")
            drag_to(sock, cur, tgt)
            cur = tgt
            click_at(sock, *cur)

    finally:
        sock.close()

    if args.serial:
        time.sleep(0.5)
        log = Path(args.serial)
        if not log.exists():
            print(f"\n(serial log not at {log})")
            return 0
        text = log.read_text(errors="replace").splitlines()
        click_lines = [l for l in text if "[FOLKUI-DEMO] click" in l]
        print("\n=== folkui-demo click log ===")
        for l in click_lines[-20:]:
            print(l)
        if not click_lines:
            print("(no [FOLKUI-DEMO] click lines — input pipeline didn't deliver)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
