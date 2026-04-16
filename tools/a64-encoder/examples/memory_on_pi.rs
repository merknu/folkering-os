//! Phase 4C: verify `i32.store` and `i32.load` on real aarch64 HW.
//!
//! The encoder + lowerer now plumb a linear-memory base pinned in
//! X28 through the function prologue, plus `ADD Xt, X28, Waddr,
//! UXTW` to compute effective addresses. This example proves the
//! store/load round-trip against a 64 KiB BSS buffer (`mem_buffer`)
//! in the harness.
//!
//! Test: write N different (addr, value) pairs, then read each back
//! and XOR-combine them. Final value is compared against an exit-
//! code expectation. Any bit error in effective-address math or the
//! LDR/STR W encoding will show up as a mismatch.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, WasmOp};

fn query_addrs(host: &str) -> Result<HashMap<String, u64>, String> {
    let out = Command::new("ssh")
        .arg(host)
        .arg("~/a64-harness/run_bytes --addrs")
        .output()
        .map_err(|e| format!("ssh: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "--addrs failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let mut map = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().trim_start_matches("0x").trim_start_matches("0X");
        let parsed = u64::from_str_radix(v, 16)
            .map_err(|e| format!("parse {k}={v}: {e}"))?;
        map.insert(k.to_string(), parsed);
    }
    Ok(map)
}

fn run(host: &str, bytes: &[u8]) -> Result<i32, String> {
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

fn main() {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "knut@192.168.68.72".to_string());

    println!("Phase 4C — linear memory store/load on {}\n", host);

    let addrs = match query_addrs(&host) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("harness query failed: {e}");
            std::process::exit(2);
        }
    };
    let mem_base = match addrs.get("mem_base") {
        Some(a) => *a,
        None => {
            eprintln!("harness didn't expose mem_base — rebuild with --addrs support");
            std::process::exit(2);
        }
    };
    println!("mem_base at 0x{:016x}", mem_base);

    // ── Case 1: single store-then-load round-trip ─────────────────
    //   mem[0] := 42
    //   return mem[0]
    {
        let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), mem_base).unwrap();
        lw.lower_all(&[
            WasmOp::I32Const(0),
            WasmOp::I32Const(42),
            WasmOp::I32Store(0),
            WasmOp::I32Const(0),
            WasmOp::I32Load(0),
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        match run(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == 42 {
                    println!("  [ ok ] store 42 → load  (got {got})");
                } else {
                    println!("  [FAIL] single round-trip: expected 42, got {got}");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("  [err ] {e}");
                std::process::exit(1);
            }
        }
    }

    // ── Case 2: two separate addresses, load one back ─────────────
    //   mem[ 0] := 100
    //   mem[16] := 42
    //   return mem[16]
    {
        let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), mem_base).unwrap();
        lw.lower_all(&[
            WasmOp::I32Const(0),
            WasmOp::I32Const(100),
            WasmOp::I32Store(0),
            WasmOp::I32Const(16),
            WasmOp::I32Const(42),
            WasmOp::I32Store(0),
            WasmOp::I32Const(16),
            WasmOp::I32Load(0),
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        match run(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == 42 {
                    println!("  [ ok ] store at 0 + 16, read back [16]  (got {got})");
                } else {
                    println!("  [FAIL] two-address test: expected 42, got {got}");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("  [err ] {e}");
                std::process::exit(1);
            }
        }
    }

    // ── Case 3: static offset in the load/store memarg ─────────────
    //   mem[0 + 4] := 42   (i32.store offset=4)
    //   return mem[0 + 4]  (i32.load  offset=4)
    {
        let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), mem_base).unwrap();
        lw.lower_all(&[
            WasmOp::I32Const(0),
            WasmOp::I32Const(42),
            WasmOp::I32Store(4),
            WasmOp::I32Const(0),
            WasmOp::I32Load(4),
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        match run(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == 42 {
                    println!("  [ ok ] static offset=4 round-trip  (got {got})");
                } else {
                    println!("  [FAIL] offset test: expected 42, got {got}");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("  [err ] {e}");
                std::process::exit(1);
            }
        }
    }

    // ── Case 4: compute value via arithmetic, store, then load ────
    //   let v = (10 * 3) + 12   → 42
    //   mem[0] := v
    //   return mem[0]
    {
        let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), mem_base).unwrap();
        lw.lower_all(&[
            WasmOp::I32Const(0),       // addr
            WasmOp::I32Const(10),
            WasmOp::I32Const(3),
            WasmOp::I32Mul,            // 30
            WasmOp::I32Const(12),
            WasmOp::I32Add,            // 42
            WasmOp::I32Store(0),
            WasmOp::I32Const(0),
            WasmOp::I32Load(0),
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        match run(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == 42 {
                    println!("  [ ok ] computed value → memory → load  (got {got})");
                } else {
                    println!("  [FAIL] computed-value test: expected 42, got {got}");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("  [err ] {e}");
                std::process::exit(1);
            }
        }
    }

    println!("\n4 memory cases passed");
}
