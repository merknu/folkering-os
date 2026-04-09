#!/usr/bin/env python3
"""Folkering OS — Core Application Suite Bootstrapper

Generates the standard suite of WASM apps by injecting commands via COM3.
Each app is compiled by the LLM proxy and permanently cached in Synapse VFS
with semantic intent metadata.

Usage:
  1. Ensure QEMU + proxy are running (start-folkering.ps1)
  2. python tools/bootstrap_core_apps.py

The script waits for each app to compile before proceeding to the next.
Uses SO_LINGER=0 for clean TCP teardown (no CLOSE_WAIT).
"""

import socket
import struct
import time
import sys
import os
import re
import subprocess

# ── Configuration ────────────────────────────────────────────────────────

COM3_HOST = "127.0.0.1"
COM3_PORT = 4568
SERIAL_LOG = os.path.join(os.path.expanduser("~"), "folkering-mcp", "serial.log")

# Per-app timeout (seconds) — LLM gen + cargo compile
APP_TIMEOUT = 120

# Fix Windows console encoding
if sys.platform == "win32":
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

# ── Core App Definitions ─────────────────────────────────────────────────

CORE_APPS = [
    {
        "cmd": "gemini generate simple counter display that shows a number and increments it every second",
        "name": "counter",
        "desc": "Simple incrementing counter display",
    },
    {
        "cmd": "gemini generate digital clock widget showing current time with large text updated every frame using folk_get_datetime",
        "name": "digital clock",
        "desc": "Real-time digital clock with large digits",
    },
    {
        "cmd": "gemini generate interactive text notepad with cursor and keyboard input that saves to files",
        "name": "notepad",
        "desc": "Nano-style text editor with keyboard input and file save",
    },
    {
        "cmd": "gemini generate system monitor showing memory usage as bar chart and uptime counter",
        "name": "system monitor",
        "desc": "Live system stats: memory bars + uptime",
    },
    {
        "cmd": "gemini generate interactive file browser that lists all files with names and sizes",
        "name": "file explorer",
        "desc": "VFS file browser with keyboard navigation",
    },
]

# ── Helpers ──────────────────────────────────────────────────────────────

def wait_for_clean_port():
    """Wait for any CLOSE_WAIT on COM3 to clear."""
    for _ in range(60):
        result = subprocess.run(["netstat", "-an"], capture_output=True, text=True, timeout=5)
        # Check specifically for COM3 port + CLOSE_WAIT on the same line
        has_close_wait = any(
            f":{COM3_PORT}" in line and "CLOSE_WAIT" in line
            for line in result.stdout.split("\n")
        )
        if not has_close_wait:
            return True
        time.sleep(2)
    return False


def connect_com3():
    """Connect to COM3 with SO_LINGER=0."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(5)
    # Normal close (FIN) — QEMU COM3 dies permanently after RST (SO_LINGER=0)
    sock.connect((COM3_HOST, COM3_PORT))
    return sock


def inject_command(sock, command, delay=0.04):
    """Send command via COM3 with buffer flush prefix."""
    # Flush any leftover buffer
    sock.send(b"\n")
    time.sleep(0.2)
    # Send command byte-by-byte
    for ch in command + "\n":
        sock.send(ch.encode("ascii"))
        time.sleep(delay)


def get_log_baseline():
    """Get current serial log file size."""
    try:
        return os.path.getsize(SERIAL_LOG)
    except OSError:
        return 0


def wait_for_pattern(baseline, pattern, timeout):
    """Wait for regex pattern in new serial log lines."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            current_size = os.path.getsize(SERIAL_LOG)
            if current_size > baseline:
                with open(SERIAL_LOG, "rb") as f:
                    f.seek(baseline)
                    new_data = f.read(current_size - baseline)
                text = new_data.decode("utf-8", errors="replace")
                if re.search(pattern, text):
                    return True, text
        except OSError:
            pass
        time.sleep(1)
    return False, ""


# ── Main ─────────────────────────────────────────────────────────────────

def main():
    print()
    print("=" * 60)
    print("  Folkering OS — Core Application Suite Bootstrapper")
    print("=" * 60)
    print()

    # Check prerequisites
    if not os.path.exists(SERIAL_LOG):
        print(f"ERROR: Serial log not found: {SERIAL_LOG}")
        print("Start QEMU first (start-folkering.ps1)")
        return 1

    # Wait for clean COM3 port
    print("[1/3] Waiting for clean COM3 port...")
    if not wait_for_clean_port():
        print("ERROR: COM3 port stuck in CLOSE_WAIT")
        return 1
    print("  OK")

    # Connect
    print("[2/3] Connecting to COM3...")
    try:
        sock = connect_com3()
        print(f"  Connected ({COM3_HOST}:{COM3_PORT})")
    except Exception as e:
        print(f"ERROR: {e}")
        return 1

    # Generate apps
    print(f"[3/3] Generating {len(CORE_APPS)} core apps...\n")

    results = {}
    for i, app in enumerate(CORE_APPS):
        num = i + 1
        total = len(CORE_APPS)
        print(f"--- [{num}/{total}] {app['name']} ---")
        print(f"  Prompt: {app['cmd'][:70]}...")

        baseline = get_log_baseline()
        inject_command(sock, app["cmd"])
        print(f"  Injected. Waiting for compilation (max {APP_TIMEOUT}s)...")

        # Wait for WASM compilation
        found, log_text = wait_for_pattern(
            baseline,
            r"\[MCP\] WASM (assembled|single chunk)|\[Cache\] Stored WASM",
            APP_TIMEOUT
        )

        if found:
            # Extract size from log
            size_match = re.search(r"(\d+) bytes", log_text)
            size_str = f" ({size_match.group(1)} bytes)" if size_match else ""
            print(f"  PASS: {app['name']} compiled{size_str}")
            results[app["name"]] = True
        else:
            # Check if there was an error
            _, err_text = wait_for_pattern(baseline, r"Error:|CLARIFY:|QUESTION:", 2)
            if err_text:
                err_match = re.search(r"(Error:.*|CLARIFY:.*|QUESTION:.*)", err_text)
                reason = err_match.group(1)[:60] if err_match else "unknown"
                print(f"  FAIL: {reason}")
            else:
                print(f"  FAIL: Timeout ({APP_TIMEOUT}s)")
            results[app["name"]] = False

        # Pause between apps to let proxy settle
        if i < total - 1:
            print(f"  Cooling down (5s)...")
            time.sleep(5)
        print()

    # Close COM3
    sock.close()

    # Summary
    print("=" * 60)
    print("  BOOTSTRAP SUMMARY")
    print("=" * 60)

    passed = sum(1 for v in results.values() if v)
    for name, ok in results.items():
        status = "PASS" if ok else "FAIL"
        print(f"  [{status}] {name}")

    print(f"\n  {passed}/{len(results)} apps generated")

    if passed == len(results):
        print("\n  ALL APPS GENERATED SUCCESSFULLY!")
        print("  Run 'open calculator' or 'open clock' in the omnibar.")
    elif passed > 0:
        print(f"\n  {len(results) - passed} apps failed (proxy may be rate-limited).")
        print("  Re-run this script to retry failed apps.")
    else:
        print("\n  No apps generated. Check proxy connection and LLM access.")

    print()
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
