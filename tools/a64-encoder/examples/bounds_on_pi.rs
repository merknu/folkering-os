//! Sikkerhet/Phase 1 — runtime bounds checking for every load/store.
//!
//! The JIT now emits a CMP + B.LS + inline-trap sequence before each
//! memory op. Valid accesses skip the trap and behave identically to
//! the pre-bounds-check lowerer; out-of-range accesses route to the
//! trap block which sets X0 = -1 (exit code 0xFF as seen by the
//! harness) and RETs through a properly-restored stack frame.
//!
//! This test covers: valid stores/loads across i32/i64/f32/f64 still
//! produce the expected values; OOB accesses reliably return 0xFF
//! instead of SIGSEGV-ing the harness. Runs on the existing SSH
//! run_bytes harness against a Cortex-A76 Pi 5.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, WasmOp};

/// The Pi harness's mem_buffer is exactly 64 KiB (65536 bytes).
/// Anything at or above this address is out of range for any access
/// width ≥ 1. Some tests use the upper-edge 0xFFFC (last valid i32)
/// and 0x10000 (first invalid) to prove the check is exact.
const MEM_SIZE: u32 = 64 * 1024;

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

fn cases() -> Vec<Case> {
    vec![
        // ── In-bounds, still works (regression guard) ────────────────
        // i32 store+load at offset 0 — classic round-trip.
        Case {
            name: "i32 store+load @ 0 (in-bounds) → 42",
            ops: vec![
                WasmOp::I32Const(0),
                WasmOp::I32Const(42),
                WasmOp::I32Store(0),
                WasmOp::I32Const(0),
                WasmOp::I32Load(0),
                WasmOp::End,
            ],
            expected: 42,
        },
        // i32 load at the very last valid address (mem_size - 4).
        // First write a known value there, then read it back.
        Case {
            name: "i32 store+load @ 0xFFFC (last valid) → 99",
            ops: vec![
                WasmOp::I32Const(0xFFFC),
                WasmOp::I32Const(99),
                WasmOp::I32Store(0),
                WasmOp::I32Const(0xFFFC),
                WasmOp::I32Load(0),
                WasmOp::End,
            ],
            expected: 99,
        },
        // ── Out-of-bounds — the important traps ──────────────────────
        // i32 load one byte past the end. addr = 0xFFFD means bytes
        // 0xFFFD..0x10001 which spills past 0x10000 by one byte.
        Case {
            name: "i32.load @ 0xFFFD (1 byte over) → trap 0xFF",
            ops: vec![
                WasmOp::I32Const(0xFFFD),
                WasmOp::I32Load(0),
                WasmOp::End,
            ],
            expected: 0xFF,
        },
        // Address = mem_size exactly — first fully-invalid slot.
        Case {
            name: "i32.load @ 0x10000 (exact end) → trap 0xFF",
            ops: vec![
                WasmOp::I32Const(0x10000),
                WasmOp::I32Load(0),
                WasmOp::End,
            ],
            expected: 0xFF,
        },
        // i32.store far out of range — must not segfault the harness.
        // Stores leave nothing on the operand stack, so follow with a
        // dummy const so End sees a single value. The store itself
        // should trap before reaching that const — the return value
        // on success would be 1, distinguishable from the trap 0xFF.
        Case {
            name: "i32.store @ 0xDEAD_BEEF (wild pointer) → trap 0xFF",
            ops: vec![
                WasmOp::I32Const(0xDEAD_BEEFu32 as i32),
                WasmOp::I32Const(1),
                WasmOp::I32Store(0),
                WasmOp::I32Const(1),
                WasmOp::End,
            ],
            expected: 0xFF,
        },
        // Addr is in-bounds by itself but the static offset pushes the
        // access off the end. effective = 0xFFFC + 4 = 0x10000 → OOB.
        // (LDR W imm12 is scaled by 4 with imm12 ≤ 0xFFF, so the max
        // representable offset is 0x3FFC; this test uses a modest
        // one.) Proves the bounds check accounts for offset + size,
        // not just the dynamic addr alone.
        Case {
            name: "i32.load addr=0xFFFC offset=4 (offset pushes OOB) → trap 0xFF",
            ops: vec![
                WasmOp::I32Const(0xFFFC),
                WasmOp::I32Load(4),
                WasmOp::End,
            ],
            expected: 0xFF,
        },
        // ── i64 width: 8-byte access ─────────────────────────────────
        // Last valid i64 addr = mem_size - 8 = 0xFFF8.
        Case {
            name: "i64 store+load @ 0xFFF8 (last valid 8B) → 1",
            ops: vec![
                WasmOp::I32Const(0xFFF8),
                WasmOp::I64Const(0x1122_3344_5566_7788),
                WasmOp::I64Store(0),
                WasmOp::I32Const(0xFFF8),
                WasmOp::I64Load(0),
                WasmOp::I64Const(0x1122_3344_5566_7788),
                WasmOp::I64Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        // 8-byte access at 0xFFFC would overflow (reads bytes 0xFFFC..0x10004).
        Case {
            name: "i64.load @ 0xFFFC (4B short) → trap 0xFF",
            ops: vec![
                WasmOp::I32Const(0xFFFC),
                WasmOp::I64Load(0),
                WasmOp::End,
            ],
            expected: 0xFF,
        },
        // ── f32 / f64 ───────────────────────────────────────────────
        Case {
            name: "f32 store+load @ 0 (in-bounds) → 1",
            ops: vec![
                WasmOp::I32Const(0),
                WasmOp::F32Const(2.5),
                WasmOp::F32Store(0),
                WasmOp::I32Const(0),
                WasmOp::F32Load(0),
                WasmOp::F32Const(2.5),
                WasmOp::F32Eq,
                WasmOp::End,
            ],
            expected: 1,
        },
        Case {
            name: "f32.load @ 0x10000 (OOB) → trap 0xFF",
            ops: vec![
                WasmOp::I32Const(0x10000),
                WasmOp::F32Load(0),
                WasmOp::End,
            ],
            expected: 0xFF,
        },
        Case {
            name: "f64 store+load @ 0 (in-bounds) → 1",
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
        },
        Case {
            name: "f64.load @ 0xFFF9 (off by 1 past last valid 8B) → trap 0xFF",
            ops: vec![
                WasmOp::I32Const(0xFFF9),
                WasmOp::F64Load(0),
                WasmOp::End,
            ],
            expected: 0xFF,
        },
    ]
}

fn main() {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "knut@192.168.68.72".to_string());

    println!("Sikkerhet Phase 1 — bounds checks on {}\n", host);

    let mem_base = match query_addrs(&host) {
        Ok(a) => *a.get("mem_base").expect("mem_base required"),
        Err(e) => {
            eprintln!("harness query failed: {e}");
            std::process::exit(2);
        }
    };
    println!("mem_base at 0x{:016x} (size {} bytes)\n", mem_base, MEM_SIZE);

    let cases = cases();
    let mut passed = 0;
    let mut failed = 0;

    for case in &cases {
        let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), mem_base).unwrap();
        // Default mem_size is 64 KiB, matches the harness — no
        // explicit set_mem_size needed.

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
