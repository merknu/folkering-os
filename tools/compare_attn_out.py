"""
Compare Folkering OS Layer 1 attn_out with PyTorch reference.
Reads the VirtIO disk mailbox (sectors 1-7) and compares with
HuggingFace transformers model output.

Usage: py -3.12 tools/compare_attn_out.py
"""
import struct
import numpy as np

SECTOR_SIZE = 512
DISK_PATH = "boot/virtio-data.img"

def read_rust_dump():
    """Read the tensor dump from VirtIO disk mailbox sectors 1-7."""
    with open(DISK_PATH, "rb") as f:
        # Read header (sector 1)
        f.seek(1 * SECTOR_SIZE)
        hdr = f.read(SECTOR_SIZE)

        magic = hdr[0:4]
        assert magic == b"TDMP", f"Bad magic: {magic}"

        n = struct.unpack_from("<I", hdr, 8)[0]
        n_dumped = struct.unpack_from("<I", hdr, 12)[0]
        shape0 = struct.unpack_from("<I", hdr, 16)[0]
        shape1 = struct.unpack_from("<I", hdr, 20)[0]
        argmax_idx = struct.unpack_from("<I", hdr, 24)[0]
        min_val = struct.unpack_from("<f", hdr, 32)[0]
        max_val = struct.unpack_from("<f", hdr, 36)[0]
        mean_val = struct.unpack_from("<f", hdr, 40)[0]
        name_bytes = hdr[48:112].split(b"\x00")[0].decode("ascii", errors="replace")

        print(f"Rust dump: name={name_bytes}, shape=[{shape0},{shape1}], n={n}, n_dumped={n_dumped}")
        print(f"  argmax=[{argmax_idx}], min={min_val:.6f}, max={max_val:.6f}, mean={mean_val:.6f}")

        # Read data sectors (2-6)
        floats = []
        floats_per_sector = SECTOR_SIZE // 4  # 128
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

def get_python_reference(prompt, position=-1):
    """Run PyTorch reference model and capture Layer 1 self_attn output."""
    import torch
    from transformers import AutoTokenizer, AutoModelForCausalLM

    tokenizer = AutoTokenizer.from_pretrained("HuggingFaceTB/SmolLM2-135M")
    model = AutoModelForCausalLM.from_pretrained("HuggingFaceTB/SmolLM2-135M", torch_dtype=torch.float32)
    model.eval()

    inputs = tokenizer(prompt, return_tensors="pt")
    input_ids = inputs["input_ids"]
    print(f"\nPython tokens ({input_ids.shape[1]}): {input_ids[0].tolist()}")

    # Capture Layer 1 self_attn output via hook
    captured = {}
    def hook_fn(module, input, output):
        # self_attn returns (attn_output, attn_weights, past_key_value)
        captured["attn_out"] = output[0].detach()

    handle = model.model.layers[1].self_attn.register_forward_hook(hook_fn)

    with torch.no_grad():
        outputs = model(**inputs)

    handle.remove()

    attn_out = captured["attn_out"]  # [1, seq_len, 576]
    print(f"Python Layer 1 self_attn shape: {attn_out.shape}")

    # Extract specific position
    if position == -1:
        position = attn_out.shape[1] - 1

    pos_data = attn_out[0, position, :].numpy()
    print(f"Position {position} stats: min={pos_data.min():.6f}, max={pos_data.max():.6f}, "
          f"mean={pos_data.mean():.6f}, std={pos_data.std():.6f}, "
          f"argmax=[{pos_data.argmax()}]={pos_data[pos_data.argmax()]:.6f}")

    return attn_out[0].numpy(), position

def analyze_gqa_pattern(rust_data, python_data, n_heads=9, n_kv_heads=3, head_dim=64):
    """Analyze per-head divergence to find GQA-related patterns."""
    kv_group_size = n_heads // n_kv_heads  # 3

    print(f"\n{'='*70}")
    print(f"GQA Pattern Analysis: {n_heads} Q-heads, {n_kv_heads} KV-heads, group_size={kv_group_size}")
    print(f"{'='*70}")

    total_diff = np.abs(rust_data - python_data)
    print(f"\nOverall: MAE={total_diff.mean():.6f}, MaxErr={total_diff.max():.6f}")
    print(f"  Rust  range: [{rust_data.min():.6f}, {rust_data.max():.6f}]")
    print(f"  Python range: [{python_data.min():.6f}, {python_data.max():.6f}]")

    # Cosine similarity
    dot = np.dot(rust_data, python_data)
    norm_r = np.linalg.norm(rust_data)
    norm_p = np.linalg.norm(python_data)
    cos_sim = dot / (norm_r * norm_p) if norm_r > 0 and norm_p > 0 else 0
    print(f"  Cosine similarity: {cos_sim:.6f}")

    print(f"\n{'='*70}")
    print(f"Per-Head Analysis (head_dim={head_dim})")
    print(f"{'='*70}")
    print(f"{'Head':>4} {'KV_Head':>7} {'MAE':>10} {'MaxErr':>10} {'CosSim':>10} {'Rust_norm':>10} {'Py_norm':>10}")
    print("-" * 70)

    for h in range(n_heads):
        kv_h = h // kv_group_size
        start = h * head_dim
        end = (h + 1) * head_dim

        r_head = rust_data[start:end]
        p_head = python_data[start:end]

        head_diff = np.abs(r_head - p_head)
        mae = head_diff.mean()
        max_err = head_diff.max()

        dot_h = np.dot(r_head, p_head)
        norm_r_h = np.linalg.norm(r_head)
        norm_p_h = np.linalg.norm(p_head)
        cos_h = dot_h / (norm_r_h * norm_p_h) if norm_r_h > 0 and norm_p_h > 0 else 0

        print(f"{h:4d} {kv_h:7d} {mae:10.6f} {max_err:10.6f} {cos_h:10.6f} {norm_r_h:10.4f} {norm_p_h:10.4f}")

    # Check if KV groups show systematic patterns
    print(f"\n{'='*70}")
    print(f"Per KV-Head Group Analysis")
    print(f"{'='*70}")

    for kv_h in range(n_kv_heads):
        group_heads = range(kv_h * kv_group_size, (kv_h + 1) * kv_group_size)
        group_start = kv_h * kv_group_size * head_dim
        group_end = (kv_h + 1) * kv_group_size * head_dim

        r_group = rust_data[group_start:group_end]
        p_group = python_data[group_start:group_end]

        group_diff = np.abs(r_group - p_group)
        mae = group_diff.mean()
        max_err = group_diff.max()

        print(f"KV Head {kv_h} (Q heads {list(group_heads)}): MAE={mae:.6f}, MaxErr={max_err:.6f}")

    # Element-by-element comparison for first 64 elements
    print(f"\n{'='*70}")
    print(f"First 64 elements (Head 0, KV Head 0):")
    print(f"{'='*70}")
    print(f"{'idx':>4} {'Rust':>12} {'Python':>12} {'Diff':>12} {'RelErr%':>10}")
    print("-" * 55)
    for i in range(64):
        r = rust_data[i]
        p = python_data[i]
        d = r - p
        rel = abs(d / p * 100) if abs(p) > 1e-8 else 0
        marker = " ***" if abs(d) > 0.1 else ""
        print(f"{i:4d} {r:12.6f} {p:12.6f} {d:12.6f} {rel:9.2f}%{marker}")

    # Check for shifted/permuted heads
    print(f"\n{'='*70}")
    print(f"Cross-correlation check (is Rust head X actually Python head Y?)")
    print(f"{'='*70}")
    print(f"{'R\\P':>4}", end="")
    for p_h in range(n_heads):
        print(f" {p_h:7d}", end="")
    print()

    for r_h in range(n_heads):
        print(f"{r_h:4d}", end="")
        r_head = rust_data[r_h * head_dim:(r_h + 1) * head_dim]
        for p_h in range(n_heads):
            p_head = python_data[p_h * head_dim:(p_h + 1) * head_dim]
            dot_h = np.dot(r_head, p_head)
            norm_r_h = np.linalg.norm(r_head)
            norm_p_h = np.linalg.norm(p_head)
            cos = dot_h / (norm_r_h * norm_p_h) if norm_r_h > 0 and norm_p_h > 0 else 0
            print(f" {cos:7.3f}", end="")
        print()

def main():
    import os
    os.chdir(r"C:\Users\merkn\folkering\folkering-os")

    # 1. Read Rust dump from disk
    print("=" * 70)
    print("STEP 1: Read Rust disk dump")
    print("=" * 70)
    rust_data = read_rust_dump()
    print(f"Read {len(rust_data)} floats from disk")

    # 2. Get Python reference
    print(f"\n{'='*70}")
    print("STEP 2: Python reference (last prefill position)")
    print("=" * 70)

    prompt = "<|im_start|>system\nYou are Folkering OS, a helpful AI assistant.\n<|im_end|>\n<|im_start|>user\nhi\n<|im_end|>\n<|im_start|>assistant\n"

    all_positions, last_pos = get_python_reference(prompt, position=-1)
    python_last = all_positions[last_pos]

    # Also get position 0 for comparison
    python_pos0 = all_positions[0]
    print(f"\nPosition 0 first16: {python_pos0[:16]}")

    # 3. Compare last positions
    print(f"\n{'='*70}")
    print(f"STEP 3: Compare Rust pos29 (disk) vs Python pos{last_pos} (last)")
    print("=" * 70)
    analyze_gqa_pattern(rust_data, python_last)

    # 4. Also compare first 16 from Rust disk with Python pos0
    # (Rust disk has pos29, not pos0 — this is just for reference)

if __name__ == "__main__":
    main()
