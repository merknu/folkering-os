//! Phase 13 — complete i64 surface on Cortex-A76.
//!
//! Exercises:
//!   * i64 division: DivS, DivU
//!   * i64 bitops: And, Or, Xor, Shl, ShrS, ShrU
//!   * i64 unsigned comparisons: LtU, GtU, LeU, GeU
//!   * i64 signed comparisons: LeS, GeS (LtS/GtS already in Phase 12)
//!   * i64 memory: Load, Store via 8-byte-aligned LDR/STR X

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, WasmOp};

struct Case {
    name: &'static str,
    ops: Vec<WasmOp>,
    expected: u8,
    needs_mem: bool,
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
        // ── Division ────────────────────────────────────────────────
        // 1_000_000 / 7 = 142857.  Low byte = 142857 & 0xFF = 0x49.
        Case {
            name: "i64.div_s: 1_000_000 / 7 = 142857",
            ops: vec![
                WasmOp::I64Const(1_000_000),
                WasmOp::I64Const(7),
                WasmOp::I64DivS,
                WasmOp::I32WrapI64,
                WasmOp::End,
            ],
            expected: (142857u32 & 0xFF) as u8,
            needs_mem: false,
        },
        // -100 / 4 = -25 (signed).  Compare to -25 → 1.
        Case {
            name: "i64.div_s: -100 / 4 == -25 → 1",
            ops: vec![
                WasmOp::I64Const(-100),
                WasmOp::I64Const(4),
                WasmOp::I64DivS,
                WasmOp::I64Const(-25),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: false,
        },
        // Unsigned div of a huge value: 0xFFFF_FFFF_FFFF_FFFE / 2 = 0x7FFF...F
        Case {
            name: "i64.div_u: 0xFFFF..FE / 2 == 0x7FFF..FF → 1",
            ops: vec![
                WasmOp::I64Const(-2i64),                     // = 0xFFFF_FFFF_FFFF_FFFE
                WasmOp::I64Const(2),
                WasmOp::I64DivU,
                WasmOp::I64Const(0x7FFF_FFFF_FFFF_FFFFi64),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: false,
        },
        // ── Bit ops ─────────────────────────────────────────────────
        // 0xF0F0_0000_0000_F0F0 & 0x0FF0_0000_0000_0FF0 = 0x00F0_0000_0000_00F0
        Case {
            name: "i64.and: mask",
            ops: vec![
                WasmOp::I64Const(0xF0F0_0000_0000_F0F0u64 as i64),
                WasmOp::I64Const(0x0FF0_0000_0000_0FF0i64),
                WasmOp::I64And,
                WasmOp::I64Const(0x00F0_0000_0000_00F0i64),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: false,
        },
        Case {
            name: "i64.or: set high bits",
            ops: vec![
                WasmOp::I64Const(0x0000_0000_0000_00FFi64),
                WasmOp::I64Const(0xFF00_0000_0000_0000u64 as i64),
                WasmOp::I64Or,
                WasmOp::I64Const(0xFF00_0000_0000_00FFu64 as i64),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: false,
        },
        Case {
            name: "i64.xor: flip bits",
            ops: vec![
                WasmOp::I64Const(0xAAAA_AAAA_AAAA_AAAAu64 as i64),
                WasmOp::I64Const(0x5555_5555_5555_5555i64),
                WasmOp::I64Xor,
                WasmOp::I64Const(-1),  // 0xFFFF_FFFF_FFFF_FFFF
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: false,
        },
        // 1 << 40 = 0x0000_0100_0000_0000, low byte = 0.
        // Use i64.eq to test the full value.
        Case {
            name: "i64.shl: 1 << 40 == 0x0000_0100_0000_0000 → 1",
            ops: vec![
                WasmOp::I64Const(1),
                WasmOp::I64Const(40),
                WasmOp::I64Shl,
                WasmOp::I64Const(0x0000_0100_0000_0000i64),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: false,
        },
        // -1_i64 shr_s 1 = -1 (arithmetic shift preserves sign).
        Case {
            name: "i64.shr_s: -1 >> 1 == -1 → 1",
            ops: vec![
                WasmOp::I64Const(-1),
                WasmOp::I64Const(1),
                WasmOp::I64ShrS,
                WasmOp::I64Const(-1),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: false,
        },
        // -1_i64 shr_u 1 = 0x7FFF_FFFF_FFFF_FFFF.
        Case {
            name: "i64.shr_u: -1 >> 1 == 0x7FFF..FF → 1",
            ops: vec![
                WasmOp::I64Const(-1),
                WasmOp::I64Const(1),
                WasmOp::I64ShrU,
                WasmOp::I64Const(0x7FFF_FFFF_FFFF_FFFFi64),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: false,
        },
        // ── Unsigned comparisons ────────────────────────────────────
        // -1 as u64 is max unsigned; it is NOT less-than 0 unsigned.
        Case {
            name: "i64.lt_u: -1 (=max_u64) < 0 → 0",
            ops: vec![
                WasmOp::I64Const(-1),
                WasmOp::I64Const(0),
                WasmOp::I64LtU,
                WasmOp::End,
            ],
            expected: 0,
            needs_mem: false,
        },
        Case {
            name: "i64.gt_u: -1 (=max_u64) > 0 → 1",
            ops: vec![
                WasmOp::I64Const(-1),
                WasmOp::I64Const(0),
                WasmOp::I64GtU,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: false,
        },
        Case {
            name: "i64.le_u: 5 <=u 5 → 1",
            ops: vec![
                WasmOp::I64Const(5),
                WasmOp::I64Const(5),
                WasmOp::I64LeU,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: false,
        },
        Case {
            name: "i64.ge_u: 5 >=u 10 → 0",
            ops: vec![
                WasmOp::I64Const(5),
                WasmOp::I64Const(10),
                WasmOp::I64GeU,
                WasmOp::End,
            ],
            expected: 0,
            needs_mem: false,
        },
        // ── Signed <= / >= ──────────────────────────────────────────
        Case {
            name: "i64.le_s: -5 <=s -5 → 1",
            ops: vec![
                WasmOp::I64Const(-5),
                WasmOp::I64Const(-5),
                WasmOp::I64LeS,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: false,
        },
        Case {
            name: "i64.ge_s: -5 >=s 0 → 0",
            ops: vec![
                WasmOp::I64Const(-5),
                WasmOp::I64Const(0),
                WasmOp::I64GeS,
                WasmOp::End,
            ],
            expected: 0,
            needs_mem: false,
        },
        // ── Memory: store+load 64-bit round-trip ─────────────────────
        // addr 0, store 0x1234_5678_9ABC_DEF0, load it back, compare.
        Case {
            name: "i64.store + i64.load round-trip (0x1234..DEF0) → 1",
            ops: vec![
                WasmOp::I32Const(0),
                WasmOp::I64Const(0x1234_5678_9ABC_DEF0u64 as i64),
                WasmOp::I64Store(0),
                WasmOp::I32Const(0),
                WasmOp::I64Load(0),
                WasmOp::I64Const(0x1234_5678_9ABC_DEF0u64 as i64),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: true,
        },
        // Write two i64s at offsets 0 and 8, add them through memory.
        //   mem[0..8] = 1_000_000_000
        //   mem[8..16] = 2_000_000_000
        //   (mem[0] + mem[8]) == 3_000_000_000 via i64.eq → 1
        Case {
            name: "i64 sum-in-memory: 1e9 + 2e9 == 3e9 → 1",
            ops: vec![
                WasmOp::I32Const(0),
                WasmOp::I64Const(1_000_000_000),
                WasmOp::I64Store(0),
                WasmOp::I32Const(8),
                WasmOp::I64Const(2_000_000_000),
                WasmOp::I64Store(0),
                WasmOp::I32Const(0),
                WasmOp::I64Load(0),
                WasmOp::I32Const(8),
                WasmOp::I64Load(0),
                WasmOp::I64Add,
                WasmOp::I64Const(3_000_000_000),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
            needs_mem: true,
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

    println!("Phase 13 — full i64 on {}\n", host);

    // Query mem_base up-front so memory-backed cases can build with it.
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
        let mut lw = if case.needs_mem {
            Lowerer::new_function_with_memory(0, Vec::new(), mem_base).unwrap()
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
