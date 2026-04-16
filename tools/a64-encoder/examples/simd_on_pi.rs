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
