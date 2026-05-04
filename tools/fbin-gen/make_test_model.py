#!/usr/bin/env python3
"""Generate a tiny synthetic HuggingFace-shaped model directory.

The output is a real `config.json` + `model.safetensors` pair that
`hf_to_fbin.py` reads identically to a real model. We use a 1-layer
"tiny-qwen" architecture (hidden=64, n_heads=4, ffn=128, vocab=256)
that's small enough to fit in initrd while exercising every code path
in the converter.

Why this matters: D.3.1.3's correctness guarantee comes from the
end-to-end test (HF → fbin → boot self-test). Without a way to run
that test on every commit, the converter is just code we hope works.
With this generator, CI can run the full chain in milliseconds.

Usage:

    python tools/fbin-gen/make_test_model.py /tmp/tiny-qwen
    python tools/fbin-gen/hf_to_fbin.py \\
        --model-dir /tmp/tiny-qwen \\
        --out boot/iso_root/qwen_test.fbin

Real Qwen2.5-0.5B substitution:

    python tools/fbin-gen/hf_to_fbin.py \\
        --model-dir ~/models/Qwen2.5-0.5B \\
        --max-layers 1 \\
        --out boot/iso_root/qwen05b-1L.fbin
"""

import argparse
import json
import os

import numpy as np
from safetensors.numpy import save_file


def make_config(
    n_layers: int = 1,
    hidden: int = 64,
    n_heads: int = 4,
    # D.3.6: GQA back on (mirrors real Qwen2.5-0.5B's 14:2 ratio at
    # tiny scale). Pass --kv-heads 4 to fall back to the non-GQA
    # synthetic if needed for debugging.
    n_kv_heads: int = 2,
    intermediate: int = 128,
    vocab: int = 256,
    max_pos: int = 32,
    rope_theta: float = 10000.0,
) -> dict:
    """Mimic Qwen2.5's config.json with biases on q/k/v.

    Defaults match a *very* small model (sub-MB) so the output `.fbin`
    fits in the existing 64 MB current.img with comfortable margin.
    """
    head_dim = hidden // n_heads
    return {
        "architectures": ["FolkeringTestForCausalLM"],
        "hidden_size": hidden,
        "intermediate_size": intermediate,
        "num_attention_heads": n_heads,
        "num_key_value_heads": n_kv_heads,
        "num_hidden_layers": n_layers,
        "head_dim": head_dim,
        "vocab_size": vocab,
        "max_position_embeddings": max_pos,
        "rope_theta": rope_theta,
        "tie_word_embeddings": True,
        "torch_dtype": "float32",
    }


def make_weights(cfg: dict, seed: int = 42) -> dict:
    """Generate deterministic random tensors at every name a real
    Qwen2.5 checkpoint exposes. Reproducible (seeded) so CI builds
    are byte-identical on the same Python/numpy versions."""
    rng = np.random.default_rng(seed)
    H = cfg["hidden_size"]
    I = cfg["intermediate_size"]
    Hkv = cfg["head_dim"] * cfg["num_key_value_heads"]
    V = cfg["vocab_size"]
    L = cfg["num_hidden_layers"]

    def rand(*shape) -> np.ndarray:
        # Small magnitudes so the tiny model's logits stay finite
        # through one forward pass — useful when the inference runtime
        # eventually runs a smoke pass against this file.
        return rng.normal(loc=0.0, scale=0.02, size=shape).astype(np.float32)

    out = {
        "model.embed_tokens.weight":  rand(V, H),
        "model.norm.weight":          np.ones(H, dtype=np.float32),
    }
    for li in range(L):
        prefix = f"model.layers.{li}"
        out[f"{prefix}.input_layernorm.weight"]          = np.ones(H, dtype=np.float32)
        out[f"{prefix}.post_attention_layernorm.weight"] = np.ones(H, dtype=np.float32)
        out[f"{prefix}.self_attn.q_proj.weight"] = rand(H,   H)
        out[f"{prefix}.self_attn.k_proj.weight"] = rand(Hkv, H)
        out[f"{prefix}.self_attn.v_proj.weight"] = rand(Hkv, H)
        out[f"{prefix}.self_attn.o_proj.weight"] = rand(H,   H)
        # Qwen2.5 has biases on q/k/v but NOT on o. D.3.6 needs nonzero
        # values so the test fixture actually exercises the bias add
        # path; small magnitudes keep logits finite through one
        # forward pass.
        out[f"{prefix}.self_attn.q_proj.bias"] = rand(H)
        out[f"{prefix}.self_attn.k_proj.bias"] = rand(Hkv)
        out[f"{prefix}.self_attn.v_proj.bias"] = rand(Hkv)
        out[f"{prefix}.mlp.gate_proj.weight"] = rand(I, H)
        out[f"{prefix}.mlp.up_proj.weight"]   = rand(I, H)
        out[f"{prefix}.mlp.down_proj.weight"] = rand(H, I)
    # No lm_head when tied (HF convention).
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out_dir", help="Where to write config.json + model.safetensors")
    ap.add_argument("--layers", type=int, default=1)
    ap.add_argument("--hidden", type=int, default=64)
    ap.add_argument("--heads", type=int, default=4)
    ap.add_argument("--kv-heads", type=int, default=2)
    ap.add_argument("--inter", type=int, default=128)
    ap.add_argument("--vocab", type=int, default=256)
    ap.add_argument("--max-pos", type=int, default=32)
    ap.add_argument("--seed", type=int, default=42)
    args = ap.parse_args()

    cfg = make_config(
        n_layers=args.layers,
        hidden=args.hidden,
        n_heads=args.heads,
        n_kv_heads=args.kv_heads,
        intermediate=args.inter,
        vocab=args.vocab,
        max_pos=args.max_pos,
    )
    weights = make_weights(cfg, seed=args.seed)

    os.makedirs(args.out_dir, exist_ok=True)
    cfg_path = os.path.join(args.out_dir, "config.json")
    st_path  = os.path.join(args.out_dir, "model.safetensors")
    with open(cfg_path, "w") as f:
        json.dump(cfg, f, indent=2)
    save_file(weights, st_path)

    n_params = sum(arr.size for arr in weights.values())
    n_bytes  = sum(arr.nbytes for arr in weights.values())
    print(
        f"[make_test_model] wrote {args.out_dir} "
        f"({len(weights)} tensors, {n_params:,} params, "
        f"~{n_bytes / 1024:.1f} KiB)"
    )


if __name__ == "__main__":
    main()
