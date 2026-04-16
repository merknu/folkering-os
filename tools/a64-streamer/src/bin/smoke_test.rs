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

use a64_encoder::{parse_module, FnSig, Lowerer, ValType, WasmOp};
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

    // ── Real-world WASM module: parse → JIT → execute ────────────
    //
    // Proves the full compiler→binary→JIT→native-exec roundtrip
    // through the secure pipeline. We hand-encode a minimal WASM
    // binary here (what `rustc --target wasm32-unknown-unknown`
    // would emit for a trivial function), parse it with
    // `wasm_module::parse_module`, take the extracted FunctionBody,
    // lower its ops to A64, sign the result with HMAC, and ship it
    // through the TCP protocol to the Pi daemon.
    //
    // Module contents (equivalent WAT):
    //   (module
    //     (func (result i32)
    //       i32.const 3
    //       i32.const 4
    //       i32.add
    //       i32.const 5
    //       i32.mul
    //       i32.const 7
    //       i32.add))
    //
    // Computes (3 + 4) * 5 + 7 = 42.
    //
    // Binary layout (36 bytes):
    //   8 B  header:     \0asm + version 1
    //   7 B  type sec:   1 functype (no params, 1 i32 result)
    //   4 B  func sec:   1 function of type 0
    //  17 B  code sec:   1 body (12 B of ops + prologue/epilogue bytes)
    {
        let wasm_module: [u8; 36] = [
            // Magic + version
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
            // Type section: id 01, size 5, 1 functype, 0 params, 1 i32 result
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F,
            // Function section: id 03, size 2, 1 function of type 0
            0x03, 0x02, 0x01, 0x00,
            // Code section: id 0A, size 15 (0x0F), 1 entry
            //   entry size 13 (0x0D): 0 local decls + 12 bytes of ops + end
            //   ops: i32.const 3; i32.const 4; i32.add; i32.const 5;
            //        i32.mul; i32.const 7; i32.add; end
            0x0A, 0x0F, 0x01, 0x0D, 0x00,
            0x41, 0x03, 0x41, 0x04, 0x6A, 0x41, 0x05, 0x6C, 0x41, 0x07, 0x6A, 0x0B,
        ];

        let bodies = parse_module(&wasm_module).expect("parse WASM module");
        assert_eq!(bodies.len(), 1, "module has exactly one function");
        let body = &bodies[0];
        assert_eq!(body.num_locals, 0);

        // Lower the function body. new_function gives us a proper
        // AAPCS64 prologue/epilogue so the daemon can call it via
        // a plain `extern "C" fn() -> i32` pointer.
        let mut lw = Lowerer::new_function(body.num_locals as usize, Vec::new()).unwrap();
        lw.lower_all(&body.ops).unwrap();
        let bytes = lw.finish();

        let rv = send_code_and_exec(&mut sock, &bytes);
        report(
            "WASM module (3+4)*5+7 → parsed → JIT'd → executed = 42",
            rv,
            42,
            &mut passed,
            &mut failed,
        );
    }

    // ── Real-world WASM: loop + locals + conditional branch ──────
    //
    // Harder test — exercises the full structured-control-flow
    // machinery (block/loop/br_if) and locals (2 × i32) through
    // the parser → lowerer → execute pipeline. Equivalent to:
    //
    //   (func (result i32)
    //     (local $sum i32) (local $i i32)
    //     i32.const 1  local.set $i
    //     block
    //       loop
    //         local.get $i  i32.const 11  i32.ge_s  br_if 1
    //         local.get $sum  local.get $i  i32.add  local.set $sum
    //         local.get $i  i32.const 1  i32.add  local.set $i
    //         br 0
    //       end
    //     end
    //     local.get $sum)
    //
    // Sum 1+2+...+10 = 55. The loop iterates 10 times before the
    // i >= 11 check breaks out.
    {
        let wasm_module: Vec<u8> = [
            // Magic + version
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
            // Type section: 1 functype, 0 params, 1 i32 result
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F,
            // Function section: 1 function of type 0
            0x03, 0x02, 0x01, 0x00,
            // Code section: id 0A, size 0x29 (41), 1 entry of size 0x27 (39)
            0x0A, 0x29, 0x01, 0x27,
            // Locals: 1 group of 2 × i32
            0x01, 0x02, 0x7F,
            //
            // Body ops:
            //   i32.const 1 ; local.set $i (index 1)
            0x41, 0x01, 0x21, 0x01,
            //   block  (void type)
            0x02, 0x40,
            //     loop  (void)
            0x03, 0x40,
            //       local.get $i ; i32.const 11 ; i32.ge_s ; br_if 1
            0x20, 0x01, 0x41, 0x0B, 0x4E, 0x0D, 0x01,
            //       local.get $sum ; local.get $i ; i32.add ; local.set $sum
            0x20, 0x00, 0x20, 0x01, 0x6A, 0x21, 0x00,
            //       local.get $i ; i32.const 1 ; i32.add ; local.set $i
            0x20, 0x01, 0x41, 0x01, 0x6A, 0x21, 0x01,
            //       br 0 (back to loop header)
            0x0C, 0x00,
            //     end (close loop)
            0x0B,
            //   end (close block)
            0x0B,
            //   local.get $sum ; end (function)
            0x20, 0x00, 0x0B,
        ]
        .to_vec();

        let bodies = parse_module(&wasm_module).expect("parse sum-of-10 module");
        assert_eq!(bodies.len(), 1);
        let body = &bodies[0];
        assert_eq!(body.num_locals, 2, "sum + i");

        let mut lw = Lowerer::new_function(body.num_locals as usize, Vec::new()).unwrap();
        lw.lower_all(&body.ops).expect("lower sum-of-10 body");
        let bytes = lw.finish();

        let rv = send_code_and_exec_or_error(&mut sock, &bytes);
        report(
            "WASM loop: Σ(1..10) = 55 via block+loop+br_if + locals",
            rv,
            55,
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

/// Send CODE + EXEC and get result OR error. Like send_code_and_exec
/// but tolerates ERROR frames (returns -1 and prints the reason
/// instead of panicking). Useful for test cases that might crash
/// the forked child (e.g., a buggy JIT loop).
fn send_code_and_exec_or_error(sock: &mut TcpStream, bytes: &[u8]) -> i32 {
    send_code_signed(sock, bytes).expect("write CODE");
    write_frame(sock, FRAME_EXEC, &[]).expect("write EXEC");
    let (ty, payload) = read_frame(sock).expect("read response");
    if ty == FRAME_RESULT {
        parse_result(&payload).expect("parse RESULT")
    } else if ty == a64_streamer::FRAME_ERROR {
        let code_val = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let msg = String::from_utf8_lossy(&payload[4..]);
        eprintln!("  [note] daemon error code {code_val}: {msg}");
        -1
    } else {
        eprintln!("  [note] unexpected frame type 0x{ty:02x}");
        -1
    }
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
