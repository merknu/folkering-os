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
from fbin import write_fbin, write_fbin_streaming, q8_0_bytes, DTYPE_F32, DTYPE_Q8, Q8_BLOCK_SIZE  # type: ignore

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
    # Qwen3-only: per-head RMSNorm on Q / K after projection, before
    # RoPE. Shape is `[head_dim]`, applied to every head.
    "self_attn.q_norm.weight":        "q_norm",
    "self_attn.k_norm.weight":        "k_norm",
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
    Models stored as bf16 (Qwen3 default) require the torch backend
    because numpy doesn't support bfloat16; we promote to fp32 at
    load time so downstream code sees a uniform dtype.
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
    # Try numpy first (faster). Fall back to torch if any tensor is bf16.
    try:
        for f in files:
            path = os.path.join(model_dir, f)
            with safe_open(path, framework="numpy") as st:
                for k in st.keys():
                    out[k] = st.get_tensor(k)
        return out
    except TypeError as e:
        if "bfloat16" not in str(e):
            raise
        # Reset and reload via torch.
        out.clear()
        try:
            import torch  # noqa: F401
        except ImportError as ie:
            raise SystemExit(
                "model uses bfloat16, which numpy can't decode. "
                "Install torch: `pip install torch`."
            ) from ie
        for f in files:
            path = os.path.join(model_dir, f)
            with safe_open(path, framework="pt") as st:
                for k in st.keys():
                    t = st.get_tensor(k)
                    out[k] = t.to(dtype=__import__("torch").float32).numpy()
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

    HF safetensors emit bf16 / f16 / f32 depending on the model. We
    promote everything to f32 first; the .fbin then either keeps it
    or routes through a quantizer based on `emit_tensor`'s `quant`
    argument.
    """
    if arr.dtype != np.float32:
        arr = arr.astype(np.float32)
    return arr.astype("<f4").tobytes()


# Suffix sets that decide whether `emit_tensor` quantizes the tensor
# (when --quantize q8_0 is on). Projections compress well; norms,
# biases, and (by default) the embed table are kept fp32 because
# they're small and / or precision-sensitive.
QUANTIZABLE_SUFFIXES = {".q", ".k", ".v", ".o", ".gate", ".up", ".down"}
# Names that may opt into Q8 separately via `--quantize-embed`. Norms
# stay fp32 unconditionally — they're tiny and accumulate error.
EMBED_NAMES = {"embed", "lm_head"}


def emit_tensor(
    out: list,
    name: str,
    arr: np.ndarray,
    quant: str = "f32",
    quant_embed: bool = False,
):
    """Append a tensor to the output list, quantizing per the
    `quant` + `quant_embed` flags. Norms and biases always stay
    fp32; projections quantize when `quant=q8_0`; embed/lm_head
    only quantize when `quant_embed=True` (separate flag because
    embed precision matters more than projection precision)."""
    # Norms (input_layernorm, post_attention_layernorm,
    # final_norm, q_norm, k_norm) and biases — always fp32.
    if name.endswith("_norm") or name.endswith("_bias") or name == "final_norm":
        out.append((name, DTYPE_F32, list(arr.shape), to_f32_le_bytes(arr)))
        return

    # Embed / lm_head: gated by --quantize-embed.
    if name in EMBED_NAMES:
        if quant == "q8_0" and quant_embed:
            n_elem = int(np.prod(arr.shape))
            if n_elem % Q8_BLOCK_SIZE == 0:
                out.append((name, DTYPE_Q8, list(arr.shape), q8_0_bytes(arr.flatten())))
                return
        out.append((name, DTYPE_F32, list(arr.shape), to_f32_le_bytes(arr)))
        return

    # Projection matrices: gated by --quantize.
    if quant == "q8_0" and any(name.endswith(s) for s in QUANTIZABLE_SUFFIXES):
        n_elem = int(np.prod(arr.shape))
        if n_elem % Q8_BLOCK_SIZE == 0:
            out.append((name, DTYPE_Q8, list(arr.shape), q8_0_bytes(arr.flatten())))
            return
    out.append((name, DTYPE_F32, list(arr.shape), to_f32_le_bytes(arr)))


def _build_safetensors_index(model_dir: str):
    """Map every tensor key → (shard_path) without loading data.
    Lets the streaming converter pull tensors one at a time instead
    of materialising the full bf16→fp32 model in RAM."""
    files = sorted(
        os.path.join(model_dir, f) for f in os.listdir(model_dir)
        if f.endswith(".safetensors")
    )
    if not files:
        raise FileNotFoundError(
            f"no .safetensors in {model_dir} — is this a real HF dir?"
        )
    index = {}
    for path in files:
        with safe_open(path, framework="numpy") as st:
            for k in st.keys():
                index[k] = path
    return index


def _load_one_tensor(path: str, key: str) -> np.ndarray:
    """Load a single tensor as fp32 numpy. Tries numpy backend first;
    falls back to torch if dtype is bf16 (numpy can't decode bf16
    natively until recent versions)."""
    try:
        with safe_open(path, framework="numpy") as st:
            return st.get_tensor(key)
    except TypeError as e:
        if "bfloat16" not in str(e):
            raise
        try:
            import torch  # noqa: F401
        except ImportError as ie:
            raise SystemExit(
                "model uses bfloat16, which numpy can't decode. "
                "Install torch: `pip install torch`."
            ) from ie
        with safe_open(path, framework="pt") as st:
            t = st.get_tensor(key)
            return t.to(dtype=__import__("torch").float32).numpy()


def convert(
    model_dir: str,
    out_path: str,
    max_layers: int | None = None,
    max_seq_len: int | None = None,
    quant: str = "f32",
    quant_embed: bool = False,
):
    """Streaming HF safetensors → .fbin converter.

    Loads exactly ONE tensor at a time, quantizes (or just promotes
    to fp32), writes to an .fbin tempfile, and frees. Peak RAM is
    dominated by the largest single tensor (typically embed for
    Qwen3-4B at ~1.5 GiB fp32 → ~410 MiB Q8_2). The previous version
    of this function held the whole model in `weights: dict` plus
    every quantized blob in `tensors: list`, which OOM'd on hosts
    with <16 GiB free.
    """
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

    index = _build_safetensors_index(model_dir)
    print(f"[hf_to_fbin] indexed {len(index)} tensors across "
          f"{len(set(index.values()))} shard(s)")

    n_tensors = [0]
    n_bytes = [0]

    def stream_tensor(emit, our_name: str, hf_name: str):
        if hf_name not in index:
            return False
        arr = _load_one_tensor(index[hf_name], hf_name)
        # Buffered emit; emit_tensor expects a list, so we fake one
        # for one tensor at a time and forward to the streaming sink.
        sink = []
        emit_tensor(sink, our_name, arr, quant=quant, quant_embed=quant_embed)
        del arr
        for name, dtype, shape, raw in sink:
            emit(name, dtype, shape, raw)
            n_tensors[0] += 1
            n_bytes[0] += len(raw)
        return True

    def emit_synth(emit, name: str, arr):
        sink = []
        emit_tensor(sink, name, arr, quant=quant, quant_embed=quant_embed)
        for n, dtype, shape, raw in sink:
            emit(n, dtype, shape, raw)
            n_tensors[0] += 1
            n_bytes[0] += len(raw)

    def iter_tensors(emit):
        # Order matches the previous in-memory `convert` for
        # determinism / byte-for-byte regression diffs across runs.
        if not stream_tensor(emit, "embed", "model.embed_tokens.weight"):
            raise KeyError("missing model.embed_tokens.weight")

        for layer_i in range(n_layers):
            for hf_suffix, our_suffix in HF_LAYER_TENSORS.items():
                hf_name = f"model.layers.{layer_i}.{hf_suffix}"
                our_name = f"layer.{layer_i}.{our_suffix}"
                stream_tensor(emit, our_name, hf_name)

        stream_tensor(emit, "final_norm", "model.norm.weight")

        if not tie_emb:
            stream_tensor(emit, "lm_head", "lm_head.weight")

        # RoPE tables are synthesised, not loaded.
        cos_t, sin_t = precompute_rope(head_dim, max_pos, rope_theta)
        emit_synth(emit, "rope_cos", cos_t)
        emit_synth(emit, "rope_sin", sin_t)

    write_fbin_streaming(out_path, iter_tensors)

    print(
        f"[hf_to_fbin] wrote {out_path} "
        f"({n_tensors[0]} tensors, ~{n_bytes[0] / (1024 * 1024):.1f} MiB of weights)"
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
    ap.add_argument("--quantize", choices=["f32", "q8_0"], default="f32",
                    help="Quantize the projection matrices (q/k/v/o/gate/up/down). "
                         "Q8_0 stores 32-element blocks of [f16 scale, 32 i8 vals], "
                         "shrinking the projections ~4x. Embed, lm_head, norms, and "
                         "biases stay fp32 for precision.")
    ap.add_argument("--quantize-embed", action="store_true",
                    help="ALSO quantize embed / lm_head to Q8_0 (only effective "
                         "when --quantize=q8_0). Saves another ~75% on the embed "
                         "table — Qwen3-0.6B drops from 622 MB fp32 to 165 MB. "
                         "Slight precision tradeoff; argmax stability typically "
                         "preserved on short prompts. Use when fitting on edge "
                         "hardware (Pi 5, low-RAM VMs).")
    args = ap.parse_args()

    convert(
        model_dir=args.model_dir,
        out_path=args.out,
        max_layers=args.max_layers,
        max_seq_len=args.max_seq_len,
        quant=args.quantize,
        quant_embed=args.quantize_embed,
    )


if __name__ == "__main__":
    main()
