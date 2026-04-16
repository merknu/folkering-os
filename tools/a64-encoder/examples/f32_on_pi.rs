//! Phase 9 — f32 arithmetic on real Cortex-A76 via the SIMD/FP bank.
//!
//! This is the first example that uses the V-bank (S0..S15) for
//! operand-stack slots. f32 values are materialized by MOVZ+MOVK
//! into a general register, then FMOV'd into an S register. Binary
//! ops use FADD/FSUB/FMUL/FDIV. Comparisons (FCMP+CSET) produce
//! i32 results and the stack transitions back to the X-bank.
//!
//! At function end, if the stack-top is f32 we FMOV the bits back
//! into W0 so the exit code carries the IEEE-754 bit pattern
//! (low byte only). For cleaner assertions we mostly use f32.eq
//! to land an i32 (0/1) in X0.

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
        // 1.5 + 2.5 = 4.0, then f32.eq 4.0 → 1
        Case {
            name: "1.5 + 2.5 == 4.0 → 1",
            ops: vec![
                WasmOp::F32Const(1.5),
                WasmOp::F32Const(2.5),
                WasmOp::F32Add,
                WasmOp::F32Const(4.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // 3.0 * 7.0 = 21.0 (not 20.0) → 0 when compared to 20.0
        Case {
            name: "3.0 * 7.0 == 20.0 → 0",
            ops: vec![
                WasmOp::F32Const(3.0),
                WasmOp::F32Const(7.0),
                WasmOp::F32Mul,
                WasmOp::F32Const(20.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 0,
        },
        // (10.0 - 6.0) / 2.0 = 2.0; compare to 2.0 → 1
        Case {
            name: "(10 - 6) / 2 == 2 → 1",
            ops: vec![
                WasmOp::F32Const(10.0),
                WasmOp::F32Const(6.0),
                WasmOp::F32Sub,
                WasmOp::F32Const(2.0),
                WasmOp::F32Div,
                WasmOp::F32Const(2.0),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // Mixed: compute an f32, compare, then use the i32 result
        // to feed an if. Proves stack-type transitions work.
        //   if ((1.0 + 1.0) == 2.0) then 42 else 99  →  42
        Case {
            name: "mixed: if (1.0+1.0 == 2.0) then 42 else 99 → 42",
            ops: vec![
                WasmOp::F32Const(1.0),
                WasmOp::F32Const(1.0),
                WasmOp::F32Add,
                WasmOp::F32Const(2.0),
                WasmOp::F32Eq,
                WasmOp::If,
                WasmOp::I32Const(42),
                WasmOp::Else,
                WasmOp::I32Const(99),
                WasmOp::End, // close if
                WasmOp::End, // function end
            ],
            expected: 42,
        },
        // Bit-cast ladder: an f32 result lands in X0 via FMOV W, S.
        // 0.0 has bit pattern 0x00000000 → low byte 0.
        Case {
            name: "f32.const 0.0 → low byte of bit pattern (0)",
            ops: vec![WasmOp::F32Const(0.0), WasmOp::End],
            expected: 0,
        },
        // 2.0 has bit pattern 0x40000000 — but SHR + AND would
        // give us a distinctive byte. For a single f32 end though,
        // we only see the low byte of its bit pattern.
        // f32 1.75 = 0x3FE00000. Low byte = 0x00. Still 0 — not useful.
        //
        // For a distinctive exit code via f32, use f32.eq to convert
        // to i32 first. (Covered in earlier cases.)
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

    println!("Phase 9 — f32 arithmetic on {}\n", host);

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
                    println!("  [ ok ] {}  ({} bytes of A64)", case.name, bytes.len());
                    passed += 1;
                } else {
                    println!("  [FAIL] {}: expected {}, got {}", case.name, case.expected, got);
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
