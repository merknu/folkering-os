"""
Full 30-layer forward pass using Q4_0/Q8_0 weights from GGUF.
Checks if scalar f32 accumulation with Q4_0 produces correct BOS logits.
This is the DEFINITIVE test: if argmax=28 → Rust has a bug; if argmax=339 → precision issue.
"""
import struct, numpy as np, os, time
os.chdir(r"C:\Users\merkn\folkering\folkering-os")

GGUF_PATH = "boot/model.gguf"

def read_gguf_header(f):
    magic = f.read(4)
    assert magic == b"GGUF"
    version = struct.unpack("<I", f.read(4))[0]
    n_tensors = struct.unpack("<Q", f.read(8))[0]
    n_metadata = struct.unpack("<Q", f.read(8))[0]
    meta = {}
    for _ in range(n_metadata):
        kl = struct.unpack("<Q", f.read(8))[0]
        key = f.read(kl).decode("utf-8", errors="replace")
        vt = struct.unpack("<I", f.read(4))[0]
        val = read_val(f, vt)
        meta[key] = val
    tensors = {}
    for _ in range(n_tensors):
        nl = struct.unpack("<Q", f.read(8))[0]
        name = f.read(nl).decode("utf-8")
        nd = struct.unpack("<I", f.read(4))[0]
        dims = [struct.unpack("<Q", f.read(8))[0] for _ in range(nd)]
        dtype = struct.unpack("<I", f.read(4))[0]
        offset = struct.unpack("<Q", f.read(8))[0]
        tensors[name] = {"dims": dims, "dtype": dtype, "offset": offset}
    ds = (f.tell() + 31) // 32 * 32
    return tensors, ds, meta

def read_val(f, t):
    if t == 0: return struct.unpack("<B", f.read(1))[0]
    elif t == 1: return struct.unpack("<b", f.read(1))[0]
    elif t == 4: return struct.unpack("<I", f.read(4))[0]
    elif t == 5: return struct.unpack("<i", f.read(4))[0]
    elif t == 6: return struct.unpack("<f", f.read(4))[0]
    elif t == 7: return struct.unpack("<B", f.read(1))[0]
    elif t == 8:
        sl = struct.unpack("<Q", f.read(8))[0]
        return f.read(sl).decode("utf-8", errors="replace")
    elif t == 9:
        et = struct.unpack("<I", f.read(4))[0]
        n = struct.unpack("<Q", f.read(8))[0]
        return [read_val(f, et) for _ in range(n)]
    elif t == 10: return struct.unpack("<Q", f.read(8))[0]
    else:
        if t in (2,3): f.read(2); return 0
        if t in (11,): f.read(8); return 0
        if t in (12,): f.read(8); return 0.0
        return None

# Fast vectorized dequantization
def dq_q4_vec(f, ds, info):
    nc, nr = info['dims'][0], info['dims'][1] if len(info['dims']) > 1 else 1
    bpr = nc // 32
    total_blocks = nr * bpr
    f.seek(ds + info['offset'])
    raw = np.frombuffer(f.read(total_blocks * 18), dtype=np.uint8).reshape(total_blocks, 18)
    scales = raw[:, :2].copy().view(np.float16).astype(np.float32).flatten()
    packed = raw[:, 2:]
    lo = (packed & 0x0F).astype(np.float32) - 8
    hi = ((packed >> 4) & 0x0F).astype(np.float32) - 8
    vals = np.empty((total_blocks, 32), dtype=np.float32)
    vals[:, 0::2] = lo
    vals[:, 1::2] = hi
    vals *= scales[:, np.newaxis]
    return vals.reshape(nr, nc)

def dq_q8_vec(f, ds, info):
    nc, nr = info['dims'][0], info['dims'][1] if len(info['dims']) > 1 else 1
    bpr = nc // 32
    total_blocks = nr * bpr
    f.seek(ds + info['offset'])
    raw = np.frombuffer(f.read(total_blocks * 34), dtype=np.uint8).reshape(total_blocks, 34)
    scales = raw[:, :2].copy().view(np.float16).astype(np.float32).flatten()
    qvals = raw[:, 2:].view(np.int8).astype(np.float32)
    qvals *= scales[:, np.newaxis]
    return qvals.reshape(nr, nc)

def read_f32(f, ds, info):
    n = 1
    for d in info['dims']: n *= d
    f.seek(ds + info['offset'])
    return np.frombuffer(f.read(n * 4), dtype=np.float32).copy()

def rmsnorm(x, w, eps):
    return (x / np.sqrt(np.mean(x.astype(np.float64)**2) + eps).astype(np.float32)) * w

def silu(x):
    return x / (1.0 + np.exp(-np.clip(x, -88, 88).astype(np.float64)).astype(np.float32))

t0 = time.time()
with open(GGUF_PATH, "rb") as f:
    T, DS, meta = read_gguf_header(f)

eps = meta.get('smollm2.attention.layer_norm_rms_epsilon', 1e-5)
n_layers = meta.get('smollm2.block_count', 30)
n_heads = meta.get('smollm2.attention.head_count', 9)
n_kv_heads = meta.get('smollm2.attention.head_count_kv', 3)
dim = 576
head_dim = dim // n_heads
kv_dim = n_kv_heads * head_dim
inter_size = meta.get('smollm2.feed_forward_length', 1536)
kv_group = n_heads // n_kv_heads

print(f"Config: {n_layers} layers, dim={dim}, heads={n_heads}, kv_heads={n_kv_heads}, inter={inter_size}")
print(f"rms_eps={eps}")

# Load all weights
print("Loading weights...")
with open(GGUF_PATH, "rb") as f:
    embd = dq_q8_vec(f, DS, T['token_embd.weight'])
    final_norm = read_f32(f, DS, T['output_norm.weight'])
    # Output projection uses tied embeddings (same as token_embd)
    out_w = embd  # tied weights

    layers = []
    for l in range(n_layers):
        layer = {
            'attn_norm': read_f32(f, DS, T[f'blk.{l}.attn_norm.weight']),
            'wq': dq_q4_vec(f, DS, T[f'blk.{l}.attn_q.weight']),
            'wk': dq_q4_vec(f, DS, T[f'blk.{l}.attn_k.weight']),
            'wv': dq_q4_vec(f, DS, T[f'blk.{l}.attn_v.weight']),
            'wo': dq_q4_vec(f, DS, T[f'blk.{l}.attn_output.weight']),
            'ffn_norm': read_f32(f, DS, T[f'blk.{l}.ffn_norm.weight']),
            'w_gate': dq_q4_vec(f, DS, T[f'blk.{l}.ffn_gate.weight']),
            'w_up': dq_q4_vec(f, DS, T[f'blk.{l}.ffn_up.weight']),
            'w_down': dq_q4_vec(f, DS, T[f'blk.{l}.ffn_down.weight']),
        }
        layers.append(layer)
        if l % 10 == 0:
            print(f"  loaded layer {l}")

print(f"Weights loaded in {time.time()-t0:.1f}s")

# Forward pass for BOS (token 1, pos 0)
x = embd[1].copy()
print(f"\nEMB first4: {x[:4]}")

for layer_idx in range(n_layers):
    lw = layers[layer_idx]

    # Attention sublayer
    xb = rmsnorm(x, lw['attn_norm'], eps)
    q = lw['wq'] @ xb
    k = lw['wk'] @ xb
    v = lw['wv'] @ xb

    # RoPE at pos=0: cos(0)=1, sin(0)=0 -> no change

    # Attention: seq_len=1, weight=1.0, attn_out = V repeated per GQA group
    attn = np.zeros(dim, dtype=np.float32)
    for h in range(n_heads):
        kv_h = h // kv_group
        attn[h*head_dim:(h+1)*head_dim] = v[kv_h*head_dim:(kv_h+1)*head_dim]

    wo_out = lw['wo'] @ attn
    x += wo_out  # residual

    # FFN sublayer
    xb2 = rmsnorm(x, lw['ffn_norm'], eps)
    gate = silu(lw['w_gate'] @ xb2)
    up = lw['w_up'] @ xb2
    down = lw['w_down'] @ (gate * up)
    x += down  # residual

    if layer_idx in (0, 1, 14, 29):
        print(f"Layer {layer_idx} done: x[0:4]={x[:4]}, min={x.min():.4f}, max={x.max():.4f}")

# Final norm + output projection
x = rmsnorm(x, final_norm, eps)
logits = out_w @ x

# Results
top20 = np.argsort(logits)[-20:][::-1]
print(f"\n=== BOS LOGITS (Python Q4_0 full forward) ===")
print(f"argmax=[{logits.argmax()}] = {logits.max():.6f}")
print(f"min={logits.min():.6f}, max={logits.max():.6f}, mean={logits.mean():.6f}")
print(f"Top-10:")
for idx in top20[:10]:
    print(f"  [{idx:6d}] = {logits[idx]:12.6f}")

print(f"\nTotal time: {time.time()-t0:.1f}s")

# Compare with Rust (argmax=339) and llama-cpp (argmax=28)
print(f"\n=== COMPARISON ===")
print(f"Python Q4_0:  argmax={logits.argmax()}")
print(f"Rust Q4_0:    argmax=339")
print(f"llama-cpp Q4_0: argmax=28")
if logits.argmax() == 28:
    print("→ Python matches llama-cpp. Rust has a BUG!")
elif logits.argmax() == 339:
    print("→ Python matches Rust. Difference is NUMERICAL (accumulation order)")
else:
    print(f"→ Python gives different argmax ({logits.argmax()}) from both!")
