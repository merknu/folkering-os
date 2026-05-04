#!/usr/bin/env python3
"""HuggingFace Qwen2.5 / Llama safetensors → Folkering `.fbin` converter.

Reads a HuggingFace model directory:

    <model-dir>/
        config.json           ← architecture parameters
        model.safetensors     ← weights (or model-00001-of-NNNNN.safetensors shards)

Writes a single `.fbin` consumable by `userspace/inference`. The naming
convention used inside the .fbin is OUR convention (short, layer-prefixed),
not HuggingFace's verbose namespace; the inference task expects to walk:

    embed
    layer.<N>.attn_norm           (input_layernorm)
    layer.<N>.q     /  q_bias
    layer.<N>.k     /  k_bias
    layer.<N>.v     /  v_bias
    layer.<N>.o
    layer.<N>.ffn_norm            (post_attention_layernorm)
    layer.<N>.gate
    layer.<N>.up
    layer.<N>.down
    final_norm
    lm_head                       (omitted when tied to embed)
    rope_cos
    rope_sin

`rope_cos` / `rope_sin` are pre-computed for `max_position_embeddings`
positions × `head_dim/2` frequencies — no on-device sin/cos needed.

Usage:

    # Convert a real HF model dir
    python tools/fbin-gen/hf_to_fbin.py \\
        --model-dir ~/models/Qwen2.5-0.5B \\
        --out qwen05b.fbin

    # Truncate to first N layers (handy for fitting in initrd while
    # validating the format end-to-end)
    python tools/fbin-gen/hf_to_fbin.py \\
        --model-dir ~/models/Qwen2.5-0.5B \\
        --max-layers 1 \\
        --out qwen05b-1L.fbin

    # No model on hand? Generate a synthetic 1-layer "tiny-qwen" with
    # the right shape but random weights (see make_test_model.py).
    python tools/fbin-gen/make_test_model.py /tmp/tiny-qwen
    python tools/fbin-gen/hf_to_fbin.py \\
        --model-dir /tmp/tiny-qwen \\
        --out boot/iso_root/qwen_test.fbin

Out of scope today (queued for D.3.5+):
- Quantization. Today we write f32 (full precision). Q4 / Q8 land
  alongside the runtime path that consumes them.
- Tied embedding deduplication. Today if `tie_word_embeddings = True`
  we emit `embed` only and let the inference task reuse it as
  `lm_head`. (HF's actual `lm_head.weight` is dropped — the
  underlying tensor is identical.)
- GGUF / ONNX. We could read both via the same tooling later; for
  now safetensors covers ~all modern open-weight releases.
"""

import argparse
import json
import math
import os
import sys
from typing import Dict

# Local helpers
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fbin import write_fbin, DTYPE_F32  # type: ignore

import numpy as np
from safetensors import safe_open


# ── Architectures we know how to map ───────────────────────────────────
# Both Qwen2.5 and Llama-3 use the same HF naming. We intentionally
# don't probe the model-type field — naming is the source of truth and
# any future model that uses these names just works.

HF_LAYER_TENSORS = {
    "input_layernorm.weight":         "attn_norm",
    "self_attn.q_proj.weight":        "q",
    "self_attn.k_proj.weight":        "k",
    "self_attn.v_proj.weight":        "v",
    "self_attn.o_proj.weight":        "o",
    "self_attn.q_proj.bias":          "q_bias",   # Qwen2.5-only
    "self_attn.k_proj.bias":          "k_bias",
    "self_attn.v_proj.bias":          "v_bias",
    "post_attention_layernorm.weight":"ffn_norm",
    "mlp.gate_proj.weight":           "gate",
    "mlp.up_proj.weight":             "up",
    "mlp.down_proj.weight":           "down",
}

HF_TOPLEVEL = {
    "model.embed_tokens.weight": "embed",
    "model.norm.weight":         "final_norm",
    "lm_head.weight":            "lm_head",
}


def load_safetensors(model_dir: str) -> Dict[str, np.ndarray]:
    """Load every tensor from every `.safetensors` file in `model_dir`,
    returning a single name → numpy.ndarray dict.

    Walks shard files (`model-00001-of-NNNNN.safetensors`) automatically.
    """
    out: Dict[str, np.ndarray] = {}
    files = sorted(
        f for f in os.listdir(model_dir)
        if f.endswith(".safetensors")
    )
    if not files:
        raise FileNotFoundError(
            f"no .safetensors in {model_dir} — is this a real HF dir?"
        )
    for f in files:
        path = os.path.join(model_dir, f)
        with safe_open(path, framework="numpy") as st:
            for k in st.keys():
                out[k] = st.get_tensor(k)
    return out


def precompute_rope(
    head_dim: int,
    max_seq_len: int,
    theta_base: float,
) -> tuple[np.ndarray, np.ndarray]:
    """Standard Llama/Qwen RoPE cos/sin tables.

    Returns:
        cos_table, sin_table — each shape `[max_seq_len, head_dim/2]`,
        f32, pre-computed using Python's `math.cos` / `math.sin` so
        the no_std runtime never needs to call libm.

    Convention matches `tensor_math::apply_rope`: for position p and
    pair index i (0 ≤ i < head_dim/2), the rotation angle is
    `p / (theta_base ** (2*i / head_dim))`.
    """
    pairs = head_dim // 2
    cos_t = np.zeros((max_seq_len, pairs), dtype=np.float32)
    sin_t = np.zeros((max_seq_len, pairs), dtype=np.float32)
    for p in range(max_seq_len):
        for i in range(pairs):
            freq = 1.0 / (theta_base ** (2.0 * i / head_dim))
            angle = p * freq
            cos_t[p, i] = math.cos(angle)
            sin_t[p, i] = math.sin(angle)
    return cos_t, sin_t


def to_f32_le_bytes(arr: np.ndarray) -> bytes:
    """Force f32 little-endian byte layout regardless of input dtype.

    HF safetensors emit bf16 / f16 / f32 depending on the model. For
    D.3.1.3 we promote everything to f32 — the inference task's matmul
    path is f32-only. Quantization (and lower-precision compute) come
    in D.3.3 quant-aware, after we've proven the f32 reference path.
    """
    if arr.dtype != np.float32:
        arr = arr.astype(np.float32)
    return arr.astype("<f4").tobytes()


def emit_tensor(out: list, name: str, arr: np.ndarray):
    out.append((name, DTYPE_F32, list(arr.shape), to_f32_le_bytes(arr)))


def convert(
    model_dir: str,
    out_path: str,
    max_layers: int | None = None,
    max_seq_len: int | None = None,
):
    cfg_path = os.path.join(model_dir, "config.json")
    with open(cfg_path) as f:
        cfg = json.load(f)

    n_layers_full = cfg["num_hidden_layers"]
    n_layers = n_layers_full if max_layers is None else min(max_layers, n_layers_full)
    hidden_size = cfg["hidden_size"]
    n_heads = cfg["num_attention_heads"]
    head_dim = cfg.get("head_dim", hidden_size // n_heads)
    rope_theta = cfg.get("rope_theta", 10000.0)
    max_pos = max_seq_len or cfg.get("max_position_embeddings", 2048)
    tie_emb = cfg.get("tie_word_embeddings", False)

    print(
        f"[hf_to_fbin] {model_dir}: "
        f"layers={n_layers}/{n_layers_full} hidden={hidden_size} "
        f"n_heads={n_heads} head_dim={head_dim} rope_theta={rope_theta} "
        f"max_pos={max_pos} tied_embed={tie_emb}"
    )

    weights = load_safetensors(model_dir)

    # Output tensor list, in the order the inference task expects to
    # find them. Order doesn't matter for correctness (lookup is by
    # name), but consistent ordering means deterministic .fbin output
    # so byte-for-byte comparisons across runs catch regressions.
    tensors: list = []

    # ── Embedding ──
    if "model.embed_tokens.weight" in weights:
        emit_tensor(tensors, "embed", weights["model.embed_tokens.weight"])
    else:
        raise KeyError("missing model.embed_tokens.weight")

    # ── Per-layer ──
    for layer_i in range(n_layers):
        for hf_suffix, our_suffix in HF_LAYER_TENSORS.items():
            hf_name = f"model.layers.{layer_i}.{hf_suffix}"
            if hf_name not in weights:
                # Some tensors are model-specific (Qwen has q_bias,
                # Llama doesn't). Skip silently when absent — the
                # runtime will skip the corresponding op.
                continue
            our_name = f"layer.{layer_i}.{our_suffix}"
            emit_tensor(tensors, our_name, weights[hf_name])

    # ── Final norm ──
    if "model.norm.weight" in weights:
        emit_tensor(tensors, "final_norm", weights["model.norm.weight"])

    # ── lm_head (skip if tied to embed) ──
    if not tie_emb and "lm_head.weight" in weights:
        emit_tensor(tensors, "lm_head", weights["lm_head.weight"])

    # ── RoPE precomputed tables ──
    cos_t, sin_t = precompute_rope(head_dim, max_pos, rope_theta)
    emit_tensor(tensors, "rope_cos", cos_t)
    emit_tensor(tensors, "rope_sin", sin_t)

    blob = write_fbin(tensors)
    with open(out_path, "wb") as f:
        f.write(blob)

    total_data_mb = sum(len(t[3]) for t in tensors) / (1024 * 1024)
    print(
        f"[hf_to_fbin] wrote {out_path} "
        f"({len(blob):,} bytes, {len(tensors)} tensors, "
        f"~{total_data_mb:.1f} MiB of weights)"
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model-dir", required=True,
                    help="HuggingFace model directory (config.json + .safetensors)")
    ap.add_argument("--out", required=True, help="Output .fbin path")
    ap.add_argument("--max-layers", type=int, default=None,
                    help="Truncate to first N layers (default: all)")
    ap.add_argument("--max-seq-len", type=int, default=None,
                    help="Override max_position_embeddings for RoPE table size")
    args = ap.parse_args()

    convert(
        model_dir=args.model_dir,
        out_path=args.out,
        max_layers=args.max_layers,
        max_seq_len=args.max_seq_len,
    )


if __name__ == "__main__":
    main()
