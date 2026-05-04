#!/usr/bin/env python3
"""Numpy reference forward pass over a Folkering `.fbin` file.

Used to compute the expected argmax for the inference task's D.3.5
boot self-test. Implements the same algorithm the Rust runtime does:

    embed_lookup(token_ids)
    for layer in layers:
        x_n  = rmsnorm(x, attn_norm)
        attn = wo @ sdpa(rope(wq @ x_n), rope(wk @ x_n), wv @ x_n)
        x   += attn
        x_n2 = rmsnorm(x, ffn_norm)
        ffn  = down @ (silu(gate @ x_n2) * (up @ x_n2))
        x   += ffn
    final_normed = rmsnorm(x[-1], final_norm)
    logits = embed @ final_normed     # tied lm_head
    return argmax(logits)

Importantly: matches the runtime's lack of biases on QKV (synthetic
biases are zero anyway, so this is faithful for the test fixture).

Run as:
    python tools/fbin-gen/forward_ref.py boot/iso_root/qwen_test.fbin

Output is suitable for pasting into the inference task's boot test as
the expected argmax constant.
"""

import struct
import sys
from typing import Dict, Tuple

import numpy as np


DTYPE_F32 = 0
DTYPE_Q8 = 1


def dequantize_q8(blob: bytes, n_elem: int) -> np.ndarray:
    """Decode Q8_0 blocks (34 bytes each: f16 scale + 32 i8 vals)
    into an f32 array. Mirrors the Rust runtime's dequantize path
    for parity verification."""
    assert n_elem % 32 == 0, f"Q8 n_elem={n_elem} not divisible by 32"
    n_blocks = n_elem // 32
    expected_bytes = n_blocks * 34
    assert len(blob) == expected_bytes, (
        f"Q8 blob {len(blob)} bytes != expected {expected_bytes} "
        f"({n_blocks} blocks × 34)"
    )
    out = np.empty(n_elem, dtype=np.float32)
    for b in range(n_blocks):
        off = b * 34
        scale = float(np.frombuffer(blob[off:off + 2], dtype=np.float16)[0])
        vals = np.frombuffer(blob[off + 2:off + 34], dtype=np.int8)
        out[b * 32:(b + 1) * 32] = vals.astype(np.float32) * scale
    return out


def parse_fbin(path: str) -> Tuple[Dict[str, np.ndarray], dict]:
    """Parse a `.fbin` and return (name -> ndarray, header_info).
    Both fp32 and Q8 tensors land in the dict as f32 arrays — Q8
    payloads are dequantized at parse time so the rest of the
    reference forward pass stays a single code path."""
    with open(path, "rb") as f:
        data = f.read()
    assert data[:4] == b"FBN1", f"bad magic {data[:4]!r}"
    version, n_tensors = struct.unpack("<HH", data[4:8])
    assert version == 1
    metadata_len = struct.unpack("<Q", data[8:16])[0]
    metadata_end = 0x10 + metadata_len

    cur = 0x10
    metas = []
    for _ in range(n_tensors):
        (name_len,) = struct.unpack("<H", data[cur:cur + 2])
        cur += 2
        name = data[cur:cur + name_len].decode("utf-8")
        cur += name_len
        dtype_byte = data[cur]; cur += 1
        rank = data[cur]; cur += 1
        shape = list(struct.unpack(f"<{rank}I", data[cur:cur + 4 * rank]))
        cur += 4 * rank
        data_offset, data_len = struct.unpack("<QQ", data[cur:cur + 16])
        cur += 16
        metas.append((name, dtype_byte, shape, data_offset, data_len))

    out: Dict[str, np.ndarray] = {}
    n_quant = 0
    for name, dt, shape, off, n in metas:
        n_elem = int(np.prod(shape))
        blob = data[off:off + n]
        if dt == DTYPE_F32:
            arr = np.frombuffer(blob, dtype=np.float32).copy()
        elif dt == DTYPE_Q8:
            arr = dequantize_q8(blob, n_elem)
            n_quant += 1
        else:
            raise ValueError(f"unsupported dtype {dt} for tensor {name!r}")
        out[name] = arr.reshape(shape)
    return out, {
        "n_tensors": n_tensors,
        "metadata_end": metadata_end,
        "n_quant": n_quant,
    }


def fast_rsqrt(x: np.ndarray) -> np.ndarray:
    """Match the Rust 2-iteration Quake rsqrt for parity."""
    x = np.asarray(x, dtype=np.float32)
    x_safe = np.where(x > 0, x, np.float32(1.0))
    i_bits = np.frombuffer(x_safe.astype("<f4").tobytes(), dtype="<u4").copy()
    i_bits = (np.uint32(0x5F375A86) - (i_bits >> np.uint32(1))).astype(np.uint32)
    y = np.frombuffer(i_bits.tobytes(), dtype="<f4").copy().astype(np.float32)
    y = y * (np.float32(1.5) - np.float32(0.5) * x_safe * y * y)
    y = y * (np.float32(1.5) - np.float32(0.5) * x_safe * y * y)
    return np.where(x > 0, y, np.float32(0.0)).astype(np.float32)


def rmsnorm(x: np.ndarray, w: np.ndarray, eps: float = 1e-5) -> np.ndarray:
    mean_sq = (x * x).mean()
    inv_rms = fast_rsqrt(mean_sq + np.float32(eps))
    return (x * inv_rms * w).astype(np.float32)


def fast_exp(x: np.ndarray) -> np.ndarray:
    """Mirror the Rust 6th-order minimax fast_exp for parity. Vectorised."""
    x = np.asarray(x, dtype=np.float32)
    out = np.empty_like(x)
    high = x > np.float32(88.0)
    low = x < np.float32(-88.0)
    mid = ~(high | low)
    out[high] = np.float32(np.finfo(np.float32).max)
    out[low] = np.float32(0.0)
    if mid.any():
        xs = x[mid]
        ln2 = np.float32(0.6931471805599453)
        inv_ln2 = np.float32(1.4426950408889634)
        n_raw = xs * inv_ln2 + np.float32(0.5)
        n_int = n_raw.astype(np.int32)
        # floor: for negatives where casting truncates toward zero
        adj = (n_raw < 0) & (n_int.astype(np.float32) != n_raw)
        n_floor = np.where(adj, n_int - 1, n_int).astype(np.float32)
        r = xs - n_floor * ln2
        p = np.float32(1.0) + r * (
            np.float32(1.0) + r * (
            np.float32(0.5) + r * (
            np.float32(1.0/6.0) + r * (
            np.float32(1.0/24.0) + r * np.float32(1.0/120.0)))))
        bits = ((n_floor.astype(np.int32) + np.int32(127)).astype(np.uint32)
                << np.uint32(23))
        pow2 = np.frombuffer(bits.tobytes(), dtype="<f4").copy()
        out[mid] = (p * pow2).astype(np.float32)
    return out


def silu(x: np.ndarray) -> np.ndarray:
    return (x / (np.float32(1.0) + fast_exp(-x))).astype(np.float32)


def softmax_inplace(x: np.ndarray) -> np.ndarray:
    """Numerically stable, with NEG_INFINITY support."""
    m = x.max()
    if not np.isfinite(m):
        n = x.size
        return np.full_like(x, np.float32(1.0 / n))
    e = fast_exp(x - m)
    s = e.sum()
    if s > 0:
        return (e / s).astype(np.float32)
    return e


def apply_rope(qk: np.ndarray, cos_t: np.ndarray, sin_t: np.ndarray,
               seq_len: int, n_heads: int, head_dim: int) -> np.ndarray:
    """In-place would be fine but we return a new array to mirror tests."""
    out = qk.reshape(seq_len, n_heads, head_dim).copy()
    pairs = head_dim // 2
    for s in range(seq_len):
        for h in range(n_heads):
            for p in range(pairs):
                cos = cos_t[s, p]
                sin = sin_t[s, p]
                x0 = out[s, h, 2 * p]
                x1 = out[s, h, 2 * p + 1]
                out[s, h, 2 * p] = x0 * cos - x1 * sin
                out[s, h, 2 * p + 1] = x0 * sin + x1 * cos
    return out.reshape(seq_len * n_heads * head_dim)


def attention(x: np.ndarray, wq, wk, wv, wo,
              q_bias, k_bias, v_bias,
              rope_cos, rope_sin,
              seq_len, hidden_dim, n_heads, n_kv_heads) -> np.ndarray:
    head_dim = hidden_dim // n_heads
    hkv = head_dim * n_kv_heads
    # x: [seq, hidden]
    q = (x @ wq.T).astype(np.float32)
    k = (x @ wk.T).astype(np.float32)
    v = (x @ wv.T).astype(np.float32)
    if q_bias is not None: q = q + q_bias
    if k_bias is not None: k = k + k_bias
    if v_bias is not None: v = v + v_bias
    q = apply_rope(q.flatten(), rope_cos, rope_sin, seq_len, n_heads, head_dim)
    k = apply_rope(k.flatten(), rope_cos, rope_sin, seq_len, n_kv_heads, head_dim)
    q = q.reshape(seq_len, n_heads, head_dim)
    k = k.reshape(seq_len, n_kv_heads, head_dim)
    v = v.reshape(seq_len, n_kv_heads, head_dim)
    groups = n_heads // n_kv_heads
    scale = fast_rsqrt(np.float32(head_dim))
    out = np.zeros((seq_len, n_heads, head_dim), dtype=np.float32)
    for h in range(n_heads):
        kvh = h // groups
        for i in range(seq_len):
            scores = np.full(seq_len, -np.inf, dtype=np.float32)
            for j in range(seq_len):
                if j > i:
                    continue
                dot = float((q[i, h] * k[j, kvh]).sum()) * float(scale)
                scores[j] = np.float32(dot)
            attn = softmax_inplace(scores)
            for d in range(head_dim):
                out[i, h, d] = sum(attn[j] * v[j, kvh, d] for j in range(seq_len))
    out2 = out.reshape(seq_len, hidden_dim)
    return (out2 @ wo.T).astype(np.float32)


def swiglu(x: np.ndarray, gate: np.ndarray, up: np.ndarray, down: np.ndarray
           ) -> np.ndarray:
    g = (x @ gate.T).astype(np.float32)
    u = (x @ up.T).astype(np.float32)
    h = silu(g) * u
    return (h @ down.T).astype(np.float32)


def forward(tensors: Dict[str, np.ndarray], cfg: dict, ids):
    n_layers = cfg["n_layers"]
    hidden = cfg["hidden_dim"]
    n_heads = cfg["n_heads"]
    n_kv_heads = cfg["n_kv_heads"]
    eps = cfg.get("eps", 1e-5)

    embed = tensors["embed"]
    final_norm = tensors["final_norm"]
    rope_cos_full = tensors["rope_cos"]
    rope_sin_full = tensors["rope_sin"]
    seq_len = len(ids)
    rope_cos = rope_cos_full[:seq_len]
    rope_sin = rope_sin_full[:seq_len]

    x = np.stack([embed[i] for i in ids]).astype(np.float32)  # [seq, hidden]

    for li in range(n_layers):
        prefix = f"layer.{li}"
        attn_norm = tensors[f"{prefix}.attn_norm"]
        wq = tensors[f"{prefix}.q"]
        wk = tensors[f"{prefix}.k"]
        wv = tensors[f"{prefix}.v"]
        wo = tensors[f"{prefix}.o"]
        # Biases optional (Llama-3 skips them, Qwen2.5 has them on q/k/v).
        q_bias = tensors.get(f"{prefix}.q_bias")
        k_bias = tensors.get(f"{prefix}.k_bias")
        v_bias = tensors.get(f"{prefix}.v_bias")
        ffn_norm = tensors[f"{prefix}.ffn_norm"]
        gate = tensors[f"{prefix}.gate"]
        up = tensors[f"{prefix}.up"]
        down = tensors[f"{prefix}.down"]

        x_n = np.stack([rmsnorm(x[s], attn_norm, eps) for s in range(seq_len)])
        attn = attention(x_n, wq, wk, wv, wo,
                         q_bias, k_bias, v_bias,
                         rope_cos, rope_sin,
                         seq_len, hidden, n_heads, n_kv_heads)
        x = (x + attn).astype(np.float32)

        x_n2 = np.stack([rmsnorm(x[s], ffn_norm, eps) for s in range(seq_len)])
        ffn_out = swiglu(x_n2, gate, up, down)
        x = (x + ffn_out).astype(np.float32)

    last = x[-1]
    last_n = rmsnorm(last, final_norm, eps)
    logits = (embed @ last_n).astype(np.float32)
    return logits


def main():
    if len(sys.argv) < 2:
        print("usage: forward_ref.py <fbin>", file=sys.stderr)
        sys.exit(2)
    fbin_path = sys.argv[1]
    tensors, _ = parse_fbin(fbin_path)
    print(f"[forward_ref] loaded {len(tensors)} tensors from {fbin_path}")
    embed_shape = tensors["embed"].shape
    print(f"[forward_ref] embed shape = {embed_shape}")

    # Synthetic config (matches make_test_model.py defaults).
    cfg = {
        "n_layers": 1,
        "hidden_dim": 64,
        "n_heads": 4,
        "n_kv_heads": 2,
        "eps": 1e-5,
    }
    ids = [1, 2, 3]
    logits = forward(tensors, cfg, ids)
    am = int(np.argmax(logits))
    top5_idx = np.argsort(-logits)[:5]
    print(f"[forward_ref] token_ids = {ids}")
    print(f"[forward_ref] logits.shape = {logits.shape}")
    print(f"[forward_ref] argmax = {am}, logits[{am}] = {logits[am]:.6f}")
    print(f"[forward_ref] top-5 ids   = {top5_idx.tolist()}")
    print(f"[forward_ref] top-5 vals  = {logits[top5_idx].tolist()}")
    print(f"[forward_ref] sum(logits) = {logits.sum():.6f}")


if __name__ == "__main__":
    main()
