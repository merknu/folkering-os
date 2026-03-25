"""
Compare Folkering OS Layer 1 PRE-Wo attn_out with PyTorch reference.
Captures the INPUT to o_proj (pre-Wo), not the output (post-Wo).

Usage: py -3.12 tools/compare_pre_wo.py
"""
import struct
import numpy as np
import os

SECTOR_SIZE = 512
DISK_PATH = "boot/virtio-data.img"

def read_rust_dump():
    """Read the tensor dump from VirtIO disk mailbox sectors 1-7."""
    with open(DISK_PATH, "rb") as f:
        f.seek(1 * SECTOR_SIZE)
        hdr = f.read(SECTOR_SIZE)
        magic = hdr[0:4]
        assert magic == b"TDMP", f"Bad magic: {magic}"
        n = struct.unpack_from("<I", hdr, 8)[0]
        n_dumped = struct.unpack_from("<I", hdr, 12)[0]

        floats = []
        floats_per_sector = SECTOR_SIZE // 4
        data_sectors = (n_dumped + floats_per_sector - 1) // floats_per_sector
        for s in range(data_sectors):
            f.seek((2 + s) * SECTOR_SIZE)
            sector_data = f.read(SECTOR_SIZE)
            start = s * floats_per_sector
            end = min((s + 1) * floats_per_sector, n_dumped)
            for i in range(start, end):
                offset = (i - start) * 4
                val = struct.unpack_from("<f", sector_data, offset)[0]
                floats.append(val)
        return np.array(floats, dtype=np.float32)

def get_python_pre_wo(prompt, position=0):
    """Get the PRE-Wo attention output from PyTorch (input to o_proj)."""
    import torch
    from transformers import AutoTokenizer, AutoModelForCausalLM

    tokenizer = AutoTokenizer.from_pretrained("HuggingFaceTB/SmolLM2-135M")
    model = AutoModelForCausalLM.from_pretrained("HuggingFaceTB/SmolLM2-135M", dtype=torch.float32)
    model.eval()

    inputs = tokenizer(prompt, return_tensors="pt")
    input_ids = inputs["input_ids"]
    n_tokens = input_ids.shape[1]
    print(f"Python tokens ({n_tokens}): {input_ids[0].tolist()[:10]}...")

    # Capture the INPUT to o_proj (= pre-Wo attention output)
    captured = {}
    def hook_fn(module, input, output):
        # input is a tuple, first element is the attention output before o_proj
        captured["pre_wo"] = input[0].detach()

    handle = model.model.layers[1].self_attn.o_proj.register_forward_hook(hook_fn)

    with torch.no_grad():
        outputs = model(**inputs)

    handle.remove()

    pre_wo = captured["pre_wo"]  # [1, seq_len, 576]
    print(f"Pre-Wo shape: {pre_wo.shape}")

    pos_data = pre_wo[0, position, :].numpy()
    print(f"Pos {position}: min={pos_data.min():.6f}, max={pos_data.max():.6f}, "
          f"mean={pos_data.mean():.6f}, argmax=[{pos_data.argmax()}]={pos_data[pos_data.argmax()]:.6f}")

    return pos_data

def analyze(rust, python, n_heads=9, n_kv_heads=3, head_dim=64):
    """Detailed comparison."""
    kv_group_size = n_heads // n_kv_heads

    diff = np.abs(rust - python)
    cos = np.dot(rust, python) / (np.linalg.norm(rust) * np.linalg.norm(python))

    print(f"\n{'='*70}")
    print(f"Overall comparison (576 elements)")
    print(f"{'='*70}")
    print(f"  MAE:        {diff.mean():.6f}")
    print(f"  MaxErr:     {diff.max():.6f}")
    print(f"  Cosine sim: {cos:.6f}")
    print(f"  Rust  range: [{rust.min():.6f}, {rust.max():.6f}]")
    print(f"  Python range: [{python.min():.6f}, {python.max():.6f}]")

    # Check GQA repetition in Python too
    print(f"\n{'='*70}")
    print(f"GQA Repetition Check (should repeat within groups at pos=0)")
    print(f"{'='*70}")
    for group_name, data, label in [("Rust", rust, "R"), ("Python", python, "P")]:
        print(f"\n{group_name}:")
        for kv_h in range(n_kv_heads):
            heads_in_group = range(kv_h * kv_group_size, (kv_h + 1) * kv_group_size)
            first_head = list(heads_in_group)[0]
            for h in heads_in_group:
                if h == first_head:
                    continue
                h0 = data[first_head * head_dim:(first_head + 1) * head_dim]
                hx = data[h * head_dim:(h + 1) * head_dim]
                max_diff = np.abs(h0 - hx).max()
                print(f"  KV{kv_h}: Head {first_head} vs Head {h}: max_diff={max_diff:.8f}")

    # Per-head comparison
    print(f"\n{'='*70}")
    print(f"Per-Head Comparison")
    print(f"{'='*70}")
    print(f"{'Head':>4} {'KV_H':>4} {'MAE':>10} {'MaxErr':>10} {'CosSim':>10}")
    print("-" * 45)
    for h in range(n_heads):
        kv_h = h // kv_group_size
        s, e = h * head_dim, (h + 1) * head_dim
        r, p = rust[s:e], python[s:e]
        mae = np.abs(r - p).mean()
        maxe = np.abs(r - p).max()
        cos_h = np.dot(r, p) / (np.linalg.norm(r) * np.linalg.norm(p)) if np.linalg.norm(r) > 0 else 0
        print(f"{h:4d} {kv_h:4d} {mae:10.6f} {maxe:10.6f} {cos_h:10.6f}")

    # Element-by-element for head 0 (first 64)
    print(f"\n{'='*70}")
    print(f"Head 0 element-by-element (first unique head in KV group 0)")
    print(f"{'='*70}")
    print(f"{'idx':>4} {'Rust':>12} {'Python':>12} {'Diff':>12} {'RelErr%':>10}")
    print("-" * 55)
    for i in range(head_dim):
        r, p = rust[i], python[i]
        d = r - p
        rel = abs(d / p * 100) if abs(p) > 1e-8 else 0
        marker = " ***" if abs(d) > 0.05 else ""
        print(f"{i:4d} {r:12.6f} {p:12.6f} {d:12.6f} {rel:9.2f}%{marker}")

    # KV head 1 (head 3, elements 192-255)
    print(f"\n{'='*70}")
    print(f"Head 3 element-by-element (first unique head in KV group 1)")
    print(f"{'='*70}")
    print(f"{'idx':>4} {'Rust':>12} {'Python':>12} {'Diff':>12}")
    print("-" * 45)
    for i in range(head_dim):
        gi = 192 + i  # head 3 starts at 192
        r, p = rust[gi], python[gi]
        d = r - p
        marker = " ***" if abs(d) > 0.05 else ""
        print(f"{gi:4d} {r:12.6f} {p:12.6f} {d:12.6f}{marker}")

def main():
    os.chdir(r"C:\Users\merkn\folkering\folkering-os")

    print("=" * 70)
    print("STEP 1: Read Rust pos=0 Layer 1 attn_out (pre-Wo)")
    print("=" * 70)
    rust = read_rust_dump()
    print(f"  {len(rust)} floats, range [{rust.min():.6f}, {rust.max():.6f}]")

    print(f"\n{'='*70}")
    print("STEP 2: Python reference pos=0 Layer 1 pre-Wo (input to o_proj)")
    print("=" * 70)
    prompt = "<|im_start|>system\nYou are Folkering OS, a helpful AI assistant.\n<|im_end|>\n<|im_start|>user\nhi\n<|im_end|>\n<|im_start|>assistant\n"
    python = get_python_pre_wo(prompt, position=0)

    print(f"\n{'='*70}")
    print("STEP 3: Direct comparison")
    print("=" * 70)
    analyze(rust, python)

if __name__ == "__main__":
    main()
