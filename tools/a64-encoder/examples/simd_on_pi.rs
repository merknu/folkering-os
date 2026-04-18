//! SIMD Sprint 1 — f32x4 arithmetic on Cortex-A76.
//!
//! Verifies the minimal NEON subset end-to-end: two 4-vectors are
//! staged into linear memory via DATA writes (implicit in each test
//! case via `i32.const` + `i32.store` for each lane), then a JIT
//! program loads both as v128, does a lane-wise operation, extracts
//! a chosen lane as a scalar f32, and compares to an expected value
//! via `f32.eq` — returning 1 if the whole pipeline is correct.
//!
//! Matrix of cases:
//!   * pure memory — store 4 f32 as v128 via v128.store, load as
//!     v128, extract each lane, verify
//!   * f32x4.add — lane-wise sum across two vectors
//!   * f32x4.mul — lane-wise product
//!   * f32x4.add + f32x4.mul compose — prove two V128 slots stack
//!     correctly

use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, WasmOp};

struct Case {
    name: &'static str,
    ops: Vec<WasmOp>,
    expected: u8,
}

/// Helper: emit ops that store `values[0..4]` as 4 consecutive f32s
/// starting at `offset` in linear memory. Uses individual i32.store
/// calls on the reinterpret bit-pattern so we don't need any new
/// encoder support — the existing i32.store path handles 4-byte
/// writes and bounds-checks them.
fn store_4_f32(offset: u32, values: [f32; 4]) -> Vec<WasmOp> {
    let mut out = Vec::new();
    for (i, &v) in values.iter().enumerate() {
        let addr = offset + 4 * i as u32;
        out.push(WasmOp::I32Const(addr as i32));
        out.push(WasmOp::I32Const(v.to_bits() as i32));
        out.push(WasmOp::I32Store(0));
    }
    out
}

fn cases() -> Vec<Case> {
    vec![
        // ── Pure memory round-trip ────────────────────────────────
        // Store [1.0, 2.0, 3.0, 4.0] at mem[0]. Load as v128,
        // extract lane 2 = 3.0, compare to 3.0 → 1.
        Case {
            name: "v128.load + f32x4.extract_lane(2) = 3.0",
            ops: {
                let mut ops = store_4_f32(0, [1.0, 2.0, 3.0, 4.0]);
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4ExtractLane(2),
                    WasmOp::F32Const(3.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // Extract lane 0 from the same vector → 1.0.
        Case {
            name: "v128.load + extract_lane(0) = 1.0",
            ops: {
                let mut ops = store_4_f32(0, [1.0, 2.0, 3.0, 4.0]);
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4ExtractLane(0),
                    WasmOp::F32Const(1.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // Extract lane 3 → 4.0.
        Case {
            name: "v128.load + extract_lane(3) = 4.0",
            ops: {
                let mut ops = store_4_f32(0, [1.0, 2.0, 3.0, 4.0]);
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4ExtractLane(3),
                    WasmOp::F32Const(4.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // ── f32x4.add ─────────────────────────────────────────────
        // mem[0..16]  = [1.0, 2.0, 3.0, 4.0]
        // mem[16..32] = [10.0, 20.0, 30.0, 40.0]
        // sum         = [11, 22, 33, 44]; extract_lane(1) = 22.
        Case {
            name: "f32x4.add lane 1: 2 + 20 = 22",
            ops: {
                let mut ops = store_4_f32(0, [1.0, 2.0, 3.0, 4.0]);
                ops.extend(store_4_f32(16, [10.0, 20.0, 30.0, 40.0]));
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::I32Const(16),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4Add,
                    WasmOp::F32x4ExtractLane(1),
                    WasmOp::F32Const(22.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // Same add, check lane 3 (4 + 40 = 44).
        Case {
            name: "f32x4.add lane 3: 4 + 40 = 44",
            ops: {
                let mut ops = store_4_f32(0, [1.0, 2.0, 3.0, 4.0]);
                ops.extend(store_4_f32(16, [10.0, 20.0, 30.0, 40.0]));
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::I32Const(16),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4Add,
                    WasmOp::F32x4ExtractLane(3),
                    WasmOp::F32Const(44.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // ── f32x4.mul ─────────────────────────────────────────────
        // [1.5, 2.5, 3.5, 4.5] * [2, 4, 6, 8] = [3, 10, 21, 36]
        // extract_lane(2) = 21.
        Case {
            name: "f32x4.mul lane 2: 3.5 × 6 = 21",
            ops: {
                let mut ops = store_4_f32(0, [1.5, 2.5, 3.5, 4.5]);
                ops.extend(store_4_f32(16, [2.0, 4.0, 6.0, 8.0]));
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::I32Const(16),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4Mul,
                    WasmOp::F32x4ExtractLane(2),
                    WasmOp::F32Const(21.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // ── Compose: (a + b) * c, lane-wise ───────────────────────
        // a = [1, 2, 3, 4], b = [5, 6, 7, 8], c = [10, 10, 10, 10]
        // (a+b)*c lane 0 = (1+5)*10 = 60.
        Case {
            name: "f32x4.add then f32x4.mul, lane 0: (1+5)*10 = 60",
            ops: {
                let mut ops = store_4_f32(0, [1.0, 2.0, 3.0, 4.0]);
                ops.extend(store_4_f32(16, [5.0, 6.0, 7.0, 8.0]));
                ops.extend(store_4_f32(32, [10.0, 10.0, 10.0, 10.0]));
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),     // a
                    WasmOp::I32Const(16),
                    WasmOp::V128Load(0),     // b
                    WasmOp::F32x4Add,        // a+b
                    WasmOp::I32Const(32),
                    WasmOp::V128Load(0),     // c
                    WasmOp::F32x4Mul,        // (a+b)*c
                    WasmOp::F32x4ExtractLane(0),
                    WasmOp::F32Const(60.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // ── f32x4.splat ───────────────────────────────────────────
        // Broadcast 7.25 to 4 lanes. Extract lane 2 → must be 7.25.
        Case {
            name: "f32x4.splat 7.25 → lane 2 = 7.25",
            ops: vec![
                WasmOp::F32Const(7.25),
                WasmOp::F32x4Splat,
                WasmOp::F32x4ExtractLane(2),
                WasmOp::F32Const(7.25),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── i32x4.splat + add + extract_lane ──────────────────────
        // Splat 10 → [10,10,10,10], splat 5 → [5,5,5,5], add → [15]×4,
        // extract lane 0 → 15.
        Case {
            name: "i32x4.splat(10) + splat(5) → extract lane 0 = 15",
            ops: vec![
                WasmOp::I32Const(10),
                WasmOp::I32x4Splat,
                WasmOp::I32Const(5),
                WasmOp::I32x4Splat,
                WasmOp::I32x4Add,
                WasmOp::I32x4ExtractLane(0),
                WasmOp::End,
            ],
            expected: 15,
        },
        // ── i32x4.sub ─────────────────────────────────────────────
        // splat(100) - splat(58) = 42 in every lane, extract lane 3.
        Case {
            name: "i32x4.sub splat(100) - splat(58) → lane 3 = 42",
            ops: vec![
                WasmOp::I32Const(100),
                WasmOp::I32x4Splat,
                WasmOp::I32Const(58),
                WasmOp::I32x4Splat,
                WasmOp::I32x4Sub,
                WasmOp::I32x4ExtractLane(3),
                WasmOp::End,
            ],
            expected: 42,
        },
        // ── i32x4.mul ─────────────────────────────────────────────
        // splat(6) * splat(7) = 42, extract lane 1.
        Case {
            name: "i32x4.mul splat(6) * splat(7) → lane 1 = 42",
            ops: vec![
                WasmOp::I32Const(6),
                WasmOp::I32x4Splat,
                WasmOp::I32Const(7),
                WasmOp::I32x4Splat,
                WasmOp::I32x4Mul,
                WasmOp::I32x4ExtractLane(1),
                WasmOp::End,
            ],
            expected: 42,
        },
        // ── Mixed i32x4 vector from memory ────────────────────────
        // Write [10, 20, 30, 40] as four consecutive i32s at mem[0],
        // load as v128, extract lane 2 → 30.
        Case {
            name: "i32x4 mem load → extract_lane(2) = 30",
            ops: {
                let mut ops = Vec::new();
                for (i, &v) in [10i32, 20, 30, 40].iter().enumerate() {
                    ops.push(WasmOp::I32Const((4 * i) as i32));
                    ops.push(WasmOp::I32Const(v));
                    ops.push(WasmOp::I32Store(0));
                }
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::I32x4ExtractLane(2),
                    WasmOp::End,
                ]);
                ops
            },
            expected: 30,
        },
        // ── f32x4.sub ─────────────────────────────────────────────
        // splat(100) - splat(58) = 42.0 (each lane). Extract lane 0.
        Case {
            name: "f32x4.sub splat(100.0) - splat(58.0) → lane 0 = 42.0",
            ops: vec![
                WasmOp::F32Const(100.0),
                WasmOp::F32x4Splat,
                WasmOp::F32Const(58.0),
                WasmOp::F32x4Splat,
                WasmOp::F32x4Sub,
                WasmOp::F32x4ExtractLane(0),
                WasmOp::F32Const(42.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── f32x4.div ─────────────────────────────────────────────
        // splat(42) / splat(2) = 21.0 per lane.
        Case {
            name: "f32x4.div splat(42.0) / splat(2.0) → lane 2 = 21.0",
            ops: vec![
                WasmOp::F32Const(42.0),
                WasmOp::F32x4Splat,
                WasmOp::F32Const(2.0),
                WasmOp::F32x4Splat,
                WasmOp::F32x4Div,
                WasmOp::F32x4ExtractLane(2),
                WasmOp::F32Const(21.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── f32x4.fma — the GEMM primitive ────────────────────────
        // acc = splat(10), a = splat(3), b = splat(4) → acc + a*b = 22
        // Per lane: 10 + 3 * 4 = 22.
        Case {
            name: "f32x4.fma: 10 + 3*4 = 22, lane 0",
            ops: vec![
                WasmOp::F32Const(10.0),
                WasmOp::F32x4Splat,
                WasmOp::F32Const(3.0),
                WasmOp::F32x4Splat,
                WasmOp::F32Const(4.0),
                WasmOp::F32x4Splat,
                WasmOp::F32x4Fma,
                WasmOp::F32x4ExtractLane(0),
                WasmOp::F32Const(22.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── f32x4.fma chain — mimics an inner GEMM loop ───────────
        // acc = splat(0)
        // acc = acc + splat(2) * splat(5)   = 10
        // acc = acc + splat(3) * splat(7)   = 31
        // acc = acc + splat(1) * splat(11)  = 42
        // Extract lane 1, compare to 42.0.
        Case {
            name: "f32x4.fma chain: 2*5 + 3*7 + 1*11 + 0 = 42, lane 1",
            ops: vec![
                WasmOp::F32Const(0.0),
                WasmOp::F32x4Splat,
                // iter 1
                WasmOp::F32Const(2.0),
                WasmOp::F32x4Splat,
                WasmOp::F32Const(5.0),
                WasmOp::F32x4Splat,
                WasmOp::F32x4Fma,
                // iter 2
                WasmOp::F32Const(3.0),
                WasmOp::F32x4Splat,
                WasmOp::F32Const(7.0),
                WasmOp::F32x4Splat,
                WasmOp::F32x4Fma,
                // iter 3
                WasmOp::F32Const(1.0),
                WasmOp::F32x4Splat,
                WasmOp::F32Const(11.0),
                WasmOp::F32x4Splat,
                WasmOp::F32x4Fma,
                // extract + compare
                WasmOp::F32x4ExtractLane(1),
                WasmOp::F32Const(42.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── f32x4.horizontal_sum (Folkering extension) ───────────
        // splat(3.25) → 4 × 3.25 = 13.0. Use as baseline.
        Case {
            name: "f32x4.horizontal_sum splat(3.25) = 13.0",
            ops: vec![
                WasmOp::F32Const(3.25),
                WasmOp::F32x4Splat,
                WasmOp::F32x4HorizontalSum,
                WasmOp::F32Const(13.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // Heterogeneous vector: [1, 2, 3, 4] → 10.
        Case {
            name: "f32x4.horizontal_sum [1,2,3,4] = 10.0",
            ops: {
                let mut ops = store_4_f32(0, [1.0, 2.0, 3.0, 4.0]);
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4HorizontalSum,
                    WasmOp::F32Const(10.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // ── Real matvec: 4×4 @ 4×1 via FMA accumulator ────────────
        //
        // This is the shape of a transformer attention step's
        // inner loop — a micro-GEMM. Proves the full SIMD stack
        // (load, splat, FMA, extract) composes into a real ML
        // primitive on Cortex-A76 hardware.
        //
        // Matrix A (4×4, stored column-major in mem):
        //   col0 @ mem[0..16]   = [1, 5,  9, 13]
        //   col1 @ mem[16..32]  = [2, 6, 10, 14]
        //   col2 @ mem[32..48]  = [3, 7, 11, 15]
        //   col3 @ mem[48..64]  = [4, 8, 12, 16]
        //
        // x = [1, 1, 1, 1]  (implicit — we splat(1.0) for each col)
        //
        // y = A @ x = [10, 26, 42, 58] (sum of each row)
        //
        // Computed as:
        //   acc = splat(0)
        //   for j in 0..4:
        //     acc += splat(x[j]) * A[:, j]   // FMA
        //   return acc   (4-lane result)
        //
        // Extract lane 1 → 5+6+7+8 = 26.
        Case {
            name: "matvec 4x4 @ [1,1,1,1]: FMA accumulator → lane 1 = 26",
            ops: {
                // Store A column-major.
                let mut ops = store_4_f32(0,  [1.0,  5.0,  9.0, 13.0]);
                ops.extend(store_4_f32(16,   [2.0,  6.0, 10.0, 14.0]));
                ops.extend(store_4_f32(32,   [3.0,  7.0, 11.0, 15.0]));
                ops.extend(store_4_f32(48,   [4.0,  8.0, 12.0, 16.0]));
                // acc = splat(0)
                ops.push(WasmOp::F32Const(0.0));
                ops.push(WasmOp::F32x4Splat);
                // Fold in each column: acc += splat(x[j]) * A[:, j]
                for j in 0..4u32 {
                    // x[j] = 1.0 for every j in this test
                    ops.push(WasmOp::F32Const(1.0));
                    ops.push(WasmOp::F32x4Splat);
                    // load column j
                    ops.push(WasmOp::I32Const((16 * j) as i32));
                    ops.push(WasmOp::V128Load(0));
                    // FMA expects stack: [acc, a, b] with top=b. We
                    // have [acc, splat(x[j]), col] — that's [acc, a, b]
                    // correctly ordered.
                    ops.push(WasmOp::F32x4Fma);
                }
                // Extract lane 1, compare to 26.0.
                ops.push(WasmOp::F32x4ExtractLane(1));
                ops.push(WasmOp::F32Const(26.0));
                ops.push(WasmOp::F32Eq);
                ops.push(WasmOp::End);
                ops
            },
            expected: 1,
        },
        // Dot product of two 4-vectors via mul + horizontal_sum.
        // u = [1, 2, 3, 4], v = [5, 6, 7, 8]
        // dot(u, v) = 5 + 12 + 21 + 32 = 70.
        Case {
            name: "dot([1,2,3,4], [5,6,7,8]) = 70",
            ops: {
                let mut ops = store_4_f32(0,  [1.0, 2.0, 3.0, 4.0]);
                ops.extend(store_4_f32(16,   [5.0, 6.0, 7.0, 8.0]));
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::I32Const(16),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4Mul,
                    WasmOp::F32x4HorizontalSum,
                    WasmOp::F32Const(70.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // ── ReLU — f32x4.max(x, splat(0)) ─────────────────────────
        //
        // Input vector x = [-1.5, 2.5, -3.75, 4.0].
        // ReLU(x) = [max(-1.5, 0), max(2.5, 0), max(-3.75, 0), max(4.0, 0)]
        //         = [0.0, 2.5, 0.0, 4.0]
        // Check lane 0 → 0.0 (negative clamped).
        Case {
            name: "ReLU: f32x4.max(x, splat(0)) lane 0: max(-1.5, 0) = 0.0",
            ops: vec![
                WasmOp::V128Const(
                    (((-1.5_f32).to_bits() as u128) <<   0)
                  | (( 2.5_f32 .to_bits() as u128) <<  32)
                  | (((-3.75_f32).to_bits() as u128) << 64)
                  | (( 4.0_f32 .to_bits() as u128) <<  96),
                ),
                WasmOp::F32Const(0.0),
                WasmOp::F32x4Splat,
                WasmOp::F32x4Max,
                WasmOp::F32x4ExtractLane(0),
                WasmOp::F32Const(0.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // Lane 1 should be UNCLAMPED (2.5 stays 2.5).
        Case {
            name: "ReLU lane 1: max(2.5, 0) = 2.5 (positive pass-through)",
            ops: vec![
                WasmOp::V128Const(
                    (((-1.5_f32).to_bits() as u128) <<   0)
                  | (( 2.5_f32 .to_bits() as u128) <<  32)
                  | (((-3.75_f32).to_bits() as u128) << 64)
                  | (( 4.0_f32 .to_bits() as u128) <<  96),
                ),
                WasmOp::F32Const(0.0),
                WasmOp::F32x4Splat,
                WasmOp::F32x4Max,
                WasmOp::F32x4ExtractLane(1),
                WasmOp::F32Const(2.5),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── Clamp: min(max(x, lo), hi) ─────────────────────────────
        // x = [-5, 2, 10, 0.5], lo = 0, hi = 3
        // max(x, 0)            = [0, 2, 10, 0.5]
        // min([0,2,10,0.5], 3) = [0, 2, 3, 0.5]
        // Lane 2 = 3 (clipped from 10).
        Case {
            name: "Clamp: min(max(x, 0), 3) lane 2: clip(10) = 3.0",
            ops: vec![
                WasmOp::V128Const(
                    (((-5.0_f32).to_bits() as u128) <<   0)
                  | (( 2.0_f32 .to_bits() as u128) <<  32)
                  | ((10.0_f32 .to_bits() as u128) <<  64)
                  | (( 0.5_f32 .to_bits() as u128) <<  96),
                ),
                WasmOp::F32Const(0.0),
                WasmOp::F32x4Splat,
                WasmOp::F32x4Max,
                WasmOp::F32Const(3.0),
                WasmOp::F32x4Splat,
                WasmOp::F32x4Min,
                WasmOp::F32x4ExtractLane(2),
                WasmOp::F32Const(3.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── Unary: f32x4.abs ─────────────────────────────────────
        // abs([-1.5, 2.5, -3.75, 4.0]) = [1.5, 2.5, 3.75, 4.0]
        // Check lane 2 → 3.75 (negative flipped).
        Case {
            name: "f32x4.abs lane 2: |-3.75| = 3.75",
            ops: vec![
                WasmOp::V128Const(
                    (((-1.5_f32).to_bits() as u128) <<   0)
                  | (( 2.5_f32 .to_bits() as u128) <<  32)
                  | (((-3.75_f32).to_bits() as u128) << 64)
                  | (( 4.0_f32 .to_bits() as u128) <<  96),
                ),
                WasmOp::F32x4Abs,
                WasmOp::F32x4ExtractLane(2),
                WasmOp::F32Const(3.75),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── Unary: f32x4.neg ─────────────────────────────────────
        // neg(splat(7.0)) lane 0 = -7.0.
        Case {
            name: "f32x4.neg splat(7.0) lane 0 = -7.0",
            ops: vec![
                WasmOp::F32Const(7.0),
                WasmOp::F32x4Splat,
                WasmOp::F32x4Neg,
                WasmOp::F32x4ExtractLane(0),
                WasmOp::F32Const(-7.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── f32x4.sqrt + horizontal_sum: L2 norm of [3, 4, 0, 0] = 5 ─
        // L2 norm: sqrt(sum(x²))  = sqrt(9 + 16 + 0 + 0) = sqrt(25) = 5.
        // Build as: x², horizontal_sum, scalar sqrt via x² trick
        // (f32.mul self + f32.sqrt not directly in our scalar ISA —
        // but f32x4.sqrt on a splat of the scalar works).
        //
        //   load [3,4,0,0], square via f32x4.mul self*self,
        //   horizontal_sum → 25.0 (scalar),
        //   f32x4.splat to get [25,25,25,25],
        //   f32x4.sqrt → [5,5,5,5],
        //   extract_lane 0 = 5.
        Case {
            name: "L2 norm via sqrt: ||[3,4,0,0]||₂ = 5",
            ops: {
                let mut ops = store_4_f32(0, [3.0, 4.0, 0.0, 0.0]);
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    // x²: multiply vector by itself
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4Mul,
                    // Sum of squares
                    WasmOp::F32x4HorizontalSum,
                    // Splat scalar back to a vector so we can vector-sqrt
                    WasmOp::F32x4Splat,
                    WasmOp::F32x4Sqrt,
                    // Extract lane 0 and compare to 5.0
                    WasmOp::F32x4ExtractLane(0),
                    WasmOp::F32Const(5.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // ── Compare + bitselect — emulated max via mask ──────────
        //
        // Reimplement f32x4.max(a, b) as:
        //   mask = f32x4.gt(a, b)           // bitmask: 1s where a>b
        //   result = v128.bitselect(a, b, mask)
        //                                   // where mask=1 → a; else b
        //
        // Input: a = [1, 5, 3, 7], b = [2, 4, 10, 0]
        //   gt(a, b)          = [0, 1, 0, 1]  (all-0/all-1 masks)
        //   select(a, b, mask) = [2, 5, 10, 7]
        // Check lane 0 → 2 (a < b, should pick b since mask=0 → v2).
        Case {
            name: "bitselect as max: lane 0 where a=1 < b=2 → 2",
            ops: {
                let mut ops = store_4_f32(0,  [1.0, 5.0,  3.0, 7.0]);
                ops.extend(store_4_f32(16,   [2.0, 4.0, 10.0, 0.0]));
                ops.extend_from_slice(&[
                    // Push v1 = a
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    // Push v2 = b
                    WasmOp::I32Const(16),
                    WasmOp::V128Load(0),
                    // Build mask on top: push a and b again, compare.
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::I32Const(16),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4Gt,        // top = a > b mask
                    // Stack: [a, b, mask]
                    WasmOp::V128Bitselect,  // → max(a, b) per lane
                    WasmOp::F32x4ExtractLane(0),
                    WasmOp::F32Const(2.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // Same pattern, lane 1 where a > b → picks a = 5.
        Case {
            name: "bitselect as max: lane 1 where a=5 > b=4 → 5",
            ops: {
                let mut ops = store_4_f32(0,  [1.0, 5.0,  3.0, 7.0]);
                ops.extend(store_4_f32(16,   [2.0, 4.0, 10.0, 0.0]));
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::I32Const(16),
                    WasmOp::V128Load(0),
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::I32Const(16),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4Gt,
                    WasmOp::V128Bitselect,
                    WasmOp::F32x4ExtractLane(1),
                    WasmOp::F32Const(5.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // f32x4.eq as equality mask. Splat(3.0) vs [3, 0, 3, 0] →
        // mask lanes = [-1, 0, -1, 0] (as i32 bitmask). Use bitselect
        // with splat(100), splat(0) → [100, 0, 100, 0]. Extract lane 2 → 100.
        Case {
            name: "f32x4.eq + bitselect: splat(100) where lane==3, else 0; lane 2 = 100",
            ops: {
                let mut ops = store_4_f32(0, [3.0, 0.0, 3.0, 0.0]);
                ops.extend_from_slice(&[
                    // Push v1 = splat(100), v2 = splat(0)
                    WasmOp::F32Const(100.0),
                    WasmOp::F32x4Splat,
                    WasmOp::F32Const(0.0),
                    WasmOp::F32x4Splat,
                    // mask = eq(load(0), splat(3))
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::F32Const(3.0),
                    WasmOp::F32x4Splat,
                    WasmOp::F32x4Eq,
                    // Stack: [splat(100), splat(0), mask]
                    WasmOp::V128Bitselect,
                    WasmOp::F32x4ExtractLane(2),
                    WasmOp::F32Const(100.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
        // ── v128.const — inline 16-byte literal ─────────────────────
        //
        // Materializes a 128-bit constant via the PC-relative literal
        // pool. The f32 lanes [1.0, 2.0, 3.0, 4.0] encoded as u128
        // (little-endian byte order):
        //   lane 0 bits = 0x3F800000 (1.0)
        //   lane 1 bits = 0x40000000 (2.0)
        //   lane 2 bits = 0x40400000 (3.0)
        //   lane 3 bits = 0x40800000 (4.0)
        // Concatenated LE: 00_00_80_3F 00_00_00_40 00_00_40_40 00_00_80_40
        // As u128 (LSB first): 0x40800000_40400000_40000000_3F800000
        Case {
            name: "v128.const [1,2,3,4] extract lane 2 = 3.0",
            ops: vec![
                WasmOp::V128Const(
                    ((1.0_f32.to_bits() as u128) <<   0)
                  | ((2.0_f32.to_bits() as u128) <<  32)
                  | ((3.0_f32.to_bits() as u128) <<  64)
                  | ((4.0_f32.to_bits() as u128) <<  96),
                ),
                WasmOp::F32x4ExtractLane(2),
                WasmOp::F32Const(3.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // Combine two const vectors via FMA:
        //   acc = const [0,0,0,0]
        //   a   = const [2,2,2,2]
        //   b   = const [3,3,3,3]
        //   acc += a * b  →  [6,6,6,6]
        //   extract lane 1 → 6.
        Case {
            name: "v128.const × 3 + FMA: 0 + 2*3 → lane 1 = 6",
            ops: vec![
                WasmOp::V128Const(0), // [0,0,0,0]
                WasmOp::V128Const(
                    ((2.0_f32.to_bits() as u128) <<   0)
                  | ((2.0_f32.to_bits() as u128) <<  32)
                  | ((2.0_f32.to_bits() as u128) <<  64)
                  | ((2.0_f32.to_bits() as u128) <<  96),
                ),
                WasmOp::V128Const(
                    ((3.0_f32.to_bits() as u128) <<   0)
                  | ((3.0_f32.to_bits() as u128) <<  32)
                  | ((3.0_f32.to_bits() as u128) <<  64)
                  | ((3.0_f32.to_bits() as u128) <<  96),
                ),
                WasmOp::F32x4Fma,
                WasmOp::F32x4ExtractLane(1),
                WasmOp::F32Const(6.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── f64x2 — double-precision SIMD ────────────────────────
        // Store two f64 values as bit patterns (2 × i64.store).
        // Then v128.load + f64x2 ops + extract_lane.
        // [1.5, 2.5] + [10.0, 20.0] = [11.5, 22.5].
        // Extract lane 0 = 11.5; compare to 11.5 → 1.
        Case {
            name: "f64x2.add [1.5,2.5]+[10,20] lane 0 = 11.5",
            ops: {
                // Store f64 values as i64 bit patterns
                let a0 = 1.5_f64.to_bits() as i64;
                let a1 = 2.5_f64.to_bits() as i64;
                let b0 = 10.0_f64.to_bits() as i64;
                let b1 = 20.0_f64.to_bits() as i64;
                vec![
                    WasmOp::I32Const(0), WasmOp::I64Const(a0), WasmOp::I64Store(0),
                    WasmOp::I32Const(8), WasmOp::I64Const(a1), WasmOp::I64Store(0),
                    WasmOp::I32Const(16), WasmOp::I64Const(b0), WasmOp::I64Store(0),
                    WasmOp::I32Const(24), WasmOp::I64Const(b1), WasmOp::I64Store(0),
                    WasmOp::I32Const(0), WasmOp::V128Load(0),
                    WasmOp::I32Const(16), WasmOp::V128Load(0),
                    WasmOp::F64x2Add,
                    WasmOp::F64x2ExtractLane(0),
                    WasmOp::F64Const(11.5),
                    WasmOp::F64Eq,
                    WasmOp::End,
                ]
            },
            expected: 1,
        },
        // f64x2.mul lane 1: 2.5 * 20.0 = 50.0
        Case {
            name: "f64x2.mul [1.5,2.5]*[10,20] lane 1 = 50.0",
            ops: {
                let a0 = 1.5_f64.to_bits() as i64;
                let a1 = 2.5_f64.to_bits() as i64;
                let b0 = 10.0_f64.to_bits() as i64;
                let b1 = 20.0_f64.to_bits() as i64;
                vec![
                    WasmOp::I32Const(0), WasmOp::I64Const(a0), WasmOp::I64Store(0),
                    WasmOp::I32Const(8), WasmOp::I64Const(a1), WasmOp::I64Store(0),
                    WasmOp::I32Const(16), WasmOp::I64Const(b0), WasmOp::I64Store(0),
                    WasmOp::I32Const(24), WasmOp::I64Const(b1), WasmOp::I64Store(0),
                    WasmOp::I32Const(0), WasmOp::V128Load(0),
                    WasmOp::I32Const(16), WasmOp::V128Load(0),
                    WasmOp::F64x2Mul,
                    WasmOp::F64x2ExtractLane(1),
                    WasmOp::F64Const(50.0),
                    WasmOp::F64Eq,
                    WasmOp::End,
                ]
            },
            expected: 1,
        },
        // ── i8x16 — packed byte SIMD ──────────────────────────────
        // splat(10) + splat(32) = 42 per byte. Extract lane 7.
        Case {
            name: "i8x16.add splat(10)+splat(32) lane 7 = 42",
            ops: vec![
                WasmOp::I32Const(10),
                WasmOp::I8x16Splat,
                WasmOp::I32Const(32),
                WasmOp::I8x16Splat,
                WasmOp::I8x16Add,
                WasmOp::I8x16ExtractLaneU(7),
                WasmOp::End,
            ],
            expected: 42,
        },
        // ── i16x8 — packed halfword SIMD ──────────────────────────
        // splat(300) + splat(200) = 500 → low byte = 244 (500 & 0xFF)
        // But we compare i32 result directly. 500 & 0xFF = 0xF4 = 244.
        Case {
            name: "i16x8.add splat(6)*splat(7) lane 3 = 42",
            ops: vec![
                WasmOp::I32Const(6),
                WasmOp::I16x8Splat,
                WasmOp::I32Const(7),
                WasmOp::I16x8Splat,
                WasmOp::I16x8Mul,
                WasmOp::I16x8ExtractLaneU(3),
                WasmOp::End,
            ],
            expected: 42,
        },
        // f64x2.splat + f64x2.sqrt: sqrt([4.0, 4.0]) = [2.0, 2.0]
        Case {
            name: "f64x2.splat(4.0) → sqrt → lane 0 = 2.0",
            ops: vec![
                WasmOp::F64Const(4.0),
                WasmOp::F64x2Splat,
                WasmOp::F64x2Sqrt,
                WasmOp::F64x2ExtractLane(0),
                WasmOp::F64Const(2.0),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── v128.store round-trip ─────────────────────────────────
        // Load a vector from mem[0], store it to mem[48], then read
        // back mem[48] as v128, extract lane 1, compare to original.
        Case {
            name: "v128.store round-trip: lane 1 of stored copy = 2.0",
            ops: {
                let mut ops = store_4_f32(0, [1.0, 2.0, 3.0, 4.0]);
                ops.extend_from_slice(&[
                    WasmOp::I32Const(0),
                    WasmOp::V128Load(0),
                    WasmOp::I32Const(48),
                    // Stack: [..., v128]. Need [addr, v128] for store.
                    // Pre-push addr before the value: reorder the ops.
                ]);
                // The WASM convention is "push addr, push val, store".
                // Let me redo the last 3 ops cleanly:
                ops.truncate(ops.len() - 3);
                ops.extend_from_slice(&[
                    WasmOp::I32Const(48),        // store addr
                    WasmOp::I32Const(0),         // load addr
                    WasmOp::V128Load(0),         // v
                    WasmOp::V128Store(0),        // mem[48] = v
                    // Now read back and extract.
                    WasmOp::I32Const(48),
                    WasmOp::V128Load(0),
                    WasmOp::F32x4ExtractLane(1),
                    WasmOp::F32Const(2.0),
                    WasmOp::F32Eq,
                    WasmOp::End,
                ]);
                ops
            },
            expected: 1,
        },
    ]
}

fn run_on_pi(host: &str, bytes: &[u8]) -> Result<i32, String> {
    let mut child = Command::new("ssh")
        .arg(host)
        .arg("~/a64-harness/run_bytes")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(bytes)
        .map_err(|e| format!("write: {e}"))?;
    drop(child.stdin.take());
    let out = child.wait_with_output().map_err(|e| format!("wait: {e}"))?;
    if !out.stderr.is_empty() {
        eprintln!("stderr: {}", String::from_utf8_lossy(&out.stderr));
    }
    out.status.code().ok_or_else(|| "no exit code".into())
}

fn query_mem_base(host: &str) -> Result<u64, String> {
    let out = Command::new("ssh")
        .arg(host)
        .arg("~/a64-harness/run_bytes --addrs")
        .output()
        .map_err(|e| format!("ssh: {e}"))?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k == "mem_base" {
                let v = v.trim().trim_start_matches("0x").trim_start_matches("0X");
                return u64::from_str_radix(v, 16).map_err(|e| format!("parse: {e}"));
            }
        }
    }
    Err("mem_base not found".into())
}

fn main() {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "knut@192.168.68.72".to_string());

    println!("SIMD Sprint 1 — f32x4 on {}\n", host);

    let mem_base = match query_mem_base(&host) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mem_base query failed: {e}");
            std::process::exit(2);
        }
    };
    println!("mem_base at 0x{:016x}\n", mem_base);

    let cases = cases();
    let mut passed = 0;
    let mut failed = 0;

    for case in &cases {
        let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), mem_base).unwrap();
        if let Err(e) = lw.lower_all(&case.ops) {
            println!("  [err ] {}: lower: {:?}", case.name, e);
            failed += 1;
            continue;
        }
        let bytes = lw.finish();

        match run_on_pi(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == case.expected {
                    println!("  [ ok ] {}  ({} bytes)", case.name, bytes.len());
                    passed += 1;
                } else {
                    println!(
                        "  [FAIL] {}: expected {} (0x{:02X}), got {} (0x{:02X})",
                        case.name, case.expected, case.expected, got, got
                    );
                    failed += 1;
                }
            }
            Err(e) => {
                println!("  [err ] {}: {}", case.name, e);
                failed += 1;
            }
        }
    }

    println!("\n{} passed, {} failed", passed, failed);
    if failed > 0 {
        std::process::exit(1);
    }
}
