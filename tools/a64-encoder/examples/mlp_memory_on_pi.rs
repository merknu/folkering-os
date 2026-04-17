//! Memory-based MLP — weights loaded from linear memory via f32.load.
//!
//! This is the production-ready pattern: weights live in the DATA
//! segment (linear memory), not as inline constants. The function
//! loads each weight at runtime via f32.load, computes dot products,
//! applies ReLU via select, and returns the scaled output.
//!
//! Network: 4→4→4→1 (same as mlp_on_pi.rs for comparison)
//! Memory layout:
//!   offset 0:    4 input values  (16 bytes)
//!   offset 16:   w1[4][4]        (64 bytes)
//!   offset 80:   b1[4]           (16 bytes)
//!   offset 96:   w2[4][4]        (64 bytes)
//!   offset 160:  b2[4]           (16 bytes)
//!   offset 176:  w3[4]           (16 bytes)
//!   offset 192:  b3              (4 bytes)
//! Total: 196 bytes of weight data

use std::net::TcpStream;
use std::time::Duration;

use a64_encoder::{Lowerer, ValType, WasmOp};
use a64_streamer::{
    auth, parse_hello, parse_result, read_frame, serialize_data, write_frame, DEFAULT_PORT,
    FRAME_CODE, FRAME_DATA, FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
};

const OFF_INPUTS: u32 = 0;
const OFF_W1: u32 = 16;
const OFF_B1: u32 = 80;
const OFF_W2: u32 = 96;
const OFF_B2: u32 = 160;
const OFF_W3: u32 = 176;
const OFF_B3: u32 = 192;

fn build_weight_buffer() -> Vec<u8> {
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
            let bytes = v.to_le_bytes();
            buf[off + i * 4..off + i * 4 + 4].copy_from_slice(&bytes);
        }
    }
    write_f32s(&mut buf, OFF_INPUTS as usize, &inputs);
    write_f32s(&mut buf, OFF_W1 as usize, &w1);
    write_f32s(&mut buf, OFF_B1 as usize, &b1);
    write_f32s(&mut buf, OFF_W2 as usize, &w2);
    write_f32s(&mut buf, OFF_B2 as usize, &b2);
    write_f32s(&mut buf, OFF_W3 as usize, &w3);
    write_f32s(&mut buf, OFF_B3 as usize, &[b3]);
    buf
}

fn build_memory_mlp_ops() -> Vec<WasmOp> {
    let mut ops = Vec::new();

    // Layer 1: 4 neurons, each loads 4 inputs + 4 weights from memory
    for neuron in 0..4u32 {
        let w_base = OFF_W1 + neuron * 16; // 4 weights × 4 bytes
        emit_dot4_mem(&mut ops, OFF_INPUTS, w_base);
        // Add bias from memory
        ops.push(WasmOp::I32Const(0));
        ops.push(WasmOp::F32Load(OFF_B1 + neuron * 4));
        ops.push(WasmOp::F32Add);
        emit_relu_store(&mut ops, neuron);
    }

    // Layer 2: reads hidden from locals 0..3, weights from memory
    for neuron in 0..4u32 {
        let w_base = OFF_W2 + neuron * 16;
        // dot(locals[0..3], mem[w_base..w_base+16]) + bias
        ops.push(WasmOp::LocalGet(0));
        ops.push(WasmOp::I32Const(0));
        ops.push(WasmOp::F32Load(w_base));
        ops.push(WasmOp::F32Mul);
        for j in 1..4u32 {
            ops.push(WasmOp::LocalGet(j));
            ops.push(WasmOp::I32Const(0));
            ops.push(WasmOp::F32Load(w_base + j * 4));
            ops.push(WasmOp::F32Mul);
            ops.push(WasmOp::F32Add);
        }
        ops.push(WasmOp::I32Const(0));
        ops.push(WasmOp::F32Load(OFF_B2 + neuron * 4));
        ops.push(WasmOp::F32Add);
        emit_relu_store(&mut ops, 4 + neuron);
    }

    // Output layer
    ops.push(WasmOp::LocalGet(4));
    ops.push(WasmOp::I32Const(0));
    ops.push(WasmOp::F32Load(OFF_W3));
    ops.push(WasmOp::F32Mul);
    for j in 1..4u32 {
        ops.push(WasmOp::LocalGet(4 + j));
        ops.push(WasmOp::I32Const(0));
        ops.push(WasmOp::F32Load(OFF_W3 + j * 4));
        ops.push(WasmOp::F32Mul);
        ops.push(WasmOp::F32Add);
    }
    ops.push(WasmOp::I32Const(0));
    ops.push(WasmOp::F32Load(OFF_B3));
    ops.push(WasmOp::F32Add);

    ops.push(WasmOp::F32Const(100.0));
    ops.push(WasmOp::F32Mul);
    ops.push(WasmOp::I32TruncF32S);
    ops.push(WasmOp::End);

    ops
}

/// Load 4 f32 values from mem[input_base..+16], multiply with
/// 4 f32 weights from mem[weight_base..+16], accumulate.
fn emit_dot4_mem(ops: &mut Vec<WasmOp>, input_base: u32, weight_base: u32) {
    // input[0] * weight[0]
    ops.push(WasmOp::I32Const(0));
    ops.push(WasmOp::F32Load(input_base));
    ops.push(WasmOp::I32Const(0));
    ops.push(WasmOp::F32Load(weight_base));
    ops.push(WasmOp::F32Mul);
    for j in 1..4u32 {
        ops.push(WasmOp::I32Const(0));
        ops.push(WasmOp::F32Load(input_base + j * 4));
        ops.push(WasmOp::I32Const(0));
        ops.push(WasmOp::F32Load(weight_base + j * 4));
        ops.push(WasmOp::F32Mul);
        ops.push(WasmOp::F32Add);
    }
}

fn emit_relu_store(ops: &mut Vec<WasmOp>, local: u32) {
    ops.push(WasmOp::LocalTee(local));
    ops.push(WasmOp::F32Const(0.0));
    ops.push(WasmOp::LocalGet(local));
    ops.push(WasmOp::F32Const(0.0));
    ops.push(WasmOp::F32Gt);
    ops.push(WasmOp::Select);
    ops.push(WasmOp::LocalSet(local));
}

fn main() {
    let weight_buf = build_weight_buffer();
    let ops = build_memory_mlp_ops();
    println!("[MEM-MLP] {} ops generated", ops.len());
    println!("[MEM-MLP] {} bytes weight data", weight_buf.len());

    let expected = compute_expected();
    let expected_exit = (expected * 100.0) as i32;
    println!("[MEM-MLP] Host reference: {:.4} (exit code {})", expected, expected_exit);

    // Resolve daemon address: PI_HOST env may be "host:port", "host", or
    // "user@host" (legacy). Strip any user@ prefix and append default port.
    let raw = std::env::var("PI_HOST").unwrap_or_else(|_| "192.168.68.72".to_string());
    let host_part = raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw);
    let addr = if host_part.contains(':') {
        host_part.to_string()
    } else {
        format!("{host_part}:{DEFAULT_PORT}")
    };

    println!("[MEM-MLP] connecting to a64-stream-daemon at {addr}");
    let mut sock = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => {
            println!("[MEM-MLP] connect failed: {e}");
            println!("[MEM-MLP] Start daemon: a64-stream-daemon 0.0.0.0:7700");
            std::process::exit(2);
        }
    };
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();

    // ── HELLO: receive the daemon's real mem_base ─────────────────
    let (ty, payload) = read_frame(&mut sock).expect("read HELLO");
    assert_eq!(ty, FRAME_HELLO, "first frame must be HELLO");
    let hello = parse_hello(&payload).expect("parse HELLO");
    println!(
        "[MEM-MLP] HELLO: mem_base=0x{:016x} mem_size={}",
        hello.mem_base, hello.mem_size
    );
    assert!(
        weight_buf.len() as u32 <= hello.mem_size,
        "weight buffer ({} B) larger than daemon mem_size ({} B)",
        weight_buf.len(),
        hello.mem_size
    );

    // ── Build JIT with the REAL mem_base baked into X28 ───────────
    let local_types = vec![ValType::F32; 8];
    let mut lw = Lowerer::new_function_with_memory_typed(
        &local_types,
        Vec::new(),
        hello.mem_base,
    )
    .expect("constructor");
    lw.set_mem_size(hello.mem_size);

    lw.lower_all(&ops).expect("lower_all");
    let code = lw.finish();
    println!("[MEM-MLP] {} bytes AArch64 code", code.len());
    println!(
        "[MEM-MLP] Total payload: {} bytes (code + weights)",
        code.len() + weight_buf.len()
    );
    println!("[MEM-MLP] vs const-scalar: 213 ops / 1548B code");
    println!("[MEM-MLP] vs const-SIMD:   169 ops / 1328B code");

    // ── DATA frame: populate linear memory at offset 0 ────────────
    let data_payload = serialize_data(0, &weight_buf);
    write_frame(&mut sock, FRAME_DATA, &data_payload).expect("write DATA");
    println!("[MEM-MLP] DATA frame sent ({} bytes at offset 0)", weight_buf.len());

    // ── CODE frame (HMAC-signed) ──────────────────────────────────
    let tag = auth::sign(&code);
    let mut code_payload = Vec::with_capacity(code.len() + auth::TAG_LEN);
    code_payload.extend_from_slice(&code);
    code_payload.extend_from_slice(&tag);
    write_frame(&mut sock, FRAME_CODE, &code_payload).expect("write CODE");

    // ── EXEC + RESULT ─────────────────────────────────────────────
    write_frame(&mut sock, FRAME_EXEC, &[]).expect("write EXEC");
    let (ty, payload) = read_frame(&mut sock).expect("read RESULT");
    assert_eq!(ty, FRAME_RESULT, "expected RESULT, got 0x{:02x}", ty);
    let exit = parse_result(&payload).expect("parse RESULT");

    println!("[MEM-MLP] Pi exit code: {} (expected {})", exit, expected_exit);
    if exit == expected_exit {
        println!("[MEM-MLP] MATCH — JIT inference matches host computation!");
    } else if (exit - expected_exit).abs() <= 1 {
        println!("[MEM-MLP] MATCH (within f32 rounding)");
    } else {
        println!("[MEM-MLP] MISMATCH");
        std::process::exit(1);
    }
}

fn compute_expected() -> f32 {
    let inputs: [f32; 4] = [1.0, 0.5, -0.3, 0.8];
    let w1: [[f32; 4]; 4] = [
        [0.5, -0.3, 0.8, 0.1], [-0.2, 0.7, 0.4, -0.6],
        [0.9, -0.1, -0.5, 0.3], [0.1, 0.4, 0.6, -0.2],
    ];
    let b1: [f32; 4] = [0.1, -0.1, 0.2, 0.0];
    let w2: [[f32; 4]; 4] = [
        [0.3, -0.2, 0.5, 0.1], [-0.1, 0.4, 0.2, -0.3],
        [0.2, -0.5, 0.1, 0.4], [0.4, 0.1, -0.2, 0.3],
    ];
    let b2: [f32; 4] = [0.1, 0.0, -0.1, 0.2];
    let w3: [f32; 4] = [0.5, -0.3, 0.4, 0.2];
    let b3: f32 = 0.1;
    let mut h1 = [0.0f32; 4];
    for i in 0..4 { let mut s = b1[i]; for j in 0..4 { s += inputs[j]*w1[i][j]; } h1[i] = s.max(0.0); }
    let mut h2 = [0.0f32; 4];
    for i in 0..4 { let mut s = b2[i]; for j in 0..4 { s += h1[j]*w2[i][j]; } h2[i] = s.max(0.0); }
    let mut out = b3;
    for j in 0..4 { out += h2[j]*w3[j]; }
    out
}
