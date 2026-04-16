//! a64-stream-smoke-test — end-to-end verification client.
//!
//! Connects to an a64-stream-daemon, exercises the full protocol,
//! verifies the JIT path works over TCP. Runs from any platform —
//! the client only needs to *build* A64 bytes (host-agnostic) and
//! speak TCP.
//!
//! Cases (all target helpers baked by name from HELLO):
//!   1. CODE+EXEC of `helper_return_42()` — returns 42.
//!   2. CODE+EXEC of a JIT that calls `helper_add_five(37)` — 42.
//!   3. DATA to write an i32 into mem[0]; CODE that reads mem[0],
//!      doubles it; EXEC; result is 2 × sent value. Exercises the
//!      sensor-streaming flow end-to-end.
//!
//! Usage:
//!   a64-stream-smoke-test               # defaults to 192.168.68.72:14712
//!   a64-stream-smoke-test 127.0.0.1     # explicit host
//!   a64-stream-smoke-test host:14712    # host:port

use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;

use a64_encoder::{FnSig, Lowerer, ValType, WasmOp};
use a64_streamer::{
    auth, parse_hello, parse_result, read_frame, serialize_data, write_frame, DEFAULT_PORT,
    FRAME_BYE, FRAME_CODE, FRAME_DATA, FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let target = args.next().unwrap_or_else(|| "192.168.68.72".to_string());
    let addr = if target.contains(':') {
        target
    } else {
        format!("{target}:{DEFAULT_PORT}")
    };

    println!("[smoke] connecting to {addr}");
    let mut sock = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[smoke] connect failed: {e}");
            std::process::exit(2);
        }
    };
    // Reasonable timeouts so a hung daemon doesn't wedge the test.
    sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();
    sock.set_write_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();

    // ── 1. HELLO ──────────────────────────────────────────────────
    let (ty, payload) = read_frame(&mut sock).expect("read HELLO");
    assert_eq!(ty, FRAME_HELLO, "first frame must be HELLO");
    let hello = parse_hello(&payload).expect("parse HELLO");
    println!(
        "[smoke] HELLO received: mem_base=0x{:016x} mem_size={} helpers={}",
        hello.mem_base,
        hello.mem_size,
        hello.helpers.len()
    );
    let helpers: HashMap<String, u64> = hello
        .helpers
        .iter()
        .map(|h| (h.name.clone(), h.addr))
        .collect();
    for (name, addr) in &helpers {
        println!("[smoke]   {name} = 0x{addr:016x}");
    }

    let ret_42 = *helpers
        .get("helper_return_42")
        .expect("daemon must expose helper_return_42");
    let add_five = *helpers
        .get("helper_add_five")
        .expect("daemon must expose helper_add_five");
    let add = *helpers
        .get("helper_add")
        .expect("daemon must expose helper_add (2-arg)");
    let linear = *helpers
        .get("helper_linear")
        .expect("daemon must expose helper_linear (3-arg)");

    // Signatures for the helpers we'll be calling. Indexed per
    // call-target slot in each Lowerer — order matters.
    let sig_noarg = FnSig { params: vec![], result: Some(ValType::I32) };
    let sig_1i32 = FnSig { params: vec![ValType::I32], result: Some(ValType::I32) };
    let sig_2i32 = FnSig {
        params: vec![ValType::I32, ValType::I32],
        result: Some(ValType::I32),
    };
    let sig_3i32 = FnSig {
        params: vec![ValType::I32, ValType::I32, ValType::I32],
        result: Some(ValType::I32),
    };

    let mut passed = 0;
    let mut failed = 0;

    // ── Case 1: `call helper_return_42()` → 42 ────────────────────
    {
        let mut lw = Lowerer::new_function(0, vec![ret_42]).unwrap();
        lw.lower_all(&[WasmOp::Call(0), WasmOp::End]).unwrap();
        let bytes = lw.finish();
        let rv = send_code_and_exec(&mut sock, &bytes);
        report("call helper_return_42 → 42", rv, 42, &mut passed, &mut failed);
    }

    // ── Case 2: pure JIT arithmetic (37 + 5 = 42) ─────────────────
    //
    // `lower_call` is 0-arg-only today (Phase 4A design, see the
    // Phase 16 note — proper arg-marshalling lives in call_indirect).
    // Arithmetic alone exercises CODE + EXEC + RESULT with a non-
    // trivial instruction sequence (MOVZ + ADD + RET).
    {
        let mut lw = Lowerer::new();
        lw.lower_all(&[
            WasmOp::I32Const(37),
            WasmOp::I32Const(5),
            WasmOp::I32Add,
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        let rv = send_code_and_exec(&mut sock, &bytes);
        report("JIT arith 37 + 5 → 42", rv, 42, &mut passed, &mut failed);
    }
    // Also prove Call(0) to a 0-arg helper works through the link.
    {
        let mut lw = Lowerer::new_function(0, vec![ret_42])
            .unwrap()
            .with_call_sigs(vec![sig_noarg.clone()]);
        lw.lower_all(&[WasmOp::Call(0), WasmOp::End]).unwrap();
        let bytes = lw.finish();
        let rv = send_code_and_exec(&mut sock, &bytes);
        report(
            "0-arg helper via BLR through TCP → 42",
            rv,
            42,
            &mut passed,
            &mut failed,
        );
    }

    // ── Case 3: 1-arg helper — proves X0 packing ──────────────────
    {
        let mut lw = Lowerer::new_function(0, vec![add_five])
            .unwrap()
            .with_call_sigs(vec![sig_1i32.clone()]);
        lw.lower_all(&[
            WasmOp::I32Const(37),
            WasmOp::Call(0),
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        let rv = send_code_and_exec(&mut sock, &bytes);
        report(
            "1-arg helper_add_five(37) → 42",
            rv,
            42,
            &mut passed,
            &mut failed,
        );
    }

    // ── Case 4: 2-arg helper — proves X0 + X1 packing ─────────────
    {
        let mut lw = Lowerer::new_function(0, vec![add])
            .unwrap()
            .with_call_sigs(vec![sig_2i32.clone()]);
        lw.lower_all(&[
            WasmOp::I32Const(17),
            WasmOp::I32Const(25),
            WasmOp::Call(0),
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        let rv = send_code_and_exec(&mut sock, &bytes);
        report(
            "2-arg helper_add(17, 25) → 42",
            rv,
            42,
            &mut passed,
            &mut failed,
        );
    }

    // ── Case 5: 3-arg helper — proves X0 + X1 + X2 packing ────────
    //
    // helper_linear(a, b, c) = a*b + c; choosing args where no two
    // permutations give the same answer catches mis-indexed regs:
    //   linear(5, 6, 12) = 42
    //   linear(6, 5, 12) = 42  ← commutative, still 42 (bad case!)
    //   linear(12, 5, 6) = 66  ← different → test distinguishes
    // So also assert that linear(12, 5, 6) gives 66, not 42.
    {
        let mut lw = Lowerer::new_function(0, vec![linear])
            .unwrap()
            .with_call_sigs(vec![sig_3i32.clone()]);
        lw.lower_all(&[
            WasmOp::I32Const(5),
            WasmOp::I32Const(6),
            WasmOp::I32Const(12),
            WasmOp::Call(0),
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        let rv = send_code_and_exec(&mut sock, &bytes);
        report(
            "3-arg helper_linear(5, 6, 12) → 42",
            rv,
            42,
            &mut passed,
            &mut failed,
        );
    }
    // Same helper, reordered args — proves X0/X1/X2 aren't shuffled.
    {
        let mut lw = Lowerer::new_function(0, vec![linear])
            .unwrap()
            .with_call_sigs(vec![sig_3i32.clone()]);
        lw.lower_all(&[
            WasmOp::I32Const(12),
            WasmOp::I32Const(5),
            WasmOp::I32Const(6),
            WasmOp::Call(0),
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        let rv = send_code_and_exec(&mut sock, &bytes);
        report(
            "3-arg helper_linear(12, 5, 6) → 66 (order-sensitive)",
            rv,
            66,
            &mut passed,
            &mut failed,
        );
    }

    // ── Case 6: compose — use the result of one call in another ──
    // add(add_five(10), 7) = add(15, 7) = 22
    {
        let mut lw = Lowerer::new_function(0, vec![add_five, add])
            .unwrap()
            .with_call_sigs(vec![sig_1i32.clone(), sig_2i32.clone()]);
        lw.lower_all(&[
            WasmOp::I32Const(10),
            WasmOp::Call(0),     // add_five(10) → 15; leaves 15 on stack
            WasmOp::I32Const(7),
            WasmOp::Call(1),     // add(15, 7) → 22
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        let rv = send_code_and_exec(&mut sock, &bytes);
        report(
            "compose: add(add_five(10), 7) → 22",
            rv,
            22,
            &mut passed,
            &mut failed,
        );
    }

    // ── SIMD through the HMAC-signed pipeline ─────────────────────
    //
    // The SIMD stack has been verified via the raw SSH `run_bytes`
    // harness (see tools/a64-encoder/examples/simd_on_pi.rs). This
    // case proves the same code also executes through our full
    // secure pipeline: HMAC-signed CODE, fork-timeout EXEC,
    // MAP_SHARED linear memory. The JIT program is the same dot-
    // product kernel used in the raw-harness suite:
    //
    //   u = [1, 2, 3, 4]  (written as 4 × i32.store)
    //   v = [5, 6, 7, 8]
    //   dot(u, v) = 1×5 + 2×6 + 3×7 + 4×8 = 70
    //
    // The f32.eq at the end returns 1 on match — 1 fits in the
    // 8-bit exit code without ambiguity.
    {
        let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), hello.mem_base).unwrap();
        let u = [1.0f32, 2.0, 3.0, 4.0];
        let v = [5.0f32, 6.0, 7.0, 8.0];
        let mut ops = Vec::new();
        for (i, val) in u.iter().enumerate() {
            ops.push(WasmOp::I32Const((4 * i) as i32));
            ops.push(WasmOp::I32Const(val.to_bits() as i32));
            ops.push(WasmOp::I32Store(0));
        }
        for (i, val) in v.iter().enumerate() {
            ops.push(WasmOp::I32Const(16 + 4 * i as i32));
            ops.push(WasmOp::I32Const(val.to_bits() as i32));
            ops.push(WasmOp::I32Store(0));
        }
        ops.extend_from_slice(&[
            WasmOp::I32Const(0),
            WasmOp::V128Load(0),
            WasmOp::I32Const(16),
            WasmOp::V128Load(0),
            WasmOp::F32x4Mul,
            WasmOp::F32x4HorizontalSum,
            WasmOp::F32Const(70.0),
            WasmOp::F32Eq,
            WasmOp::End,
        ]);
        lw.lower_all(&ops).unwrap();
        let bytes = lw.finish();
        let rv = send_code_and_exec(&mut sock, &bytes);
        report(
            "SIMD dot([1..4],[5..8]) = 70 via HMAC+timeout pipeline",
            rv,
            1,
            &mut passed,
            &mut failed,
        );
    }

    // ── Case 3: DATA → JIT reads → EXEC (sensor-stream flow) ──────
    //
    // Write i32 = 19 at mem[0]. JIT program:
    //   load mem[0], double it via `+ self`, return → 38.
    //
    // This is the pattern a Folkering client would use for sensor
    // streaming: ship the model (CODE) once, then pump values
    // through DATA+EXEC repeatedly. No helper call needed — the
    // JIT is the model.
    {
        let sample: i32 = 19;
        let data = serialize_data(0, &sample.to_le_bytes());
        write_frame(&mut sock, FRAME_DATA, &data).expect("write DATA");

        let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), hello.mem_base).unwrap();
        lw.lower_all(&[
            WasmOp::I32Const(0), // addr
            WasmOp::I32Load(0),  // load mem[0]
            WasmOp::I32Const(2),
            WasmOp::I32Mul,      // ×2
            WasmOp::End,
        ])
        .unwrap();
        let bytes = lw.finish();
        let rv = send_code_and_exec(&mut sock, &bytes);
        report(
            "DATA[0]=19 → JIT load×2 → 38",
            rv,
            38,
            &mut passed,
            &mut failed,
        );
    }

    // ── Case 4: streaming loop — same code, many DATA+EXEC ────────
    //
    // Prove the happy path: install the multiplier-model once, then
    // stream several samples without re-installing code. This is the
    // actual WASM Streaming Service flow.
    {
        // Keep the same code loaded (it's still load×2).
        let samples = [1i32, 2, 3, 7, 21];
        let mut all_ok = true;
        for s in samples {
            write_frame(&mut sock, FRAME_DATA, &serialize_data(0, &s.to_le_bytes()))
                .expect("DATA");
            write_frame(&mut sock, FRAME_EXEC, &[]).expect("EXEC");
            let (ty, pay) = read_frame(&mut sock).expect("RESULT");
            assert_eq!(ty, FRAME_RESULT);
            let rv = parse_result(&pay).expect("parse RESULT");
            let expect = s.wrapping_mul(2);
            if rv != expect {
                eprintln!("  [FAIL] stream sample {s}: got {rv}, want {expect}");
                all_ok = false;
            }
        }
        if all_ok {
            println!("  [ ok ] stream 5 samples × 2 without CODE re-install");
            passed += 1;
        } else {
            failed += 1;
        }
    }

    // ── BYE ───────────────────────────────────────────────────────
    let _ = write_frame(&mut sock, FRAME_BYE, &[]);
    let _ = sock.flush();

    println!("\n{passed} passed, {failed} failed");
    if failed > 0 {
        std::process::exit(1);
    }
}

/// Send CODE+EXEC, return the i32 result. Panics on protocol errors.
fn send_code_and_exec(sock: &mut TcpStream, bytes: &[u8]) -> i32 {
    send_code_signed(sock, bytes).expect("write CODE");
    write_frame(sock, FRAME_EXEC, &[]).expect("write EXEC");
    let (ty, payload) = read_frame(sock).expect("read RESULT");
    assert_eq!(ty, FRAME_RESULT, "expected RESULT, got 0x{ty:02x}");
    parse_result(&payload).expect("parse RESULT")
}

/// Send a CODE frame with its HMAC-SHA256 tag appended. The Pi-side
/// daemon rejects any CODE without a valid tag, so every client that
/// wants to ship code has to go through this helper.
fn send_code_signed(sock: &mut TcpStream, code: &[u8]) -> std::io::Result<()> {
    let tag = auth::sign(code);
    let mut payload = Vec::with_capacity(code.len() + auth::TAG_LEN);
    payload.extend_from_slice(code);
    payload.extend_from_slice(&tag);
    write_frame(sock, FRAME_CODE, &payload)
}

fn report(name: &str, got: i32, expected: i32, passed: &mut i32, failed: &mut i32) {
    if got == expected {
        println!("  [ ok ] {name}");
        *passed += 1;
    } else {
        println!("  [FAIL] {name}: got {got}, expected {expected}");
        *failed += 1;
    }
}
