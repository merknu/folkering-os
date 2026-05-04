//! Tensor math primitives — the future Burn local backend's "physical"
//! storage + ops layer.
//!
//! Why this file exists at D.1 already: when D.2 wires Burn's
//! `Backend` trait, we need a no_std-safe f32 storage + matmul impl
//! to plug into it. `burn-ndarray` pulls in `ndarray` (std-only);
//! `burn-candle`, `burn-tch`, `burn-wgpu` all need real OS I/O. The
//! only no_std path is a custom backend over a `Vec<f32>` storage,
//! and that storage is what this file provides.
//!
//! For D.1 (router/IPC abstraction) the local backend is a stub, but
//! `self_test()` runs at boot to verify the math is correct so D.2
//! starts on a known-good foundation. When D.5 swaps in the VirGL
//! compute backend, this file becomes the reference we diff against.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

/// Yield budget: how many cells we compute per matmul row before
/// calling `yield_cpu()`. With a 32-cell-wide row this yields once
/// per row; for the D.1 2×2 demo we yield once per matmul (the
/// `m * k` loop body never reaches 32 cells). Tunable per-phase.
const MATMUL_YIELD_EVERY: usize = 32;

/// Row-major 2-D tensor of f32. Owns its storage on the bump heap.
pub struct Tensor2 {
    rows: usize,
    cols: usize,
    data: Vec<f32>,
}

impl Tensor2 {
    pub fn zeros(rows: usize, cols: usize) -> Self {
        Self { rows, cols, data: vec![0.0; rows * cols] }
    }

    pub fn from_rows(rows: &[&[f32]]) -> Self {
        let r = rows.len();
        let c = if r == 0 { 0 } else { rows[0].len() };
        let mut data = Vec::with_capacity(r * c);
        for row in rows {
            assert!(row.len() == c, "ragged rows in Tensor2::from_rows");
            data.extend_from_slice(row);
        }
        Self { rows: r, cols: c, data }
    }

    /// Construct from a row-major flat `Vec<f32>`. Used by the local
    /// backend to bridge `burn_tensor::TensorData` (whose underlying
    /// storage is just bytes + a separate shape) into a typed
    /// `Tensor2` for the matmul path.
    pub fn from_flat(rows: usize, cols: usize, data: Vec<f32>) -> Self {
        assert!(data.len() == rows * cols,
            "Tensor2::from_flat length mismatch: rows*cols != data.len()");
        Self { rows, cols, data }
    }

    #[inline]
    pub fn get(&self, r: usize, c: usize) -> f32 {
        self.data[r * self.cols + c]
    }

    #[inline]
    fn set(&mut self, r: usize, c: usize, v: f32) {
        self.data[r * self.cols + c] = v;
    }

    pub fn rows(&self) -> usize { self.rows }
    pub fn cols(&self) -> usize { self.cols }

}

/// `out = a @ b`. Row-major naive triple loop. Yields cooperatively
/// every `MATMUL_YIELD_EVERY` accumulator updates so the compositor
/// + net driver don't stall while we're crunching.
///
/// For D.1 dimensions (2×2 @ 2×2), the inner loop never reaches the
/// yield threshold, so this is effectively a tight loop. As soon as
/// D.2 starts loading 0.5B-parameter models, the same yield pattern
/// keeps the GUI alive — the K dimension (model dim) will be in the
/// thousands and we want one yield per row at minimum.
pub fn matmul(a: &Tensor2, b: &Tensor2) -> Tensor2 {
    assert!(a.cols() == b.rows(), "matmul shape mismatch");
    let m = a.rows();
    let n = b.cols();
    let k = a.cols();
    let mut out = Tensor2::zeros(m, n);
    let mut since_yield: usize = 0;

    for i in 0..m {
        for j in 0..n {
            let mut acc = 0.0f32;
            for kk in 0..k {
                acc += a.get(i, kk) * b.get(kk, j);
                since_yield += 1;
                if since_yield >= MATMUL_YIELD_EVERY {
                    since_yield = 0;
                    libfolk::sys::yield_cpu();
                }
            }
            out.set(i, j, acc);
        }
    }
    out
}

/// Linear layer (matmul-vector, no bias): `out = W @ x`, where `W`
/// has shape `[out_dim, in_dim]` row-major. Qwen and Llama don't use
/// bias on these projections, so we skip it. Allocates a fresh
/// `Vec<f32>` of length `out_dim`.
///
/// Returns `None` on shape mismatch — easier to propagate than panic
/// in the request-driven hot path.
pub fn linear(weights: &[f32], in_dim: usize, out_dim: usize, x: &[f32]) -> Option<Vec<f32>> {
    if x.len() != in_dim || weights.len() != in_dim * out_dim { return None; }
    let mut out = Vec::with_capacity(out_dim);
    let mut since_yield: usize = 0;
    for i in 0..out_dim {
        let mut acc = 0.0f32;
        let row_off = i * in_dim;
        for k in 0..in_dim {
            acc += weights[row_off + k] * x[k];
            since_yield += 1;
            if since_yield >= MATMUL_YIELD_EVERY {
                since_yield = 0;
                libfolk::sys::yield_cpu();
            }
        }
        out.push(acc);
    }
    Some(out)
}

/// Element-wise multiply (Hadamard product). Used by SwiGLU between
/// the SiLU-gated branch and the up-projection branch.
pub fn elemwise_mul(a: &[f32], b: &[f32]) -> Option<Vec<f32>> {
    if a.len() != b.len() { return None; }
    let mut out = Vec::with_capacity(a.len());
    for i in 0..a.len() {
        out.push(a[i] * b[i]);
    }
    Some(out)
}

/// Fast exp(x) using a 6th-order minimax polynomial after range
/// reduction. Max error ~3e-5 for |x| < 88. Lifted from
/// `libtensor::ops::fast_exp`. We avoid libm because under QEMU TCG
/// the FPU-emulated `expf` is ~100× slower than this.
#[inline]
fn fast_exp(x: f32) -> f32 {
    if x > 88.0 { return f32::MAX; }
    if x < -88.0 { return 0.0; }

    let ln2 = 0.6931471805599453_f32;
    let inv_ln2 = 1.4426950408889634_f32;
    // Floor without libm: cast to int, adjust for negatives.
    let n_raw = x * inv_ln2 + 0.5;
    let n_int = n_raw as i32;
    let n = if n_raw < 0.0 && (n_int as f32) != n_raw {
        (n_int - 1) as f32
    } else {
        n_int as f32
    };
    let r = x - n * ln2;

    // Horner-evaluated 1 + r + r²/2 + r³/6 + r⁴/24 + r⁵/120.
    let p = 1.0_f32
        + r * (1.0
        + r * (0.5
        + r * (1.0/6.0
        + r * (1.0/24.0
        + r * (1.0/120.0)))));

    // Multiply by 2^n via direct manipulation of the IEEE 754 exponent.
    let bits = ((n as i32 + 127) as u32).wrapping_shl(23);
    let pow2 = f32::from_bits(bits);
    p * pow2
}

/// Sigmoid Linear Unit (a.k.a. swish).
///
///   silu(x) = x * sigmoid(x) = x / (1 + exp(-x))
///
/// Used by Qwen and Llama as the activation function inside SwiGLU.
/// We compute `sigmoid` via `fast_exp` for the no-libm reasons above;
/// max error ~3e-5 vs the reference is well below the precision
/// budget RMSNorm-stacking already swallows.
pub fn silu(x: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(x.len());
    for &v in x {
        let e_neg = fast_exp(-v);
        let sigmoid = 1.0 / (1.0 + e_neg);
        out.push(v * sigmoid);
    }
    out
}

/// SwiGLU FFN block (Qwen2.5 / Llama style).
///
///   y = down_proj( silu(gate_proj(x)) ⊙ up_proj(x) )
///
/// Shapes:
///   x          : [hidden]
///   gate_proj  : [intermediate, hidden] row-major
///   up_proj    : [intermediate, hidden] row-major
///   down_proj  : [hidden, intermediate] row-major
///   y          : [hidden]
///
/// Returns None on any shape mismatch. We allocate three intermediate
/// `Vec<f32>` (gate output, up output, hadamard output); D.3.4 will
/// switch to a pre-allocated scratch slab when per-token throughput
/// starts mattering.
pub fn swiglu_ffn(
    x: &[f32],
    gate_proj: &[f32],
    up_proj: &[f32],
    down_proj: &[f32],
    hidden: usize,
    intermediate: usize,
) -> Option<Vec<f32>> {
    let g = linear(gate_proj, hidden, intermediate, x)?;
    let u = linear(up_proj, hidden, intermediate, x)?;
    let g_silu = silu(&g);
    let mixed = elemwise_mul(&g_silu, &u)?;
    linear(down_proj, intermediate, hidden, &mixed)
}

/// In-place softmax over a 1-D slice. Numerically stable: subtract
/// the max before `fast_exp` so exponents stay in the well-behaved
/// region (`fast_exp` clamps |x| < 88 anyway, but the stability
/// trick keeps the precision at the high end of the input).
///
/// Handles `f32::NEG_INFINITY` cleanly — those entries get
/// `fast_exp(-inf) == 0` (per the implementation's clamp at -88),
/// which is exactly the masked-position behaviour we want for
/// causal attention.
pub fn softmax_inplace(x: &mut [f32]) {
    if x.is_empty() { return; }
    let mut max = f32::NEG_INFINITY;
    for &v in x.iter() {
        if v > max { max = v; }
    }
    if max == f32::NEG_INFINITY {
        // All entries masked → distribute uniform mass. Shouldn't
        // happen for causal attention, but defend against it.
        let n = x.len() as f32;
        for v in x.iter_mut() { *v = 1.0 / n; }
        return;
    }
    let mut sum = 0.0f32;
    for v in x.iter_mut() {
        *v = fast_exp(*v - max);
        sum += *v;
    }
    if sum > 0.0 {
        let inv = 1.0 / sum;
        for v in x.iter_mut() { *v *= inv; }
    }
}

/// Apply Rotary Position Embedding (RoPE) in-place to a Q or K tensor
/// of shape `[seq_len, n_heads, head_dim]` row-major (= flat
/// `[seq_len * n_heads * head_dim]`).
///
/// `cos_table` and `sin_table` both have shape `[seq_len, head_dim/2]`,
/// pre-computed by the build-time tooling (`gen_test_blobs.py` and the
/// future HuggingFace converter). For each (s, h, i) where `i` indexes
/// the pair (`pair_i = i / 2`), rotates `(qk[s,h,2*pair], qk[s,h,2*pair+1])`
/// by the angle `arctan2(sin, cos)` baked into the tables.
///
/// Why pre-compute the tables instead of running sin/cos in the kernel:
/// fast `sin/cos` approximations cost ~30 cycles per call, and we'd
/// burn one per RoPE pair per token per layer. Pre-computing once at
/// build time and reading f32s from the .fbin keeps the hot path
/// pure linear algebra — the compositor has no idea inference is
/// happening, frame budget stays clean.
pub fn apply_rope(
    qk: &mut [f32],
    cos_table: &[f32],
    sin_table: &[f32],
    seq_len: usize,
    n_heads: usize,
    head_dim: usize,
) -> Option<()> {
    if head_dim % 2 != 0 { return None; }
    let pairs = head_dim / 2;
    if cos_table.len() != seq_len * pairs || sin_table.len() != seq_len * pairs {
        return None;
    }
    if qk.len() != seq_len * n_heads * head_dim { return None; }

    for s in 0..seq_len {
        for h in 0..n_heads {
            for p in 0..pairs {
                let i = s * n_heads * head_dim + h * head_dim + 2 * p;
                let j = i + 1;
                let cos = cos_table[s * pairs + p];
                let sin = sin_table[s * pairs + p];
                let x = qk[i];
                let y = qk[j];
                qk[i] = x * cos - y * sin;
                qk[j] = x * sin + y * cos;
            }
        }
    }
    Some(())
}

/// Scaled dot-product attention with causal mask.
///
/// All inputs are `[seq_len, n_heads, head_dim]` row-major. The
/// returned tensor has the same shape, flattened.
///
/// Per query position `i`:
///   1. Compute scores[j] = (Q[i] · K[j]) / sqrt(head_dim) for j ≤ i,
///      mask scores[j] = -inf for j > i (causal).
///   2. Softmax across j.
///   3. out[i] = sum_j(scores[j] * V[j]).
///
/// Allocates a `Vec<f32>` per query row for the score buffer plus
/// the output. D.3.5 will pre-allocate when we start caring about
/// per-token throughput; D.3.4's job is correctness.
pub fn scaled_dot_product_attention(
    q: &[f32],
    k: &[f32],
    v: &[f32],
    seq_len: usize,
    n_heads: usize,
    head_dim: usize,
) -> Option<Vec<f32>> {
    let total = seq_len * n_heads * head_dim;
    if q.len() != total || k.len() != total || v.len() != total {
        return None;
    }

    let scale = fast_rsqrt(head_dim as f32);
    let mut out = vec![0.0f32; total];
    let mut scores = vec![0.0f32; seq_len];

    for h in 0..n_heads {
        for i in 0..seq_len {
            // ── Compute Q·K scores with causal mask ────────────────
            for j in 0..seq_len {
                if j > i {
                    scores[j] = f32::NEG_INFINITY;
                    continue;
                }
                let mut dot = 0.0f32;
                for d in 0..head_dim {
                    let q_idx = i * n_heads * head_dim + h * head_dim + d;
                    let k_idx = j * n_heads * head_dim + h * head_dim + d;
                    dot += q[q_idx] * k[k_idx];
                }
                scores[j] = dot * scale;
            }

            softmax_inplace(&mut scores);

            // ── attn @ V ───────────────────────────────────────────
            for d in 0..head_dim {
                let mut acc = 0.0f32;
                for j in 0..seq_len {
                    let v_idx = j * n_heads * head_dim + h * head_dim + d;
                    acc += scores[j] * v[v_idx];
                }
                let out_idx = i * n_heads * head_dim + h * head_dim + d;
                out[out_idx] = acc;
            }
            libfolk::sys::yield_cpu();
        }
    }
    Some(out)
}

/// Full attention block: QKV projection → RoPE on Q/K → causal SDPA
/// → output projection. The shape Qwen2.5 / Llama use, modulo
/// grouped-query attention (GQA) where `n_kv_heads < n_heads` —
/// today we assume `n_kv_heads == n_heads`; D.3.5 adds GQA.
///
/// `x` is `[seq_len, hidden_dim]`. Wq / Wk / Wv / Wo are all
/// `[hidden_dim, hidden_dim]` row-major (HuggingFace native).
/// `rope_cos` / `rope_sin` are `[seq_len, head_dim/2]`.
pub fn attention_block(
    x: &[f32],
    wq: &[f32], wk: &[f32], wv: &[f32], wo: &[f32],
    rope_cos: &[f32], rope_sin: &[f32],
    seq_len: usize, hidden_dim: usize, n_heads: usize,
) -> Option<Vec<f32>> {
    if hidden_dim % n_heads != 0 { return None; }
    let head_dim = hidden_dim / n_heads;
    if x.len() != seq_len * hidden_dim { return None; }

    // ── 1. Project x → Q, K, V (per-row matvec) ─────────────────────
    let mut q = Vec::with_capacity(seq_len * hidden_dim);
    let mut k = Vec::with_capacity(seq_len * hidden_dim);
    let mut v = Vec::with_capacity(seq_len * hidden_dim);
    for s in 0..seq_len {
        let row = &x[s * hidden_dim..(s + 1) * hidden_dim];
        q.extend(linear(wq, hidden_dim, hidden_dim, row)?);
        k.extend(linear(wk, hidden_dim, hidden_dim, row)?);
        v.extend(linear(wv, hidden_dim, hidden_dim, row)?);
    }

    // ── 2. RoPE on Q and K (V is not rotated) ───────────────────────
    apply_rope(&mut q, rope_cos, rope_sin, seq_len, n_heads, head_dim)?;
    apply_rope(&mut k, rope_cos, rope_sin, seq_len, n_heads, head_dim)?;

    // ── 3. Scaled dot-product attention ─────────────────────────────
    let attn = scaled_dot_product_attention(&q, &k, &v, seq_len, n_heads, head_dim)?;

    // ── 4. Output projection (per-row matvec) ───────────────────────
    let mut out = Vec::with_capacity(seq_len * hidden_dim);
    for s in 0..seq_len {
        let row = &attn[s * hidden_dim..(s + 1) * hidden_dim];
        out.extend(linear(wo, hidden_dim, hidden_dim, row)?);
    }
    Some(out)
}

/// Embedding lookup: `table[vocab_id, :]` from a row-major
/// `[n_vocab, hidden_dim]` table. Allocates a fresh `Vec<f32>` of
/// length `hidden_dim` so the caller can mutate it for the rest of
/// the forward pass without poisoning the table.
///
/// Returns None on out-of-range vocab_id, NOT an empty Vec — empty
/// would be silently wrong if the caller forgot to validate.
pub fn embedding_lookup(
    table: &[f32],
    n_vocab: usize,
    hidden_dim: usize,
    vocab_id: u32,
) -> Option<Vec<f32>> {
    let id = vocab_id as usize;
    if id >= n_vocab { return None; }
    if table.len() != n_vocab * hidden_dim { return None; }
    let start = id * hidden_dim;
    let mut out = Vec::with_capacity(hidden_dim);
    out.extend_from_slice(&table[start..start + hidden_dim]);
    Some(out)
}

/// Fast inverse square root (Quake-style) with 2 Newton-Raphson
/// iterations. ~0.0001% precision — needed because Qwen2.5 stacks
/// RMSNorm 60× across 30 layers, and a single-iteration version's
/// 0.175% drift compounds to ~11% by the final layer (lifted from
/// `libtensor::ops::fast_rsqrt`, project memory:
/// `folkering-bpe-tokenizer.md`'s sibling lessons).
///
/// Avoids pulling in libm. Stable on QEMU TCG where the FPU-emulated
/// libm sqrt is ~100× slower than this.
#[inline]
fn fast_rsqrt(x: f32) -> f32 {
    if x <= 0.0 { return 0.0; }
    let i = 0x5F375A86u32.wrapping_sub(x.to_bits() >> 1);
    let y = f32::from_bits(i);
    let y = y * (1.5 - 0.5 * x * y * y);
    y * (1.5 - 0.5 * x * y * y)
}

/// RMSNorm — Qwen2.5 / Llama-style normalization.
///
///   y_i = x_i / sqrt(mean(x²) + eps) * weight_i
///
/// Operates in-place-style by allocating a fresh `Vec<f32>`. We
/// could mutate `x` directly to save the alloc, but at D.3.2 scale
/// the explicit allocation makes the data flow obvious; we'll
/// switch to in-place when D.3.4 starts caring about per-token
/// throughput.
pub fn rmsnorm(x: &[f32], weight: &[f32], eps: f32) -> Option<Vec<f32>> {
    if x.len() != weight.len() { return None; }
    if x.is_empty() { return Some(Vec::new()); }
    let n = x.len();
    let mut sum_sq: f32 = 0.0;
    for &v in x { sum_sq += v * v; }
    let mean_sq = sum_sq / (n as f32);
    let inv_rms = fast_rsqrt(mean_sq + eps);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(x[i] * inv_rms * weight[i]);
    }
    Some(out)
}

/// Boot-time correctness check. Runs all `tensor_math` ops on small
/// inputs with hand-computed expected outputs. Cheap; invoked once
/// from `main` so a regression in any op shows up immediately
/// rather than 800 LOC into a real model forward pass.
pub fn self_test() -> bool {
    // ── 1. matmul (2×2 @ 2×2) ──
    let a = Tensor2::from_rows(&[
        &[1.0, 2.0],
        &[3.0, 4.0],
    ]);
    let b = Tensor2::from_rows(&[
        &[5.0, 6.0],
        &[7.0, 8.0],
    ]);
    let c = matmul(&a, &b);
    let matmul_ok = (c.get(0, 0) - 19.0).abs() < 1e-6
        && (c.get(0, 1) - 22.0).abs() < 1e-6
        && (c.get(1, 0) - 43.0).abs() < 1e-6
        && (c.get(1, 1) - 50.0).abs() < 1e-6;
    if !matmul_ok { return false; }

    // ── 2. fast_rsqrt sanity ──
    // 1/sqrt(4) = 0.5 exactly. Our 2-iteration NR converges close.
    let r = fast_rsqrt(4.0);
    if (r - 0.5).abs() > 1e-3 { return false; }

    // ── 3. RMSNorm against hand-computed reference ──
    // x = [5, 6, 7, 8], mean(x²) = (25+36+49+64)/4 = 43.5
    // rms = sqrt(43.5) ≈ 6.5954529791
    // w = [0.25, 0.5, 0.75, 1.0]
    // y_i = x_i / rms * w_i:
    //   y_0 = 5 / 6.5954529791 * 0.25 ≈ 0.18952
    //   y_1 = 6 / 6.5954529791 * 0.50 ≈ 0.45486
    //   y_2 = 7 / 6.5954529791 * 0.75 ≈ 0.79601
    //   y_3 = 8 / 6.5954529791 * 1.00 ≈ 1.21297
    //   sum ≈ 2.65336
    let xs = [5.0_f32, 6.0, 7.0, 8.0];
    let ws = [0.25_f32, 0.5, 0.75, 1.0];
    let normed = match rmsnorm(&xs, &ws, 1e-6) {
        Some(v) => v,
        None => return false,
    };
    let sum: f32 = normed.iter().sum();
    if (sum - 2.65336).abs() > 5e-3 { return false; }

    // ── 4. embedding_lookup ──
    // table is 4×4 row-major [1..16]. Row 1 = [5, 6, 7, 8].
    let table: Vec<f32> = (1..=16).map(|n| n as f32).collect();
    let row1 = match embedding_lookup(&table, 4, 4, 1) {
        Some(v) => v,
        None => return false,
    };
    if row1 != [5.0_f32, 6.0, 7.0, 8.0] { return false; }
    // out-of-range returns None
    if embedding_lookup(&table, 4, 4, 9).is_some() { return false; }

    // ── 5. fast_exp sanity ──
    // exp(0) = 1, exp(1) ≈ 2.71828, exp(-1) ≈ 0.36788
    if (fast_exp(0.0) - 1.0).abs() > 1e-3 { return false; }
    if (fast_exp(1.0) - 2.71828).abs() > 1e-2 { return false; }
    if (fast_exp(-1.0) - 0.36788).abs() > 1e-3 { return false; }

    // ── 6. silu against PyTorch reference ──
    // silu(0) = 0
    // silu(1) ≈ 0.7311
    // silu(-1) ≈ -0.2689
    let silu_out = silu(&[0.0_f32, 1.0, -1.0, 2.0]);
    if silu_out[0].abs() > 1e-3 { return false; }
    if (silu_out[1] - 0.7311).abs() > 1e-2 { return false; }
    if (silu_out[2] - (-0.2689)).abs() > 1e-2 { return false; }
    if (silu_out[3] - 1.7616).abs() > 1e-2 { return false; }

    // ── 7. linear (matvec) ──
    // W = [[1, 2], [3, 4]] row-major, x = [5, 6]
    // out = [1*5 + 2*6, 3*5 + 4*6] = [17, 39]
    let w = [1.0_f32, 2.0, 3.0, 4.0];
    let lo = match linear(&w, 2, 2, &[5.0_f32, 6.0]) {
        Some(v) => v,
        None => return false,
    };
    if (lo[0] - 17.0).abs() > 1e-6 { return false; }
    if (lo[1] - 39.0).abs() > 1e-6 { return false; }

    // ── 8. SwiGLU FFN end-to-end with hand-computed reference ──
    // x = [1, 2]
    // gate_proj 4×2 = [[1, 0], [0, 1], [1, 1], [1, -1]]   row-major
    // up_proj   4×2 = [[1, 0], [0, 1], [0, 1], [1, 0]]
    // down_proj 2×4 = [[1, 0, 1, 0], [0, 1, 0, 1]]
    //
    // gate(x) = [1, 2, 3, -1]
    // up(x)   = [1, 2, 2, 1]
    // silu(gate) ≈ [0.7311, 1.7616, 2.8577, -0.2689]
    // hadamard ≈ [0.7311, 3.5232, 5.7155, -0.2689]
    // down([h]) = [h0+h2, h1+h3] = [6.4466, 3.2543]
    // sum ≈ 9.7009
    let gate = [1.0_f32, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, -1.0];
    let up   = [1.0_f32, 0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0];
    let down = [1.0_f32, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 1.0];
    let ffn = match swiglu_ffn(&[1.0_f32, 2.0], &gate, &up, &down, 2, 4) {
        Some(v) => v,
        None => return false,
    };
    let ffn_sum: f32 = ffn.iter().sum();
    if (ffn_sum - 9.7009).abs() > 5e-2 { return false; }

    // ── 9. softmax_inplace ──
    // softmax([1, 2, 3]) ≈ [0.0900, 0.2447, 0.6652]
    let mut sm = [1.0_f32, 2.0, 3.0];
    softmax_inplace(&mut sm);
    if (sm[0] - 0.0900).abs() > 1e-2 { return false; }
    if (sm[1] - 0.2447).abs() > 1e-2 { return false; }
    if (sm[2] - 0.6652).abs() > 1e-2 { return false; }
    // Sum to 1
    let sm_sum: f32 = sm.iter().sum();
    if (sm_sum - 1.0).abs() > 1e-3 { return false; }

    // softmax with -inf entry (causal mask)
    let mut sm_masked = [0.7071_f32, f32::NEG_INFINITY];
    softmax_inplace(&mut sm_masked);
    if (sm_masked[0] - 1.0).abs() > 1e-3 { return false; }
    if sm_masked[1].abs() > 1e-3 { return false; }

    // ── 10. apply_rope ──
    // qk = [1, 0] (1 token, 1 head, head_dim=2)
    // cos = [cos(1)] ≈ 0.5403, sin = [sin(1)] ≈ 0.8415
    // After: [1*0.5403 - 0*0.8415, 1*0.8415 + 0*0.5403] = [0.5403, 0.8415]
    let mut qk = [1.0_f32, 0.0];
    let cos = [0.5403_f32];
    let sin = [0.8415_f32];
    apply_rope(&mut qk, &cos, &sin, 1, 1, 2);
    if (qk[0] - 0.5403).abs() > 1e-3 { return false; }
    if (qk[1] - 0.8415).abs() > 1e-3 { return false; }

    true
}

