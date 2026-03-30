#!/usr/bin/env python3
"""Kernel Integration Test — boots QEMU, checks serial for assertions, auto-exits.

Usage: python tests/kernel-boot-test.py

Expects QEMU to boot Folkering OS with isa-debug-exit device.
The kernel writes test results to COM1 (serial).
On success, kernel writes 0x31 to port 0xf4 → QEMU exits with code (0x31*2+1)=99.
On failure, kernel writes 0x01 to port 0xf4 → QEMU exits with code (0x01*2+1)=3.

Test looks for "[TEST] PASS" or "[TEST] FAIL" in serial output.
"""

import subprocess
import sys
import os
import time

PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOOT_IMG = os.path.join(PROJECT, "boot", "current.img")
DATA_IMG = os.path.join(PROJECT, "boot", "virtio-data.img")
SERIAL_LOG = os.path.join(PROJECT, "tests", "test-serial.log")
QEMU = r"C:\Program Files\qemu\qemu-system-x86_64.exe"

def run_test(timeout=30):
    """Boot QEMU with isa-debug-exit, capture serial, check results."""

    # Clear log
    with open(SERIAL_LOG, "w") as f:
        f.write("")

    cmd = [
        QEMU,
        "-drive", f"file={BOOT_IMG},format=raw,if=ide",
        "-drive", f"file={DATA_IMG},format=raw,if=none,id=vdisk0",
        "-device", "virtio-blk-pci,drive=vdisk0",
        "-netdev", "user,id=net0",
        "-device", "virtio-net-pci,netdev=net0",
        "-vga", "virtio",
        "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04",  # Test exit device
        "-accel", "tcg",       # TCG for reliable testing (no WHPX timer issues)
        "-cpu", "max,rdrand=on",
        "-smp", "1",           # Single core for deterministic tests
        "-m", "512M",          # Minimal RAM
        "-serial", f"file:{SERIAL_LOG}",
        "-display", "none",
        "-no-reboot",
    ]

    print(f"[TEST] Starting QEMU (timeout={timeout}s)...")
    try:
        result = subprocess.run(cmd, timeout=timeout, capture_output=True, text=True)
        exit_code = result.returncode
    except subprocess.TimeoutExpired:
        print("[TEST] TIMEOUT — kernel hung or never called debug-exit")
        return False

    # Read serial output
    serial = ""
    try:
        with open(SERIAL_LOG, "r", errors="replace") as f:
            serial = f.read()
    except FileNotFoundError:
        pass

    # Check exit code: 99 = success (0x31*2+1), 3 = failure (0x01*2+1)
    # QEMU isa-debug-exit: exit_code = (value_written * 2) + 1
    print(f"[TEST] QEMU exit code: {exit_code}")

    # Parse serial for test results
    pass_count = serial.count("[TEST] PASS")
    fail_count = serial.count("[TEST] FAIL")

    # Show test lines
    for line in serial.split("\n"):
        if "[TEST]" in line or "[BOOT]" in line:
            print(f"  {line.strip()}")

    if fail_count > 0:
        print(f"\n[RESULT] FAILED — {fail_count} test(s) failed")
        return False

    if pass_count > 0 and exit_code == 99:
        print(f"\n[RESULT] ALL PASSED — {pass_count} test(s)")
        return True

    if exit_code == 99:
        print(f"\n[RESULT] PASSED (exit code 99)")
        return True

    print(f"\n[RESULT] UNKNOWN — exit={exit_code}, pass={pass_count}, fail={fail_count}")
    return False


if __name__ == "__main__":
    ok = run_test()
    sys.exit(0 if ok else 1)
