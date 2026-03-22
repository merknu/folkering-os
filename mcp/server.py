#!/usr/bin/env python3
"""
Folkering OS - MCP Debug Server v3.0 (Bare-Metal ML Inspection Studio)

Tools:
  kernel_symbol_lookup    — resolve hex addresses to function names
  serial_throttle_analyzer — collapse loop patterns in serial logs
  qemu_inspect_registers  — read CPU state via QMP
  tensor_dump             — read inference tensor data from disk mailbox or serial log
  python_ref_runner       — PyTorch whitebox oracle with forward hooks (ULTRA 50)
  attention_heatmap       — visual attention heatmap with drift comparison (v3.0)
  topo_parity_map         — automated MSE/cosine drift analysis per layer+module (v3.0)
"""

import sys
import json
import struct
import subprocess
import os
import re
import socket
import time
import collections
from pathlib import Path

try:
    import numpy as np
    HAS_NUMPY = True
except ImportError:
    HAS_NUMPY = False

try:
    from llama_cpp import Llama
    HAS_LLAMA = True
except ImportError:
    HAS_LLAMA = False

import io
import base64

try:
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    HAS_MATPLOTLIB = True
except ImportError:
    HAS_MATPLOTLIB = False

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
    },
    {
        "name": "tensor_dump",
        "description": (
            "Read a tensor dumped by inference-server to the VirtIO debug mailbox "
            "(sectors 1-7 of virtio-data.img). Returns tensor stats (min/max/mean/argmax), "
            "shape, name, and optionally the raw float values. "
            "The inference-server writes here via debug_dump_tensor() after each forward pass. "
            "No QEMU interaction needed — reads the disk image file directly on the host."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "disk_image": {
                    "type": "string",
                    "description": "Path to VirtIO disk image (default: boot/virtio-data.img)"
                },
                "return_data": {
                    "type": "boolean",
                    "description": "Return raw float values (up to max_values). Default: false (stats only)",
                    "default": False
                },
                "max_values": {
                    "type": "integer",
                    "description": "Max float values to return when return_data=true (default: 64)",
                    "default": 64
                },
                "slice_start": {
                    "type": "integer",
                    "description": "Start index for data slice (default: 0)",
                    "default": 0
                },
                "slice_end": {
                    "type": "integer",
                    "description": "End index for data slice (default: max_values from start)",
                },
                "top_k": {
                    "type": "integer",
                    "description": "Return top-K values sorted by magnitude (useful for logits). Default: 0 (disabled)",
                    "default": 0
                },
                "serial_log": {
                    "type": "string",
                    "description": "Path to serial log file. If provided, parses [TDMP] lines instead of reading disk. Use for stats-only quick checks."
                },
                "name_filter": {
                    "type": "string",
                    "description": "Filter [TDMP] entries by tensor name (used with serial_log mode)"
                }
            }
        }
    },
    {
        "name": "python_ref_runner",
        "description": (
            "Run a prompt through the SmolLM-135M model loaded in llama-cpp-python "
            "and return reference logits/tokens for comparison with the Rust inference. "
            "Model is loaded ONCE and kept in memory for instant subsequent calls. "
            "Use this as a ground-truth oracle when debugging transformer divergence."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Input prompt to run through the model"
                },
                "mode": {
                    "type": "string",
                    "enum": ["logits", "generate", "tokens", "compare"],
                    "description": (
                        "logits: Return raw logits for the last token position (top-K). "
                        "generate: Generate N tokens and return them. "
                        "tokens: Return the token IDs the model produces for the prompt. "
                        "compare: Compare with a Rust tensor_dump (reads disk mailbox automatically). "
                        "Default: logits"
                    ),
                    "default": "logits"
                },
                "top_k": {
                    "type": "integer",
                    "description": "Number of top logits to return (default: 20)",
                    "default": 20
                },
                "max_tokens": {
                    "type": "integer",
                    "description": "Max tokens to generate in 'generate' mode (default: 32)",
                    "default": 32
                },
                "model_path": {
                    "type": "string",
                    "description": "Path to GGUF model file (default: boot/model.gguf)"
                },
                "temperature": {
                    "type": "number",
                    "description": "Sampling temperature (default: 0.0 = greedy for deterministic comparison)",
                    "default": 0.0
                },
                "capture_layers": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": (
                        "List of module names to capture via forward hooks. "
                        "E.g. ['model.layers.0.self_attn.q_proj', 'model.layers.0.mlp.gate_proj']. "
                        "Returns shape, stats, and first 16 values of each captured activation."
                    )
                },
                "layer": {
                    "type": "integer",
                    "description": "Convenience: capture a specific layer number (used with module_name)"
                },
                "module_name": {
                    "type": "string",
                    "description": (
                        "Convenience: module within a layer to capture. "
                        "E.g. 'self_attn.q_proj', 'self_attn.k_proj', 'self_attn.v_proj', "
                        "'self_attn.o_proj', 'mlp.gate_proj', 'mlp.up_proj', 'mlp.down_proj', "
                        "'input_layernorm', 'post_attention_layernorm'"
                    )
                }
            },
            "required": ["prompt"]
        }
    },
    {
        "name": "attention_heatmap",
        "description": (
            "Generate a visual heatmap of attention patterns from the PyTorch oracle. "
            "If Rust data is available in the VirtIO disk mailbox, shows the DRIFT "
            "(absolute difference) between Python and Rust attention. "
            "If no Rust data, shows the PyTorch baseline (titled 'BASELINE ONLY'). "
            "Applies causal mask (lower-triangular) so masked values don't distort colors. "
            "Returns a PNG heatmap image alongside diagnostic statistics."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Input prompt to visualize attention for"
                },
                "layer": {
                    "type": "integer",
                    "description": "Transformer layer to visualize (0-29, default: 0)",
                    "default": 0
                },
                "head": {
                    "description": "Attention head index (0-8), or 'all' for all 9 heads + average (default: 'all')",
                    "default": "all"
                },
                "model_path": {
                    "type": "string",
                    "description": "Path to GGUF model file (default: boot/model.gguf)"
                },
                "chatml": {
                    "type": "boolean",
                    "description": "Wrap prompt in ChatML template matching Folkering inference server (default: true)",
                    "default": True
                }
            },
            "required": ["prompt"]
        }
    },
    {
        "name": "topo_parity_map",
        "description": (
            "Automated drift analysis for ONE specific layer+module. "
            "Runs prompt through PyTorch oracle, extracts the raw float activation array, "
            "then reads the matching raw float array from the VirtIO disk mailbox. "
            "Computes MSE, cosine similarity, max absolute error, and per-element divergence counts. "
            "WARNING: If the Rust tensor was truncated (mailbox limit), metrics are flagged as partial."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Input prompt to run through both Python and compare with Rust"
                },
                "layer": {
                    "type": "integer",
                    "description": "Layer index (0-29, default: 0)",
                    "default": 0
                },
                "module": {
                    "type": "string",
                    "description": (
                        "Module within the layer. Default: 'self_attn.q_proj'. "
                        "Options: self_attn.q_proj, self_attn.k_proj, self_attn.v_proj, "
                        "self_attn.o_proj, self_attn, mlp.gate_proj, mlp.up_proj, "
                        "mlp.down_proj, mlp, input_layernorm, post_attention_layernorm"
                    ),
                    "default": "self_attn.q_proj"
                },
                "model_path": {
                    "type": "string",
                    "description": "Path to GGUF model file (default: boot/model.gguf)"
                },
                "chatml": {
                    "type": "boolean",
                    "description": "Wrap prompt in ChatML template matching Folkering inference server (default: true)",
                    "default": True
                }
            },
            "required": ["prompt"]
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


def _parse_serial_tdmp(log_path: str, name_filter: str | None = None) -> str:
    """Parse [TDMP] lines from a serial log file."""
    p = Path(log_path)
    if not p.exists():
        p = PROJECT_ROOT / log_path
    if not p.exists():
        # Try common locations
        for candidate in [
            Path("/tmp/folkering-serial.log"),
            PROJECT_ROOT / "boot" / "qemu-output.log",
        ]:
            if candidate.exists():
                p = candidate
                break
        else:
            return f"ERROR: Serial log not found: {log_path}"

    try:
        with open(p, "r", errors="replace") as f:
            lines = f.readlines()
    except Exception as e:
        return f"ERROR reading serial log: {e}"

    tdmp_lines = [l.strip() for l in lines if "[TDMP]" in l]
    if name_filter:
        tdmp_lines = [l for l in tdmp_lines if f"name={name_filter}" in l]

    if not tdmp_lines:
        return f"No [TDMP] entries found in {p.name}" + (f" (filter: {name_filter})" if name_filter else "")

    result = [
        f"{'═' * 60}",
        f"Tensor Dumps from serial log: {p.name}",
        f"Found {len(tdmp_lines)} entries" + (f" (filter: {name_filter})" if name_filter else ""),
        f"{'═' * 60}",
    ]
    for line in tdmp_lines[-20:]:  # last 20 entries
        result.append(f"  {line}")

    if len(tdmp_lines) > 20:
        result.append(f"  ... ({len(tdmp_lines) - 20} earlier entries omitted)")

    return "\n".join(result)


def tensor_dump(
    disk_image: str | None = None,
    return_data: bool = False,
    max_values: int = 64,
    slice_start: int = 0,
    slice_end: int | None = None,
    top_k: int = 0,
    serial_log: str | None = None,
    name_filter: str | None = None,
) -> str:
    """Read tensor data from VirtIO disk mailbox (sectors 1-7) or serial log.

    Two extraction paths:
    1. Disk mailbox (default): reads raw f32 data from virtio-data.img sectors 1-7
    2. Serial log: parses [TDMP] lines for stats (no raw data, but always available)

    If serial_log is provided, returns parsed [TDMP] entries (optionally filtered by name).
    """
    # Path 1: Serial log parsing (stats only, no raw data)
    if serial_log:
        return _parse_serial_tdmp(serial_log, name_filter)

    if not HAS_NUMPY:
        return "ERROR: numpy not installed. Run: py -3.12 -m pip install numpy"

    # Find disk image
    img_path = Path(disk_image) if disk_image else PROJECT_ROOT / "boot" / "virtio-data.img"
    if not img_path.exists():
        return f"ERROR: Disk image not found: {img_path}"

    SECTOR = 512
    HDR_SECTOR = 1
    DATA_SECTOR = 2
    MAX_DATA_SECTORS = 256  # sectors 2-257 (128KB, 32768 f32)

    try:
        with open(img_path, "rb") as f:
            # Read header (sector 1)
            f.seek(HDR_SECTOR * SECTOR)
            hdr = f.read(SECTOR)

            # Check magic
            magic = hdr[0:4]
            if magic != b"TDMP":
                return (
                    f"No tensor dump found (magic={magic!r}, expected b'TDMP').\n"
                    "The inference-server hasn't written a dump yet.\n"
                    "Run an inference request first, then call this tool."
                )

            # Parse header
            seq        = struct.unpack_from("<I", hdr, 4)[0]
            n_elements = struct.unpack_from("<I", hdr, 8)[0]
            n_dumped   = struct.unpack_from("<I", hdr, 12)[0]
            shape0     = struct.unpack_from("<I", hdr, 16)[0]
            shape1     = struct.unpack_from("<I", hdr, 20)[0]
            argmax_idx = struct.unpack_from("<I", hdr, 24)[0]
            min_val    = struct.unpack_from("<f", hdr, 32)[0]
            max_val    = struct.unpack_from("<f", hdr, 36)[0]
            mean_val   = struct.unpack_from("<f", hdr, 40)[0]
            argmax_val = struct.unpack_from("<f", hdr, 44)[0]
            name_raw   = hdr[48:112]
            name       = name_raw.split(b"\x00")[0].decode("utf-8", errors="replace")

            # Parse summary values from header (offset 112, up to 100 f32s)
            summary_count = min(n_dumped, 100, (SECTOR - 112) // 4)
            summary = np.frombuffer(hdr[112:112 + summary_count * 4], dtype=np.float32).copy()

            # Read data sectors if needed
            data = None
            if return_data or top_k > 0:
                data_bytes = n_dumped * 4
                data_sectors_needed = min((data_bytes + SECTOR - 1) // SECTOR, MAX_DATA_SECTORS)
                f.seek(DATA_SECTOR * SECTOR)
                raw = f.read(data_sectors_needed * SECTOR)
                data = np.frombuffer(raw[:n_dumped * 4], dtype=np.float32).copy()

    except Exception as e:
        return f"ERROR reading disk image: {e}"

    # Build result
    shape_str = f"[{shape0}]" if shape1 == 0 else f"[{shape0}, {shape1}]"
    lines = [
        f"{'═' * 60}",
        f"Tensor Dump: {name}",
        f"{'═' * 60}",
        f"  seq:       {seq}",
        f"  shape:     {shape_str}",
        f"  elements:  {n_elements} (dumped: {n_dumped})",
        f"  min:       {min_val:.6f}",
        f"  max:       {max_val:.6f}",
        f"  mean:      {mean_val:.6f}",
        f"  argmax:    [{argmax_idx}] = {argmax_val:.6f}",
    ]

    # Compute stddev from available data
    if data is not None and len(data) > 1:
        lines.append(f"  std:       {float(np.std(data)):.6f}")

    # Top-K mode (most useful for logits)
    if top_k > 0 and data is not None:
        k = min(top_k, len(data))
        top_indices = np.argpartition(data, -k)[-k:]
        top_indices = top_indices[np.argsort(data[top_indices])[::-1]]
        lines.append(f"\n  Top-{k} values:")
        for i, idx in enumerate(top_indices):
            lines.append(f"    [{idx:6d}] = {data[idx]:12.6f}")

    # Raw data slice
    if return_data:
        src = data if data is not None else summary
        end = slice_end if slice_end is not None else slice_start + max_values
        end = min(end, len(src))
        start = min(slice_start, end)
        sliced = src[start:end]
        lines.append(f"\n  Data [{start}:{end}]:")
        # Format in rows of 8
        for row_start in range(0, len(sliced), 8):
            row = sliced[row_start:row_start + 8]
            vals = "  ".join(f"{v:10.5f}" for v in row)
            lines.append(f"    [{start + row_start:4d}] {vals}")

    return "\n".join(lines)


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


# ── Python Reference Runner (ULTRA 50: PyTorch Whitebox Oracle) ───────────

# Global model state — loaded once, kept in memory
_REF_MODEL = None       # transformers AutoModelForCausalLM
_REF_TOKENIZER = None   # transformers AutoTokenizer
_REF_MODEL_PATH = None  # path used to load (for cache invalidation)
_REF_HOOKS = {}         # {module_name: captured_output_tensor}


def _ensure_ref_model(model_path: str | None = None) -> str | None:
    """Load SmolLM model into global state. Returns error string or None on success."""
    global _REF_MODEL, _REF_TOKENIZER, _REF_MODEL_PATH

    try:
        import torch
        from transformers import AutoModelForCausalLM, AutoTokenizer
    except ImportError as e:
        return f"ERROR: Missing dependency: {e}\nInstall: py -3.12 -m pip install torch transformers"

    gguf_path = Path(model_path) if model_path else PROJECT_ROOT / "boot" / "model.gguf"
    if not gguf_path.exists():
        return f"ERROR: Model file not found: {gguf_path}"

    # Already loaded?
    if _REF_MODEL is not None and _REF_MODEL_PATH == str(gguf_path):
        return None

    try:
        # Load GGUF model via transformers (supports GGUF natively since v4.35)
        _REF_TOKENIZER = AutoTokenizer.from_pretrained(
            str(gguf_path),
            gguf_file=gguf_path.name,
            local_files_only=True,
        )
    except Exception:
        # GGUF tokenizer loading can be finicky — try HuggingFace Hub as fallback
        try:
            _REF_TOKENIZER = AutoTokenizer.from_pretrained("HuggingFaceTB/SmolLM2-135M")
        except Exception as e:
            return f"ERROR loading tokenizer: {e}"

    try:
        _REF_MODEL = AutoModelForCausalLM.from_pretrained(
            str(gguf_path),
            gguf_file=gguf_path.name,
            local_files_only=True,
            dtype=torch.float32,
            attn_implementation="eager",
        )
        _REF_MODEL.eval()
        _REF_MODEL_PATH = str(gguf_path)
    except Exception:
        # Fallback: load from HuggingFace Hub (will download ~270MB on first use)
        try:
            _REF_MODEL = AutoModelForCausalLM.from_pretrained(
                "HuggingFaceTB/SmolLM2-135M",
                dtype=torch.float32,
                attn_implementation="eager",
            )
            _REF_MODEL.eval()
            _REF_MODEL_PATH = "HuggingFaceTB/SmolLM2-135M"
        except Exception as e:
            return f"ERROR loading model: {e}"

    return None


def _install_hooks(module_names: list[str] | None = None):
    """Install forward hooks on specified modules to capture intermediate activations."""
    global _REF_HOOKS
    import torch

    _REF_HOOKS.clear()

    if not module_names or not _REF_MODEL:
        return

    for name, module in _REF_MODEL.named_modules():
        if name in module_names or any(name.endswith(m) for m in module_names):
            def make_hook(n):
                def hook_fn(mod, inp, out):
                    if isinstance(out, tuple):
                        _REF_HOOKS[n] = out[0].detach().cpu()
                    elif isinstance(out, torch.Tensor):
                        _REF_HOOKS[n] = out.detach().cpu()
                return hook_fn
            module.register_forward_hook(make_hook(name))


def python_ref_runner(
    prompt: str,
    mode: str = "logits",
    top_k: int = 20,
    max_tokens: int = 32,
    model_path: str | None = None,
    temperature: float = 0.0,
    capture_layers: list[str] | None = None,
    layer: int | None = None,
    module_name: str | None = None,
) -> str:
    """Run prompt through PyTorch SmolLM and return reference activations/logits."""
    import torch

    # Ensure model is loaded
    err = _ensure_ref_model(model_path)
    if err:
        return err

    # Build capture list from convenience params
    if capture_layers is None and layer is not None and module_name:
        capture_layers = [f"model.layers.{layer}.{module_name}"]

    # Install hooks if capturing intermediate activations
    if capture_layers:
        _install_hooks(capture_layers)

    # Tokenize
    inputs = _REF_TOKENIZER(prompt, return_tensors="pt")
    input_ids = inputs["input_ids"]
    token_list = input_ids[0].tolist()

    lines = [
        f"{'═' * 60}",
        f"Python Reference Runner (PyTorch)",
        f"{'═' * 60}",
        f"  model:     {_REF_MODEL_PATH}",
        f"  prompt:    {prompt!r}",
        f"  tokens:    {token_list} ({len(token_list)} tokens)",
    ]

    if mode == "tokens":
        lines.append(f"\n  Token IDs: {token_list}")
        token_strs = [_REF_TOKENIZER.decode([t]) for t in token_list]
        lines.append(f"  Token strs: {token_strs}")
        return "\n".join(lines)

    if mode in ("logits", "compare"):
        with torch.no_grad():
            outputs = _REF_MODEL(input_ids)
            logits = outputs.logits[0, -1, :]  # last position

        logits_np = logits.numpy()
        n_vocab = len(logits_np)
        argmax_idx = int(logits_np.argmax())
        argmax_val = float(logits_np[argmax_idx])
        argmax_token = _REF_TOKENIZER.decode([argmax_idx])

        lines.extend([
            f"\n  Logits (last position):",
            f"  vocab_size: {n_vocab}",
            f"  argmax:     [{argmax_idx}] = {argmax_val:.6f} ({argmax_token!r})",
            f"  min:        {float(logits_np.min()):.6f}",
            f"  max:        {float(logits_np.max()):.6f}",
            f"  mean:       {float(logits_np.mean()):.6f}",
            f"  std:        {float(logits_np.std()):.6f}",
        ])

        # Top-K
        k = min(top_k, n_vocab)
        top_indices = logits_np.argsort()[-k:][::-1]
        lines.append(f"\n  Top-{k} logits:")
        for idx in top_indices:
            tok_str = _REF_TOKENIZER.decode([idx])
            lines.append(f"    [{idx:6d}] = {logits_np[idx]:12.6f}  ({tok_str!r})")

        # Compare mode: read disk mailbox and compute diff
        if mode == "compare" and HAS_NUMPY:
            import numpy as np
            disk_result = tensor_dump(return_data=True, max_values=n_vocab, top_k=0)
            if "Tensor Dump:" in disk_result:
                lines.append(f"\n{'─' * 60}")
                lines.append("  Comparison with Rust tensor dump:")

                # Try to extract Rust data for comparison
                img_path = PROJECT_ROOT / "boot" / "virtio-data.img"
                if img_path.exists():
                    try:
                        with open(img_path, "rb") as f:
                            f.seek(512)  # sector 1
                            hdr = f.read(512)
                            if hdr[:4] == b"TDMP":
                                rust_argmax = struct.unpack_from("<I", hdr, 24)[0]
                                rust_argmax_val = struct.unpack_from("<f", hdr, 44)[0]
                                rust_min = struct.unpack_from("<f", hdr, 32)[0]
                                rust_max = struct.unpack_from("<f", hdr, 36)[0]
                                rust_mean = struct.unpack_from("<f", hdr, 40)[0]
                                n_dumped = struct.unpack_from("<I", hdr, 12)[0]

                                lines.extend([
                                    f"  Rust argmax:  [{rust_argmax}] = {rust_argmax_val:.6f}",
                                    f"  Python argmax:[{argmax_idx}] = {argmax_val:.6f}",
                                    f"  Match: {'YES' if rust_argmax == argmax_idx else 'NO — DIVERGENCE'}",
                                    f"  Rust  stats: min={rust_min:.6f} max={rust_max:.6f} mean={rust_mean:.6f}",
                                    f"  Python stats: min={float(logits_np.min()):.6f} max={float(logits_np.max()):.6f} mean={float(logits_np.mean()):.6f}",
                                ])

                                # Element-wise comparison if data available
                                if n_dumped > 0:
                                    f.seek(1024)  # sector 2
                                    raw = f.read(min(n_dumped * 4, 256 * 512))
                                    rust_data = np.frombuffer(raw[:n_dumped * 4], dtype=np.float32).copy()
                                    py_data = logits_np[:len(rust_data)]
                                    diff = np.abs(rust_data - py_data)
                                    lines.extend([
                                        f"  Element-wise diff (first {len(rust_data)} values):",
                                        f"    max_abs_diff: {float(diff.max()):.6f}",
                                        f"    mean_abs_diff: {float(diff.mean()):.6f}",
                                        f"    >0.01 count: {int((diff > 0.01).sum())} / {len(rust_data)}",
                                        f"    >0.1 count: {int((diff > 0.1).sum())} / {len(rust_data)}",
                                        f"    >1.0 count: {int((diff > 1.0).sum())} / {len(rust_data)}",
                                    ])
                                    # Show top divergent indices
                                    worst = diff.argsort()[-5:][::-1]
                                    lines.append(f"    Worst 5 divergences:")
                                    for wi in worst:
                                        lines.append(f"      [{wi}] rust={rust_data[wi]:.6f} python={py_data[wi]:.6f} diff={diff[wi]:.6f}")
                    except Exception as e:
                        lines.append(f"  Comparison failed: {e}")
            else:
                lines.append(f"\n  No Rust tensor dump available for comparison. Run inference first.")

    elif mode == "generate":
        with torch.no_grad():
            gen = _REF_MODEL.generate(
                input_ids,
                max_new_tokens=max_tokens,
                do_sample=(temperature > 0),
                temperature=temperature if temperature > 0 else 1.0,
            )
        gen_tokens = gen[0].tolist()
        new_tokens = gen_tokens[len(token_list):]
        gen_text = _REF_TOKENIZER.decode(new_tokens)
        lines.extend([
            f"\n  Generated tokens: {new_tokens}",
            f"  Generated text: {gen_text!r}",
        ])

    # Captured intermediate activations
    if _REF_HOOKS:
        lines.append(f"\n{'─' * 60}")
        lines.append(f"  Captured activations ({len(_REF_HOOKS)} modules):")
        for hook_name, tensor in _REF_HOOKS.items():
            shape = list(tensor.shape)
            t_np = tensor.numpy().flatten()
            lines.extend([
                f"\n  {hook_name}:",
                f"    shape:  {shape}",
                f"    min:    {float(t_np.min()):.6f}",
                f"    max:    {float(t_np.max()):.6f}",
                f"    mean:   {float(t_np.mean()):.6f}",
                f"    std:    {float(t_np.std()):.6f}",
            ])
            # Show first 16 values
            preview = t_np[:16]
            vals = "  ".join(f"{v:.5f}" for v in preview)
            lines.append(f"    first16: [{vals}]")
        _REF_HOOKS.clear()

    return "\n".join(lines)


# ── Attention Heatmap (v3.0) ───────────────────────────────────────────────────

# Max data sectors for expanded mailbox reader (256 sectors = 128KB = 32768 f32)
MAILBOX_MAX_SECTORS = 256
MAILBOX_MAX_FLOATS = MAILBOX_MAX_SECTORS * 512 // 4  # 32768


def _read_mailbox_raw(disk_image: str | None = None) -> tuple[str | None, np.ndarray | None, int]:
    """Read raw f32 array from VirtIO disk mailbox.

    Returns (tensor_name, data_array, total_elements).
    tensor_name is None if no dump found.
    total_elements is the FULL tensor size (may be > len(data_array) if truncated).
    """
    if not HAS_NUMPY:
        return None, None, 0

    img_path = Path(disk_image) if disk_image else PROJECT_ROOT / "boot" / "virtio-data.img"
    if not img_path.exists():
        return None, None, 0

    SECTOR = 512
    try:
        with open(img_path, "rb") as f:
            f.seek(SECTOR)  # sector 1 = header
            hdr = f.read(SECTOR)
            if hdr[:4] != b"TDMP":
                return None, None, 0

            n_elements = struct.unpack_from("<I", hdr, 8)[0]
            n_dumped = struct.unpack_from("<I", hdr, 12)[0]
            name = hdr[48:112].split(b"\x00")[0].decode("utf-8", errors="replace")

            if n_dumped == 0:
                return name, np.array([], dtype=np.float32), n_elements

            # Read data sectors (2+), up to MAILBOX_MAX_SECTORS
            data_bytes = n_dumped * 4
            sectors_needed = min((data_bytes + SECTOR - 1) // SECTOR, MAILBOX_MAX_SECTORS)
            f.seek(2 * SECTOR)  # sector 2 = first data sector
            raw = f.read(sectors_needed * SECTOR)
            data = np.frombuffer(raw[:n_dumped * 4], dtype=np.float32).copy()

            return name, data, n_elements
    except Exception:
        return None, None, 0


def _wrap_chatml(query: str) -> str:
    """Wrap a user query in the same ChatML template as the Folkering inference server."""
    return (
        "<|im_start|>system\n"
        "You are Folkering OS, a helpful AI assistant.\n"
        "<|im_end|>\n"
        "<|im_start|>user\n"
        f"{query}\n"
        "<|im_end|>\n"
        "<|im_start|>assistant\n"
    )


def attention_heatmap(
    prompt: str,
    layer: int = 0,
    head: str | int = "all",
    model_path: str | None = None,
    chatml: bool = True,
) -> list[dict]:
    """Generate attention heatmap. Returns MCP content items (text + image).

    If chatml=True (default), wraps prompt in ChatML template matching the
    Folkering inference server for accurate Rust/Python drift comparison.
    """
    if not HAS_MATPLOTLIB:
        return [{"type": "text", "text": "ERROR: matplotlib not installed. Run: py -3.12 -m pip install matplotlib"}]
    if not HAS_NUMPY:
        return [{"type": "text", "text": "ERROR: numpy not installed."}]

    import torch

    err = _ensure_ref_model(model_path)
    if err:
        return [{"type": "text", "text": err}]

    # Validate params
    if not 0 <= layer <= 29:
        return [{"type": "text", "text": f"ERROR: layer must be 0-29, got {layer}"}]

    # Wrap in ChatML template if requested (matches Rust inference server prompt)
    effective_prompt = _wrap_chatml(prompt) if chatml else prompt
    inputs = _REF_TOKENIZER(effective_prompt, return_tensors="pt")
    input_ids = inputs["input_ids"]
    token_list = input_ids[0].tolist()
    token_strs = [_REF_TOKENIZER.decode([t]).replace("\n", "\\n") for t in token_list]
    seq_len = len(token_list)

    if seq_len < 2:
        return [{"type": "text", "text": f"Prompt tokenizes to {seq_len} token(s). Need >= 2 for a meaningful heatmap."}]

    # Forward pass with attention output (requires eager attention)
    with torch.no_grad():
        outputs = _REF_MODEL(input_ids, output_attentions=True)

    # outputs.attentions[layer] shape: [1, n_heads, seq, seq]
    py_attn = outputs.attentions[layer][0].numpy()  # [9, seq, seq]
    n_heads = py_attn.shape[0]

    # Check for Rust data in disk mailbox
    rust_name, rust_data, rust_total = _read_mailbox_raw()
    has_rust = rust_data is not None and len(rust_data) > 0

    # If Rust data matches attention shape, compute diff
    # Attention is [n_heads, seq, seq] = n_heads * seq * seq floats
    expected_attn_floats = n_heads * seq_len * seq_len
    rust_attn = None
    is_drift_mode = False

    if has_rust and len(rust_data) >= expected_attn_floats:
        try:
            rust_attn = rust_data[:expected_attn_floats].reshape(n_heads, seq_len, seq_len)
            is_drift_mode = True
        except Exception:
            pass

    # Apply causal mask: upper triangle → NaN
    causal_mask = np.tri(seq_len, dtype=bool)  # lower triangle = True
    py_masked = py_attn.copy()
    py_masked[:, ~causal_mask] = np.nan

    if is_drift_mode:
        diff = np.abs(py_attn - rust_attn)
        diff[:, ~causal_mask] = np.nan
        plot_data = diff
        title_suffix = "DRIFT (|Python - Rust|)"
        cmap = "hot"
        vmin, vmax = 0, float(np.nanmax(diff))
    else:
        plot_data = py_masked
        title_suffix = "BASELINE ONLY - NO RUST DATA"
        cmap = "viridis"
        vmin, vmax = 0, float(np.nanmax(py_masked))

    # Determine which heads to plot
    if head == "all":
        head_indices = list(range(n_heads))
    else:
        h = int(head)
        if not 0 <= h < n_heads:
            return [{"type": "text", "text": f"ERROR: head must be 0-{n_heads-1}, got {h}"}]
        head_indices = [h]

    # Generate figure
    if head == "all":
        ncols = 5
        nrows = 2
        fig, axes = plt.subplots(nrows, ncols, figsize=(4 * ncols, 4 * nrows))
        axes_flat = axes.flatten()

        # Plot average in first cell (suppress nanmean warning for masked cells)
        import warnings
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", RuntimeWarning)
            avg = np.nanmean(plot_data, axis=0)
        im = axes_flat[0].imshow(avg, cmap=cmap, vmin=vmin, vmax=vmax, aspect="auto")
        axes_flat[0].set_title("Average", fontsize=10, fontweight="bold")
        _label_attn_axes(axes_flat[0], token_strs, seq_len)

        # Plot each head
        for i, h in enumerate(head_indices):
            ax = axes_flat[i + 1]
            ax.imshow(plot_data[h], cmap=cmap, vmin=vmin, vmax=vmax, aspect="auto")
            ax.set_title(f"Head {h}", fontsize=9)
            _label_attn_axes(ax, token_strs, seq_len)

        fig.suptitle(f"Layer {layer} — {title_suffix}", fontsize=13, fontweight="bold")
        fig.subplots_adjust(right=0.88)
        fig.colorbar(im, ax=axes_flat.tolist(), shrink=0.6, pad=0.02)
    else:
        fig, ax = plt.subplots(1, 1, figsize=(8, 7))
        h = head_indices[0]
        im = ax.imshow(plot_data[h], cmap=cmap, vmin=vmin, vmax=vmax, aspect="auto")
        ax.set_title(f"Layer {layer}, Head {h} — {title_suffix}", fontsize=12, fontweight="bold")
        ax.set_xlabel("Key position")
        ax.set_ylabel("Query position")
        _label_attn_axes(ax, token_strs, seq_len)
        fig.colorbar(im, ax=ax)

    if head != "all":
        plt.tight_layout()

    # Encode to base64 PNG
    buf = io.BytesIO()
    fig.savefig(buf, format="png", dpi=150, bbox_inches="tight")
    plt.close(fig)
    buf.seek(0)
    img_b64 = base64.b64encode(buf.read()).decode("ascii")

    # Diagnostic stats
    valid_attn = py_masked[~np.isnan(py_masked)]
    bos_frac = float(np.nanmean(py_masked[:, :, 0])) if seq_len > 0 else 0
    entropy_per_row = -np.nansum(py_masked * np.log(py_masked + 1e-10), axis=-1)
    mean_entropy = float(np.nanmean(entropy_per_row))
    max_entropy = float(np.log(seq_len))

    lines = [
        f"Attention Heatmap — Layer {layer} | {title_suffix}",
        f"Prompt: {prompt!r}",
        f"Tokens ({seq_len}): {token_strs}",
        f"BOS attention fraction: {bos_frac:.3f}",
        f"Mean entropy: {mean_entropy:.3f} / {max_entropy:.3f} (max)",
    ]
    if bos_frac > 0.8:
        lines.append("WARNING: Attention is collapsing to BOS token!")
    if mean_entropy < 0.3 * max_entropy and seq_len > 2:
        lines.append("WARNING: Very low entropy — attention may be collapsing")
    if is_drift_mode:
        lines.append(f"Drift max: {float(np.nanmax(diff)):.6f}, mean: {float(np.nanmean(diff)):.6f}")
    else:
        lines.append("(Run inference in QEMU and dump attention to disk mailbox for drift comparison)")

    content = [
        {"type": "text", "text": "\n".join(lines)},
        {"type": "image", "data": img_b64, "mimeType": "image/png"},
    ]
    return content


def _label_attn_axes(ax, token_strs: list[str], seq_len: int):
    """Label attention heatmap axes with token strings."""
    if seq_len <= 20:
        ax.set_xticks(range(seq_len))
        ax.set_xticklabels(token_strs, rotation=45, ha="right", fontsize=max(6, 10 - seq_len // 4))
        ax.set_yticks(range(seq_len))
        ax.set_yticklabels(token_strs, fontsize=max(6, 10 - seq_len // 4))
    else:
        # Too many tokens for labels — show every Nth
        step = max(1, seq_len // 10)
        ax.set_xticks(range(0, seq_len, step))
        ax.set_xticklabels([token_strs[i] for i in range(0, seq_len, step)], rotation=45, ha="right", fontsize=6)
        ax.set_yticks(range(0, seq_len, step))
        ax.set_yticklabels([token_strs[i] for i in range(0, seq_len, step)], fontsize=6)


# ── Topological Parity Map (v3.0) ────────────────────────────────────────────


def topo_parity_map(
    prompt: str,
    layer: int = 0,
    module: str = "self_attn.q_proj",
    model_path: str | None = None,
    chatml: bool = True,
) -> str:
    """Compare ONE Python activation with ONE Rust tensor from the disk mailbox.

    If chatml=True (default), wraps prompt in ChatML template matching the
    Folkering inference server for accurate comparison.
    """
    if not HAS_NUMPY:
        return "ERROR: numpy not installed."

    import torch

    err = _ensure_ref_model(model_path)
    if err:
        return err

    if not 0 <= layer <= 29:
        return f"ERROR: layer must be 0-29, got {layer}"

    # Step 1: Run PyTorch oracle with forward hook
    hook_target = f"model.layers.{layer}.{module}"
    _install_hooks([hook_target])

    effective_prompt = _wrap_chatml(prompt) if chatml else prompt
    inputs = _REF_TOKENIZER(effective_prompt, return_tensors="pt")
    token_list = inputs["input_ids"][0].tolist()

    with torch.no_grad():
        _REF_MODEL(inputs["input_ids"])

    if hook_target not in _REF_HOOKS:
        _REF_HOOKS.clear()
        return f"ERROR: Forward hook did not capture '{hook_target}'. Check module name."

    py_tensor = _REF_HOOKS[hook_target]
    _REF_HOOKS.clear()
    py_np = py_tensor.numpy().flatten().astype(np.float32)
    py_shape = list(py_tensor.shape)

    # Step 2: Read Rust tensor from disk mailbox
    rust_name, rust_data, rust_total = _read_mailbox_raw()

    lines = [
        f"{'=' * 70}",
        f"Topological Parity Map",
        f"{'=' * 70}",
        f"  prompt:     {prompt!r}",
        f"  tokens:     {token_list} ({len(token_list)} tokens)",
        f"  target:     {hook_target}",
        f"",
        f"  Python:",
        f"    shape:    {py_shape}",
        f"    elements: {len(py_np)}",
        f"    min:      {float(py_np.min()):.6f}",
        f"    max:      {float(py_np.max()):.6f}",
        f"    mean:     {float(py_np.mean()):.6f}",
        f"    std:      {float(py_np.std()):.6f}",
    ]

    if rust_data is None or len(rust_data) == 0:
        lines.extend([
            f"",
            f"  Rust: NO DATA IN DISK MAILBOX",
            f"  (Run inference in QEMU first. The inference-server dumps tensors to sectors 1-257.)",
            f"",
            f"  To get Rust data for this specific module, add to inference-server/main.rs:",
            f"    debug_dump_hidden(&{module.replace('.', '_')}, \"L{layer}_{module.replace('.', '_')}\");",
        ])
        return "\n".join(lines)

    # Check if mailbox data matches what we're comparing
    if rust_name and rust_name.startswith("attn_layer") and module != "self_attn":
        lines.extend([
            f"",
            f"  WARNING: Mailbox contains attention weights ('{rust_name}') but you requested '{module}'.",
            f"  The mailbox currently dumps post-softmax attention, not intermediate activations.",
            f"  Use attention_heatmap(prompt, layer={layer}) for attention comparison.",
            f"  For module-level comparison, modify inference-server to dump '{module}' instead.",
        ])
        return "\n".join(lines)

    # Step 3: Compare
    truncated = len(rust_data) < len(py_np)
    compare_len = min(len(py_np), len(rust_data))
    py_slice = py_np[:compare_len]
    r_slice = rust_data[:compare_len]

    diff = np.abs(py_slice - r_slice)
    mse = float(np.mean(diff ** 2))
    py_norm = float(np.linalg.norm(py_slice))
    r_norm = float(np.linalg.norm(r_slice))
    cosine = float(np.dot(py_slice, r_slice) / (py_norm * r_norm + 1e-10))
    max_err = float(diff.max())
    mean_err = float(diff.mean())
    count_01 = int((diff > 0.01).sum())
    count_1 = int((diff > 0.1).sum())
    count_10 = int((diff > 1.0).sum())

    lines.extend([
        f"",
        f"  Rust (disk mailbox):",
        f"    name:     {rust_name}",
        f"    elements: {len(rust_data)} (full tensor: {rust_total})",
        f"    min:      {float(r_slice.min()):.6f}",
        f"    max:      {float(r_slice.max()):.6f}",
        f"    mean:     {float(r_slice.mean()):.6f}",
        f"    std:      {float(r_slice.std()):.6f}",
    ])

    if truncated:
        lines.extend([
            f"",
            f"  *** WARNING: METRICS COMPUTED ON TRUNCATED SLICE ***",
            f"  *** Rust: {len(rust_data)} / Python: {len(py_np)} floats ***",
            f"  *** Full tensor has {rust_total} elements, mailbox holds {len(rust_data)} ***",
            f"  *** Expand Rust DUMP_MAX_SECTORS for full comparison ***",
        ])

    lines.extend([
        f"",
        f"{'─' * 70}",
        f"  Comparison ({compare_len} elements):",
        f"{'─' * 70}",
        f"  MSE:              {mse:.2e}",
        f"  Cosine Similarity:{cosine:.8f}",
        f"  Max Abs Error:    {max_err:.6f}",
        f"  Mean Abs Error:   {mean_err:.6f}",
        f"  |diff| > 0.01:   {count_01} / {compare_len} ({100*count_01/compare_len:.1f}%)",
        f"  |diff| > 0.1:    {count_1} / {compare_len} ({100*count_1/compare_len:.1f}%)",
        f"  |diff| > 1.0:    {count_10} / {compare_len} ({100*count_10/compare_len:.1f}%)",
    ])

    # Top-5 worst divergences
    if compare_len > 0:
        worst_idx = diff.argsort()[-5:][::-1]
        lines.extend([
            f"",
            f"  Top-5 worst divergences:",
        ])
        for i, idx in enumerate(worst_idx):
            lines.append(f"    [{idx:6d}] python={py_slice[idx]:12.6f}  rust={r_slice[idx]:12.6f}  diff={diff[idx]:.6f}")

    # Verdict
    lines.append(f"")
    if cosine > 0.999 and mse < 1e-4:
        lines.append(f"  VERDICT: EXCELLENT — tensors are nearly identical")
    elif cosine > 0.99 and mse < 1e-2:
        lines.append(f"  VERDICT: GOOD — minor quantization noise, direction preserved")
    elif cosine > 0.95:
        lines.append(f"  VERDICT: CONCERNING — noticeable drift, investigate accumulation")
    else:
        lines.append(f"  VERDICT: CRITICAL DIVERGENCE — tensors have different directions")

    return "\n".join(lines)


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
                "serverInfo": {"name": "folkering-debug", "version": "3.0.0"}
            }
        }

    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": req_id, "result": {"tools": TOOLS}}

    if method == "tools/call":
        name   = req["params"]["name"]
        args   = req["params"].get("arguments", {})

        try:
            if name == "kernel_symbol_lookup":
                result = kernel_symbol_lookup(
                    addresses=args["addresses"],
                    elf_path=args.get("elf_path")
                )
            elif name == "serial_throttle_analyzer":
                result = serial_throttle_analyzer(
                    log_path=args["log_path"],
                    window=args.get("window", 5),
                    threshold=args.get("threshold", 10),
                    max_output_lines=args.get("max_output_lines", 200)
                )
            elif name == "qemu_inspect_registers":
                result = qemu_inspect_registers(
                    include_xmm=args.get("include_xmm", False),
                    qmp_socket=args.get("qmp_socket")
                )
            elif name == "tensor_dump":
                result = tensor_dump(
                    disk_image=args.get("disk_image"),
                    return_data=args.get("return_data", False),
                    max_values=args.get("max_values", 64),
                    slice_start=args.get("slice_start", 0),
                    slice_end=args.get("slice_end"),
                    top_k=args.get("top_k", 0),
                    serial_log=args.get("serial_log"),
                    name_filter=args.get("name_filter"),
                )
            elif name == "python_ref_runner":
                result = python_ref_runner(
                    prompt=args["prompt"],
                    mode=args.get("mode", "logits"),
                    top_k=args.get("top_k", 20),
                    max_tokens=args.get("max_tokens", 32),
                    model_path=args.get("model_path"),
                    temperature=args.get("temperature", 0.0),
                    capture_layers=args.get("capture_layers"),
                    layer=args.get("layer"),
                    module_name=args.get("module_name"),
                )
            elif name == "attention_heatmap":
                result = attention_heatmap(
                    prompt=args["prompt"],
                    layer=args.get("layer", 0),
                    head=args.get("head", "all"),
                    model_path=args.get("model_path"),
                    chatml=args.get("chatml", True),
                )
            elif name == "topo_parity_map":
                result = topo_parity_map(
                    prompt=args["prompt"],
                    layer=args.get("layer", 0),
                    module=args.get("module", "self_attn.q_proj"),
                    model_path=args.get("model_path"),
                    chatml=args.get("chatml", True),
                )
            else:
                result = f"Unknown tool: {name}"
        except Exception as e:
            import traceback
            result = f"Tool error: {e}\n{traceback.format_exc()}"

        # Support mixed content (text + image) from tools
        if isinstance(result, list):
            content = result
        else:
            content = [{"type": "text", "text": result}]

        return {
            "jsonrpc": "2.0", "id": req_id,
            "result": {"content": content}
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
