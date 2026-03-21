#!/usr/bin/env python3
"""
Folkering OS - MCP Debug Server
Tools: kernel_symbol_lookup, serial_throttle_analyzer, qemu_inspect_registers
"""

import sys
import json
import subprocess
import os
import re
import socket
import time
import collections
from pathlib import Path

# ── MCP stdio transport ────────────────────────────────────────────────────────

def send(msg: dict):
    line = json.dumps(msg)
    sys.stdout.write(line + "\n")
    sys.stdout.flush()

def recv() -> dict | None:
    line = sys.stdin.readline()
    if not line:
        return None
    return json.loads(line.strip())

# ── Paths ──────────────────────────────────────────────────────────────────────

PROJECT_ROOT = Path(__file__).parent.parent
QMP_SOCKET   = Path("/tmp/folkering-qmp.sock")

# Known ELF targets — addr2line uses native WSL /tmp copies for speed
_ELF_REGISTRY = {
    "kernel":     PROJECT_ROOT / "kernel"    / "target" / "x86_64-folkering"           / "release" / "kernel",
    "compositor": PROJECT_ROOT / "userspace" / "target" / "x86_64-folkering-userspace" / "release" / "compositor",
    "shell":      PROJECT_ROOT / "userspace" / "target" / "x86_64-folkering-userspace" / "release" / "shell",
    "synapse":    PROJECT_ROOT / "userspace" / "target" / "x86_64-folkering-userspace" / "release" / "synapse",
    "inference":  PROJECT_ROOT / "userspace" / "target" / "x86_64-folkering-userspace" / "release" / "inference",
}
# Where symbols live in memory (kernel is high-half, userspace is low)
_ELF_RANGES = {
    "kernel":     (0xffffffff80000000, 0xffffffffffffffff),
    "compositor": (0x200000,           0x4fffff),
    "shell":      (0x200000,           0x4fffff),
    "synapse":    (0x200000,           0x4fffff),
    "inference":  (0x200000,           0x4fffff),
}

def _wsl_path(win_path: Path) -> str:
    s = str(win_path).replace("\\", "/")
    if len(s) >= 2 and s[1] == ":":
        drive = s[0].lower()
        s = f"/mnt/{drive}{s[2:]}"
    return s

def _wsl_run(args: list, timeout: int = 15) -> subprocess.CompletedProcess:
    return subprocess.run(["wsl", "-e", "bash", "-c", " ".join(f'"{a}"' if " " in a else a for a in args)],
                          capture_output=True, text=True, timeout=timeout)

def _ensure_wsl_copy(name: str, win_path: Path) -> str | None:
    """Copy ELF to WSL /tmp if needed. Returns WSL path or None."""
    wsl_dest = f"/tmp/folkering-{name}"
    src = _wsl_path(win_path)
    r = subprocess.run(
        ["wsl", "-e", "bash", "-c",
         f'[ -f "{wsl_dest}" ] && [ "{src}" -ot "{wsl_dest}" ] && echo CACHED || cp "{src}" "{wsl_dest}" && echo COPIED'],
        capture_output=True, text=True, timeout=15
    )
    if r.returncode != 0:
        return None
    return wsl_dest

def _guess_elf_for_address(addr_int: int) -> str:
    """Return which ELF likely contains this address based on memory range."""
    for name, (lo, hi) in _ELF_RANGES.items():
        if lo <= addr_int <= hi:
            return name
    return "kernel"  # fallback

# ── Tool definitions ───────────────────────────────────────────────────────────

TOOLS = [
    {
        "name": "kernel_symbol_lookup",
        "description": (
            "Resolve a hex address (or list of addresses) to function name, "
            "source file and line number using the kernel ELF debug symbols. "
            "Stops blind address-guessing instantly."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "addresses": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Hex addresses to resolve, e.g. ['0x20636B', '0x205AD5']"
                },
                "elf_path": {
                    "type": "string",
                    "description": "Override ELF path (optional, defaults to release kernel)"
                }
            },
            "required": ["addresses"]
        }
    },
    {
        "name": "serial_throttle_analyzer",
        "description": (
            "Read a serial/QEMU log file and collapse repeated loop patterns. "
            "Turns 10 million lines of 'LA LB LC' noise into '[LA-LC Loop] x10000' "
            "so anomalies like #GP faults are immediately visible."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "log_path": {
                    "type": "string",
                    "description": "Path to the serial/QEMU log file"
                },
                "window": {
                    "type": "integer",
                    "description": "Lines to consider as one 'pattern unit' (default: 5)",
                    "default": 5
                },
                "threshold": {
                    "type": "integer",
                    "description": "How many repeats before collapsing (default: 10)",
                    "default": 10
                },
                "max_output_lines": {
                    "type": "integer",
                    "description": "Maximum lines to return after throttling (default: 200)",
                    "default": 200
                }
            },
            "required": ["log_path"]
        }
    },
    {
        "name": "qemu_inspect_registers",
        "description": (
            "Query the live QEMU CPU state via QMP. Returns RAX-R15, RIP, RSP, RBP, "
            "RFLAGS, CS/SS/DS/ES, and optionally XMM0-XMM15. "
            "QEMU must be running with: -qmp unix:/tmp/folkering-qmp.sock,server,nowait"
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "include_xmm": {
                    "type": "boolean",
                    "description": "Include XMM0-XMM15 SSE registers (default: false)",
                    "default": False
                },
                "qmp_socket": {
                    "type": "string",
                    "description": "Override QMP socket path (default: /tmp/folkering-qmp.sock)"
                }
            }
        }
    }
]

# ── Tool implementations ───────────────────────────────────────────────────────

def kernel_symbol_lookup(addresses: list[str], elf_path: str | None = None) -> str:
    results = []

    # Pre-load nm tables for each ELF we'll need (avoid re-running nm per address)
    nm_cache: dict[str, list[tuple[int,str]]] = {}

    def _get_nm(wsl_elf: str, name: str) -> list[tuple[int,str]]:
        if wsl_elf in nm_cache:
            return nm_cache[wsl_elf]
        try:
            r = subprocess.run(
                ["wsl", "-e", "bash", "-c", f"nm -n --demangle {wsl_elf}"],
                capture_output=True, text=True, timeout=20
            )
            table = []
            for line in r.stdout.splitlines():
                parts = line.split(None, 2)
                if len(parts) >= 3:
                    try:
                        table.append((int(parts[0], 16), parts[2]))
                    except ValueError:
                        pass
            nm_cache[wsl_elf] = table
        except Exception:
            nm_cache[wsl_elf] = []
        return nm_cache[wsl_elf]

    for addr in addresses:
        addr = addr.strip()
        if not addr.startswith(("0x", "0X")):
            addr = "0x" + addr
        try:
            int_addr = int(addr, 16)
        except ValueError:
            results.append(f"ERROR: invalid address '{addr}'")
            continue

        # Auto-detect which binary this address belongs to
        if elf_path:
            win_elf = Path(elf_path)
            elf_name = win_elf.stem
        else:
            elf_name = _guess_elf_for_address(int_addr)
            win_elf  = _ELF_REGISTRY.get(elf_name, _ELF_REGISTRY["kernel"])

        if not win_elf.exists():
            results.append(f"─── {addr} ───")
            results.append(f"  ERROR: {elf_name} ELF not found at {win_elf}")
            continue

        # Ensure WSL copy is fresh
        wsl_elf = _ensure_wsl_copy(elf_name, win_elf)
        if not wsl_elf:
            results.append(f"  ERROR: could not copy {elf_name} to WSL /tmp")
            continue

        # addr2line (DWARF — may return ?? for release builds)
        try:
            r = subprocess.run(
                ["wsl", "-e", "bash", "-c", f"addr2line -e {wsl_elf} -f -C -i {addr}"],
                capture_output=True, text=True, timeout=10
            )
            a2l = r.stdout.strip()
        except Exception as e:
            a2l = f"addr2line error: {e}"

        # nm nearest symbol (always works even without DWARF)
        nm_sym = "?"
        table = _get_nm(wsl_elf, elf_name)
        best_addr, best_name = 0, None
        for sym_addr, sym_name in table:
            if sym_addr <= int_addr and sym_addr > best_addr:
                best_addr, best_name = sym_addr, sym_name
        if best_name:
            offset = int_addr - best_addr
            nm_sym = f"{best_name}  +0x{offset:x}"

        results.append(f"{'─'*60}")
        results.append(f"Address  : {addr}  [{elf_name}]")
        results.append(f"Symbol   : {nm_sym}")
        if a2l and "??" not in a2l:
            results.append(f"Source   :")
            for line in a2l.splitlines():
                results.append(f"  {line}")
        else:
            results.append(f"Source   : (no DWARF — build with debug_assertions or use debug profile)")

    return "\n".join(results) if results else "No addresses provided"


def serial_throttle_analyzer(
    log_path: str,
    window: int = 5,
    threshold: int = 10,
    max_output_lines: int = 200
) -> str:
    p = Path(log_path)
    if not p.exists():
        # Try relative to project root
        p = PROJECT_ROOT / log_path
    if not p.exists():
        return f"ERROR: Log file not found: {log_path}"

    try:
        with open(p, "r", errors="replace") as f:
            lines = f.readlines()
    except Exception as e:
        return f"ERROR reading log: {e}"

    total_lines = len(lines)
    lines = [l.rstrip("\n") for l in lines]

    output = []
    i = 0
    while i < len(lines):
        # Take a window of lines as candidate pattern
        pattern = tuple(lines[i:i+window])
        if len(pattern) < window:
            output.extend(lines[i:])
            break

        # Count how many times this pattern repeats
        count = 0
        j = i
        while j + window <= len(lines) and tuple(lines[j:j+window]) == pattern:
            count += 1
            j += window

        if count >= threshold:
            # Collapse
            short = " | ".join(l.strip() for l in pattern[:3] if l.strip())
            if len(pattern) > 3:
                short += " | ..."
            output.append(f"[LOOP x{count}] {short}")
            i = j
        else:
            output.append(lines[i])
            i += 1

    # Find anomalies (non-loop content)
    anomalies = [l for l in output if not l.startswith("[LOOP")]
    loop_count = len([l for l in output if l.startswith("[LOOP")])

    summary = [
        f"{'═'*60}",
        f"Serial Throttle Analysis: {p.name}",
        f"Original: {total_lines} lines  →  After throttle: {len(output)} lines",
        f"Collapsed loops: {loop_count}  |  Unique lines: {len(anomalies)}",
        f"{'═'*60}",
        ""
    ]

    # Trim output if still too long
    if len(output) > max_output_lines:
        trimmed = output[:max_output_lines]
        trimmed.append(f"... [{len(output) - max_output_lines} more lines truncated]")
        output = trimmed

    return "\n".join(summary + output)


def qemu_inspect_registers(include_xmm: bool = False, qmp_socket: str | None = None) -> str:
    sock_path = qmp_socket or "/tmp/folkering-qmp.sock"

    # QMP runs in WSL — use a Python one-liner inside WSL for the socket connection
    py_script = f"""
import socket, json, time, sys

sock_path = "{sock_path}"
try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5)
    s.connect(sock_path)
except FileNotFoundError:
    print("QMP_ERROR: socket not found at " + sock_path)
    sys.exit(1)
except Exception as e:
    print("QMP_ERROR: " + str(e))
    sys.exit(1)

def recv_msg(s):
    data = b""
    while True:
        try:
            chunk = s.recv(65536)
            if not chunk:
                break
            data += chunk
            try:
                json.loads(data.decode())
                break
            except:
                pass
        except socket.timeout:
            break
    return data.decode().strip()

def send_recv(s, cmd):
    s.sendall((json.dumps(cmd) + "\\n").encode())
    time.sleep(0.05)
    return recv_msg(s)

# Handshake
recv_msg(s)
send_recv(s, {{"execute": "qmp_capabilities"}})
r = send_recv(s, {{"execute": "human-monitor-command", "arguments": {{"command-line": "info registers"}}}})
s.close()
try:
    print(json.loads(r).get("return", r))
except:
    print(r)
""".strip()

    try:
        result = subprocess.run(
            ["wsl", "-d", "Ubuntu-22.04", "python3", "-c", py_script],
            capture_output=True, text=True, timeout=10
        )
        output = result.stdout.strip()
        if not output:
            output = result.stderr.strip()
    except subprocess.TimeoutExpired:
        return "ERROR: QMP query timed out (is QEMU running with -qmp flag?)"
    except Exception as e:
        return f"ERROR: {e}"

    if output.startswith("QMP_ERROR: socket not found"):
        return (
            f"ERROR: QMP socket not found at {sock_path}\n\n"
            "Start QEMU with QMP enabled:\n"
            "  wsl -e bash -c 'cd /mnt/c/Users/merkn/folkering/folkering-os && ./tools/qemu-debug-live.sh'\n\n"
            "Or add these flags to any QEMU launch:\n"
            "  -qmp unix:/tmp/folkering-qmp.sock,server,nowait\n"
            "  -gdb tcp::1234 -S"
        )

    if output.startswith("QMP_ERROR:"):
        return f"ERROR: {output}"

    if not include_xmm:
        lines = [l for l in output.splitlines() if not re.match(r'\s*XMM', l, re.IGNORECASE)]
        output = "\n".join(lines)

    return f"QEMU CPU Register State\n{'═'*60}\n{output}"


# ── MCP dispatch ───────────────────────────────────────────────────────────────

def handle(req: dict) -> dict:
    method = req.get("method", "")
    req_id = req.get("id")

    if method == "initialize":
        return {
            "jsonrpc": "2.0", "id": req_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "folkering-debug", "version": "1.0.0"}
            }
        }

    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": req_id, "result": {"tools": TOOLS}}

    if method == "tools/call":
        name   = req["params"]["name"]
        args   = req["params"].get("arguments", {})

        try:
            if name == "kernel_symbol_lookup":
                text = kernel_symbol_lookup(
                    addresses=args["addresses"],
                    elf_path=args.get("elf_path")
                )
            elif name == "serial_throttle_analyzer":
                text = serial_throttle_analyzer(
                    log_path=args["log_path"],
                    window=args.get("window", 5),
                    threshold=args.get("threshold", 10),
                    max_output_lines=args.get("max_output_lines", 200)
                )
            elif name == "qemu_inspect_registers":
                text = qemu_inspect_registers(
                    include_xmm=args.get("include_xmm", False),
                    qmp_socket=args.get("qmp_socket")
                )
            else:
                text = f"Unknown tool: {name}"
        except Exception as e:
            text = f"Tool error: {e}"

        return {
            "jsonrpc": "2.0", "id": req_id,
            "result": {"content": [{"type": "text", "text": text}]}
        }

    if method == "notifications/initialized":
        return None  # no response needed

    return {
        "jsonrpc": "2.0", "id": req_id,
        "error": {"code": -32601, "message": f"Method not found: {method}"}
    }


def main():
    while True:
        req = recv()
        if req is None:
            break
        resp = handle(req)
        if resp is not None:
            send(resp)


if __name__ == "__main__":
    main()
