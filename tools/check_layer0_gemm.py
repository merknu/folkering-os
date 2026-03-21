"""
Manual computation of Layer 0 RMSNorm + Wq GEMM using Q4_0/Q8_0 weights from GGUF.
Compares with Rust serial output to isolate GEMM bugs.

Pipeline: embedding(Q8_0) → RMSNorm(f32 weights) → GEMM(f32 × Q4_0)
"""
import struct
import numpy as np
import os

os.chdir(r"C:\Users\merkn\folkering\folkering-os")
GGUF_PATH = "boot/model.gguf"

def read_gguf_header(f):
    """Parse GGUF header, return tensor info dict and data_start offset."""
    magic = f.read(4)
    assert magic == b"GGUF"
    version = struct.unpack("<I", f.read(4))[0]
    n_tensors = struct.unpack("<Q", f.read(8))[0]
    n_metadata = struct.unpack("<Q", f.read(8))[0]

    # Read metadata (extract rms_norm_eps)
    rms_eps = 1e-5
    for _ in range(n_metadata):
        key_len = struct.unpack("<Q", f.read(8))[0]
        key = f.read(key_len).decode("utf-8", errors="replace")
        val_type = struct.unpack("<I", f.read(4))[0]
        val = read_gguf_value(f, val_type)
        if 'rms_norm_eps' in key:
            rms_eps = val

    tensors = {}
    for _ in range(n_tensors):
        name_len = struct.unpack("<Q", f.read(8))[0]
        name = f.read(name_len).decode("utf-8")
        n_dims = struct.unpack("<I", f.read(4))[0]
        dims = [struct.unpack("<Q", f.read(8))[0] for _ in range(n_dims)]
        dtype = struct.unpack("<I", f.read(4))[0]
        offset = struct.unpack("<Q", f.read(8))[0]
        tensors[name] = {"dims": dims, "dtype": dtype, "offset": offset}

    alignment = 32
    data_start = (f.tell() + alignment - 1) // alignment * alignment
    return tensors, data_start, rms_eps

def read_gguf_value(f, val_type):
    if val_type == 0: return struct.unpack("<B", f.read(1))[0]
    elif val_type == 1: return struct.unpack("<b", f.read(1))[0]
    elif val_type == 2: return struct.unpack("<H", f.read(2))[0]
    elif val_type == 3: return struct.unpack("<h", f.read(2))[0]
    elif val_type == 4: return struct.unpack("<I", f.read(4))[0]
    elif val_type == 5: return struct.unpack("<i", f.read(4))[0]
    elif val_type == 6: return struct.unpack("<f", f.read(4))[0]
    elif val_type == 7: return struct.unpack("<B", f.read(1))[0]
    elif val_type == 8:
        slen = struct.unpack("<Q", f.read(8))[0]
        return f.read(slen).decode("utf-8", errors="replace")
    elif val_type == 9:
        elem_type = struct.unpack("<I", f.read(4))[0]
        n = struct.unpack("<Q", f.read(8))[0]
        return [read_gguf_value(f, elem_type) for _ in range(n)]
    elif val_type == 10: return struct.unpack("<Q", f.read(8))[0]
    elif val_type == 11: return struct.unpack("<q", f.read(8))[0]
    elif val_type == 12: return struct.unpack("<d", f.read(8))[0]
    return None

def read_f32_tensor(f, data_start, info):
    """Read an F32 tensor."""
    n = 1
    for d in info['dims']:
        n *= d
    f.seek(data_start + info['offset'])
    data = np.frombuffer(f.read(n * 4), dtype=np.float32).copy()
    return data

def dequant_q8_0(f, data_start, info):
    """Dequantize a Q8_0 tensor to f32."""
    n_cols = info['dims'][0]
    n_rows = info['dims'][1] if len(info['dims']) > 1 else 1
    block_size = 32
    block_bytes = 34
    blocks_per_row = n_cols // block_size

    f.seek(data_start + info['offset'])
    raw = f.read(n_rows * blocks_per_row * block_bytes)

    result = np.zeros((n_rows, n_cols), dtype=np.float32)
    for row in range(n_rows):
        for b in range(blocks_per_row):
            off = (row * blocks_per_row + b) * block_bytes
            scale = struct.unpack_from("<e", raw, off)[0]
            for i in range(block_size):
                qval = struct.unpack_from("<b", raw, off + 2 + i)[0]
                result[row, b * block_size + i] = scale * qval
    return result

def dequant_q4_0(f, data_start, info):
    """Dequantize a Q4_0 tensor to f32. Returns [n_rows, n_cols]."""
    n_cols = info['dims'][0]
    n_rows = info['dims'][1] if len(info['dims']) > 1 else 1
    block_size = 32
    block_bytes = 18
    blocks_per_row = n_cols // block_size

    f.seek(data_start + info['offset'])
    raw = f.read(n_rows * blocks_per_row * block_bytes)

    result = np.zeros((n_rows, n_cols), dtype=np.float32)
    for row in range(n_rows):
        for b in range(blocks_per_row):
            off = (row * blocks_per_row + b) * block_bytes
            scale = struct.unpack_from("<e", raw, off)[0]
            for i in range(block_size):
                byte_idx = off + 2 + i // 2
                byte_val = raw[byte_idx]
                if i % 2 == 0:
                    nibble = byte_val & 0x0F
                else:
                    nibble = (byte_val >> 4) & 0x0F
                result[row, b * block_size + i] = scale * (nibble - 8)
    return result

def rmsnorm(x, weight, eps):
    """RMSNorm: x * weight / sqrt(mean(x^2) + eps)"""
    rms = np.sqrt(np.mean(x ** 2) + eps)
    return (x / rms) * weight

def main():
    with open(GGUF_PATH, "rb") as f:
        tensors, data_start, rms_eps = read_gguf_header(f)

    print(f"rms_norm_eps = {rms_eps}")
    print(f"data_start = {data_start}")

    with open(GGUF_PATH, "rb") as f:
        # 1. Get BOS embedding
        embd_info = tensors["token_embd.weight"]
        embd = dequant_q8_0(f, data_start, embd_info)
        bos_emb = embd[1]  # token 1 = BOS
        print(f"\n=== BOS Embedding ===")
        print(f"first16: {bos_emb[:16]}")

        # 2. Get Layer 0 attn_norm weights (F32)
        norm_info = tensors["blk.0.attn_norm.weight"]
        norm_w = read_f32_tensor(f, data_start, norm_info)
        print(f"\n=== Layer 0 attn_norm ===")
        print(f"shape: {norm_w.shape}, first8: {norm_w[:8]}")

        # 3. RMSNorm
        xb = rmsnorm(bos_emb, norm_w, rms_eps)
        print(f"\n=== After RMSNorm (xb) ===")
        print(f"first16: {xb[:16]}")

        # 4. Get Layer 0 Wq weights (Q4_0)
        wq_info = tensors["blk.0.attn_q.weight"]
        print(f"\n=== Layer 0 Wq ===")
        print(f"dims: {wq_info['dims']}, dtype: {wq_info['dtype']}")
        wq = dequant_q4_0(f, data_start, wq_info)
        print(f"Wq shape: {wq.shape}")
        print(f"Wq[0,:8]: {wq[0,:8]}")
        print(f"Wq[1,:8]: {wq[1,:8]}")

        # 5. GEMM: q = xb × Wq^T (output[j] = sum_i(xb[i] * Wq[j][i]))
        # In GGUF, Wq has dims [576, 576] stored as [n_cols=576, n_rows=576]
        # Row j of Wq corresponds to output element j
        # q[j] = dot(xb, Wq[j])
        q = wq @ xb  # [576, 576] @ [576] = [576]
        print(f"\n=== Q projection (xb × Wq) ===")
        print(f"first16: {q[:16]}")
        print(f"min: {q.min():.6f}, max: {q.max():.6f}")

if __name__ == "__main__":
    main()
