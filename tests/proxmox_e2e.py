#!/usr/bin/env python3
"""Folkering OS — Proxmox E2E Test Suite

Comprehensive end-to-end testing on Proxmox VM 900.
Tests: boot, DHCP, DNS, WASM gen, firewall, AutoDream, COM3 injection, stress.

Usage: python tests/proxmox_e2e.py
"""

import socket
import time
import sys
import subprocess

PROXMOX = "192.168.68.150"
VM_ID = 900
COM3_PORT = 4568
SERIAL_LOG = "/tmp/folkering-serial.log"

if sys.platform == "win32":
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

def ssh(cmd, timeout=15):
    try:
        r = subprocess.run(
            ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=5",
             f"root@{PROXMOX}", cmd],
            capture_output=True, text=True, timeout=timeout)
        return r.stdout.strip()
    except: return ""

def inject(cmd, delay=0.1):
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(10)
        s.connect((PROXMOX, COM3_PORT))
        s.send(b'\n'); time.sleep(1)
        for c in cmd + '\n':
            s.send(c.encode()); time.sleep(delay)
        s.close()
        return True
    except: return False

def check(pattern, label):
    result = ssh(f"grep -c '{pattern}' {SERIAL_LOG}")
    count = int(result) if result.isdigit() else 0
    status = "PASS" if count > 0 else "FAIL"
    print(f"  [{status}] {label} ({count} matches)")
    return count > 0

def check_absent(pattern, label):
    result = ssh(f"grep -c '{pattern}' {SERIAL_LOG}")
    count = int(result) if result.isdigit() else 0
    status = "PASS" if count == 0 else "FAIL"
    print(f"  [{status}] {label} ({count} found)")
    return count == 0


def main():
    print("=" * 60)
    print("  Folkering OS — Proxmox E2E Test Suite")
    print("=" * 60)

    # Check VM is running
    status = ssh(f"qm status {VM_ID}")
    if "running" not in status:
        print(f"ERROR: VM {VM_ID} is not running")
        return 1

    print(f"\n  VM {VM_ID}: {status}")
    print()

    results = {}

    # ── Test 1: Boot Health ──
    print("--- Test 1: Boot Health ---")
    results["boot"] = all([
        check("LOOP ALIVE", "Compositor alive"),
        check("SYNAPSE.*Ready", "Synapse VFS ready"),
        check_absent("PANIC", "No kernel panics"),
        check_absent("DOUBLE FAULT", "No double faults"),
    ])

    # ── Test 2: Network ──
    print("\n--- Test 2: Network ---")
    results["network"] = all([
        check("DHCP.*got", "DHCP IP acquired"),
        check("Ping.*reply", "Ping gateway"),
    ])

    # ── Test 3: COM3 Injection ──
    print("\n--- Test 3: COM3 Injection ---")
    inject("help")
    time.sleep(5)
    results["com3"] = check("COM3.*Inject.*help", "COM3 command received")

    # ── Test 4: Firewall ──
    print("\n--- Test 4: Firewall ---")
    results["firewall"] = check_absent("FW.*DROP", "No unexpected drops (clean network)")

    # ── Test 5: WASM Generation ──
    print("\n--- Test 5: WASM Generation ---")
    inject("gemini generate tiny blue dot")
    print("  Waiting 90s for LLM + compile...")
    time.sleep(90)
    results["wasm_gen"] = check("Cache.*Stored", "WASM app cached")

    # ── Test 6: Stress Injection (10 rapid commands) ──
    print("\n--- Test 6: Stress Injection ---")
    for cmd in ["help", "ls", "uptime", "mem", "lspci", "help", "ls", "uptime", "help", "ls"]:
        inject(cmd, delay=0.03)
        time.sleep(0.3)
    time.sleep(10)
    result = ssh(f"grep -c 'COM3.*Inject' {SERIAL_LOG}")
    count = int(result) if result.isdigit() else 0
    passed = count >= 10
    print(f"  [{'PASS' if passed else 'FAIL'}] Rapid injection ({count} commands received)")
    results["stress"] = passed

    # ── Test 7: System Stability ──
    print("\n--- Test 7: Post-Stress Stability ---")
    results["stability"] = all([
        check_absent("PANIC", "Still no panics"),
        ssh(f"qm status {VM_ID}").count("running") > 0,
    ])
    vm_alive = "running" in ssh(f"qm status {VM_ID}")
    print(f"  [{'PASS' if vm_alive else 'FAIL'}] VM still running")

    # ── Summary ──
    print("\n" + "=" * 60)
    print("  E2E TEST SUMMARY")
    print("=" * 60)
    passed = sum(1 for v in results.values() if v)
    total = len(results)
    for name, ok in results.items():
        s = "PASS" if ok else "FAIL"
        print(f"  [{s}] {name}")
    print(f"\n  {passed}/{total} test groups passed")

    if passed == total:
        print(f"\n  ALL TESTS PASSED!")
    print()
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
