//! Phase 4D: the victory lap.
//!
//! A real, non-trivial program — iterative Fibonacci — compiled
//! from WASM-like ops by our hand-written AArch64 emitter and
//! executed on a Cortex-A76. Exercises every feature of the
//! encoder in one function:
//!
//!   * 4 locals (a, b, tmp, n) in X19..X22
//!   * `if` / `end` with CBZ forward-patch
//!   * `loop` / `end` with B/CBNZ backward branch
//!   * `br_if` with CBNZ to loop-start
//!   * `i32.const` / `i32.add` / `i32.sub`
//!   * `local.get` / `local.set` via ADD Xd, XZR, Xsrc
//!   * Function prologue/epilogue (STP/LDP frame)
//!
//! The JIT output is bit-for-bit deterministic; the only thing we
//! verify here is that it computes Fibonacci correctly when run on
//! real aarch64 silicon.
//!
//! # Algorithm (`fib(n)` as WasmOps)
//!
//! ```text
//! locals: X19=a (←0), X20=b (←0), X21=tmp (←0), X22=n (←0)
//!
//! I32Const(N)   LocalSet(3)   ; n = N
//! I32Const(1)   LocalSet(1)   ; b = 1
//! LocalGet(3)                 ; push n as if-cond
//! If                           ; if n != 0
//!   Loop                       ;   do {
//!     LocalGet(0) LocalGet(1) I32Add LocalSet(2)  ; tmp = a + b
//!     LocalGet(1) LocalSet(0)                      ; a   = b
//!     LocalGet(2) LocalSet(1)                      ; b   = tmp
//!     LocalGet(3) I32Const(1) I32Sub LocalSet(3)   ; n  -= 1
//!     LocalGet(3) BrIf(0)                          ; } while (n != 0)
//!   End
//! End
//! LocalGet(0)                  ; return a
//! End
//! ```

use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, WasmOp};

fn build_fib(n: i32) -> Vec<u8> {
    let mut lw = Lowerer::new_with_locals(4).expect("4 locals fits in budget");
    let prog = vec![
        // n = N
        WasmOp::I32Const(n), WasmOp::LocalSet(3),
        // b = 1  (a stays 0, tmp stays 0 — both zero-init'd by the lowerer)
        WasmOp::I32Const(1), WasmOp::LocalSet(1),

        // if (n != 0) {
        WasmOp::LocalGet(3),
        WasmOp::If,
            // loop {
            WasmOp::Loop,
                //   tmp = a + b
                WasmOp::LocalGet(0),
                WasmOp::LocalGet(1),
                WasmOp::I32Add,
                WasmOp::LocalSet(2),
                //   a = b
                WasmOp::LocalGet(1),
                WasmOp::LocalSet(0),
                //   b = tmp
                WasmOp::LocalGet(2),
                WasmOp::LocalSet(1),
                //   n -= 1
                WasmOp::LocalGet(3),
                WasmOp::I32Const(1),
                WasmOp::I32Sub,
                WasmOp::LocalSet(3),
                //   if (n != 0) continue;
                WasmOp::LocalGet(3),
                WasmOp::BrIf(0),
            // }
            WasmOp::End,
        // }
        WasmOp::End,

        // return a
        WasmOp::LocalGet(0),
        WasmOp::End,
    ];
    lw.lower_all(&prog).expect("fib lowers cleanly");
    lw.finish()
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

/// Reference Fibonacci, used to produce the expected exit code.
fn fib_ref(n: i32) -> i32 {
    if n == 0 { return 0; }
    let (mut a, mut b) = (0i32, 1i32);
    let mut n = n;
    while n != 0 {
        let tmp = a + b;
        a = b;
        b = tmp;
        n -= 1;
    }
    a
}

fn main() {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "knut@192.168.68.72".to_string());

    println!("Phase 4D — Fibonacci on {}\n", host);

    let cases = [0, 1, 2, 5, 6, 8, 10, 12];
    let mut passed = 0;
    let mut failed = 0;

    for &n in &cases {
        let expected = fib_ref(n);
        let bytes = build_fib(n);
        match run_on_pi(&host, &bytes) {
            Ok(rv) => {
                let got = (rv & 0xFF) as i32;
                if got == expected & 0xFF {
                    println!(
                        "  [ ok ] fib({:2}) = {:3}   ({} bytes of JIT)",
                        n,
                        expected,
                        bytes.len()
                    );
                    passed += 1;
                } else {
                    println!(
                        "  [FAIL] fib({}) expected {}, got {}",
                        n, expected, got
                    );
                    failed += 1;
                }
            }
            Err(e) => {
                println!("  [err ] fib({}) — {}", n, e);
                failed += 1;
            }
        }
    }

    println!("\n{} passed, {} failed", passed, failed);
    if failed > 0 {
        std::process::exit(1);
    }
}
