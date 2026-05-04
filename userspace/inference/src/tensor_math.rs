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

    true
}

