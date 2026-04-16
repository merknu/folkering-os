//! Phase 8 — verify bitwise ops (AND/OR/XOR/SHL/SHR_S/SHR_U) on Cortex-A76.
//!
//! Every op lowers to a single A64 W-variant instruction: AND W,
//! ORR W, EOR W, LSL W (LSLV), LSR W (LSRV), ASR W (ASRV).  The
//! 32-bit variant matters — WASM shifts use `count mod 32`, while
//! 64-bit LSLV masks to `mod 64`, so W-variants keep semantics
//! exact even for large shift counts.

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
            name: "0xFF & 0x0F → 15",
            ops: vec![WasmOp::I32Const(0xFF), WasmOp::I32Const(0x0F), WasmOp::I32And, WasmOp::End],
            expected: 15,
        },
        Case {
            name: "0x0F | 0x30 → 63",
            ops: vec![WasmOp::I32Const(0x0F), WasmOp::I32Const(0x30), WasmOp::I32Or, WasmOp::End],
            expected: 63,
        },
        Case {
            name: "0xAA ^ 0x55 → 255",
            ops: vec![WasmOp::I32Const(0xAA), WasmOp::I32Const(0x55), WasmOp::I32Xor, WasmOp::End],
            expected: 255,
        },
        Case {
            name: "1 << 6 → 64",
            ops: vec![WasmOp::I32Const(1), WasmOp::I32Const(6), WasmOp::I32Shl, WasmOp::End],
            expected: 64,
        },
        Case {
            name: "128 shr_u 2 → 32",
            ops: vec![WasmOp::I32Const(128), WasmOp::I32Const(2), WasmOp::I32ShrU, WasmOp::End],
            expected: 32,
        },
        Case {
            name: "-16 shr_s 1 → -8 (sign-preserving)",
            ops: vec![WasmOp::I32Const(-16), WasmOp::I32Const(1), WasmOp::I32ShrS, WasmOp::End],
            // -8 as u32 = 0xFFFFFFF8, low byte = 0xF8 = 248
            expected: 248,
        },
        Case {
            name: "0xFFFFFFFF shr_u 25 → 127",
            // 0xFFFFFFFF >> 25 = 0x7F = 127
            ops: vec![WasmOp::I32Const(-1), WasmOp::I32Const(25), WasmOp::I32ShrU, WasmOp::End],
            expected: 127,
        },
        Case {
            // WASM i32.shl spec: shift count mod 32. So `5 << 33` = `5 << 1` = 10.
            // Proves 32-bit LSLV masks to 5 bits, not 64-bit LSL (which masks to 6).
            name: "5 << 33 → 10 (shift count mod 32)",
            ops: vec![WasmOp::I32Const(5), WasmOp::I32Const(33), WasmOp::I32Shl, WasmOp::End],
            expected: 10,
        },
        Case {
            // Compose: (0xF0 | 0x0F) ^ 0xFF = 0xFF ^ 0xFF = 0. Checks chained bitops.
            name: "(0xF0 | 0x0F) ^ 0xFF → 0",
            ops: vec![
                WasmOp::I32Const(0xF0),
                WasmOp::I32Const(0x0F),
                WasmOp::I32Or,
                WasmOp::I32Const(0xFF),
                WasmOp::I32Xor,
                WasmOp::End,
            ],
            expected: 0,
        },
        Case {
            // Realistic use: extract byte 2 from a u32.  Value 0xDEADBEEF,
            // `(v >> 8) & 0xFF` = 0xBE = 190.
            name: "extract byte 2 of 0xDEADBEEF → 190",
            ops: vec![
                WasmOp::I32Const(0xDEADBEEFu32 as i32),
                WasmOp::I32Const(8),
                WasmOp::I32ShrU,
                WasmOp::I32Const(0xFF),
                WasmOp::I32And,
                WasmOp::End,
            ],
            expected: 190,
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

    println!("Phase 8 — bitops on {}\n", host);

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
