//! Single-head self-attention compiled to WebAssembly.
//!
//! Implements the canonical scaled-dot-product attention formula
//!
//!     Out = softmax(QK^T / √D) · V
//!
//! where Q = X·Wq, K = X·Wk, V = X·Wv, all projections being plain
//! matrix-multiplications. The host populates linear memory with X
//! and the projection weights via a DATA frame; the daemon EXECs
//! `attention()`; the function returns a checksum (sum of all output
//! cells × 1000) as i32 so the exit code carries the result.
//!
//! Sized small enough to fit one f32-local-budget (8 slots) but
//! large enough to be a real attention head (S=D=4 ⇒ 16-element
//! matrices throughout, with non-trivial softmax dynamics).
//!
//! All `lf`/`sf` reads/writes go through linear memory at offsets
//! ≥ 0x1000 to defeat LLVM's null-page UB folding (which otherwise
//! replaces low-address loads with undef→0.0).
//!
//! Memory layout (matches `reference.py` and `run_real_wasm_attention.rs`):
//!
//!     0x1000  X       [S×D]  64 B    host-fed
//!     0x1040  Wq      [D×D]  64 B    host-fed
//!     0x1080  Wk      [D×D]  64 B    host-fed
//!     0x10C0  Wv      [D×D]  64 B    host-fed
//!     0x1100  Q       [S×D]  64 B    scratch
//!     0x1140  K       [S×D]  64 B    scratch
//!     0x1180  V       [S×D]  64 B    scratch
//!     0x11C0  Scores  [S×S]  64 B    scratch
//!     0x1200  Probs   [S×S]  64 B    scratch
//!     0x1240  Output  [S×D]  64 B    scratch + checksum source

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

// ── Geometry ───────────────────────────────────────────────────────
pub const S: u32 = 4;
pub const D: u32 = 4;

// ── Memory base offsets (all in linear memory) ────────────────────
const BASE: u32   = 0x1000;
const X_OFF:  u32 = BASE;
const WQ_OFF: u32 = X_OFF  + S * D * 4;     // 0x1040
const WK_OFF: u32 = WQ_OFF + D * D * 4;     // 0x1080
const WV_OFF: u32 = WK_OFF + D * D * 4;     // 0x10C0
const Q_OFF:  u32 = WV_OFF + D * D * 4;     // 0x1100
const K_OFF:  u32 = Q_OFF  + S * D * 4;     // 0x1140
const V_OFF:  u32 = K_OFF  + S * D * 4;     // 0x1180
const SC_OFF: u32 = V_OFF  + S * D * 4;     // 0x11C0
const PR_OFF: u32 = SC_OFF + S * S * 4;     // 0x1200
const OUT_OFF: u32 = PR_OFF + S * S * 4;    // 0x1240

// 1 / sqrt(D) for D=4 — baked in to keep the attention scale a
// compile-time constant. Update both this AND the Python reference
// if D changes.
const INV_SQRT_D: f32 = 0.5;

// ── Memory I/O ─────────────────────────────────────────────────────

#[inline(always)]
fn lf(off: u32) -> f32 {
    unsafe { core::ptr::read(off as *const f32) }
}

#[inline(always)]
fn sf(off: u32, val: f32) {
    unsafe { core::ptr::write(off as *mut f32, val) }
}

// ── exp approximation (matches reference.py byte-for-byte) ────────
//
// Strategy: range-reduce by dividing by 32, evaluate a degree-4
// Taylor polynomial in the small-input range, then square the
// result 5 times to get exp(x) = p^32. For x ∈ [-16, 0], y = x/32
// is in [-0.5, 0] where the polynomial agrees with exp() to ~6
// decimals. Squaring compounds error as (1+ε)^32 ≈ 1 + 32ε —
// final accuracy is still ~0.005 % which is well below softmax
// numerical noise.
//
// Evaluation order is fixed (NOT Horner) so the Python reference
// can mirror it operation-for-operation and reproduce bit-exact
// f32 results.
//
// Clip x < -16 to 0.0 — softmax results past that point are
// numerically irrelevant and the polynomial would otherwise be
// extrapolated outside its valid range.
#[inline(always)]
fn exp_approx(x: f32) -> f32 {
    if x < -16.0 {
        return 0.0;
    }
    let y  = x * 0.03125;          // x / 32
    let y2 = y  * y;
    let y3 = y2 * y;
    let y4 = y2 * y2;
    let p  = 1.0
           + y
           + y2 * 0.5
           + y3 * 0.16666667
           + y4 * 0.04166667;
    // p^32 via 5 successive squarings.
    let p2  = p   * p;
    let p4  = p2  * p2;
    let p8  = p4  * p4;
    let p16 = p8  * p8;
    p16 * p16
}

// ── The attention head ─────────────────────────────────────────────
//
// One monolithic function with nested loops. Loop counters are
// the only persistent locals; per-iteration values live in WASM
// operand-stack temporaries that LLVM materialises in registers,
// not local slots. This keeps us comfortably under the 8-f32-
// local budget.
#[no_mangle]
pub extern "C" fn attention() -> i32 {
    // ── Phase 1: fused Q/K/V projections ───────────────────────
    //
    // Reading X[s,k] is the loop-invariant in the inner loop, so
    // we amortise its load cost across three accumulators (q, k,
    // v) computed simultaneously. This also halves the total
    // load count compared to three separate matmul passes.
    let mut s = 0u32;
    while s < S {
        let mut d_out = 0u32;
        while d_out < D {
            let mut q_acc = 0.0f32;
            let mut k_acc = 0.0f32;
            let mut v_acc = 0.0f32;
            let mut k = 0u32;
            while k < D {
                let x = lf(X_OFF + (s * D + k) * 4);
                let wq_off = WQ_OFF + (k * D + d_out) * 4;
                let wk_off = WK_OFF + (k * D + d_out) * 4;
                let wv_off = WV_OFF + (k * D + d_out) * 4;
                q_acc = q_acc + x * lf(wq_off);
                k_acc = k_acc + x * lf(wk_off);
                v_acc = v_acc + x * lf(wv_off);
                k += 1;
            }
            sf(Q_OFF + (s * D + d_out) * 4, q_acc);
            sf(K_OFF + (s * D + d_out) * 4, k_acc);
            sf(V_OFF + (s * D + d_out) * 4, v_acc);
            d_out += 1;
        }
        s += 1;
    }

    // ── Phase 2: Scores = Q · K^T · (1/√D) ─────────────────────
    let mut i = 0u32;
    while i < S {
        let mut j = 0u32;
        while j < S {
            let mut acc = 0.0f32;
            let mut d = 0u32;
            while d < D {
                acc = acc + lf(Q_OFF + (i * D + d) * 4)
                          * lf(K_OFF + (j * D + d) * 4);
                d += 1;
            }
            sf(SC_OFF + (i * S + j) * 4, acc * INV_SQRT_D);
            j += 1;
        }
        i += 1;
    }

    // ── Phase 3+4+5: stable softmax row-by-row ─────────────────
    //
    // Per row i:
    //   max_i   = max_j Scores[i,j]
    //   exps    = exp_approx(Scores[i,j] - max_i)   → Probs[i,j]
    //   sum_i   = Σ_j exps
    //   Probs   = exps / sum_i
    //
    // We write the unnormalised exps to Probs first, then a second
    // pass divides — this avoids needing a second scratch region.
    let mut row = 0u32;
    while row < S {
        // Phase 3: row max.
        let mut row_max = lf(SC_OFF + row * S * 4);
        let mut j = 1u32;
        while j < S {
            let v = lf(SC_OFF + (row * S + j) * 4);
            if v > row_max {
                row_max = v;
            }
            j += 1;
        }
        // Phase 4: shifted exps + running sum.
        let mut sum = 0.0f32;
        let mut j2 = 0u32;
        while j2 < S {
            let e = exp_approx(lf(SC_OFF + (row * S + j2) * 4) - row_max);
            sf(PR_OFF + (row * S + j2) * 4, e);
            sum = sum + e;
            j2 += 1;
        }
        // Phase 5: normalise (multiply by reciprocal — one divide
        // per row instead of S divides).
        let inv = 1.0 / sum;
        let mut j3 = 0u32;
        while j3 < S {
            let off = PR_OFF + (row * S + j3) * 4;
            sf(off, lf(off) * inv);
            j3 += 1;
        }
        row += 1;
    }

    // ── Phase 6: Output = Probs · V ────────────────────────────
    let mut oi = 0u32;
    while oi < S {
        let mut od = 0u32;
        while od < D {
            let mut acc = 0.0f32;
            let mut k2 = 0u32;
            while k2 < S {
                acc = acc + lf(PR_OFF + (oi * S + k2) * 4)
                          * lf(V_OFF  + (k2 * D + od) * 4);
                k2 += 1;
            }
            sf(OUT_OFF + (oi * D + od) * 4, acc);
            od += 1;
        }
        oi += 1;
    }

    // ── Checksum: Σ Output × 1000 ──────────────────────────────
    //
    // Returned as i32 via i32.trunc_sat_f32_s. The caller compares
    // to a Python reference computed with the same exp_approx
    // polynomial → byte-exact match expected.
    let mut sum = 0.0f32;
    let mut idx = 0u32;
    let total = S * D;
    while idx < total {
        sum = sum + lf(OUT_OFF + idx * 4);
        idx += 1;
    }
    (sum * 1000.0) as i32
}
