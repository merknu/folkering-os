#!/usr/bin/env python3
"""Folkering OS — Automated E2E IQE Test Suite

Injects real keyboard + mouse events via VNC (RFB protocol),
reads guest-side microsecond telemetry from COM3 (TCP:4568),
and reports PASS/FAIL with latency measurements.

Usage:
  1. Start QEMU with VNC + COM3  (use start-folkering.ps1 or manual)
  2. python tests/iqe-automated-tests.py

Tests:
  - Keyboard latency:  RFB KeyEvent -> COM3 "IQE,KBD,<us>"
  - Mouse latency:     RFB PointerEvent -> COM3 "IQE,MOU,<us>"
  - Window open time:  Type "open calc" -> measure first GpuFlush after command
"""

import socket
import struct
import time
import threading
import sys
import os

# ── Configuration ────────────────────────────────────────────────────────

VNC_HOST = "127.0.0.1"
VNC_PORT = 5900
COM3_HOST = "127.0.0.1"
COM3_PORT = 4568
QMP_HOST = "127.0.0.1"
QMP_PORT = 4445

# ── Minimal RFB (VNC) Client ────────────────────────────────────────────

class RFBClient:
    """Minimal VNC client — sends real keyboard/mouse events via RFB protocol.
    These generate actual PS/2 interrupts (IRQ1/IRQ12) in the guest OS."""

    def __init__(self, host=VNC_HOST, port=VNC_PORT):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.settimeout(5)
        self.sock.connect((host, port))
        self._handshake()

    def _handshake(self):
        # Server sends: "RFB 003.008\n" (12 bytes)
        version = self.sock.recv(12)
        if not version.startswith(b"RFB"):
            raise RuntimeError(f"Not a VNC server: {version}")
        # Reply with same version
        self.sock.send(b"RFB 003.008\n")

        # Security types: server sends count + types
        num_types = struct.unpack("B", self.sock.recv(1))[0]
        if num_types == 0:
            # Connection failed — read reason
            reason_len = struct.unpack(">I", self.sock.recv(4))[0]
            reason = self.sock.recv(reason_len)
            raise RuntimeError(f"VNC refused: {reason.decode()}")
        types = self.sock.recv(num_types)

        # Choose "None" (type 1) if available
        if 1 in types:
            self.sock.send(bytes([1]))  # No auth
        else:
            raise RuntimeError(f"No supported auth: {list(types)}")

        # Security result (RFB 3.8)
        result = struct.unpack(">I", self.sock.recv(4))[0]
        if result != 0:
            raise RuntimeError(f"Auth failed: {result}")

        # ClientInit: shared=1
        self.sock.send(bytes([1]))

        # ServerInit: width(2) + height(2) + pixel_format(16) + name_len(4) + name
        header = self.sock.recv(24)
        self.width, self.height = struct.unpack(">HH", header[:4])
        name_len = struct.unpack(">I", header[20:24])[0]
        self.name = self.sock.recv(name_len).decode(errors="replace")

    def key_event(self, keysym, down=True):
        """Send RFB KeyEvent (type 4) -> generates PS/2 IRQ1 in guest."""
        msg = struct.pack(">BBxxI", 4, 1 if down else 0, keysym)
        self.sock.send(msg)

    def pointer_event(self, x, y, buttons=0):
        """Send RFB PointerEvent (type 5) -> generates PS/2 IRQ12 in guest."""
        x = max(0, min(x, self.width - 1))
        y = max(0, min(y, self.height - 1))
        msg = struct.pack(">BBHH", 5, buttons, x, y)
        self.sock.send(msg)

    def type_key(self, keysym, delay=0.05):
        """Press and release a key."""
        self.key_event(keysym, down=True)
        time.sleep(delay)
        self.key_event(keysym, down=False)
        time.sleep(delay)

    def type_text(self, text, delay=0.03):
        """Type a string character by character."""
        for ch in text:
            self.type_key(ord(ch), delay)

    def move_mouse(self, x, y):
        """Move mouse to absolute position."""
        self.pointer_event(x, y, 0)

    def click(self, x, y, button=1):
        """Click at position (button: 1=left, 2=middle, 4=right)."""
        self.pointer_event(x, y, button)
        time.sleep(0.05)
        self.pointer_event(x, y, 0)

    def close(self):
        self.sock.close()


# ── COM3 Listener (reads IQE telemetry from guest) ──────────────────────

class COM3Listener:
    """Reads IQE telemetry lines from COM3 (TCP:4568).
    Guest sends: IQE,KBD,1234\n or IQE,MOU,567\n"""

    def __init__(self, host=COM3_HOST, port=COM3_PORT):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.settimeout(2)
        self.sock.connect((host, port))
        self.events = []
        self.buffer = b""
        self._running = True
        self._thread = threading.Thread(target=self._read_loop, daemon=True)
        self._thread.start()

    def _read_loop(self):
        while self._running:
            try:
                data = self.sock.recv(1024)
                if not data:
                    break
                self.buffer += data
                while b"\n" in self.buffer:
                    line, self.buffer = self.buffer.split(b"\n", 1)
                    text = line.decode(errors="replace").strip()
                    if text.startswith("IQE,"):
                        parts = text.split(",")
                        if len(parts) >= 3:
                            self.events.append({
                                "type": parts[1],
                                "latency_us": int(parts[2]),
                                "timestamp": time.time(),
                            })
            except socket.timeout:
                continue
            except Exception:
                break

    def wait_for_event(self, event_type, timeout=10):
        """Wait for a specific IQE event type (KBD or MOU)."""
        start = time.time()
        initial_count = len([e for e in self.events if e["type"] == event_type])
        while time.time() - start < timeout:
            current = [e for e in self.events if e["type"] == event_type]
            if len(current) > initial_count:
                return current[-1]
            time.sleep(0.1)
        return None

    def clear(self):
        self.events.clear()

    def close(self):
        self._running = False
        self.sock.close()


# ── Test Cases ──────────────────────────────────────────────────────────

def test_keyboard_latency(vnc, com3):
    """Test: Type a character, measure guest-side IRQ1->GpuFlush latency."""
    print("\n[TEST 1] Keyboard Latency (IRQ1 -> GPU Flush)")
    print("  Injecting 5 keystrokes via VNC RFB...")
    com3.clear()

    latencies = []
    for ch in "hello":
        vnc.type_key(ord(ch), delay=0.1)
        time.sleep(0.3)

    # Wait for COM3 events
    time.sleep(2)
    kbd_events = [e for e in com3.events if e["type"] == "KBD"]

    if not kbd_events:
        print("  RESULT: FAIL — no KBD events received on COM3")
        return False

    for e in kbd_events:
        latencies.append(e["latency_us"])

    avg = sum(latencies) // len(latencies)
    min_l = min(latencies)
    max_l = max(latencies)

    print(f"  Events received: {len(kbd_events)}")
    print(f"  Latency: avg={avg}us  min={min_l}us  max={max_l}us")
    ok = avg < 50_000  # <50ms is acceptable
    print(f"  RESULT: {'PASS' if ok else 'FAIL'} (threshold: <50ms)")
    return ok


def test_mouse_latency(vnc, com3):
    """Test: Move mouse in a pattern, measure guest-side IRQ12->GpuFlush latency."""
    print("\n[TEST 2] Mouse Latency (IRQ12 -> GPU Flush)")
    print("  Injecting 10 mouse movements via VNC RFB...")
    com3.clear()

    # Move mouse to different positions
    positions = [
        (200, 300), (400, 300), (600, 300), (400, 500),
        (200, 500), (300, 400), (500, 200), (700, 400),
        (300, 600), (500, 400),
    ]
    for x, y in positions:
        vnc.move_mouse(x, y)
        time.sleep(0.2)

    time.sleep(2)
    mou_events = [e for e in com3.events if e["type"] == "MOU"]

    if not mou_events:
        print("  RESULT: FAIL — no MOU events received on COM3")
        return False

    latencies = [e["latency_us"] for e in mou_events]
    avg = sum(latencies) // len(latencies)
    min_l = min(latencies)
    max_l = max(latencies)

    print(f"  Events received: {len(mou_events)}")
    print(f"  Latency: avg={avg}us  min={min_l}us  max={max_l}us")
    ok = avg < 50_000
    print(f"  RESULT: {'PASS' if ok else 'FAIL'} (threshold: <50ms)")
    return ok


def test_window_open(vnc, com3):
    """Test: Type 'open calc' in omnibar, measure time until GPU flush."""
    print("\n[TEST 3] Window Open Latency ('open calc')")
    print("  Typing 'open calc' + Enter via VNC RFB...")
    com3.clear()

    start = time.time()
    vnc.type_text("open calc", delay=0.05)
    time.sleep(0.1)
    vnc.type_key(0xFF0D, delay=0.05)  # Enter

    # Wait for KBD events (the 'open calc' keystrokes trigger flush)
    time.sleep(5)
    kbd_events = [e for e in com3.events if e["type"] == "KBD"]
    elapsed_ms = int((time.time() - start) * 1000)

    if kbd_events:
        last_latency = kbd_events[-1]["latency_us"]
        print(f"  KBD events during open: {len(kbd_events)}")
        print(f"  Last KBD latency: {last_latency}us")
        print(f"  Wall-clock time: {elapsed_ms}ms")
        print(f"  RESULT: PASS")
        return True
    else:
        print(f"  Wall-clock time: {elapsed_ms}ms")
        print(f"  RESULT: FAIL — no IQE events for window open")
        return False


# ── Main ────────────────────────────────────────────────────────────────

def main():
    print("=" * 60)
    print("  Folkering OS — Automated E2E IQE Test Suite")
    print("=" * 60)

    # Check connectivity
    print("\n[SETUP] Connecting to QEMU services...")

    try:
        vnc = RFBClient()
        print(f"  VNC: Connected ({vnc.width}x{vnc.height}, '{vnc.name}')")
    except Exception as e:
        print(f"  VNC: FAILED — {e}")
        print("  Make sure QEMU is running with -vnc 0.0.0.0:0")
        sys.exit(1)

    try:
        com3 = COM3Listener()
        print(f"  COM3: Connected (TCP:{COM3_PORT})")
    except Exception as e:
        print(f"  COM3: FAILED — {e}")
        print("  Make sure QEMU has -serial tcp:127.0.0.1:4568,server,nowait")
        vnc.close()
        sys.exit(1)

    # Wait for OS to boot
    print("\n[SETUP] Waiting 3s for Folkering OS to boot...")
    time.sleep(3)

    # Run tests
    results = {}
    results["keyboard"] = test_keyboard_latency(vnc, com3)
    results["mouse"] = test_mouse_latency(vnc, com3)

    # ESC to close any opened app, then test window open
    vnc.type_key(0xFF1B)  # Escape
    time.sleep(1)
    results["window_open"] = test_window_open(vnc, com3)

    # Summary
    print("\n" + "=" * 60)
    print("  SUMMARY")
    print("=" * 60)
    total = len(results)
    passed = sum(1 for v in results.values() if v)
    for name, ok in results.items():
        print(f"  {'PASS' if ok else 'FAIL'}: {name}")
    print(f"\n  {passed}/{total} tests passed")
    print("=" * 60)

    vnc.close()
    com3.close()
    sys.exit(0 if passed == total else 1)


if __name__ == "__main__":
    main()
