"""
Full Layer 0 forward pass using Q4_0 weights from GGUF.
Compares hidden state after layer 0 with Rust.
"""
import struct, numpy as np, os
os.chdir(r"C:\Users\merkn\folkering\folkering-os")

GGUF_PATH = "boot/model.gguf"

def read_gguf_header(f):
    magic = f.read(4)
    assert magic == b"GGUF"
    version = struct.unpack("<I", f.read(4))[0]
    n_tensors = struct.unpack("<Q", f.read(8))[0]
    n_metadata = struct.unpack("<Q", f.read(8))[0]
    rms_eps = 1e-5
    for _ in range(n_metadata):
        key_len = struct.unpack("<Q", f.read(8))[0]
        key = f.read(key_len).decode("utf-8", errors="replace")
        val_type = struct.unpack("<I", f.read(4))[0]
        val = read_val(f, val_type)
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

def read_val(f, t):
    if t == 0: return struct.unpack("<B", f.read(1))[0]
    elif t == 1: return struct.unpack("<b", f.read(1))[0]
    elif t == 2: return struct.unpack("<H", f.read(2))[0]
    elif t == 3: return struct.unpack("<h", f.read(2))[0]
    elif t == 4: return struct.unpack("<I", f.read(4))[0]
    elif t == 5: return struct.unpack("<i", f.read(4))[0]
    elif t == 6: return struct.unpack("<f", f.read(4))[0]
    elif t == 7: return struct.unpack("<B", f.read(1))[0]
    elif t == 8:
        slen = struct.unpack("<Q", f.read(8))[0]
        return f.read(slen).decode("utf-8", errors="replace")
    elif t == 9:
        et = struct.unpack("<I", f.read(4))[0]
        n = struct.unpack("<Q", f.read(8))[0]
        return [read_val(f, et) for _ in range(n)]
    elif t == 10: return struct.unpack("<Q", f.read(8))[0]
    elif t == 11: return struct.unpack("<q", f.read(8))[0]
    elif t == 12: return struct.unpack("<d", f.read(8))[0]

def read_f32(f, ds, info):
    n = 1
    for d in info['dims']: n *= d
    f.seek(ds + info['offset'])
    return np.frombuffer(f.read(n * 4), dtype=np.float32).copy()

def dq_q8(f, ds, info):
    nc, nr = info['dims'][0], info['dims'][1] if len(info['dims']) > 1 else 1
    bpr = nc // 32
    f.seek(ds + info['offset'])
    raw = f.read(nr * bpr * 34)
    r = np.zeros((nr, nc), dtype=np.float32)
    for row in range(nr):
        for b in range(bpr):
            off = (row * bpr + b) * 34
            s = struct.unpack_from("<e", raw, off)[0]
            for i in range(32):
                r[row, b*32+i] = s * struct.unpack_from("<b", raw, off+2+i)[0]
    return r

def dq_q4(f, ds, info):
    nc, nr = info['dims'][0], info['dims'][1] if len(info['dims']) > 1 else 1
    bpr = nc // 32
    f.seek(ds + info['offset'])
    raw = f.read(nr * bpr * 18)
    r = np.zeros((nr, nc), dtype=np.float32)
    for row in range(nr):
        for b in range(bpr):
            off = (row * bpr + b) * 18
            s = struct.unpack_from("<e", raw, off)[0]
            for i in range(32):
                bv = raw[off + 2 + i//2]
                nib = (bv & 0xF) if i%2==0 else ((bv>>4) & 0xF)
                r[row, b*32+i] = s * (nib - 8)
    return r

def rmsnorm(x, w, eps):
    return (x / np.sqrt(np.mean(x**2) + eps)) * w

with open(GGUF_PATH, "rb") as f:
    T, DS, EPS = read_gguf_header(f)

with open(GGUF_PATH, "rb") as f:
    x = dq_q8(f, DS, T['token_embd.weight'])[1].copy()
    print(f"EMB first4: {x[:4]}")

    # Layer 0 attention
    xb = rmsnorm(x, read_f32(f, DS, T['blk.0.attn_norm.weight']), EPS)
    print(f"XB first4: {xb[:4]}")

    q = dq_q4(f, DS, T['blk.0.attn_q.weight']) @ xb
    k = dq_q4(f, DS, T['blk.0.attn_k.weight']) @ xb
    v = dq_q4(f, DS, T['blk.0.attn_v.weight']) @ xb
    print(f"Q first4: {q[:4]}")
    print(f"V first4: {v[:4]}")

    # RoPE at pos=0: cos(0)=1, sin(0)=0 -> no change
    # Attention: seq_len=1, all weights=1.0, attn_out = V repeated per GQA group
    attn = np.zeros(576, dtype=np.float32)
    for h in range(9):
        kv_h = h // 3
        attn[h*64:(h+1)*64] = v[kv_h*64:(kv_h+1)*64]

    wo = dq_q4(f, DS, T['blk.0.attn_output.weight']) @ attn
    x += wo
    print(f"After attn+res first4: {x[:4]}")

    # FFN
    xb2 = rmsnorm(x, read_f32(f, DS, T['blk.0.ffn_norm.weight']), EPS)
    gate = dq_q4(f, DS, T['blk.0.ffn_gate.weight']) @ xb2
    gate = gate * (1.0 / (1.0 + np.exp(-gate.clip(-88, 88))))  # SiLU with clamp
    up = dq_q4(f, DS, T['blk.0.ffn_up.weight']) @ xb2
    down = dq_q4(f, DS, T['blk.0.ffn_down.weight']) @ (gate * up)
    x += down

    print(f"\n=== Layer 0 output ===")
    print(f"first16: {x[:16]}")
    print(f"min={x.min():.6f} max={x.max():.6f} mean={x.mean():.6f}")

print("\nDone!")
