//! Phase 14 — f64 (double-precision) on Cortex-A76.
//!
//! Exercises the full f64 surface:
//!   * constants via FMOV Dd, Xn (64-bit integer bit-pattern → D)
//!   * arithmetic: FADD/SUB/MUL/DIV (ftype=01)
//!   * comparisons: FCMP D → CSET (produces i32 result)
//!   * memory: LDR/STR D with 8-aligned offsets
//!   * f64 locals in V-bank (reusing F32's region)
//!
//! Exit codes are 8-bit so tests compare f64 → i32 bool via `f64.eq`
//! when the value itself wouldn't fit in a byte.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, ValType, WasmOp};

struct Case {
    name: &'static str,
    ops: Vec<WasmOp>,
    expected: u8,
    memory: bool,
    locals: Option<Vec<ValType>>,
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

fn cases() -> Vec<Case> {
    vec![
        // ── Arithmetic ───────────────────────────────────────────────
        // 1.5 + 2.25 == 3.75 → 1
        Case {
            name: "f64.add: 1.5 + 2.25 == 3.75 → 1",
            ops: vec![
                WasmOp::F64Const(1.5),
                WasmOp::F64Const(2.25),
                WasmOp::F64Add,
                WasmOp::F64Const(3.75),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
            memory: false,
            locals: None,
        },
        // 10.0 - 2.5 == 7.5 → 1
        Case {
            name: "f64.sub: 10.0 - 2.5 == 7.5 → 1",
            ops: vec![
                WasmOp::F64Const(10.0),
                WasmOp::F64Const(2.5),
                WasmOp::F64Sub,
                WasmOp::F64Const(7.5),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
            memory: false,
            locals: None,
        },
        // 1e10 * 3.14 — large magnitude that would lose precision in f32.
        // 10_000_000_000 * 3.14 = 31_400_000_000
        Case {
            name: "f64.mul: 1e10 * 3.14 == 3.14e10 → 1",
            ops: vec![
                WasmOp::F64Const(1e10),
                WasmOp::F64Const(3.14),
                WasmOp::F64Mul,
                WasmOp::F64Const(3.14e10),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
            memory: false,
            locals: None,
        },
        // 22.0 / 7.0 ≈ 3.142857142857143 (IEEE-754 exact)
        Case {
            name: "f64.div: 22/7 roundtrip → 1",
            ops: vec![
                WasmOp::F64Const(22.0),
                WasmOp::F64Const(7.0),
                WasmOp::F64Div,
                WasmOp::F64Const(22.0 / 7.0),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
            memory: false,
            locals: None,
        },
        // ── Comparisons ──────────────────────────────────────────────
        Case {
            name: "f64.ne: 1.0 != 2.0 → 1",
            ops: vec![
                WasmOp::F64Const(1.0),
                WasmOp::F64Const(2.0),
                WasmOp::F64Ne,
                WasmOp::End,
            ],
            expected: 1,
            memory: false,
            locals: None,
        },
        Case {
            name: "f64.lt: 1.0 < 1.000000001 → 1",
            ops: vec![
                WasmOp::F64Const(1.0),
                WasmOp::F64Const(1.000_000_001),
                WasmOp::F64Lt,
                WasmOp::End,
            ],
            expected: 1,
            memory: false,
            locals: None,
        },
        Case {
            name: "f64.gt: 3.0 > 2.0 → 1",
            ops: vec![
                WasmOp::F64Const(3.0),
                WasmOp::F64Const(2.0),
                WasmOp::F64Gt,
                WasmOp::End,
            ],
            expected: 1,
            memory: false,
            locals: None,
        },
        Case {
            name: "f64.le: 5.0 <= 5.0 → 1",
            ops: vec![
                WasmOp::F64Const(5.0),
                WasmOp::F64Const(5.0),
                WasmOp::F64Le,
                WasmOp::End,
            ],
            expected: 1,
            memory: false,
            locals: None,
        },
        Case {
            name: "f64.ge: 2.0 >= 5.0 → 0",
            ops: vec![
                WasmOp::F64Const(2.0),
                WasmOp::F64Const(5.0),
                WasmOp::F64Ge,
                WasmOp::End,
            ],
            expected: 0,
            memory: false,
            locals: None,
        },
        // ── Precision: f64 captures what f32 loses ──────────────────
        // 0.1 + 0.2 in f64 ≠ 0.3 exactly (classic IEEE-754 case).
        // (0.1 + 0.2) > 0.3  → 1
        Case {
            name: "f64 precision: 0.1 + 0.2 > 0.3 → 1",
            ops: vec![
                WasmOp::F64Const(0.1),
                WasmOp::F64Const(0.2),
                WasmOp::F64Add,
                WasmOp::F64Const(0.3),
                WasmOp::F64Gt,
                WasmOp::End,
            ],
            expected: 1,
            memory: false,
            locals: None,
        },
        // ── Memory: f64 store → load round-trip ──────────────────────
        Case {
            name: "f64 store/load round-trip (π) → 1",
            ops: vec![
                WasmOp::I32Const(0),
                WasmOp::F64Const(std::f64::consts::PI),
                WasmOp::F64Store(0),
                WasmOp::I32Const(0),
                WasmOp::F64Load(0),
                WasmOp::F64Const(std::f64::consts::PI),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
            memory: true,
            locals: None,
        },
        // Sum three f64s stored at offsets 0, 8, 16.
        //   1.5 + 2.5 + 3.0 = 7.0
        Case {
            name: "f64 sum-of-3-in-memory = 7.0 → 1",
            ops: vec![
                WasmOp::I32Const(0),  WasmOp::F64Const(1.5), WasmOp::F64Store(0),
                WasmOp::I32Const(8),  WasmOp::F64Const(2.5), WasmOp::F64Store(0),
                WasmOp::I32Const(16), WasmOp::F64Const(3.0), WasmOp::F64Store(0),
                WasmOp::I32Const(0),  WasmOp::F64Load(0),
                WasmOp::I32Const(8),  WasmOp::F64Load(0),
                WasmOp::F64Add,
                WasmOp::I32Const(16), WasmOp::F64Load(0),
                WasmOp::F64Add,
                WasmOp::F64Const(7.0),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
            memory: true,
            locals: None,
        },
        // ── Locals: f64 set + get ────────────────────────────────────
        Case {
            name: "f64 local set+get: 2.75 round-trip → 1",
            ops: vec![
                WasmOp::F64Const(2.75),
                WasmOp::LocalSet(0),
                WasmOp::LocalGet(0),
                WasmOp::F64Const(2.75),
                WasmOp::F64Eq,
                WasmOp::End,
            ],
            expected: 1,
            memory: false,
            locals: Some(vec![ValType::F64]),
        },
        // Mixed i32 + f64 locals.
        //   local 0: i32 = 7
        //   local 1: f64 = 1.5
        //   f64_eq(local1, 1.5) + 6 = 7
        Case {
            name: "mixed i32+f64 locals compose → 7",
            ops: vec![
                WasmOp::I32Const(7),
                WasmOp::LocalSet(0),
                WasmOp::F64Const(1.5),
                WasmOp::LocalSet(1),
                WasmOp::LocalGet(1),
                WasmOp::F64Const(1.5),
                WasmOp::F64Eq,
                WasmOp::I32Const(6),
                WasmOp::I32Add,
                WasmOp::End,
            ],
            expected: 7,
            memory: false,
            locals: Some(vec![ValType::I32, ValType::F64]),
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

    println!("Phase 14 — f64 on {}\n", host);

    let mem_base = match query_addrs(&host) {
        Ok(a) => *a.get("mem_base").expect("harness needs mem_base symbol"),
        Err(e) => {
            eprintln!("harness query failed: {e}");
            std::process::exit(2);
        }
    };
    println!("mem_base at 0x{:016x}\n", mem_base);

    let cases = cases();
    let mut passed = 0;
    let mut failed = 0;

    for case in &cases {
        let mut lw = match (case.memory, case.locals.as_ref()) {
            (true, None) => Lowerer::new_function_with_memory(0, Vec::new(), mem_base).unwrap(),
            (false, Some(types)) => Lowerer::new_with_typed_locals(types).unwrap(),
            (false, None) => Lowerer::new(),
            (true, Some(_)) => {
                // Not exercised by current cases; would need a mem+typed-locals ctor.
                println!("  [skip] {}: combined memory+typed-locals unsupported here", case.name);
                continue;
            }
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
