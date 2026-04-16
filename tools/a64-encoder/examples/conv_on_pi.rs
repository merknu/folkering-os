//! Phase 15 — conversions & sign-extensions on Cortex-A76.
//!
//! Covers the full WASM conversion surface:
//!   * sign-extensions: i32/i64 extend{8,16,32}_s
//!   * FP → INT: trunc_f{32,64}_{s,u} for both i32 and i64 targets
//!   * INT → FP: convert_i{32,64}_{s,u} for both f32 and f64 targets
//!   * FP ↔ FP: f32.demote_f64, f64.promote_f32
//!   * bit-cast reinterpret: 4 variants (free via FMOV)
//!
//! Each case produces a boolean i32 on the stack so the 8-bit exit
//! code comparison is clean. Float comparisons use exact IEEE-754
//! values (no denormal corner cases).

use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, WasmOp};

struct Case {
    name: &'static str,
    ops: Vec<WasmOp>,
    expected: u8,
}

fn cases() -> Vec<Case> {
    vec![
        // ── Sign extensions ──────────────────────────────────────────
        // 0x80 as i8 = -128; sign-extend to i32 = -128 = 0xFFFFFF80.
        // Compare to -128 → 1.
        Case {
            name: "i32.extend8_s(0x80) == -128 → 1",
            ops: vec![
                WasmOp::I32Const(0x80),
                WasmOp::I32Extend8S,
                WasmOp::I32Const(-128),
                WasmOp::I32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // 0x8000 as i16 = -32768; extend16_s to i32 = -32768.
        Case {
            name: "i32.extend16_s(0x8000) == -32768 → 1",
            ops: vec![
                WasmOp::I32Const(0x8000),
                WasmOp::I32Extend16S,
                WasmOp::I32Const(-32768),
                WasmOp::I32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // i64 path: low byte 0xFF of an i64 should sign-extend to -1.
        Case {
            name: "i64.extend8_s(0xFF) == -1_i64 → 1",
            ops: vec![
                WasmOp::I64Const(0xFF),
                WasmOp::I64Extend8S,
                WasmOp::I64Const(-1),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // i64.extend32_s: 0x8000_0000 treated as i32 = -2147483648 → i64.
        Case {
            name: "i64.extend32_s(0x80000000) == -2147483648_i64 → 1",
            ops: vec![
                WasmOp::I64Const(0x8000_0000),
                WasmOp::I64Extend32S,
                WasmOp::I64Const(-2_147_483_648),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── FP → INT (trunc, round toward zero) ─────────────────────
        // 3.7 → 3 (truncates toward zero, not rounds).
        Case {
            name: "i32.trunc_f32_s(3.7) == 3 → 3",
            ops: vec![
                WasmOp::F32Const(3.7),
                WasmOp::I32TruncF32S,
                WasmOp::End,
            ],
            expected: 3,
        },
        // -3.7 → -3 (toward zero).
        Case {
            name: "i32.trunc_f32_s(-3.7) == -3 → 1",
            ops: vec![
                WasmOp::F32Const(-3.7),
                WasmOp::I32TruncF32S,
                WasmOp::I32Const(-3),
                WasmOp::I32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // f64 precision: 1e15 + 0.5 truncates to 1e15 as i64 (f64 has
        // enough mantissa to represent 1e15 exactly).
        Case {
            name: "i64.trunc_f64_s(1e15 + 0.25) == 1e15 → 1",
            ops: vec![
                WasmOp::F64Const(1e15 + 0.25),
                WasmOp::I64TruncF64S,
                WasmOp::I64Const(1_000_000_000_000_000),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // trunc_f32_u truncates positive floats to unsigned int.
        //   4294967040.0 (close to 2^32 - 256) → 0xFFFFFF00 (u32).
        Case {
            name: "i32.trunc_f32_u(4_294_967_040.0) == 0xFFFFFF00 → 1",
            ops: vec![
                WasmOp::F32Const(4_294_967_040.0),
                WasmOp::I32TruncF32U,
                WasmOp::I32Const(0xFFFFFF00u32 as i32),
                WasmOp::I32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── INT → FP ─────────────────────────────────────────────────
        Case {
            name: "f32.convert_i32_s(-5) == -5.0 → 1",
            ops: vec![
                WasmOp::I32Const(-5),
                WasmOp::F32ConvertI32S,
                WasmOp::F32Const(-5.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // Unsigned: -1_i32 treated as u32 = 4294967295.0 in f64.
        // f32 cannot represent 4294967295 exactly; use f64.
        Case {
            name: "f64.convert_i32_u(-1) == 4294967295.0_f64 → 1",
            ops: vec![
                WasmOp::I32Const(-1),
                WasmOp::F64ConvertI32U,
                WasmOp::F64Const(4_294_967_295.0),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // Large i64 → f64. 2^53 fits exactly.
        Case {
            name: "f64.convert_i64_s(2^53) exact → 1",
            ops: vec![
                WasmOp::I64Const(1i64 << 53),
                WasmOp::F64ConvertI64S,
                WasmOp::F64Const((1u64 << 53) as f64),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── FP ↔ FP ──────────────────────────────────────────────────
        // promote f32 1.5 → f64 1.5 (exact, f32 is subset of f64).
        Case {
            name: "f64.promote_f32(1.5) == 1.5_f64 → 1",
            ops: vec![
                WasmOp::F32Const(1.5),
                WasmOp::F64PromoteF32,
                WasmOp::F64Const(1.5),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // demote lossy: f64 0.1 → f32 0.1 rounds; compare to f32 0.1.
        Case {
            name: "f32.demote_f64(0.1_f64) == 0.1_f32 → 1",
            ops: vec![
                WasmOp::F64Const(0.1),
                WasmOp::F32DemoteF64,
                WasmOp::F32Const(0.1_f32),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── Reinterpret (bit-cast) ──────────────────────────────────
        // f32 1.0 has bit pattern 0x3F800000.
        Case {
            name: "i32.reinterpret_f32(1.0) == 0x3F800000 → 1",
            ops: vec![
                WasmOp::F32Const(1.0),
                WasmOp::I32ReinterpretF32,
                WasmOp::I32Const(0x3F80_0000),
                WasmOp::I32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // f64 2.0 has bit pattern 0x4000000000000000.
        Case {
            name: "i64.reinterpret_f64(2.0) == 0x40..00 → 1",
            ops: vec![
                WasmOp::F64Const(2.0),
                WasmOp::I64ReinterpretF64,
                WasmOp::I64Const(0x4000_0000_0000_0000i64),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // Reverse: make an f32 from its bit pattern. 0x40490FDB ≈ π.
        Case {
            name: "f32.reinterpret_i32(0x40490FDB) == π_f32 → 1",
            ops: vec![
                WasmOp::I32Const(0x4049_0FDBu32 as i32),
                WasmOp::F32ReinterpretI32,
                WasmOp::F32Const(f32::from_bits(0x4049_0FDB)),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // Round-trip: f64 → i64 bits → f64 identity.
        Case {
            name: "f64 reinterpret round-trip (e) → 1",
            ops: vec![
                WasmOp::F64Const(std::f64::consts::E),
                WasmOp::I64ReinterpretF64,
                WasmOp::F64ReinterpretI64,
                WasmOp::F64Const(std::f64::consts::E),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // ── Cross-type chain: prove the lowerer routes bank-swaps ───
        // Convert i32 5 → f32 5.0 → f64 5.0 → i64 5 → low byte 5.
        Case {
            name: "i32 → f32 → f64 → i64 round-trip: 5",
            ops: vec![
                WasmOp::I32Const(5),
                WasmOp::F32ConvertI32S,
                WasmOp::F64PromoteF32,
                WasmOp::I64TruncF64S,
                WasmOp::I32WrapI64,
                WasmOp::End,
            ],
            expected: 5,
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
    child.stdin.as_mut().unwrap().write_all(bytes).map_err(|e| format!("write: {e}"))?;
    drop(child.stdin.take());
    let out = child.wait_with_output().map_err(|e| format!("wait: {e}"))?;
    if !out.stderr.is_empty() {
        eprintln!("stderr: {}", String::from_utf8_lossy(&out.stderr));
    }
    out.status.code().ok_or_else(|| "no exit code".into())
}

fn main() {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "knut@192.168.68.72".to_string());

    println!("Phase 15 — conversions on {}\n", host);

    let cases = cases();
    let mut passed = 0;
    let mut failed = 0;

    for case in &cases {
        let mut lw = Lowerer::new();

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
