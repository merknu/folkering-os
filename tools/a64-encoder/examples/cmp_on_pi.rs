//! Phase 5 — verify i32 comparisons on real Cortex-A76.
//!
//! Each WASM comparison (`i32.eq`, `i32.ne`, `i32.lt_s`, `i32.gt_s`)
//! lowers to `CMP Wn, Wm` + `CSET Xd, cond`. We ship a grid of cases
//! that exercise every comparison both directly (producing 0/1) and
//! composed with `if/else` (proving the boolean result feeds control
//! flow correctly).

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
        // ── Direct comparisons ────────────────────────────────────
        Case {
            name: "5 == 5 → 1",
            ops: vec![WasmOp::I32Const(5), WasmOp::I32Const(5), WasmOp::I32Eq, WasmOp::End],
            expected: 1,
        },
        Case {
            name: "5 == 3 → 0",
            ops: vec![WasmOp::I32Const(5), WasmOp::I32Const(3), WasmOp::I32Eq, WasmOp::End],
            expected: 0,
        },
        Case {
            name: "5 != 3 → 1",
            ops: vec![WasmOp::I32Const(5), WasmOp::I32Const(3), WasmOp::I32Ne, WasmOp::End],
            expected: 1,
        },
        Case {
            name: "5 != 5 → 0",
            ops: vec![WasmOp::I32Const(5), WasmOp::I32Const(5), WasmOp::I32Ne, WasmOp::End],
            expected: 0,
        },
        Case {
            name: "3 < 5 → 1",
            ops: vec![WasmOp::I32Const(3), WasmOp::I32Const(5), WasmOp::I32LtS, WasmOp::End],
            expected: 1,
        },
        Case {
            name: "5 < 3 → 0",
            ops: vec![WasmOp::I32Const(5), WasmOp::I32Const(3), WasmOp::I32LtS, WasmOp::End],
            expected: 0,
        },
        Case {
            name: "5 > 3 → 1",
            ops: vec![WasmOp::I32Const(5), WasmOp::I32Const(3), WasmOp::I32GtS, WasmOp::End],
            expected: 1,
        },
        Case {
            name: "3 > 5 → 0",
            ops: vec![WasmOp::I32Const(3), WasmOp::I32Const(5), WasmOp::I32GtS, WasmOp::End],
            expected: 0,
        },
        Case {
            name: "-1 < 0 → 1 (signed)",
            ops: vec![WasmOp::I32Const(-1), WasmOp::I32Const(0), WasmOp::I32LtS, WasmOp::End],
            expected: 1,
        },
        // ── Composition with if/else ──────────────────────────────
        Case {
            name: "if (5 > 3) then 42 else 99 → 42",
            ops: vec![
                WasmOp::I32Const(5),
                WasmOp::I32Const(3),
                WasmOp::I32GtS,
                WasmOp::If,
                WasmOp::I32Const(42),
                WasmOp::Else,
                WasmOp::I32Const(99),
                WasmOp::End, // close if
                WasmOp::End, // function end
            ],
            expected: 42,
        },
        Case {
            name: "if (-1 == 0) then 42 else 7 → 7",
            ops: vec![
                WasmOp::I32Const(-1),
                WasmOp::I32Const(0),
                WasmOp::I32Eq,
                WasmOp::If,
                WasmOp::I32Const(42),
                WasmOp::Else,
                WasmOp::I32Const(7),
                WasmOp::End,
                WasmOp::End,
            ],
            expected: 7,
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

    println!("Phase 5 — comparisons on {}\n", host);

    let cases = cases();
    let mut passed = 0;
    let mut failed = 0;

    for case in &cases {
        let mut lw = Lowerer::new();
        lw.lower_all(&case.ops).expect("lower");
        let bytes = lw.finish();

        match run_on_pi(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == case.expected {
                    println!("  [ ok ] {}", case.name);
                    passed += 1;
                } else {
                    println!("  [FAIL] {}: got {}", case.name, got);
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
