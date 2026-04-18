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
    parse_module, Lowerer, ValType, WasmOp, FnSig,
    validate,
};

use crate::net::a64_stream::A64Session;

/// Conventional offset where host writes data via DATA frame.
/// Matches the BASE constant in our example Rust → WASM crates;
/// kept ≥ 0x1000 to defeat LLVM's null-page UB folding (which
/// otherwise turns reads from low addresses into undef → 0.0).
pub const DEFAULT_DATA_BASE: u32 = 0x1000;

/// Length of the HMAC-SHA256 tag the daemon expects appended to
/// every CODE frame. Mirrors `a64_streamer::auth::TAG_LEN`.
pub const HMAC_TAG_LEN: usize = 32;

/// Shared HMAC-SHA256 key — the same 32 random bytes that
/// `tools/a64-streamer/secret.key` baked into the daemon. The
/// kernel's build.rs copies it into OUT_DIR; if either side
/// rotates the key, both must rebuild. Without this signature
/// the daemon refuses to mmap+execute the CODE.
const SHARED_HMAC_KEY: &[u8; 32] =
    include_bytes!(concat!(env!("KERNEL_SECRET_KEY_PATH")));

pub mod bench;

/// Compute HMAC-SHA256 tag over `data` using the shared key.
/// The daemon refuses any CODE frame whose payload doesn't end
/// in this tag.
pub(super) fn hmac_sign(data: &[u8]) -> [u8; HMAC_TAG_LEN] {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type H = Hmac<Sha256>;
    let mut mac = <H as Mac>::new_from_slice(SHARED_HMAC_KEY)
        .expect("HMAC takes a 32-byte key");
    mac.update(data);
    let out = mac.finalize().into_bytes();
    let mut tag = [0u8; HMAC_TAG_LEN];
    tag.copy_from_slice(&out);
    tag
}

/// Configuration for a JIT compilation + remote execution.
pub struct JitRequest {
    pub pi_ip: [u8; 4],
    pub pi_port: u16,
    pub local_types: Vec<ValType>,
    pub ops: Vec<WasmOp>,
    pub weight_data: Option<Vec<u8>>,
    /// Offset in the daemon's linear memory where `weight_data` is
    /// written (when present). Default convention is `DEFAULT_DATA_BASE`.
    /// Keep ≥ 0x1000 — see the constant's docs.
    pub data_offset: u32,
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

    // Step 4: Sign + send CODE. Daemon enforces HMAC-SHA256 over
    // the code bytes before mmaping the page, so we append the
    // 32-byte tag to the CODE frame payload.
    let tag = hmac_sign(&code);
    let mut signed = Vec::with_capacity(code.len() + HMAC_TAG_LEN);
    signed.extend_from_slice(&code);
    signed.extend_from_slice(&tag);
    session.send_code(&signed)?;

    // Step 5: Send weight data if present
    if let Some(ref weights) = req.weight_data {
        crate::serial_str!("[JIT] sending ");
        crate::drivers::serial::write_dec(weights.len() as u32);
        crate::serial_str!(" bytes of data at offset 0x");
        crate::drivers::serial::write_hex(req.data_offset as u64);
        crate::serial_str!("\n");
        session.send_data(req.data_offset, weights)?;
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

/// Map a WASM valtype byte to our `ValType` enum.
/// Returns Err for unsupported types — caller decides how to react.
pub(super) fn valtype_from_byte_pub(b: u8) -> Result<ValType, &'static str> {
    match b {
        0x7F => Ok(ValType::I32),
        0x7E => Ok(ValType::I64),
        0x7D => Ok(ValType::F32),
        0x7C => Ok(ValType::F64),
        _ => Err("unsupported WASM valtype"),
    }
}
fn valtype_from_byte(b: u8) -> Result<ValType, &'static str> {
    valtype_from_byte_pub(b)
}

/// Compile and run an arbitrary WASM module on the Pi.
///
/// Pipeline: parse_module → take fn[0] → JIT-compile → stream
/// CODE+DATA+EXEC over TCP → return RESULT exit code.
///
/// Multi-function modules use only the first body. The data buffer
/// is written to the daemon's linear memory at `data_offset` (use
/// `DEFAULT_DATA_BASE` unless your WASM module uses a different
/// memory layout).
pub fn jit_run_wasm(
    wasm_bytes: &[u8],
    data_bytes: Option<&[u8]>,
    data_offset: u32,
    pi_ip: [u8; 4],
    pi_port: u16,
) -> Result<JitResult, &'static str> {
    crate::serial_str!("[JIT] parsing ");
    crate::drivers::serial::write_dec(wasm_bytes.len() as u32);
    crate::serial_str!(" bytes of WASM...\n");

    let bodies = parse_module(wasm_bytes).map_err(|_| "WASM parse failed")?;
    let body = bodies.into_iter().next().ok_or("no function in module")?;

    crate::serial_str!("[JIT] fn[0]: ");
    crate::drivers::serial::write_dec(body.ops.len() as u32);
    crate::serial_str!(" ops, ");
    crate::drivers::serial::write_dec(body.num_locals);
    crate::serial_str!(" locals\n");

    let mut local_types = Vec::with_capacity(body.local_types.len());
    for &b in &body.local_types {
        local_types.push(valtype_from_byte(b)?);
    }

    let req = JitRequest {
        pi_ip,
        pi_port,
        local_types,
        ops: body.ops,
        weight_data: data_bytes.map(|d| d.to_vec()),
        data_offset,
        call_sigs: Vec::new(),
    };

    jit_exec(&req)
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
        data_offset: DEFAULT_DATA_BASE,
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
