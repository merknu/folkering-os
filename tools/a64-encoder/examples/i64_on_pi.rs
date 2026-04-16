//! Phase 12 — i64 arithmetic + conversions on Cortex-A76.
//!
//! i64 and i32 share the X register bank; the instruction width
//! (ADD vs AND W vs CMP X) distinguishes them. The lowerer routes
//! each WasmOp to the right-width encoder via the typed operand
//! stack.
//!
//! Exit codes are only 8 bits so tests either (a) mask to the low
//! byte or (b) use i32.wrap_i64 + compare to produce a 0/1 result
//! that fits cleanly.

use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, ValType, WasmOp};

struct Case {
    name: &'static str,
    ops: Vec<WasmOp>,
    expected: u8,
}

fn cases() -> Vec<Case> {
    vec![
        // Low byte of a large i64 constant through wrap_i64.
        //   i64.const 0x0000_1234_5678_ABCD  →  wrap_i64  →  0xCD
        Case {
            name: "i64.const 0x1234_5678_ABCD → wrap → 0xCD",
            ops: vec![
                WasmOp::I64Const(0x0000_1234_5678_ABCDi64),
                WasmOp::I32WrapI64,
                WasmOp::End,
            ],
            expected: 0xCD,
        },
        // 64-bit add that overflows i32 but fits in i64:
        //   (1_000_000_000 + 2_000_000_000) = 3_000_000_000 = 0xB2D05E00
        //   Low byte after i32.wrap_i64 = 0x00. Distinctive against
        //   zero-on-error because we then compare against the full
        //   expected value via i64.eq and succeed → low byte 1.
        Case {
            name: "1e9 + 2e9 == 3e9 (i64.eq) → 1",
            ops: vec![
                WasmOp::I64Const(1_000_000_000),
                WasmOp::I64Const(2_000_000_000),
                WasmOp::I64Add,
                WasmOp::I64Const(3_000_000_000),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // 64-bit mul: 100_000 * 100_000 = 10_000_000_000 (overflows i32)
        // Low byte = (10e9 & 0xFF) = 0x00. Use i64.eq to make distinct.
        //   10_000_000_000 == 10_000_000_000 → 1
        Case {
            name: "100k*100k == 10e9 (i64.eq) → 1",
            ops: vec![
                WasmOp::I64Const(100_000),
                WasmOp::I64Const(100_000),
                WasmOp::I64Mul,
                WasmOp::I64Const(10_000_000_000),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        Case {
            name: "i64.eqz(0) → 1",
            ops: vec![WasmOp::I64Const(0), WasmOp::I64Eqz, WasmOp::End],
            expected: 1,
        },
        Case {
            name: "i64.eqz(42) → 0",
            ops: vec![WasmOp::I64Const(42), WasmOp::I64Eqz, WasmOp::End],
            expected: 0,
        },
        Case {
            name: "i64: -1 < 0 (signed) → 1",
            ops: vec![
                WasmOp::I64Const(-1),
                WasmOp::I64Const(0),
                WasmOp::I64LtS,
                WasmOp::End,
            ],
            expected: 1,
        },
        // Conversion round-trip: i32.const 42 → extend_s → wrap → 42
        Case {
            name: "i32 42 → extend_s → wrap_i64 → 42",
            ops: vec![
                WasmOp::I32Const(42),
                WasmOp::I64ExtendI32S,
                WasmOp::I32WrapI64,
                WasmOp::End,
            ],
            expected: 42,
        },
        // Signed extension of negative: -1 as i32 → i64 → still -1
        // (cast to i64 still -1 = 0xFFFFFFFFFFFFFFFF)
        // Compare to i64.const -1 → should equal → 1
        Case {
            name: "i64.extend_i32_s(-1) == -1_i64 → 1",
            ops: vec![
                WasmOp::I32Const(-1),
                WasmOp::I64ExtendI32S,
                WasmOp::I64Const(-1),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // Unsigned extension: -1_i32 as unsigned = 0xFFFFFFFF
        // extend_u gives i64 0x0000_0000_FFFF_FFFF = 4294967295
        // which is NOT equal to -1_i64 (=0xFFFFFFFFFFFFFFFF)
        Case {
            name: "i64.extend_i32_u(-1) != -1_i64 → 1",
            ops: vec![
                WasmOp::I32Const(-1),
                WasmOp::I64ExtendI32U,
                WasmOp::I64Const(-1),
                WasmOp::I64Ne,
                WasmOp::End,
            ],
            expected: 1,
        },
        // Mixed i32 + i64 locals: declare one of each, compute.
        //   (local i32) (local i64)
        //     i32.const 10   local.set 0     ; local 0 = 10_i32
        //     i64.const 32   local.set 1     ; local 1 = 32_i64
        //     local.get 1    i32.wrap_i64    ; 32 as i32
        //     local.get 0    i32.add          ; 42
        Case {
            name: "mixed i32+i64 locals: 32_i64 wrap + 10_i32 = 42",
            ops: vec![
                WasmOp::I32Const(10),
                WasmOp::LocalSet(0),
                WasmOp::I64Const(32),
                WasmOp::LocalSet(1),
                WasmOp::LocalGet(1),
                WasmOp::I32WrapI64,
                WasmOp::LocalGet(0),
                WasmOp::I32Add,
                WasmOp::End,
            ],
            expected: 42,
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

    println!("Phase 12 — i64 on {}\n", host);

    let cases = cases();
    let mut passed = 0;
    let mut failed = 0;

    for case in &cases {
        // Build a lowerer. The last case uses mixed locals; everything
        // else uses the stackless default constructor.
        let mut lw = if case.name.starts_with("mixed") {
            Lowerer::new_with_typed_locals(&[ValType::I32, ValType::I64]).unwrap()
        } else {
            Lowerer::new()
        };

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
