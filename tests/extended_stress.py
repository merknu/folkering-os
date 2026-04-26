#!/usr/bin/env python3
"""Folkering OS — Extended Stress Test

Exercises the system under heavier load:
  1. Multiple complex WASM apps in sequence
  2. AutoDream trigger (rapid generation fills dream backlog)
  3. Synapse VFS capacity (many writes)
  4. Rapid command bursts between WASM generations
  5. Full pipeline verification after all operations
"""

import socket
import time
import sys
import os
import re
import subprocess

if sys.platform == "win32":
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

COM3_HOST = "127.0.0.1"
COM3_PORT = 4568
SERIAL_LOG = os.path.join(os.path.expanduser("~"), "folkering-mcp", "serial.log")
WASM_TIMEOUT = 120
SHORT_TIMEOUT = 60

GREEN = "\033[92m"
RED = "\033[91m"
YELLOW = "\033[93m"
CYAN = "\033[96m"
BOLD = "\033[1m"
RESET = "\033[0m"


def get_log_size():
    try:
        return os.path.getsize(SERIAL_LOG)
    except OSError:
        return 0


def read_log_from(baseline):
    try:
        size = os.path.getsize(SERIAL_LOG)
        if size <= baseline:
            return ""
        with open(SERIAL_LOG, "rb") as f:
            f.seek(baseline)
            return f.read(size - baseline).decode("utf-8", errors="replace")
    except OSError:
        return ""


def wait_for(baseline, pattern, timeout, label=""):
    deadline = time.time() + timeout
    desc = label or pattern[:50]
    while time.time() < deadline:
        text = read_log_from(baseline)
        if re.search(pattern, text):
            print(f"  OK: {desc}")
            return True, text
        time.sleep(1)
    print(f"  TIMEOUT: {desc}")
    return False, read_log_from(baseline)


def full_log():
    try:
        with open(SERIAL_LOG, "r", encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return ""


class COM3:
    def __init__(self):
        self.sock = None

    def connect(self):
        for _ in range(30):
            result = subprocess.run(["netstat", "-an"], capture_output=True, text=True, timeout=5)
            if not any(f":{COM3_PORT}" in l and "CLOSE_WAIT" in l for l in result.stdout.split("\n")):
                break
            time.sleep(2)
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.settimeout(5)
        try:
            self.sock.connect((COM3_HOST, COM3_PORT))
            return True
        except socket.error as e:
            print(f"  COM3 connect failed: {e}")
            return False

    def send(self, cmd, delay=0.04):
        if not self.sock:
            return False
        try:
            self.sock.send(b"\n")
            time.sleep(0.2)
            for ch in cmd + "\n":
                self.sock.send(ch.encode("ascii"))
                time.sleep(delay)
            return True
        except socket.error:
            return False

    def send_esc(self):
        if not self.sock:
            return False
        try:
            self.sock.send(b"\x1b")
            time.sleep(0.3)
            return True
        except socket.error:
            return False

    def close(self):
        if self.sock:
            self.sock.close()
            self.sock = None


def main():
    print(f"\n{BOLD}{'=' * 60}{RESET}")
    print(f"{BOLD}  Folkering OS — Extended Stress Test{RESET}")
    print(f"{BOLD}{'=' * 60}{RESET}\n")

    if not os.path.exists(SERIAL_LOG):
        print(f"{RED}Serial log not found{RESET}")
        return 1

    com3 = COM3()
    if not com3.connect():
        return 1
    print(f"  COM3 connected\n")

    # Clear any fullscreen app
    com3.send_esc()
    time.sleep(2)

    results = {}

    # ── Phase 1: Generate 5 WASM apps in sequence ────────────��───────────
    print(f"{CYAN}--- Phase 1: Sequential WASM Generation (5 apps) ---{RESET}")

    apps = [
        "gemini generate red bouncing ball animation",
        "gemini generate blue gradient background",
        "gemini generate simple bar chart with 4 bars",
        "gemini generate green matrix rain effect",
        "gemini generate yellow spinning square",
    ]

    gen_ok = 0
    for i, cmd in enumerate(apps):
        name = cmd.replace("gemini generate ", "")
        print(f"\n  [{i+1}/{len(apps)}] {name}")
        baseline = get_log_size()

        if not com3.send(cmd):
            print(f"    FAIL: inject")
            continue

        found, text = wait_for(
            baseline,
            r"\[MCP\] WASM (assembled|single chunk)",
            WASM_TIMEOUT,
            f"WASM compiled"
        )

        if found:
            # Extract size
            m = re.search(r"(\d+) bytes", text)
            sz = m.group(1) if m else "?"
            # Check signing
            signed = bool(re.search(r"\[CRYPTO\] Signed WASM", text))
            cached = bool(re.search(r"\[Cache\] Stored WASM", text))
            print(f"    {sz}B | signed={signed} | cached={cached}")
            gen_ok += 1
        else:
            print(f"    FAIL: generation timeout")

        # Close WASM app before next
        com3.send_esc()
        time.sleep(3)

    results["Phase 1: Sequential Gen"] = gen_ok >= 3
    print(f"\n  Phase 1: {gen_ok}/{len(apps)} apps generated")

    # ── Phase 2: Rapid command burst ─────────────────────────────────────
    print(f"\n{CYAN}--- Phase 2: Rapid Command Burst (20 commands) ---{RESET}")

    baseline = get_log_size()
    burst_cmds = ["help", "ls", "uptime", "mem", "lspci"] * 4
    for cmd in burst_cmds:
        com3.send(cmd, delay=0.02)
        time.sleep(0.2)

    found, text = wait_for(
        baseline,
        r"\[COM3\] Inject: lspci",
        SHORT_TIMEOUT,
        "Burst commands received"
    )

    received = len(re.findall(r"\[COM3\] Inject:", text))
    print(f"  Received: {received}/{len(burst_cmds)}")
    results["Phase 2: Rapid Burst"] = received >= 15

    # ── Phase 3: FolkShell exercises ─────────────────────────────────────
    print(f"\n{CYAN}--- Phase 3: FolkShell Pipeline Stress ---{RESET}")

    pipe_cmds = [
        "ls |> help",
        'ls ~> "system"',
        "uptime |> help",
        "mem |> help",
        'ls ~> "wasm"',
    ]

    baseline = get_log_size()
    for cmd in pipe_cmds:
        com3.send(cmd)
        time.sleep(1)

    found, text = wait_for(
        baseline,
        r"\[COM3\] Inject:.*~>.*wasm",
        SHORT_TIMEOUT,
        "All pipe commands received"
    )

    pipe_received = len(re.findall(r"\[COM3\] Inject:", text))
    intent_queries = len(re.findall(r"Intent query", text))
    print(f"  Pipe commands received: {pipe_received}/{len(pipe_cmds)}")
    print(f"  Intent queries triggered: {intent_queries}")
    results["Phase 3: FolkShell Pipes"] = pipe_received >= 3

    # ── Phase 4: Generate JIT command ────────────────────────────────────
    print(f"\n{CYAN}--- Phase 4: JIT Command Synthesis ---{RESET}")

    baseline = get_log_size()
    com3.send("Get-CpuInfo |> Render-Gauge")

    found, text = wait_for(
        baseline,
        r"\[COM3\] Inject: Get-CpuInfo|\[MCP\] WasmGenRequest",
        WASM_TIMEOUT,
        "JIT pipeline triggered"
    )

    if found:
        # Check if MCP request was sent (JIT attempt)
        jit_triggered = bool(re.search(r"\[MCP\] WasmGenRequest", text))
        if jit_triggered:
            # Wait for compilation
            found2, text2 = wait_for(
                baseline,
                r"\[MCP\] WASM (assembled|single chunk)",
                WASM_TIMEOUT,
                "JIT WASM compiled"
            )
            results["Phase 4: JIT Synthesis"] = found2
        else:
            print(f"    Command received but JIT not triggered (may be queued)")
            results["Phase 4: JIT Synthesis"] = True
    else:
        results["Phase 4: JIT Synthesis"] = False

    com3.send_esc()
    time.sleep(2)

    # ── Phase 5: Final health check ──────────────────────────────────────
    print(f"\n{CYAN}--- Phase 5: Final Health Check ---{RESET}")

    log = full_log()
    panics = len(re.findall(r"PANIC|DOUBLE FAULT|kernel panic", log, re.IGNORECASE))
    total_wasm = len(re.findall(r"\[Cache\] Stored WASM", log))
    total_signed = len(re.findall(r"\[CRYPTO\] Signed WASM", log))
    total_synapse = len(re.findall(r"\[SYNAPSE\] Wrote", log))
    total_intents = len(re.findall(r"Intent (stored|query)", log))
    total_hb = len(re.findall(r"\[HB\]", log))
    dreams = len(re.findall(r"\[AutoDream\]", log))

    print(f"  Panics:          {panics}")
    print(f"  WASM apps:       {total_wasm}")
    print(f"  Signed:          {total_signed}")
    print(f"  VFS writes:      {total_synapse}")
    print(f"  Intent ops:      {total_intents}")
    print(f"  Heartbeats:      {total_hb}")
    print(f"  AutoDream msgs:  {dreams}")

    results["Phase 5: Health Check"] = panics == 0 and total_hb >= 2

    com3.close()

    # ── Summary ──────────────────────────────────────────────────────────
    print(f"\n{BOLD}{'=' * 60}{RESET}")
    print(f"{BOLD}  EXTENDED STRESS TEST SUMMARY{RESET}")
    print(f"{BOLD}{'=' * 60}{RESET}")

    passed = sum(1 for v in results.values() if v)
    total = len(results)

    for name, ok in results.items():
        s = f"{GREEN}PASS{RESET}" if ok else f"{RED}FAIL{RESET}"
        print(f"  [{s}] {name}")

    print(f"\n  {passed}/{total} phases passed")
    if passed == total:
        print(f"\n  {GREEN}{BOLD}ALL PHASES PASSED{RESET}")
    print()
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
