"""
Verify the Q8_0-quantized GEMM path by replicating EXACTLY what Rust does:
1. Quantize f32 activations to Q8_0 (f16 scale, round-to-nearest)
2. Integer dot product: Q4_0_weight × Q8_0_activation → i32 → f32
3. Full 30-layer forward pass
4. Check BOS logits argmax

If this gives argmax=28 → Rust code bug
If argmax≠28 → Q8_0 quantization approach differs from llama.cpp
"""
import struct, numpy as np, os, time
os.chdir(r"C:\Users\merkn\folkering\folkering-os")

GGUF_PATH = "boot/model.gguf"

# GGUF reading functions (same as before)
def read_gguf_header(f):
    magic = f.read(4); assert magic == b"GGUF"
    struct.unpack("<I", f.read(4)); nt = struct.unpack("<Q", f.read(8))[0]; nm = struct.unpack("<Q", f.read(8))[0]
    meta = {}
    for _ in range(nm):
        kl = struct.unpack("<Q", f.read(8))[0]; key = f.read(kl).decode("utf-8", errors="replace")
        vt = struct.unpack("<I", f.read(4))[0]; val = read_val(f, vt); meta[key] = val
    tensors = {}
    for _ in range(nt):
        nl = struct.unpack("<Q", f.read(8))[0]; name = f.read(nl).decode("utf-8")
        nd = struct.unpack("<I", f.read(4))[0]; dims = [struct.unpack("<Q", f.read(8))[0] for _ in range(nd)]
        dtype = struct.unpack("<I", f.read(4))[0]; offset = struct.unpack("<Q", f.read(8))[0]
        tensors[name] = {"dims": dims, "dtype": dtype, "offset": offset}
    ds = (f.tell() + 31) // 32 * 32
    return tensors, ds, meta

def read_val(f, t):
    if t in (0,7): return struct.unpack("<B", f.read(1))[0]
    elif t == 1: return struct.unpack("<b", f.read(1))[0]
    elif t in (2,3): f.read(2); return 0
    elif t == 4: return struct.unpack("<I", f.read(4))[0]
    elif t == 5: return struct.unpack("<i", f.read(4))[0]
    elif t == 6: return struct.unpack("<f", f.read(4))[0]
    elif t == 8:
        sl = struct.unpack("<Q", f.read(8))[0]; return f.read(sl).decode("utf-8", errors="replace")
    elif t == 9:
        et = struct.unpack("<I", f.read(4))[0]; n = struct.unpack("<Q", f.read(8))[0]
        return [read_val(f, et) for _ in range(n)]
    elif t in (10,11): return struct.unpack("<Q", f.read(8))[0]
    elif t == 12: return struct.unpack("<d", f.read(8))[0]
    return None

def dq_q8_vec(f, ds, info):
    nc, nr = info['dims'][0], info['dims'][1] if len(info['dims']) > 1 else 1
    bpr = nc // 32; total = nr * bpr
    f.seek(ds + info['offset']); raw = np.frombuffer(f.read(total * 34), dtype=np.uint8).reshape(total, 34)
    scales = raw[:, :2].copy().view(np.float16).astype(np.float32).flatten()
    qvals = raw[:, 2:].view(np.int8).astype(np.float32)
    qvals *= scales[:, np.newaxis]
    return qvals.reshape(nr, nc)

def read_f32(f, ds, info):
    n = 1
    for d in info['dims']: n *= d
    f.seek(ds + info['offset']); return np.frombuffer(f.read(n * 4), dtype=np.float32).copy()

def read_q4_raw(f, ds, info):
    """Read Q4_0 tensor as raw bytes (for integer dot product)."""
    nc, nr = info['dims'][0], info['dims'][1] if len(info['dims']) > 1 else 1
    bpr = nc // 32
    total_bytes = nr * bpr * 18
    f.seek(ds + info['offset'])
    return f.read(total_bytes), nr, nc

# Q8_0 quantization matching Rust (round-to-nearest, f16 scale)
def quantize_f32_to_q8_0(x):
    """Quantize f32 vector to Q8_0 blocks. Returns raw bytes."""
    n = len(x)
    n_blocks = (n + 31) // 32
    result = bytearray(n_blocks * 34)
    for b in range(n_blocks):
        start = b * 32
        end = min(start + 32, n)
        block = x[start:end]

        amax = np.max(np.abs(block))
        scale = amax / 127.0 if amax > 0 else 0.0
        # Convert to f16 and back (matching Rust's f16 round-trip)
        scale_f16 = np.float16(scale)
        scale_f32 = float(scale_f16)
        inv_scale = 1.0 / scale_f32 if scale_f32 > 0 else 0.0

        # Store f16 scale
        struct.pack_into("<e", result, b * 34, float(scale_f16))

        # Quantize with round-to-nearest
        for i in range(32):
            if start + i < end:
                v = float(x[start + i]) * inv_scale
                q = int(round(v))
                q = max(-128, min(127, q))
                result[b * 34 + 2 + i] = q & 0xFF  # store as unsigned byte
            else:
                result[b * 34 + 2 + i] = 0
    return bytes(result)

# Integer dot product Q4_0 × Q8_0 (matching Rust's dot_q4_0_q8_0_block)
def dot_q4_q8_block(q4_bytes, q8_bytes):
    """One block of 32 values: Q4_0 × Q8_0 → f32."""
    q4_scale = struct.unpack_from("<e", q4_bytes, 0)[0]
    q8_scale = struct.unpack_from("<e", q8_bytes, 0)[0]

    sum_prod = 0
    sum_q8 = 0
    for i in range(16):
        byte = q4_bytes[2 + i]
        q4_lo = byte & 0x0F
        q4_hi = (byte >> 4) & 0x0F
        q8_lo = struct.unpack_from("b", q8_bytes, 2 + i * 2)[0]  # signed i8
        q8_hi = struct.unpack_from("b", q8_bytes, 2 + i * 2 + 1)[0]
        sum_prod += q4_lo * q8_lo + q4_hi * q8_hi
        sum_q8 += q8_lo + q8_hi

    corrected = sum_prod - 8 * sum_q8
    return corrected * q4_scale * q8_scale

def gemm_q4_q8(act_q8, weight_q4_raw, k, n):
    """Matrix-vector multiply: Q4_0_weight[n×k] × Q8_0_activation[k] → f32[n]."""
    n_blocks = k // 32
    q4_block_bytes = 18
    q8_block_bytes = 34
    q4_row_bytes = n_blocks * q4_block_bytes
    output = np.zeros(n, dtype=np.float32)

    for col in range(n):
        total = 0.0
        for blk in range(n_blocks):
            q4_off = col * q4_row_bytes + blk * q4_block_bytes
            q8_off = blk * q8_block_bytes
            total += dot_q4_q8_block(
                weight_q4_raw[q4_off:q4_off + q4_block_bytes],
                act_q8[q8_off:q8_off + q8_block_bytes]
            )
        output[col] = total
    return output

def rmsnorm(x, w, eps):
    return (x / np.sqrt(np.mean(x.astype(np.float64)**2) + eps).astype(np.float32)) * w

def silu(x):
    return x / (1.0 + np.exp(-np.clip(x, -88, 88).astype(np.float64)).astype(np.float32))

# Load model
t0 = time.time()
with open(GGUF_PATH, "rb") as f:
    T, DS, meta = read_gguf_header(f)

dim = 576; n_layers = 30; n_heads = 9; n_kv_heads = 3
head_dim = dim // n_heads; kv_dim = n_kv_heads * head_dim
inter = 1536; kv_group = n_heads // n_kv_heads; eps = 1e-5

print("Loading weights...")
with open(GGUF_PATH, "rb") as f:
    embd = dq_q8_vec(f, DS, T['token_embd.weight'])
    final_norm = read_f32(f, DS, T['output_norm.weight'])
    out_w = embd  # tied

    layers = []
    for l in range(n_layers):
        lw = {
            'attn_norm': read_f32(f, DS, T[f'blk.{l}.attn_norm.weight']),
            'wq_raw': read_q4_raw(f, DS, T[f'blk.{l}.attn_q.weight']),
            'wk_raw': read_q4_raw(f, DS, T[f'blk.{l}.attn_k.weight']),
            'wv_raw': read_q4_raw(f, DS, T[f'blk.{l}.attn_v.weight']),
            'wo_raw': read_q4_raw(f, DS, T[f'blk.{l}.attn_output.weight']),
            'ffn_norm': read_f32(f, DS, T[f'blk.{l}.ffn_norm.weight']),
            'wg_raw': read_q4_raw(f, DS, T[f'blk.{l}.ffn_gate.weight']),
            'wu_raw': read_q4_raw(f, DS, T[f'blk.{l}.ffn_up.weight']),
            'wd_raw': read_q4_raw(f, DS, T[f'blk.{l}.ffn_down.weight']),
        }
        layers.append(lw)

print(f"Loaded in {time.time()-t0:.1f}s. Running Q8_0 GEMM forward pass...")

x = embd[1].copy()

for layer_idx in range(n_layers):
    lw = layers[layer_idx]

    # Attention
    xb = rmsnorm(x, lw['attn_norm'], eps)
    xb_q8 = quantize_f32_to_q8_0(xb)

    wq_raw, _, _ = lw['wq_raw']
    wk_raw, _, _ = lw['wk_raw']
    wv_raw, _, _ = lw['wv_raw']
    wo_raw, _, _ = lw['wo_raw']

    q = gemm_q4_q8(xb_q8, wq_raw, dim, dim)
    k = gemm_q4_q8(xb_q8, wk_raw, dim, kv_dim)
    v = gemm_q4_q8(xb_q8, wv_raw, dim, kv_dim)

    # Attention: seq_len=1, attn_out = V repeated per GQA group
    attn = np.zeros(dim, dtype=np.float32)
    for h in range(n_heads):
        attn[h*head_dim:(h+1)*head_dim] = v[(h//kv_group)*head_dim:(h//kv_group+1)*head_dim]

    attn_q8 = quantize_f32_to_q8_0(attn)
    wo_out = gemm_q4_q8(attn_q8, wo_raw, dim, dim)
    x += wo_out

    # FFN
    xb2 = rmsnorm(x, lw['ffn_norm'], eps)
    xb2_q8 = quantize_f32_to_q8_0(xb2)

    wg_raw, _, _ = lw['wg_raw']
    wu_raw, _, _ = lw['wu_raw']
    wd_raw, _, _ = lw['wd_raw']

    gate = silu(gemm_q4_q8(xb2_q8, wg_raw, dim, inter))
    up = gemm_q4_q8(xb2_q8, wu_raw, dim, inter)
    ffn_mid = gate * up

    ffn_q8 = quantize_f32_to_q8_0(ffn_mid)
    down = gemm_q4_q8(ffn_q8, wd_raw, inter, dim)
    x += down

    if layer_idx % 10 == 0:
        print(f"  Layer {layer_idx}: x[0]={x[0]:.4f}, min={x.min():.4f}, max={x.max():.4f}")

# Final
x = rmsnorm(x, final_norm, eps)
logits = out_w @ x  # f32 × Q8_0 for output (same as Rust)

top10 = np.argsort(logits)[-10:][::-1]
print(f"\n=== BOS LOGITS (Q8_0 GEMM path) ===")
print(f"argmax=[{logits.argmax()}] = {logits.max():.6f}")
print(f"Top-5:")
for idx in top10[:5]:
    print(f"  [{idx:6d}] = {logits[idx]:12.6f}")

print(f"\nllama-cpp reference: argmax=28 = 19.90")
print(f"f32×Q4_0 reference:  argmax=339 = 16.34")
