//! Phase 11 — f32 locals + f32.load / f32.store on Cortex-A76.
//!
//! Completes f32 parity with i32:
//!   * typed locals: i32 in X19..X27, f32 in V16..V23
//!   * f32.load / f32.store via LDR Si / STR Si with memory-base X28
//!
//! Cross-type test cases prove the lowerer correctly routes each
//! op between register banks.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, ValType, WasmOp};

fn query_addrs(host: &str) -> Result<HashMap<String, u64>, String> {
    let out = Command::new("ssh")
        .arg(host)
        .arg("~/a64-harness/run_bytes --addrs")
        .output()
        .map_err(|e| format!("ssh: {e}"))?;
    if !out.status.success() {
        return Err(format!("--addrs failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    let mut map = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_start_matches("0x").trim_start_matches("0X");
            let parsed = u64::from_str_radix(v, 16)
                .map_err(|e| format!("parse {k}={v}: {e}"))?;
            map.insert(k.to_string(), parsed);
        }
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

    println!("Phase 11 — f32 locals + memory on {}\n", host);

    let addrs = match query_addrs(&host) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("harness query failed: {e}");
            std::process::exit(2);
        }
    };
    let mem_base = *addrs.get("mem_base").expect("harness needs mem_base symbol");
    println!("mem_base at 0x{:016x}\n", mem_base);

    let mut passed = 0;
    let mut failed = 0;

    // ── Case 1: f32 local set + get ────────────────────────────────
    // (func (result i32) (local f32)
    //   f32.const 2.5  local.set 0
    //   local.get 0  f32.const 2.5  f32.eq)
    // → 1
    {
        let mut lw = Lowerer::new_with_typed_locals(&[ValType::F32]).unwrap();
        lw.lower_all(&[
            WasmOp::F32Const(2.5),
            WasmOp::LocalSet(0),
            WasmOp::LocalGet(0),
            WasmOp::F32Const(2.5),
            WasmOp::F32Eq,
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        match run(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == 1 {
                    println!("  [ ok ] f32 local set+get round-trip → {got}");
                    passed += 1;
                } else {
                    println!("  [FAIL] f32 local round-trip: got {got}");
                    failed += 1;
                }
            }
            Err(e) => { println!("  [err ] {e}"); failed += 1; }
        }
    }

    // ── Case 2: mixed locals — i32 + f32 ────────────────────────────
    // (local i32) (local f32)
    //   i32.const 10  local.set 0     ; int local = 10
    //   f32.const 1.5  local.set 1    ; float local = 1.5
    //   ;; compute: (i32 local) + (f32 local == 1.5 as i32 bool)
    //   local.get 0                    ; 10
    //   local.get 1  f32.const 1.5  f32.eq  ; 1
    //   i32.add                        ; 11
    //   end
    {
        let mut lw = Lowerer::new_with_typed_locals(&[ValType::I32, ValType::F32]).unwrap();
        lw.lower_all(&[
            WasmOp::I32Const(10),
            WasmOp::LocalSet(0),
            WasmOp::F32Const(1.5),
            WasmOp::LocalSet(1),
            WasmOp::LocalGet(0),
            WasmOp::LocalGet(1),
            WasmOp::F32Const(1.5),
            WasmOp::F32Eq,
            WasmOp::I32Add,
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        match run(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == 11 {
                    println!("  [ ok ] mixed i32+f32 locals compose → {got}");
                    passed += 1;
                } else {
                    println!("  [FAIL] mixed-locals: got {got}");
                    failed += 1;
                }
            }
            Err(e) => { println!("  [err ] {e}"); failed += 1; }
        }
    }

    // ── Case 3: f32 store → load round-trip through memory ──────────
    // addr=0, store 7.25, load it back, compare to 7.25 → 1
    {
        let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), mem_base).unwrap();
        lw.lower_all(&[
            WasmOp::I32Const(0),
            WasmOp::F32Const(7.25),
            WasmOp::F32Store(0),
            WasmOp::I32Const(0),
            WasmOp::F32Load(0),
            WasmOp::F32Const(7.25),
            WasmOp::F32Eq,
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        match run(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == 1 {
                    println!("  [ ok ] f32 store→load round-trip → {got}");
                    passed += 1;
                } else {
                    println!("  [FAIL] f32 store/load: got {got}");
                    failed += 1;
                }
            }
            Err(e) => { println!("  [err ] {e}"); failed += 1; }
        }
    }

    // ── Case 4: sum-of-array in f32 ─────────────────────────────────
    // Write three f32s to mem[0..12], then sum them via loads.
    // 1.0 + 2.5 + 3.5 = 7.0. Compare to 7.0 → 1.
    {
        let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), mem_base).unwrap();
        lw.lower_all(&[
            WasmOp::I32Const(0),  WasmOp::F32Const(1.0), WasmOp::F32Store(0),
            WasmOp::I32Const(4),  WasmOp::F32Const(2.5), WasmOp::F32Store(0),
            WasmOp::I32Const(8),  WasmOp::F32Const(3.5), WasmOp::F32Store(0),
            // Load + accumulate.
            WasmOp::I32Const(0),  WasmOp::F32Load(0),
            WasmOp::I32Const(4),  WasmOp::F32Load(0),
            WasmOp::F32Add,
            WasmOp::I32Const(8),  WasmOp::F32Load(0),
            WasmOp::F32Add,
            // Compare.
            WasmOp::F32Const(7.0),
            WasmOp::F32Eq,
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        match run(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == 1 {
                    println!("  [ ok ] f32 sum-of-3-in-memory == 7.0 → {got}");
                    passed += 1;
                } else {
                    println!("  [FAIL] f32 sum-of-memory: got {got}");
                    failed += 1;
                }
            }
            Err(e) => { println!("  [err ] {e}"); failed += 1; }
        }
    }

    println!("\n{} passed, {} failed", passed, failed);
    if failed > 0 { std::process::exit(1); }
}
