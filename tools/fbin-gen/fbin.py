#!/usr/bin/env python3
"""Folkering OS `.fbin` writer.

Hosts the build-time bridge from "I have an array of f32s" to "I have
the bytes the inference task can mmap from Synapse VFS". Spec lives
in `userspace/inference/src/weights.rs`.

Today we use it to generate test blobs for the boot self-tests
(D.3.1, D.3.3, ...). When D.3.1.3 lands the real HuggingFace →
`.fbin` converter, it's the same writer with a different front-end.
"""

import struct
import sys
from typing import List, Tuple

MAGIC = b"FBN1"
VERSION = 1
PAGE = 4096

DTYPE_F32 = 0
DTYPE_Q8 = 1
DTYPE_Q4 = 2


def write_fbin(tensors: List[Tuple[str, int, List[int], bytes]]) -> bytes:
    """Pack `(name, dtype, shape, raw_bytes)` tuples into a `.fbin` blob.

    Caller is responsible for raw_bytes being the right length for
    `prod(shape) * elem_size(dtype)`.
    """
    # ── First pass: lay out metadata + data offsets ─────────────────
    metadata = bytearray()
    # We need data_offset for each tensor before we can finalize
    # metadata. Strategy: build metadata with placeholder offsets,
    # measure metadata_len, page-align, then fill offsets in a
    # second pass.
    placeholders = []  # (offset_into_metadata, data_len)
    for name, dtype, shape, raw in tensors:
        name_bytes = name.encode("utf-8")
        metadata += struct.pack("<H", len(name_bytes))
        metadata += name_bytes
        metadata += struct.pack("<BB", dtype, len(shape))
        for d in shape:
            metadata += struct.pack("<I", d)
        # Placeholder for data_offset (8 bytes), data_len (8 bytes).
        offset_ph = len(metadata)
        metadata += struct.pack("<QQ", 0, len(raw))
        placeholders.append((offset_ph, len(raw)))

    # ── Compute data section offsets (page-aligned start) ───────────
    data_section_start = ((16 + len(metadata) + (PAGE - 1)) // PAGE) * PAGE
    cur_data_off = data_section_start
    for (ph_off, raw_len), (_, _, _, raw) in zip(placeholders, tensors):
        struct.pack_into("<QQ", metadata, ph_off, cur_data_off, raw_len)
        cur_data_off += raw_len

    # ── Header ──────────────────────────────────────────────────────
    out = bytearray()
    out += MAGIC
    out += struct.pack("<H", VERSION)
    out += struct.pack("<H", len(tensors))
    out += struct.pack("<Q", len(metadata))
    out += metadata

    # ── Pad to page boundary, then append data ─────────────────────
    while len(out) < data_section_start:
        out.append(0)
    for _, _, _, raw in tensors:
        out += raw

    return bytes(out)


def f32_bytes(values) -> bytes:
    """Pack a flat list of floats to little-endian f32 bytes."""
    return struct.pack(f"<{len(values)}f", *values)


# ── Q8_0 quantization ──────────────────────────────────────────────────
#
# Same on-disk layout as llama.cpp's Q8_0:
#
#   for each block of 32 elements:
#       scale: f16 (2 bytes)
#       vals : i8 × 32 (32 bytes)   → 34 bytes per block
#
# Quantize: scale = max(|x|) / 127, q[i] = round(x[i] / scale)
# Dequantize: x[i] = q[i] * scale
#
# We require N % 32 == 0 today (every Qwen / Llama projection matches —
# hidden_size and intermediate_size are always multiples of 32). For
# tensors that would round into a partial trailing block, the converter
# refuses rather than silently padding.

Q8_BLOCK_SIZE = 32
Q8_HALF = 16
# Block layout: [scale_lo: f16][scale_hi: f16][q[0..32]: i8] = 36 bytes.
# scale_lo quantizes q[0..16], scale_hi quantizes q[16..32]. Splitting
# the scale per half-block roughly halves quantization noise vs the
# original single-scale Q8_0 layout (which was 2+32 = 34 bytes), at a
# cost of +6% file size. Empirically motivated by Folkering OS Qwen3-4B
# bringup: with 36-layer compounded Q8 noise, single-scale blocks
# produced 0.94-logit gaps on close-tier tokens that flipped greedy
# argmax in roughly 6% of decoding steps. Two-scale blocks shrink the
# per-element scale-noise floor (~0.4% absmax-uniform → ~0.2%) which
# is enough to rescue most of those flips.
Q8_BLOCK_BYTES = 4 + 32


def q8_0_bytes(values) -> bytes:
    """Pack a flat list of floats as Q8_2 blocks (two f16 scales per
    32 i8 values). Returns the raw bytes; caller emits with
    `dtype = DTYPE_Q8`.

    Function name kept as q8_0_bytes for call-site compatibility —
    callers don't care about the internal layout, just that the bytes
    pair with a DTYPE_Q8 tensor entry. The runtime readers in
    `userspace::inference::tensor_math` and `kernel::arch::x86_64::smp`
    interpret these bytes per the matching Q8_BLOCK_BYTES constant on
    their side.

    Importing numpy lazily so callers that only need the f32 path
    don't pay the import cost (and so this module stays usable in
    `gen_test_blobs.py` which runs without numpy when emitting only
    fp32 fixtures).
    """
    import numpy as np
    arr = np.asarray(values, dtype=np.float32)
    n = arr.size
    if n % Q8_BLOCK_SIZE != 0:
        raise ValueError(
            f"Q8: tensor size {n} not divisible by block size "
            f"{Q8_BLOCK_SIZE}"
        )
    n_blocks = n // Q8_BLOCK_SIZE
    out = bytearray()
    for b in range(n_blocks):
        block = arr[b * Q8_BLOCK_SIZE : (b + 1) * Q8_BLOCK_SIZE]
        scales_b = bytearray()
        quants_b = bytearray()
        for half in range(2):
            sub = block[half * Q8_HALF : (half + 1) * Q8_HALF]
            absmax = float(np.max(np.abs(sub)))
            if absmax == 0.0:
                scale = np.float32(0.0)
                qs = np.zeros(Q8_HALF, dtype=np.int8)
            else:
                scale = np.float32(absmax / 127.0)
                qs = np.round(sub / scale).astype(np.int32)
                # Saturate to int8 range. Round-half-to-even can drift
                # to ±128 on edge cases; clamp keeps us in the legal
                # range so the kernel maddubs sign-fold trick stays
                # safe (`sign_epi8(-128, _)` overflows).
                qs = np.clip(qs, -127, 127).astype(np.int8)
            scales_b += np.float16(scale).tobytes()
            quants_b += qs.tobytes()
        out += scales_b
        out += quants_b
    return bytes(out)


def emit_rust_const(name: str, blob: bytes) -> str:
    """Render a `pub const NAME: &[u8] = &[ ... ];` literal of the
    blob bytes, 16 per row, with a one-line comment header so the
    generated file is easy to diff."""
    lines = [
        "// AUTO-GENERATED by tools/fbin-gen/fbin.py — do not edit by hand.",
        f"// Re-run `python tools/fbin-gen/gen_test_blobs.py` if shapes",
        "// or values change.",
        "",
        f"pub const {name}: &[u8] = &[",
    ]
    for i in range(0, len(blob), 16):
        chunk = blob[i:i + 16]
        hex_parts = ", ".join(f"0x{b:02x}" for b in chunk)
        lines.append(f"    {hex_parts},")
    lines.append("];")
    return "\n".join(lines) + "\n"
