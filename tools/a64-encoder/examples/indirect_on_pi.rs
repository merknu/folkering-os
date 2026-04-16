//! Phase 16 — `call_indirect` through a function-reference table.
//!
//! Lays out a two-entry table in the Pi harness's `mem_buffer`:
//!
//!   table[0] → helper_add_five  (i32) → i32
//!   table[1] → helper_multiply_two (i32) → i32
//!
//! Then JIT-emits code that populates both entries via `i64.store`,
//! selects one via a stack-pushed table index, and performs the call
//! via `call_indirect`. Each case returns a single i32 in X0 that
//! becomes the process exit code on the Pi.
//!
//! This is the final ISA gap — direct `call` already worked (Phase
//! 4A); `call_indirect` unlocks vtables, trait objects, function
//! pointers, and any WASM module beyond a single monolithic function.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{FnSig, Lowerer, ValType, WasmOp};

struct Case {
    name: &'static str,
    ops: Vec<WasmOp>,
    expected: u8,
}

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

/// Prelude that writes table[0].addr and table[1].addr into mem_buffer
/// at offsets 0 and 16 (16 bytes per entry: addr u64 + type_id u32 +
/// 4 bytes padding).
fn table_setup(add_five: u64, mul_two: u64) -> Vec<WasmOp> {
    vec![
        // table[0].addr = helper_add_five
        WasmOp::I32Const(0),
        WasmOp::I64Const(add_five as i64),
        WasmOp::I64Store(0),
        // table[1].addr = helper_multiply_two
        WasmOp::I32Const(16),
        WasmOp::I64Const(mul_two as i64),
        WasmOp::I64Store(0),
    ]
}

fn cases(add_five: u64, mul_two: u64) -> Vec<Case> {
    let mut out = Vec::new();

    // idx=0 → add_five(37) = 42
    {
        let mut ops = table_setup(add_five, mul_two);
        ops.extend_from_slice(&[
            WasmOp::I32Const(37), // arg
            WasmOp::I32Const(0),  // table index
            WasmOp::CallIndirect(0),
            WasmOp::End,
        ]);
        out.push(Case {
            name: "call_indirect idx=0 (add_five 37) → 42",
            ops,
            expected: 42,
        });
    }

    // idx=1 → multiply_two(21) = 42
    {
        let mut ops = table_setup(add_five, mul_two);
        ops.extend_from_slice(&[
            WasmOp::I32Const(21),
            WasmOp::I32Const(1),
            WasmOp::CallIndirect(0),
            WasmOp::End,
        ]);
        out.push(Case {
            name: "call_indirect idx=1 (multiply_two 21) → 42",
            ops,
            expected: 42,
        });
    }

    // Compose: add_five(multiply_two(6)) = add_five(12) = 17.
    // Two indirect calls through the same table, proves the JIT
    // handles back-to-back BLR sequences correctly (caller-save
    // hygiene, stack depth bookkeeping).
    {
        let mut ops = table_setup(add_five, mul_two);
        ops.extend_from_slice(&[
            WasmOp::I32Const(6),
            WasmOp::I32Const(1),  // multiply_two
            WasmOp::CallIndirect(0),
            // stack now has 12
            WasmOp::I32Const(0),  // add_five
            WasmOp::CallIndirect(0),
            WasmOp::End,
        ]);
        out.push(Case {
            name: "compose add_five(multiply_two(6)) → 17",
            ops,
            expected: 17,
        });
    }

    // Use the i32 result in further arithmetic — proves the pushed
    // result slot is a proper i32 (upper 32 bits masked).
    // add_five(100) + 1 = 106
    {
        let mut ops = table_setup(add_five, mul_two);
        ops.extend_from_slice(&[
            WasmOp::I32Const(100),
            WasmOp::I32Const(0),  // add_five
            WasmOp::CallIndirect(0),
            WasmOp::I32Const(1),
            WasmOp::I32Add,
            WasmOp::End,
        ]);
        out.push(Case {
            name: "call_indirect result feeds arithmetic → 106",
            ops,
            expected: 106,
        });
    }

    out
}

fn main() {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "knut@192.168.68.72".to_string());

    println!("Phase 16 — call_indirect on {}\n", host);

    let addrs = match query_addrs(&host) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("harness query failed: {e}");
            std::process::exit(2);
        }
    };
    let mem_base = *addrs.get("mem_base").expect("mem_base required");
    let add_five = *addrs.get("helper_add_five").expect("helper_add_five required");
    let mul_two = *addrs
        .get("helper_multiply_two")
        .expect("helper_multiply_two required");
    println!("mem_base            0x{:016x}", mem_base);
    println!("helper_add_five     0x{:016x}", add_five);
    println!("helper_multiply_two 0x{:016x}", mul_two);
    println!();

    // One signature shared by both helpers: (i32) -> i32.
    let sigs = vec![FnSig {
        params: vec![ValType::I32],
        result: Some(ValType::I32),
    }];

    let cases = cases(add_five, mul_two);
    let mut passed = 0;
    let mut failed = 0;

    for case in &cases {
        // Table lives at mem_base (the Pi harness's mem_buffer).
        let mut lw =
            Lowerer::new_function_with_table(0, Vec::new(), mem_base, mem_base, sigs.clone())
                .unwrap();

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
