//! Phase 4A: verify `WasmOp::Call(n)` end-to-end on real aarch64 HW.
//!
//! The encoder's Call lowering emits a MOVZ/MOVK chain into X16
//! followed by BLR X16. Byte-level tests already confirm the
//! encoding is correct; this example confirms the **runtime** path
//! — that a JITted BLR actually transfers control to a foreign
//! function, and that the foreign return lands back in our X0 as
//! AAPCS64 expects.
//!
//! Flow:
//!   1. Ask the Pi harness for the runtime addresses of its
//!      `helper_*` functions (ASLR randomises these per run).
//!   2. Build a tiny JIT function whose body is just `Call(0); End`
//!      with the target set to `helper_return_42`.
//!   3. Pipe the bytes to `run_bytes`, read the exit code.
//!   4. Expected: 42 (from the helper).
//!
//! The test passes only if the BLR actually dispatched — a broken
//! MOVZ/MOVK chain would either jump somewhere random (SIGSEGV)
//! or return an unrelated value. A non-matching exit code is a
//! genuine signal, not a slow-path.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, WasmOp};

fn query_helper_addrs(host: &str) -> Result<HashMap<String, u64>, String> {
    let out = Command::new("ssh")
        .arg(host)
        .arg("~/a64-harness/run_bytes --addrs")
        .output()
        .map_err(|e| format!("ssh spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "harness --addrs failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = HashMap::new();
    for line in text.lines() {
        // Expect "name=0x....."
        let Some((name, addr)) = line.split_once('=') else { continue };
        let addr = addr.trim();
        let addr = addr
            .strip_prefix("0x")
            .or_else(|| addr.strip_prefix("0X"))
            .unwrap_or(addr);
        let parsed = u64::from_str_radix(addr, 16)
            .map_err(|e| format!("parse {name}={addr}: {e}"))?;
        map.insert(name.to_string(), parsed);
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
        .map_err(|e| format!("spawn ssh: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "no stdin".to_string())?
        .write_all(bytes)
        .map_err(|e| format!("write bytes: {e}"))?;
    drop(child.stdin.take());
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.is_empty() {
        eprintln!("harness stderr: {stderr}");
    }
    out.status.code().ok_or_else(|| format!("no exit code; stderr: {stderr}"))
}

fn main() {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "knut@192.168.68.72".to_string());

    println!("Phase 4A — Call() end-to-end on {}\n", host);

    let addrs = match query_helper_addrs(&host) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("failed to query harness: {e}");
            std::process::exit(2);
        }
    };

    let addr = match addrs.get("helper_return_42") {
        Some(a) => *a,
        None => {
            eprintln!("helper_return_42 not in --addrs output");
            std::process::exit(2);
        }
    };

    println!("helper_return_42 at 0x{:016x}", addr);

    // Emit: new_function with call target → Call(0) → End
    // Emitted sequence should be:
    //   STP  X29, X30, [SP, #-16]!
    //   MOVZ X16, #<h0>
    //   MOVK X16, #<h1>, LSL #16   (if h1 != 0)
    //   MOVK X16, #<h2>, LSL #32   (if h2 != 0)
    //   MOVK X16, #<h3>, LSL #48   (if h3 != 0)
    //   BLR  X16
    //   LDP  X29, X30, [SP], #16
    //   RET
    let mut lw = Lowerer::new_function(0, vec![addr]).expect("new_function");
    lw.lower_all(&[WasmOp::Call(0), WasmOp::End])
        .expect("lower Call/End");
    let bytes = lw.finish();
    println!("emitted {} bytes of A64:", bytes.len());
    for (i, chunk) in bytes.chunks_exact(4).enumerate() {
        let w = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        println!("  [{:2}] 0x{:08X}", i, w);
    }

    let rv = match run_on_pi(&host, &bytes) {
        Ok(rv) => rv,
        Err(e) => {
            eprintln!("run failed: {e}");
            std::process::exit(2);
        }
    };

    let got = (rv & 0xFF) as u8;
    if got == 42 {
        println!("[ ok ] Call(helper_return_42) returned {got} via BLR");
    } else {
        println!("[FAIL] expected 42, got {got}");
        std::process::exit(1);
    }
}
