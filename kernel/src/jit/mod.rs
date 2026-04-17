//! Kernel-side AArch64 JIT — compiles WASM ops to native AArch64 code
//! and streams the result to a Raspberry Pi 5 for execution.
//!
//! This is the crown jewel: Folkering OS itself cross-compiles WASM
//! to ARM, without any host-side tooling in the loop. The kernel
//! uses the a64-encoder crate (no_std) to JIT-compile, then sends
//! the machine code over TCP to the Pi daemon.
//!
//! # Architecture
//!
//! ```text
//!   Folkering OS (x86-64)          Pi 5 (AArch64)
//!   ┌─────────────────┐           ┌──────────────────┐
//!   │ WASM bytecode   │           │ a64-stream-daemon │
//!   │   ↓ parse       │   TCP     │   ↓               │
//!   │ WasmOps         │ ------→   │ HELLO frame       │
//!   │   ↓ JIT compile │ ←------   │   ↓               │
//!   │ AArch64 bytes   │ ------→   │ CODE frame        │
//!   │                 │ ------→   │ DATA frame (wts)  │
//!   │                 │ ------→   │ EXEC frame        │
//!   │   result ← ──  │ ←------   │ RESULT frame      │
//!   └─────────────────┘           └──────────────────┘
//! ```

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use a64_encoder::{
    Lowerer, ValType, WasmOp, FnSig,
    validate, ValidationError,
};

use crate::net::a64_stream::{A64Session, Hello};

/// Configuration for a JIT compilation + remote execution.
pub struct JitRequest {
    pub pi_ip: [u8; 4],
    pub pi_port: u16,
    pub local_types: Vec<ValType>,
    pub ops: Vec<WasmOp>,
    pub weight_data: Option<Vec<u8>>,
    pub call_sigs: Vec<FnSig>,
}

/// Result of a JIT execution on the Pi.
pub struct JitResult {
    pub exit_code: i32,
    pub code_bytes: usize,
    pub compile_us: u64,
}

/// Validate, compile, stream, execute — the full pipeline.
pub fn jit_exec(req: &JitRequest) -> Result<JitResult, &'static str> {
    let t0 = crate::net::tls::tsc_ms();

    // Step 1: Validate
    crate::serial_str!("[JIT] validating ");
    crate::drivers::serial::write_dec(req.ops.len() as u32);
    crate::serial_str!(" ops...\n");

    validate(&req.local_types, &req.ops, &req.call_sigs, &[])
        .map_err(|_| "WASM validation failed")?;

    // Step 2: Connect to Pi
    let session = A64Session::connect(req.pi_ip, req.pi_port)?;

    // Step 3: JIT compile
    let code = if req.weight_data.is_some() {
        let mut lw = Lowerer::new_function_with_memory_typed(
            &req.local_types,
            Vec::new(),
            session.hello.mem_base,
        ).map_err(|_| "lowerer construction failed")?;
        lw.set_mem_size(session.hello.mem_size);
        lw.lower_all(&req.ops).map_err(|_| "lowering failed")?;
        lw.finish()
    } else {
        let mut lw = Lowerer::new_function_typed(
            &req.local_types,
            Vec::new(),
        ).map_err(|_| "lowerer construction failed")?;
        lw.lower_all(&req.ops).map_err(|_| "lowering failed")?;
        lw.finish()
    };

    let t1 = crate::net::tls::tsc_ms();
    let compile_us = ((t1 - t0) * 1000) as u64;

    crate::serial_str!("[JIT] compiled ");
    crate::drivers::serial::write_dec(code.len() as u32);
    crate::serial_str!(" bytes in ~");
    crate::drivers::serial::write_dec(compile_us as u32);
    crate::serial_str!(" us\n");

    // Step 4: Send CODE
    session.send_code(&code)?;

    // Step 5: Send weight data if present
    if let Some(ref weights) = req.weight_data {
        crate::serial_str!("[JIT] sending ");
        crate::drivers::serial::write_dec(weights.len() as u32);
        crate::serial_str!(" bytes of weight data\n");
        session.send_data(0, weights)?;
    }

    // Step 6: Execute and get result
    crate::serial_str!("[JIT] executing on Pi...\n");
    let exit_code = session.exec()?;

    crate::serial_str!("[JIT] Pi returned: ");
    crate::drivers::serial::write_dec(exit_code as u32);
    crate::serial_str!("\n");

    // Step 7: Close session
    session.close()?;

    Ok(JitResult {
        exit_code,
        code_bytes: code.len(),
        compile_us,
    })
}

/// Quick helper: compile and run a simple MLP on the Pi.
/// Returns the scaled output (exit_code / 100.0 as f32 bits in i32).
pub fn run_mlp_on_pi(pi_ip: [u8; 4], pi_port: u16) -> Result<JitResult, &'static str> {
    crate::serial_str!("[JIT] building 4→4→4→1 MLP...\n");

    let mut ops = Vec::new();
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

    // Layer 1: const inputs → locals 0..3 with ReLU
    for neuron in 0..4 {
        emit_dot4_const(&mut ops, &inputs, &w1[neuron], b1[neuron]);
        emit_relu_store(&mut ops, neuron as u32);
    }
    // Layer 2: locals 0..3 → locals 4..7 with ReLU
    for neuron in 0..4 {
        emit_dot4_locals(&mut ops, 0, &w2[neuron], b2[neuron]);
        emit_relu_store(&mut ops, (4 + neuron) as u32);
    }
    // Output
    emit_dot4_locals(&mut ops, 4, &w3, b3);
    ops.push(WasmOp::F32Const(100.0));
    ops.push(WasmOp::F32Mul);
    ops.push(WasmOp::I32TruncF32S);
    ops.push(WasmOp::End);

    let req = JitRequest {
        pi_ip,
        pi_port,
        local_types: vec![ValType::F32; 8],
        ops,
        weight_data: None,
        call_sigs: Vec::new(),
    };

    jit_exec(&req)
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
