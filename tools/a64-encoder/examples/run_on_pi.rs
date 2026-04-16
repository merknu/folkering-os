//! Execute a64-encoder output on a real AArch64 host via SSH.
//!
//! Usage:
//!   cargo run --example run_on_pi -- [HOST]
//!
//! Default host is `knut@192.168.68.72`. Requires the `run_bytes`
//! harness to be pre-built at `~/a64-harness/run_bytes` on the
//! remote (see `tools/a64-encoder/examples/PI_SETUP.md`).
//!
//! Each case emits bytes for a small program, pipes them to `ssh
//! HOST run_bytes`, captures the exit code, and compares against the
//! expected i32 return value (truncated to 8 bits because Linux
//! `exit()` only keeps the low byte).

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
        Case {
            name: "return 42",
            ops: vec![WasmOp::I32Const(42), WasmOp::End],
            expected: 42,
        },
        Case {
            name: "i32.const 10 + 20",
            ops: vec![
                WasmOp::I32Const(10),
                WasmOp::I32Const(20),
                WasmOp::I32Add,
                WasmOp::End,
            ],
            expected: 30,
        },
        Case {
            name: "i32.const 100 - 58",
            ops: vec![
                WasmOp::I32Const(100),
                WasmOp::I32Const(58),
                WasmOp::I32Sub,
                WasmOp::End,
            ],
            expected: 42,
        },
        Case {
            name: "nested add: 1+2+3",
            ops: vec![
                WasmOp::I32Const(1),
                WasmOp::I32Const(2),
                WasmOp::I32Const(3),
                WasmOp::I32Add,
                WasmOp::I32Add,
                WasmOp::End,
            ],
            expected: 6,
        },
        Case {
            name: "if-else truthy: cond=1 → 10",
            ops: vec![
                WasmOp::I32Const(1),
                WasmOp::If,
                WasmOp::I32Const(10),
                WasmOp::Else,
                WasmOp::I32Const(20),
                WasmOp::End,
                WasmOp::End,
            ],
            expected: 10,
        },
        Case {
            name: "if-else falsy: cond=0 → 20",
            ops: vec![
                WasmOp::I32Const(0),
                WasmOp::If,
                WasmOp::I32Const(10),
                WasmOp::Else,
                WasmOp::I32Const(20),
                WasmOp::End,
                WasmOp::End,
            ],
            expected: 20,
        },
        Case {
            name: "mul: 6 * 7",
            ops: vec![
                WasmOp::I32Const(6),
                WasmOp::I32Const(7),
                WasmOp::I32Mul,
                WasmOp::End,
            ],
            expected: 42,
        },
        Case {
            name: "sdiv: 84 / 2",
            ops: vec![
                WasmOp::I32Const(84),
                WasmOp::I32Const(2),
                WasmOp::I32DivS,
                WasmOp::End,
            ],
            expected: 42,
        },
        Case {
            name: "udiv: 126 / 3",
            ops: vec![
                WasmOp::I32Const(126),
                WasmOp::I32Const(3),
                WasmOp::I32DivU,
                WasmOp::End,
            ],
            expected: 42,
        },
        Case {
            name: "chained: (10 * 3) / 2 + 27",
            // (10 * 3) / 2 = 15; 15 + 27 = 42
            ops: vec![
                WasmOp::I32Const(10),
                WasmOp::I32Const(3),
                WasmOp::I32Mul,
                WasmOp::I32Const(2),
                WasmOp::I32DivS,
                WasmOp::I32Const(27),
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
        .map_err(|e| format!("spawn ssh: {e}"))?;

    child
        .stdin
        .as_mut()
        .ok_or_else(|| "no stdin".to_string())?
        .write_all(bytes)
        .map_err(|e| format!("write bytes: {e}"))?;
    // Close stdin so harness's read() sees EOF.
    drop(child.stdin.take());

    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait: {e}"))?;

    // Exit code is what we want. Non-zero is the program's result,
    // not necessarily an error.
    out.status
        .code()
        .ok_or_else(|| format!("no exit code; stderr: {}", String::from_utf8_lossy(&out.stderr)))
}

fn main() {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "knut@192.168.68.72".to_string());

    let cases = cases();
    let mut passed = 0;
    let mut failed = 0;

    println!("Running {} cases on {}...\n", cases.len(), host);

    for case in &cases {
        let mut lw = Lowerer::new();
        if let Err(e) = lw.lower_all(&case.ops) {
            println!("  [skip] {}: lower error: {:?}", case.name, e);
            continue;
        }
        let bytes = lw.finish();

        match run_on_pi(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == case.expected {
                    println!("  [ ok ] {}: got {}", case.name, got);
                    passed += 1;
                } else {
                    println!(
                        "  [FAIL] {}: expected {}, got {} ({} bytes)",
                        case.name,
                        case.expected,
                        got,
                        bytes.len()
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
