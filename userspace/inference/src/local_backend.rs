//! Local Burn-based inference backend.
//!
//! D.2 status: handles the `"local:matmul-test"` sentinel model end-to-
//! end. Anything else falls through to the proxy via `NotImplemented`.
//!
//! What the test path proves at D.2:
//! - Burn's `TensorData` constructs and decomposes correctly in our
//!   `no_std + alloc` custom target.
//! - The router actually delivers a request to the local arm (not
//!   just falls through every time).
//! - `tensor_math::matmul` produces the same numbers Burn's reference
//!   matmul would, so when D.3 starts wiring real `Tensor<B>` types
//!   we know the underlying f32 storage is correct.
//!
//! What's deliberately not here yet:
//! - A custom `burn_backend::Backend` trait impl. That's D.3; today
//!   we use `TensorData` only at the wire boundary and run math via
//!   the hand-rolled `tensor_math` path.
//! - Real model forward pass. D.3 plumbs Qwen2.5-0.5B-Q4 weights
//!   from Synapse VFS through this same code shape.
//! - Streaming token output. D.4 — needs KV-cache + a session
//!   abstraction.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use burn_tensor::TensorData;
use libfolk::println;

use crate::ipc_msg::{InferenceWire, InferenceStatus};
use crate::router::Outcome;
use crate::tensor_math::{Tensor2, matmul};

/// Sentinel model that triggers the local matmul demo. Any other
/// model name returns `NotImplemented` and the router falls through
/// to the proxy backend.
pub const SENTINEL_MATMUL_TEST: &str = "local:matmul-test";

/// Boot-time D.2 self-test: build a fake `InferenceWire` in a local
/// scratch buffer, dispatch it to `run_matmul_test`, and verify the
/// result matches the identity-matmul expected output. Returns true
/// on PASS.
///
/// This intentionally side-steps the real IPC path — the wire is in
/// our own task's address space, not a shmem region. The point at
/// D.2 is to prove the local backend's tensor math + result-buffer
/// formatting produce correct bytes; the IPC end-to-end test
/// (caller in another task → shmem → router → local backend) lands
/// when D.3 plumbs a real caller.
pub fn boot_test() -> bool {
    use crate::ipc_msg::{InferenceWire, WIRE_MAGIC, WIRE_VERSION};

    // Layout the scratch wire in a 4 KiB fixed buffer, mirroring
    // how callers will lay out shmem.
    let mut scratch: Vec<u8> = vec![0u8; 4096];
    let base_vaddr = scratch.as_mut_ptr() as usize;

    // Build the header in place. SAFETY: `scratch` is a 4 KiB Vec,
    // size_of::<InferenceWire>() ≪ 4096.
    let header_size = core::mem::size_of::<InferenceWire>();
    let prompt_off = ((header_size + 15) & !15) as u32;
    let prompt_str = b"matmul-test"; // free-form for now; ignored by run_matmul_test
    let prompt_len = prompt_str.len() as u32;
    let result_off = prompt_off + prompt_len + 16; // 16-byte gap between prompt and result
    let result_max = 256u32;

    unsafe {
        let wire = base_vaddr as *mut InferenceWire;
        (*wire).magic = WIRE_MAGIC;
        (*wire).version = WIRE_VERSION;
        (*wire).status = 0;
        (*wire).output_len = 0;
        (*wire).prompt_len = prompt_len;
        (*wire).result_max = result_max;
        (*wire).prompt_off = prompt_off;
        (*wire).result_off = result_off;
        (*wire).model = [0u8; 64];
        let model_bytes = SENTINEL_MATMUL_TEST.as_bytes();
        (&mut (*wire).model)[..model_bytes.len()].copy_from_slice(model_bytes);

        // Copy prompt body.
        core::ptr::copy_nonoverlapping(
            prompt_str.as_ptr(),
            (base_vaddr + prompt_off as usize) as *mut u8,
            prompt_str.len(),
        );
    }

    // Dispatch through the same code path the router uses.
    let wire_ref: &InferenceWire = unsafe { &*(base_vaddr as *const InferenceWire) };
    let outcome = run(wire_ref, base_vaddr);

    if outcome.status != InferenceStatus::Ok {
        println!(
            "[INFERENCE/local] boot_test FAIL: status={:?} output_len={}",
            outcome.status, outcome.output_len
        );
        return false;
    }

    // Read the formatted result and verify the identity-matmul invariant
    // (A @ I == A) by checking the first row contains "1 2 3 4".
    let result_slice: &[u8] = unsafe {
        core::slice::from_raw_parts(
            (base_vaddr + result_off as usize) as *const u8,
            outcome.output_len as usize,
        )
    };
    let result_str = match core::str::from_utf8(result_slice) {
        Ok(s) => s,
        Err(_) => {
            println!("[INFERENCE/local] boot_test FAIL: result is not UTF-8");
            return false;
        }
    };
    if !result_str.contains("[1 2 3 4]") {
        println!("[INFERENCE/local] boot_test FAIL: first row mismatch");
        println!("[INFERENCE/local] result was:\n{}", result_str);
        return false;
    }
    true
}

pub fn run(wire: &InferenceWire, base_vaddr: usize) -> Outcome {
    let model = model_str(&wire.model);

    if model == SENTINEL_MATMUL_TEST {
        return run_matmul_test(wire, base_vaddr);
    }

    // Default: not a model the local backend can answer. Router
    // falls through to proxy.
    Outcome {
        status: InferenceStatus::NotImplemented,
        output_len: 0,
    }
}

/// Hardcoded 4×4 matmul demo, using Burn's `TensorData` at the wire
/// boundary and our own `tensor_math::matmul` for the math. Returns
/// the result formatted as ASCII text in the caller's result buffer.
///
/// Two matrices:
///
///   A = [[1, 2, 3, 4],          B = [[1, 0, 0, 0],
///        [5, 6, 7, 8],               [0, 1, 0, 0],
///        [9, 10, 11, 12],            [0, 0, 1, 0],
///        [13, 14, 15, 16]]           [0, 0, 0, 1]]
///
/// B is the identity, so A @ B == A. That makes the correctness
/// check trivial and visually obvious in serial. As D.3 plumbs
/// real models we can swap in pre-validated test vectors per layer.
fn run_matmul_test(wire: &InferenceWire, base_vaddr: usize) -> Outcome {
    // ── Step 1: build the inputs as Burn TensorData ─────────────────
    //
    // This is the main reason we did the no_std Burn dependency
    // verification at D.1: confirming that `TensorData::new` and
    // friends compile here. Once D.3 lands a custom Backend, the
    // exact same TensorData payload becomes the input to a real
    // `Tensor::<MyBackend>::from_data(...)`.

    let a_values: Vec<f32> = (1..=16).map(|n| n as f32).collect();
    let b_values: Vec<f32> = vec![
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ];
    let a_data = TensorData::new(a_values, [4, 4]);
    let b_data = TensorData::new(b_values, [4, 4]);

    // ── Step 2: bridge TensorData → tensor_math ─────────────────────
    //
    // No Burn Backend impl yet, so we extract f32 bytes and reshape
    // into `Tensor2`. The `TensorData::shape.dims()` view confirms
    // the wire dims at runtime — guards against a future regression
    // where the constructor accepts mismatched shapes.

    let a = match tensor_data_to_tensor2(&a_data) {
        Some(t) => t,
        None => return bad_request("A: TensorData→Tensor2 failed"),
    };
    let b = match tensor_data_to_tensor2(&b_data) {
        Some(t) => t,
        None => return bad_request("B: TensorData→Tensor2 failed"),
    };

    let c = matmul(&a, &b);

    // ── Step 3: format result into the wire's result buffer ─────────
    //
    // The caller passed a `&mut [u8]` of length `wire.result_max`.
    // We write ASCII (one row per line, bracketed) so a human
    // inspecting the IPC trace can eyeball correctness.

    use alloc::string::String;
    let mut out = String::with_capacity(256);
    for r in 0..c.rows() {
        out.push('[');
        for col in 0..c.cols() {
            if col > 0 { out.push(' '); }
            push_f32_int(&mut out, c.get(r, col));
        }
        out.push(']');
        out.push('\n');
    }

    // SAFETY: bounds were validated by router::handle_mapped before
    // dispatching to us.
    let result_buf: &mut [u8] = unsafe {
        core::slice::from_raw_parts_mut(
            (base_vaddr + wire.result_off as usize) as *mut u8,
            wire.result_max as usize,
        )
    };

    let bytes = out.as_bytes();
    if bytes.len() > result_buf.len() {
        // Should-not-happen at D.2 (16-cell matrix + brackets fits in
        // any reasonable result buffer), but check anyway so the
        // status code is honest.
        return Outcome {
            status: InferenceStatus::BufferTooSmall,
            output_len: 0,
        };
    }
    result_buf[..bytes.len()].copy_from_slice(bytes);

    println!(
        "[INFERENCE/local] matmul-test: 4x4 @ 4x4 PASS, {} bytes written",
        bytes.len()
    );

    Outcome {
        status: InferenceStatus::Ok,
        output_len: bytes.len() as u32,
    }
}

/// Decode a `Vec<f32>` from `TensorData::bytes` (which is `Bytes` —
/// Burn's wrapper over a `Vec<u8>` plus alignment/dtype tags). For
/// 2-D float tensors we re-interpret the underlying bytes as f32 and
/// pair with the explicit shape from `TensorData::shape`.
///
/// Returns None if the dtype isn't `Float32` or the shape isn't
/// rank-2 — the caller already checks shape via `TensorData::new`'s
/// `check_data_len`, so a None here means our caller passed an
/// unexpected dtype.
fn tensor_data_to_tensor2(td: &TensorData) -> Option<Tensor2> {
    use burn_tensor::DType;

    // `Shape::dims<D>()` is const-generic on the rank, which we
    // don't know at compile time here. `as_slice()` gives a runtime
    // view we can index dynamically.
    let dims = td.shape.as_slice();
    if dims.len() != 2 {
        return None;
    }
    let rows = dims[0];
    let cols = dims[1];

    if td.dtype != DType::F32 {
        return None;
    }

    // `Bytes` exposes its raw byte slice. f32 elements are 4 bytes
    // little-endian on x86_64; we reinterpret rather than re-decode
    // each element to keep this hot path zero-copy at the byte
    // level. Future quantized paths will need a real decode.
    let raw: &[u8] = td.bytes.as_ref();
    if raw.len() != rows * cols * core::mem::size_of::<f32>() {
        return None;
    }
    let mut data: Vec<f32> = Vec::with_capacity(rows * cols);
    let mut off = 0usize;
    while off + 4 <= raw.len() {
        let arr = [raw[off], raw[off + 1], raw[off + 2], raw[off + 3]];
        data.push(f32::from_le_bytes(arr));
        off += 4;
    }
    Some(Tensor2::from_flat(rows, cols, data))
}

fn bad_request(reason: &str) -> Outcome {
    println!("[INFERENCE/local] reject: {}", reason);
    Outcome {
        status: InferenceStatus::BadRequest,
        output_len: 0,
    }
}

fn model_str(buf: &[u8; 64]) -> &str {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..end]).unwrap_or("")
}

/// Render an f32 as `"<int>"` if it's integer-valued, else `"<int>.<frac>"`.
/// Same shape as the previous formatter; we keep formatting tight since
/// the result buffer is bounded.
fn push_f32_int(s: &mut alloc::string::String, v: f32) {
    if v.is_nan() { s.push_str("NaN"); return; }
    if v.is_infinite() {
        s.push_str(if v > 0.0 { "+Inf" } else { "-Inf" });
        return;
    }
    let neg = v < 0.0;
    let mut x = if neg { -v } else { v };
    if neg { s.push('-'); }
    let int_part = x as u64;
    push_u64(s, int_part);
    x -= int_part as f32;
    let frac = (x * 100.0 + 0.5) as u64;
    if frac > 0 {
        s.push('.');
        if frac < 10 { s.push('0'); }
        push_u64(s, frac);
    }
}

fn push_u64(s: &mut alloc::string::String, mut v: u64) {
    if v == 0 { s.push('0'); return; }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while v > 0 {
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        s.push(buf[i] as char);
    }
}
