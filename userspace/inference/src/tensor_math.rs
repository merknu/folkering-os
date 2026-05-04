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

/// Boot-time correctness check. Runs the same 2×2 matmul we used as
/// the original D.1 demo and returns true iff every entry matches.
/// Cheap (single-digit microseconds) — invoked once from `main`
/// so a regression in our matmul shows up immediately rather than
/// 800 LOC into a real model forward pass.
pub fn self_test() -> bool {
    let a = Tensor2::from_rows(&[
        &[1.0, 2.0],
        &[3.0, 4.0],
    ]);
    let b = Tensor2::from_rows(&[
        &[5.0, 6.0],
        &[7.0, 8.0],
    ]);
    let c = matmul(&a, &b);
    (c.get(0, 0) - 19.0).abs() < 1e-6
        && (c.get(0, 1) - 22.0).abs() < 1e-6
        && (c.get(1, 0) - 43.0).abs() < 1e-6
        && (c.get(1, 1) - 50.0).abs() < 1e-6
}

