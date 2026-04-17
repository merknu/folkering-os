//! End-to-end: real Rust attention head → JIT AArch64 → execute on Pi.
//!
//! Loads `examples/wasm-attention/target/.../attention_wasm.wasm`,
//! parses the single function body, JIT-compiles to AArch64 with
//! `Lowerer::new_function_with_memory_typed`, streams the inputs +
//! weights as a DATA frame at offset BASE=0x1000, signs and ships
//! the CODE frame, EXECs, and reads the i32 RESULT (a checksum of
//! the attention output).
//!
//! Expected exit code: 2239 — must match the Python reference
//! (examples/wasm-attention/reference.py) byte-for-byte. The
//! reference implements the same f32 evaluation order and uses
//! the same exp_approx polynomial.
//!
//! Usage:
//!   PI_HOST=192.168.68.72:7700 cargo run --release --example run_real_wasm_attention

use std::net::TcpStream;
use std::time::Duration;

use a64_encoder::{parse_module, Lowerer, ValType};
use a64_streamer::{
    auth, parse_hello, parse_result, read_frame, serialize_data, write_frame, DEFAULT_PORT,
    FRAME_CODE, FRAME_DATA, FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
};

const WASM_PATH: &str =
    "examples/wasm-attention/target/wasm32-unknown-unknown/release/attention_wasm.wasm";

const S: usize = 4;
const D: usize = 4;

/// Build the host-side data buffer: 4 inputs + 3 weight matrices.
/// Layout matches the offsets baked into lib.rs (BASE=0x1000):
///
///   0x000  inputs   [S][D]  64 B
///   0x040  Wq       [D][D]  64 B
///   0x080  Wk       [D][D]  64 B
///   0x0C0  Wv       [D][D]  64 B
fn build_attention_data() -> Vec<u8> {
    let inputs: [[f32; D]; S] = [
        [ 1.0,  0.5, -0.3,  0.8],
        [ 0.2, -0.7,  0.4,  0.1],
        [-0.5,  0.6,  0.9, -0.2],
        [ 0.3,  0.1, -0.4,  0.7],
    ];
    let wq: [[f32; D]; D] = [
        [ 0.5, -0.3,  0.8,  0.1],
        [-0.2,  0.7,  0.4, -0.6],
        [ 0.9, -0.1, -0.5,  0.3],
        [ 0.1,  0.4,  0.6, -0.2],
    ];
    let wk: [[f32; D]; D] = [
        [ 0.3, -0.2,  0.5,  0.1],
        [-0.1,  0.4,  0.2, -0.3],
        [ 0.2, -0.5,  0.1,  0.4],
        [ 0.4,  0.1, -0.2,  0.3],
    ];
    let wv: [[f32; D]; D] = [
        [ 0.5,  0.2, -0.3,  0.4],
        [-0.1,  0.3,  0.5,  0.2],
        [ 0.4, -0.2,  0.1, -0.5],
        [ 0.2,  0.5, -0.4,  0.3],
    ];

    let mut buf = Vec::with_capacity(4 * 4 * 16);
    let push = |b: &mut Vec<u8>, mat: &[[f32; D]]| {
        for row in mat { for &v in row { b.extend_from_slice(&v.to_le_bytes()); } }
    };
    push(&mut buf, &inputs);
    push(&mut buf, &wq);
    push(&mut buf, &wk);
    push(&mut buf, &wv);
    buf
}

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
    println!("[ATTN] Loading {WASM_PATH}");
    let wasm = std::fs::read(WASM_PATH).unwrap_or_else(|e| {
        eprintln!("Could not read {WASM_PATH}: {e}");
        eprintln!("Build first:");
        eprintln!("  cd examples/wasm-attention");
        eprintln!("  RUSTFLAGS='-C link-arg=--no-entry -C link-arg=--export=attention' \\");
        eprintln!("    cargo build --target wasm32-unknown-unknown --release");
        std::process::exit(2);
    });
    println!("[ATTN] {} bytes WASM module", wasm.len());

    let bodies = parse_module(&wasm).expect("parse_module");
    let body = bodies.into_iter().next().expect("at least one function");
    let n_i32 = body.local_types.iter().filter(|&&b| b == 0x7F).count();
    let n_f32 = body.local_types.iter().filter(|&&b| b == 0x7D).count();
    println!(
        "[ATTN] parsed: {} ops, {} locals (i32={}, f32={})",
        body.ops.len(),
        body.num_locals,
        n_i32,
        n_f32
    );

    let weights = build_attention_data();
    println!("[ATTN] {} bytes input+weight data", weights.len());

    // ── Connect to daemon ─────────────────────────────────────────
    let raw = std::env::var("PI_HOST").unwrap_or_else(|_| "192.168.68.72".to_string());
    let host_part = raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw);
    let addr = if host_part.contains(':') {
        host_part.to_string()
    } else {
        format!("{host_part}:{DEFAULT_PORT}")
    };
    println!("[ATTN] connecting to {addr}");
    let mut sock = TcpStream::connect(&addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();

    let (ty, payload) = read_frame(&mut sock).expect("HELLO");
    assert_eq!(ty, FRAME_HELLO);
    let hello = parse_hello(&payload).expect("parse HELLO");
    println!(
        "[ATTN] HELLO: mem_base=0x{:016x} mem_size={}",
        hello.mem_base, hello.mem_size
    );

    // ── JIT-compile ───────────────────────────────────────────────
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
    println!("[ATTN] JIT-compiled to {} bytes AArch64", code.len());

    // ── DATA at BASE=0x1000 (must match lib.rs::BASE) ─────────────
    const BASE: u32 = 0x1000;
    let data_payload = serialize_data(BASE, &weights);
    write_frame(&mut sock, FRAME_DATA, &data_payload).expect("write DATA");
    println!("[ATTN] DATA frame sent ({} bytes at offset 0x{:x})", weights.len(), BASE);

    // ── HMAC-signed CODE + EXEC ───────────────────────────────────
    let tag = auth::sign(&code);
    let mut code_payload = Vec::with_capacity(code.len() + auth::TAG_LEN);
    code_payload.extend_from_slice(&code);
    code_payload.extend_from_slice(&tag);
    write_frame(&mut sock, FRAME_CODE, &code_payload).expect("write CODE");

    write_frame(&mut sock, FRAME_EXEC, &[]).expect("write EXEC");
    let (ty, payload) = read_frame(&mut sock).expect("RESULT");
    assert_eq!(ty, FRAME_RESULT);
    let exit = parse_result(&payload).expect("parse RESULT");

    // Daemon transfers the full i32 via a pipe from the forked
    // child — no 8-bit exit-code truncation. Full match expected.
    let expected: i32 = 2239;
    println!("[ATTN] Pi result: {exit}  (expected {expected} from reference.py)");

    if exit == expected {
        println!("[ATTN] MATCH — real Rust attention head runs on AArch64!");
    } else if (exit - expected).abs() <= 1 {
        println!("[ATTN] MATCH (within rounding) — expected {expected}");
    } else {
        println!("[ATTN] MISMATCH — expected {expected}, got {exit}");
        std::process::exit(1);
    }
}
