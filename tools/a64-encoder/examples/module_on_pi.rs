//! Phase 7 end-to-end — parse a real WebAssembly binary module,
//! lower its function body, execute on Cortex-A76.
//!
//! Uses hand-assembled `.wasm` bytes (the same layout `wat2wasm`
//! would produce) to avoid build-time tool dependencies. If a
//! `wat2wasm` is available the same pattern works on its output;
//! this example just bakes the bytes in so the test is hermetic.

use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{parse_module, Lowerer};

struct Case {
    name: &'static str,
    module: &'static [u8],
    expected: u8,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "(func (result i32) i32.const 42)",
            module: &[
                0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, // \0asm, version 1
                // type: () -> i32
                0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F,
                // func 0 has type 0
                0x03, 0x02, 0x01, 0x00,
                // code: one function, body = i32.const 42 ; end
                0x0A, 0x06, 0x01, 0x04, 0x00, 0x41, 0x2A, 0x0B,
            ],
            expected: 42,
        },
        Case {
            // i32.const 5 ; i32.const 3 ; i32.gt_s ; if then 100 else 1 ; end
            // Returns 100 when 5 > 3 (true).
            name: "if-then-else with i32.gt_s",
            module: &[
                0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
                0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F,
                0x03, 0x02, 0x01, 0x00,
                // code section: section_size = 0x12 (18), entry_size = 0x10 (16)
                // NB: `i32.const 100` is `0x41 0xE4 0x00` (3 bytes),
                // not `0x41 0x64` — 0x64 alone is a *signed* LEB128
                // byte whose bit 6 is set, so it sign-extends to -28.
                // Correct form: 0xE4 = continuation + low 7 bits 0x64,
                // 0x00 = terminator with bit 6 = 0 → positive 100.
                0x0A, 0x12, 0x01, 0x10, 0x00,
                0x41, 0x05,            // i32.const 5
                0x41, 0x03,            // i32.const 3
                0x4A,                  // i32.gt_s
                0x04, 0x7F,            // if (result i32)
                0x41, 0xE4, 0x00,      // i32.const 100
                0x05,                  // else
                0x41, 0x01,            // i32.const 1
                0x0B,                  // end (if)
                0x0B,                  // end (function)
            ],
            expected: 100,
        },
        Case {
            // Loop-based sum 1..=4 = 10.
            //   (func (result i32) (local i32) (local i32)
            //     ;; local 0 = accumulator, local 1 = counter (start at 4)
            //     i32.const 4    local.set 1
            //     loop
            //       local.get 0    local.get 1    i32.add    local.set 0  ;; acc += counter
            //       local.get 1    i32.const 1    i32.sub    local.set 1  ;; counter--
            //       local.get 1    br_if 0                                 ;; loop while counter != 0
            //     end
            //     local.get 0
            //   )
            name: "loop sum 1+2+3+4 = 10",
            module: &[
                0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
                0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F,
                0x03, 0x02, 0x01, 0x00,
                // code section:
                //   Section id = 0x0A, size = TBD
                //   count = 1, entry_size = TBD
                //   locals: 1 group, count=2, type=i32 (0x7F)     → 3 bytes "01 02 7F"
                //   body:
                //     41 04       i32.const 4
                //     21 01       local.set 1
                //     03 7F       loop (result? actually for pure loop no result — use 0x40 for void)
                //                 Note: loop/block type 0x40 = void (empty); using that.
                //     20 00 20 01 6A 21 00    acc += counter
                //     20 01 41 01 6B 21 01    counter--
                //     20 01 0D 00             br_if 0 if counter != 0
                //     0B                      end (loop)
                //     20 00                   local.get 0 (result)
                //     0B                      end (function)
                //
                // Rewriting with 0x40 as loop block type (void):
                //   entry bytes: 01 02 7F 41 04 21 01 03 40
                //                20 00 20 01 6A 21 00
                //                20 01 41 01 6B 21 01
                //                20 01 0D 00 0B 20 00 0B
                //   count = 27 bytes
                // section_size = 0x21 (33), entry_size = 0x1F (31)
                0x0A, 0x21, 0x01, 0x1F,
                0x01, 0x02, 0x7F,                          // locals: 2 × i32
                0x41, 0x04, 0x21, 0x01,                    // counter = 4
                0x03, 0x40,                                // loop (void)
                0x20, 0x00, 0x20, 0x01, 0x6A, 0x21, 0x00,  // acc += counter
                0x20, 0x01, 0x41, 0x01, 0x6B, 0x21, 0x01,  // counter -= 1
                0x20, 0x01, 0x0D, 0x00,                    // br_if 0 (loop if counter != 0)
                0x0B,                                       // end (loop)
                0x20, 0x00,                                 // local.get 0
                0x0B,                                       // end (function)
            ],
            expected: 10,
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

    println!("Phase 7 — parse real .wasm module → JIT → Pi ({})\n", host);

    let cases = cases();
    let mut passed = 0;
    let mut failed = 0;

    for case in &cases {
        // 1. Parse the .wasm binary.
        let bodies = match parse_module(case.module) {
            Ok(b) => b,
            Err(e) => {
                println!("  [err ] {}: parse: {:?}", case.name, e);
                failed += 1;
                continue;
            }
        };
        let body = &bodies[0];
        if std::env::var("A64_DEBUG").is_ok() {
            println!("  DEBUG {}: locals={} ops={:?}", case.name, body.num_locals, body.ops);
        }

        // 2. Lower to AArch64 bytes.
        let mut lw = Lowerer::new_with_locals(body.num_locals as usize)
            .expect("locals within budget");
        if let Err(e) = lw.lower_all(&body.ops) {
            println!("  [err ] {}: lower: {:?}", case.name, e);
            failed += 1;
            continue;
        }
        let code = lw.finish();

        // 3. Execute on the Pi.
        match run_on_pi(&host, &code) {
            Ok(rv) => {
                let got = (rv & 0xFF) as u8;
                if got == case.expected {
                    println!(
                        "  [ ok ] {} — {} bytes wasm → {} bytes A64 → exit {}",
                        case.name,
                        case.module.len(),
                        code.len(),
                        got
                    );
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
