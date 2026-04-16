//! Quick test: sum-of-1-to-10 via loop+locals on Cortex-A76.
//! Both hand-crafted WasmOps AND parsed-from-binary, to identify
//! whether a crash is from the lowerer or the parser.

use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{parse_module, Lowerer, WasmOp};

fn run_on_pi(host: &str, bytes: &[u8]) -> Result<i32, String> {
    let mut child = Command::new("ssh")
        .arg(host)
        .arg("~/a64-harness/run_bytes")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{e}"))?;
    child.stdin.as_mut().unwrap().write_all(bytes).map_err(|e| format!("{e}"))?;
    drop(child.stdin.take());
    let out = child.wait_with_output().map_err(|e| format!("{e}"))?;
    if !out.stderr.is_empty() {
        eprintln!("stderr: {}", String::from_utf8_lossy(&out.stderr));
    }
    out.status.code().ok_or_else(|| "no exit code".into())
}

fn main() {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "knut@192.168.68.72".to_string());

    println!("--- Case A: hand-crafted WasmOps ---");
    {
        let ops = vec![
            WasmOp::I32Const(1),
            WasmOp::LocalSet(1),
            WasmOp::Block,
            WasmOp::Loop,
            WasmOp::LocalGet(1),
            WasmOp::I32Const(11),
            WasmOp::I32GeS,
            WasmOp::BrIf(1),
            WasmOp::LocalGet(0),
            WasmOp::LocalGet(1),
            WasmOp::I32Add,
            WasmOp::LocalSet(0),
            WasmOp::LocalGet(1),
            WasmOp::I32Const(1),
            WasmOp::I32Add,
            WasmOp::LocalSet(1),
            WasmOp::Br(0),
            WasmOp::End, // loop
            WasmOp::End, // block
            WasmOp::LocalGet(0),
            WasmOp::End, // func
        ];
        let mut lw = Lowerer::new_function(2, Vec::new()).unwrap();
        lw.lower_all(&ops).unwrap();
        let bytes = lw.finish();
        println!("  JIT: {} bytes", bytes.len());
        match run_on_pi(&host, &bytes) {
            Ok(rv) => {
                let got = rv & 0xFF;
                let ok = got == 55;
                println!(
                    "  {} got={} expected=55",
                    if ok { "OK" } else { "FAIL" },
                    got
                );
            }
            Err(e) => println!("  ERROR: {e}"),
        }
    }

    println!("\n--- Case B: parsed from binary WASM ---");
    {
        let wasm_module: Vec<u8> = vec![
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F,
            0x03, 0x02, 0x01, 0x00,
            0x0A, 0x29, 0x01, 0x27,
            0x01, 0x02, 0x7F,
            0x41, 0x01, 0x21, 0x01,
            0x02, 0x40, 0x03, 0x40,
            0x20, 0x01, 0x41, 0x0B, 0x4E, 0x0D, 0x01,
            0x20, 0x00, 0x20, 0x01, 0x6A, 0x21, 0x00,
            0x20, 0x01, 0x41, 0x01, 0x6A, 0x21, 0x01,
            0x0C, 0x00,
            0x0B, 0x0B,
            0x20, 0x00, 0x0B,
        ];
        let bodies = parse_module(&wasm_module).expect("parse");
        let body = &bodies[0];
        println!("  Parsed: {} locals, {} ops", body.num_locals, body.ops.len());
        for (i, op) in body.ops.iter().enumerate() {
            println!("    [{:2}] {:?}", i, op);
        }

        let mut lw = Lowerer::new_function(body.num_locals as usize, Vec::new()).unwrap();
        lw.lower_all(&body.ops).unwrap();
        let bytes = lw.finish();
        println!("  JIT: {} bytes", bytes.len());
        match run_on_pi(&host, &bytes) {
            Ok(rv) => {
                let got = rv & 0xFF;
                let ok = got == 55;
                println!(
                    "  {} got={} expected=55",
                    if ok { "OK" } else { "FAIL" },
                    got
                );
            }
            Err(e) => println!("  ERROR: {e}"),
        }
    }
}
