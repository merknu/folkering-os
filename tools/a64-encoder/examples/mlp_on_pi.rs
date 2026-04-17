//! ML Inference Demo — A 3-layer MLP running as JIT-compiled AArch64
//! on a Raspberry Pi 5 via the a64-streamer pipeline.
//!
//! Network: 4 inputs → 8 hidden (ReLU) → 4 hidden (ReLU) → 1 output
//! This is a toy XOR-style network with handcrafted weights that
//! demonstrates the full pipeline: JIT compile → stream → execute.
//!
//! The network computes everything in f32 scalar ops. A SIMD-
//! accelerated version would use f32x4 for the dot products.

use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, ValType, WasmOp};

fn build_mlp_ops() -> Vec<WasmOp> {
    let mut ops = Vec::new();

    // Network: 4 inputs → 4 hidden (ReLU) → 4 hidden (ReLU) → 1 output
    // Fits in 8 f32 locals (4 for layer-1 hidden, 4 for layer-2 hidden)
    let inputs: [f32; 4] = [1.0, 0.5, -0.3, 0.8];

    let w1: [[f32; 4]; 4] = [
        [ 0.5, -0.3,  0.8,  0.1],
        [-0.2,  0.7,  0.4, -0.6],
        [ 0.9, -0.1, -0.5,  0.3],
        [ 0.1,  0.4,  0.6, -0.2],
    ];
    let b1: [f32; 4] = [0.1, -0.1, 0.2, 0.0];

    let w2: [[f32; 4]; 4] = [
        [ 0.3, -0.2,  0.5,  0.1],
        [-0.1,  0.4,  0.2, -0.3],
        [ 0.2, -0.5,  0.1,  0.4],
        [ 0.4,  0.1, -0.2,  0.3],
    ];
    let b2: [f32; 4] = [0.1, 0.0, -0.1, 0.2];

    let w3: [f32; 4] = [0.5, -0.3, 0.4, 0.2];
    let b3: f32 = 0.1;

    // Layer 1: const inputs → locals 0..3 with ReLU via select
    for neuron in 0..4 {
        emit_dot4_const(&mut ops, &inputs, &w1[neuron], b1[neuron]);
        emit_relu_store(&mut ops, neuron as u32);
    }

    // Layer 2: locals 0..3 → locals 4..7 with ReLU via select
    for neuron in 0..4 {
        emit_dot4_locals(&mut ops, 0, &w2[neuron], b2[neuron]);
        emit_relu_store(&mut ops, (4 + neuron) as u32);
    }

    // Output: dot(locals 4..7, w3) + b3 → scale to i32
    emit_dot4_locals(&mut ops, 4, &w3, b3);
    ops.push(WasmOp::F32Const(100.0));
    ops.push(WasmOp::F32Mul);
    ops.push(WasmOp::I32TruncF32S);
    ops.push(WasmOp::End);

    ops
}

fn emit_dot4_const(ops: &mut Vec<WasmOp>, inputs: &[f32; 4], weights: &[f32; 4], bias: f32) {
    ops.push(WasmOp::F32Const(inputs[0]));
    ops.push(WasmOp::F32Const(weights[0]));
    ops.push(WasmOp::F32Mul);
    for j in 1..4 {
        ops.push(WasmOp::F32Const(inputs[j]));
        ops.push(WasmOp::F32Const(weights[j]));
        ops.push(WasmOp::F32Mul);
        ops.push(WasmOp::F32Add);
    }
    ops.push(WasmOp::F32Const(bias));
    ops.push(WasmOp::F32Add);
}

fn emit_dot4_locals(ops: &mut Vec<WasmOp>, base_local: u32, weights: &[f32; 4], bias: f32) {
    ops.push(WasmOp::LocalGet(base_local));
    ops.push(WasmOp::F32Const(weights[0]));
    ops.push(WasmOp::F32Mul);
    for j in 1..4 {
        ops.push(WasmOp::LocalGet(base_local + j as u32));
        ops.push(WasmOp::F32Const(weights[j]));
        ops.push(WasmOp::F32Mul);
        ops.push(WasmOp::F32Add);
    }
    ops.push(WasmOp::F32Const(bias));
    ops.push(WasmOp::F32Add);
}

/// ReLU via select + local.tee: max(x, 0.0), stored to local N.
/// Stack before: [x:f32]  Stack after: [] (value stored in local)
///
/// Pattern:
///   local.tee N     — copy x to local, keep on stack
///   f32.const 0.0   — [x, 0.0]
///   local.get N     — [x, 0.0, x]
///   f32.const 0.0   — [x, 0.0, x, 0.0]
///   f32.gt          — [x, 0.0, cond]
///   select          — [relu(x)]
///   local.set N     — store final result
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
    // 8 f32 locals: 4 for layer-1 hidden, 4 for layer-2 hidden
    let local_types = vec![ValType::F32; 8];
    let mut lw = Lowerer::new_function_typed(&local_types, Vec::new())
        .expect("new_function_typed");

    let ops = build_mlp_ops();
    println!("[MLP] {} ops generated for 4→8→4→1 network", ops.len());

    lw.lower_all(&ops).expect("lower_all");
    let code = lw.finish();
    println!("[MLP] {} bytes of AArch64 machine code", code.len());

    // Try to run on Pi via SSH harness
    let pi = std::env::var("PI_HOST").unwrap_or_else(|_| "pi5".to_string());
    let result = Command::new("ssh")
        .args([&pi, "./run_bytes"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let expected = compute_expected();
    let expected_exit = (expected * 100.0) as i32;
    println!("[MLP] Host reference output: {:.4}", expected);
    println!("[MLP] Expected exit code: {}", expected_exit);

    match result {
        Ok(mut child) => {
            child.stdin.take().unwrap().write_all(&code).unwrap();
            let out = child.wait_with_output().unwrap();
            let exit = out.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&out.stderr);

            if exit == 255 && !stderr.is_empty() {
                println!("[MLP] Pi not reachable (SSH error): {}", stderr.trim());
                println!("[MLP] JIT code ready — deploy to Pi and retry");
            } else {
                println!("[MLP] Pi exit code: {} (network output x 100)", exit);
                println!("[MLP] Network output = {:.2}", exit as f32 / 100.0);
                if exit == expected_exit {
                    println!("[MLP] MATCH — JIT inference matches host computation!");
                } else if (exit - expected_exit).abs() <= 1 {
                    println!("[MLP] MATCH (within f32 rounding)");
                } else {
                    println!("[MLP] MISMATCH — expected {}, got {}", expected_exit, exit);
                }
            }
        }
        Err(e) => {
            println!("[MLP] SSH not available: {}", e);
            println!("[MLP] JIT code ready — {} bytes, {} ops", code.len(), ops.len());
        }
    }
}

fn compute_expected() -> f32 {
    let inputs: [f32; 4] = [1.0, 0.5, -0.3, 0.8];
    let w1: [[f32; 4]; 4] = [
        [ 0.5, -0.3,  0.8,  0.1],
        [-0.2,  0.7,  0.4, -0.6],
        [ 0.9, -0.1, -0.5,  0.3],
        [ 0.1,  0.4,  0.6, -0.2],
    ];
    let b1: [f32; 4] = [0.1, -0.1, 0.2, 0.0];
    let w2: [[f32; 4]; 4] = [
        [ 0.3, -0.2,  0.5,  0.1],
        [-0.1,  0.4,  0.2, -0.3],
        [ 0.2, -0.5,  0.1,  0.4],
        [ 0.4,  0.1, -0.2,  0.3],
    ];
    let b2: [f32; 4] = [0.1, 0.0, -0.1, 0.2];
    let w3: [f32; 4] = [0.5, -0.3, 0.4, 0.2];
    let b3: f32 = 0.1;

    let mut h1 = [0.0f32; 4];
    for i in 0..4 {
        let mut sum = b1[i];
        for j in 0..4 { sum += inputs[j] * w1[i][j]; }
        h1[i] = sum.max(0.0);
    }
    let mut h2 = [0.0f32; 4];
    for i in 0..4 {
        let mut sum = b2[i];
        for j in 0..4 { sum += h1[j] * w2[i][j]; }
        h2[i] = sum.max(0.0);
    }
    let mut out = b3;
    for j in 0..4 { out += h2[j] * w3[j]; }
    out
}
