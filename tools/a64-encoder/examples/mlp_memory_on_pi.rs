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

use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, ValType, WasmOp};

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
    let local_types = vec![ValType::F32; 8];
    let weight_buf = build_weight_buffer();
    let mem_base: u64 = 0x0040_0000; // placeholder — daemon provides real address

    let mut lw = Lowerer::new_function_with_memory_typed(
        &local_types, Vec::new(), mem_base,
    ).expect("constructor");
    lw.set_mem_size(weight_buf.len() as u32);

    let ops = build_memory_mlp_ops();
    println!("[MEM-MLP] {} ops generated", ops.len());

    lw.lower_all(&ops).expect("lower_all");
    let code = lw.finish();
    println!("[MEM-MLP] {} bytes AArch64 code", code.len());
    println!("[MEM-MLP] {} bytes weight data", weight_buf.len());
    println!("[MEM-MLP] Total payload: {} bytes (code + weights)", code.len() + weight_buf.len());

    let expected = compute_expected();
    let expected_exit = (expected * 100.0) as i32;
    println!("[MEM-MLP] Host reference: {:.4} (exit code {})", expected, expected_exit);

    // Compare with const-based versions
    println!("[MEM-MLP] vs const-scalar: 213 ops / 1548B code");
    println!("[MEM-MLP] vs const-SIMD:   169 ops / 1328B code");
    println!("[MEM-MLP] This version loads ALL weights from memory at runtime");

    let pi = std::env::var("PI_HOST").unwrap_or_else(|_| "pi5".to_string());
    let result = Command::new("ssh")
        .args([&pi, "./run_bytes"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    match result {
        Ok(mut child) => {
            child.stdin.take().unwrap().write_all(&code).unwrap();
            let out = child.wait_with_output().unwrap();
            let exit = out.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if exit == 255 && !stderr.is_empty() {
                println!("[MEM-MLP] Pi not reachable — CODE+DATA frames ready for streamer");
            } else {
                println!("[MEM-MLP] Pi exit code: {} (expected {})", exit, expected_exit);
            }
        }
        Err(_) => println!("[MEM-MLP] SSH not available — JIT code ready"),
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
