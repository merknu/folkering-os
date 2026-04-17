//! End-to-end: real Rust-compiled .wasm → JIT AArch64 → execute on Pi.
//!
//! Pipeline:
//!   1. Read examples/wasm-mlp/target/.../mlp_wasm.wasm (compiled
//!      from the `mlp-wasm` crate via `cargo build --target
//!      wasm32-unknown-unknown --release`).
//!   2. Parse the module with `parse_module()` — extracts the `infer`
//!      function body (locals + ops).
//!   3. Connect to a64-stream-daemon on the Pi, read HELLO, get the
//!      real `mem_base` of the daemon's 64 KiB linear memory.
//!   4. Build a Lowerer typed for the function's locals (f32).
//!   5. JIT-compile the WASM ops to AArch64 machine code.
//!   6. Send weights as a DATA frame; send signed CODE; EXEC; collect
//!      RESULT.
//!
//! This proves the full toolchain: real Rust compiler → real WASM →
//! our parser → our JIT → real ARM execution. No hand-built ops.
//!
//! Usage:
//!   PI_HOST=192.168.68.72:7700 cargo run --release --example run_real_wasm_mlp

use std::net::TcpStream;
use std::time::Duration;

use a64_encoder::{parse_module, Lowerer, ValType};
use a64_streamer::{
    auth, parse_hello, parse_result, read_frame, serialize_data, write_frame, DEFAULT_PORT,
    FRAME_CODE, FRAME_DATA, FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
};

const WASM_PATH: &str =
    "examples/wasm-mlp/target/wasm32-unknown-unknown/release/mlp_wasm.wasm";

fn build_weight_buffer() -> Vec<u8> {
    // Same weights as mlp_memory_on_pi.rs — produces output 0.5222 → exit 52.
    let mut buf = vec![0u8; 256];
    let inputs: [f32; 4] = [1.0, 0.5, -0.3, 0.8];
    let w1: [f32; 16] = [
        0.5, -0.3, 0.8, 0.1,  -0.2, 0.7, 0.4, -0.6,
        0.9, -0.1, -0.5, 0.3,  0.1, 0.4, 0.6, -0.2,
    ];
    let b1: [f32; 4] = [0.1, -0.1, 0.2, 0.0];
    let w2: [f32; 16] = [
        0.3, -0.2, 0.5, 0.1,  -0.1, 0.4, 0.2, -0.3,
        0.2, -0.5, 0.1, 0.4,   0.4, 0.1, -0.2, 0.3,
    ];
    let b2: [f32; 4] = [0.1, 0.0, -0.1, 0.2];
    let w3: [f32; 4] = [0.5, -0.3, 0.4, 0.2];
    let b3: f32 = 0.1;

    fn write_f32s(buf: &mut [u8], off: usize, vals: &[f32]) {
        for (i, v) in vals.iter().enumerate() {
            buf[off + i * 4..off + i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    write_f32s(&mut buf, 0, &inputs);
    write_f32s(&mut buf, 16, &w1);
    write_f32s(&mut buf, 80, &b1);
    write_f32s(&mut buf, 96, &w2);
    write_f32s(&mut buf, 160, &b2);
    write_f32s(&mut buf, 176, &w3);
    write_f32s(&mut buf, 192, &[b3]);
    buf
}

/// Map a WASM valtype byte (0x7D=f32, 0x7C=f64, 0x7F=i32, 0x7E=i64)
/// to our `ValType`. Panics on unsupported types — these would also
/// panic during lowering, so failing early is clearer.
fn vt(b: u8) -> ValType {
    match b {
        0x7F => ValType::I32,
        0x7E => ValType::I64,
        0x7D => ValType::F32,
        0x7C => ValType::F64,
        _ => panic!("unsupported valtype 0x{b:02x}"),
    }
}

fn main() {
    println!("[REAL-WASM] Loading {WASM_PATH}");
    let wasm = std::fs::read(WASM_PATH).unwrap_or_else(|e| {
        eprintln!("Could not read {WASM_PATH}: {e}");
        eprintln!("Build first:");
        eprintln!("  cd examples/wasm-mlp");
        eprintln!("  RUSTFLAGS='-C link-arg=--no-entry -C link-arg=--export=infer' \\");
        eprintln!("    cargo build --target wasm32-unknown-unknown --release");
        std::process::exit(2);
    });
    println!("[REAL-WASM] {} bytes WASM module", wasm.len());

    let bodies = parse_module(&wasm).expect("parse_module");
    let body = bodies.into_iter().next().expect("at least one function");
    println!(
        "[REAL-WASM] parsed: {} locals (types {:?}), {} ops",
        body.num_locals, body.local_types, body.ops.len()
    );

    let weights = build_weight_buffer();
    println!("[REAL-WASM] {} bytes weight data", weights.len());

    // ── Connect to daemon ─────────────────────────────────────────
    let raw = std::env::var("PI_HOST").unwrap_or_else(|_| "192.168.68.72".to_string());
    let host_part = raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw);
    let addr = if host_part.contains(':') {
        host_part.to_string()
    } else {
        format!("{host_part}:{DEFAULT_PORT}")
    };
    println!("[REAL-WASM] connecting to {addr}");
    let mut sock = TcpStream::connect(&addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();

    let (ty, payload) = read_frame(&mut sock).expect("HELLO");
    assert_eq!(ty, FRAME_HELLO);
    let hello = parse_hello(&payload).expect("parse HELLO");
    println!(
        "[REAL-WASM] HELLO: mem_base=0x{:016x} mem_size={}",
        hello.mem_base, hello.mem_size
    );

    // ── JIT-compile the parsed function ───────────────────────────
    let local_types: Vec<ValType> = body.local_types.iter().map(|&b| vt(b)).collect();
    let mut lw = Lowerer::new_function_with_memory_typed(
        &local_types,
        Vec::new(),
        hello.mem_base,
    )
    .expect("Lowerer");
    lw.set_mem_size(hello.mem_size);
    lw.lower_all(&body.ops).expect("lower_all");
    let code = lw.finish();
    println!("[REAL-WASM] JIT-compiled to {} bytes AArch64", code.len());

    // ── DATA + signed CODE + EXEC ─────────────────────────────────
    // Match the BASE constant in the WASM source (0x1000) — the
    // crate uses a high base address to avoid LLVM's null-page
    // UB optimization that folds reads from 0..0xFFF to undef.
    const BASE: u32 = 0x1000;
    let data_payload = serialize_data(BASE, &weights);
    write_frame(&mut sock, FRAME_DATA, &data_payload).expect("write DATA");

    let tag = auth::sign(&code);
    let mut code_payload = Vec::with_capacity(code.len() + auth::TAG_LEN);
    code_payload.extend_from_slice(&code);
    code_payload.extend_from_slice(&tag);
    write_frame(&mut sock, FRAME_CODE, &code_payload).expect("write CODE");

    write_frame(&mut sock, FRAME_EXEC, &[]).expect("write EXEC");
    let (ty, payload) = read_frame(&mut sock).expect("RESULT");
    assert_eq!(ty, FRAME_RESULT);
    let exit = parse_result(&payload).expect("parse RESULT");

    println!("[REAL-WASM] Pi exit code: {exit}");
    println!("[REAL-WASM] Network output: {:.4}", exit as f32 / 100.0);

    let expected = 52;
    if exit == expected {
        println!("[REAL-WASM] MATCH — real Rust→WASM→JIT→ARM works end-to-end!");
    } else if (exit - expected).abs() <= 1 {
        println!("[REAL-WASM] MATCH (within rounding) — expected {expected}");
    } else {
        println!("[REAL-WASM] MISMATCH — expected {expected}, got {exit}");
        std::process::exit(1);
    }
}
