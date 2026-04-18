//! SIMD MLP — same 4→4→4→1 network using f32x4 vector ops.
//!
//! Each 4-element dot product uses:
//!   v128.const(weights) → f32x4.mul(inputs_vec) → f32x4.horizontal_sum → add bias
//! Instead of 13 scalar ops per dot, this is ~5 vector ops.

use std::io::Write;
use std::process::{Command, Stdio};

use a64_encoder::{Lowerer, ValType, WasmOp};

fn build_simd_mlp_ops() -> Vec<WasmOp> {
    let mut ops = Vec::new();

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

    // Materialize input vector as v128 constant
    let input_bits = f32x4_to_u128(&inputs);

    // Layer 1: SIMD dot products + scalar ReLU → locals 0..3
    for neuron in 0..4 {
        let w_bits = f32x4_to_u128(&w1[neuron]);
        // v128.const(inputs) * v128.const(weights) → hsum → + bias
        ops.push(WasmOp::V128Const(input_bits));
        ops.push(WasmOp::V128Const(w_bits));
        ops.push(WasmOp::F32x4Mul);
        ops.push(WasmOp::F32x4HorizontalSum);
        ops.push(WasmOp::F32Const(b1[neuron]));
        ops.push(WasmOp::F32Add);
        emit_relu_store(&mut ops, neuron as u32);
    }

    // Layer 2: build input vector from locals, SIMD dot + ReLU → locals 4..7
    for neuron in 0..4 {
        let w_bits = f32x4_to_u128(&w2[neuron]);
        // Build v128 from 4 f32 locals: splat local 0, then replace lanes
        // Simplest: use scalar dot for layer 2 since inputs are in locals
        emit_dot4_locals(&mut ops, 0, &w2[neuron], b2[neuron]);
        emit_relu_store(&mut ops, (4 + neuron) as u32);
    }

    // Output layer: scalar dot from locals 4..7
    emit_dot4_locals(&mut ops, 4, &w3, b3);
    ops.push(WasmOp::F32Const(100.0));
    ops.push(WasmOp::F32Mul);
    ops.push(WasmOp::I32TruncF32S);
    ops.push(WasmOp::End);

    ops
}

fn f32x4_to_u128(v: &[f32; 4]) -> u128 {
    let b0 = v[0].to_bits() as u128;
    let b1 = (v[1].to_bits() as u128) << 32;
    let b2 = (v[2].to_bits() as u128) << 64;
    let b3 = (v[3].to_bits() as u128) << 96;
    b0 | b1 | b2 | b3
}

fn emit_dot4_locals(ops: &mut Vec<WasmOp>, base: u32, weights: &[f32; 4], bias: f32) {
    ops.push(WasmOp::LocalGet(base));
    ops.push(WasmOp::F32Const(weights[0]));
    ops.push(WasmOp::F32Mul);
    for j in 1..4 {
        ops.push(WasmOp::LocalGet(base + j as u32));
        ops.push(WasmOp::F32Const(weights[j]));
        ops.push(WasmOp::F32Mul);
        ops.push(WasmOp::F32Add);
    }
    ops.push(WasmOp::F32Const(bias));
    ops.push(WasmOp::F32Add);
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
    let mut lw = Lowerer::new_function_typed(&local_types, Vec::new())
        .expect("new_function_typed");

    let ops = build_simd_mlp_ops();
    println!("[SIMD-MLP] {} ops generated", ops.len());

    lw.lower_all(&ops).expect("lower_all");
    let code = lw.finish();
    println!("[SIMD-MLP] {} bytes of AArch64 machine code", code.len());

    let expected = compute_expected();
    let expected_exit = (expected * 100.0) as i32;
    println!("[SIMD-MLP] Host reference: {:.4} (exit code {})", expected, expected_exit);

    // Compare with scalar version
    let scalar_ops = 213; // from mlp_on_pi
    let scalar_bytes = 1548;
    println!("[SIMD-MLP] vs scalar: {} ops (was {}), {} bytes (was {})",
        ops.len(), scalar_ops, code.len(), scalar_bytes);
    println!("[SIMD-MLP] Layer 1 speedup: SIMD dot (5 ops/neuron) vs scalar (13 ops/neuron)");

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
                println!("[SIMD-MLP] Pi not reachable — JIT code ready to deploy");
            } else {
                println!("[SIMD-MLP] Pi exit code: {} (expected {})", exit, expected_exit);
                if (exit - expected_exit).abs() <= 1 {
                    println!("[SIMD-MLP] MATCH!");
                }
            }
        }
        Err(_) => println!("[SIMD-MLP] SSH not available — JIT code ready"),
    }
}

fn compute_expected() -> f32 {
    let inputs: [f32; 4] = [1.0, 0.5, -0.3, 0.8];
    let w1: [[f32; 4]; 4] = [
        [ 0.5, -0.3,  0.8,  0.1], [-0.2,  0.7,  0.4, -0.6],
        [ 0.9, -0.1, -0.5,  0.3], [ 0.1,  0.4,  0.6, -0.2],
    ];
    let b1: [f32; 4] = [0.1, -0.1, 0.2, 0.0];
    let w2: [[f32; 4]; 4] = [
        [ 0.3, -0.2,  0.5,  0.1], [-0.1,  0.4,  0.2, -0.3],
        [ 0.2, -0.5,  0.1,  0.4], [ 0.4,  0.1, -0.2,  0.3],
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
