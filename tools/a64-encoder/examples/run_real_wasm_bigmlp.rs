//! End-to-end run of the bigger 8 → 16 → 8 → 1 MLP via the
//! multi-function pipeline. Computes weights in Rust, computes the
//! expected output with the same f32 op-order, ships the weights
//! to the Pi, JITs the WASM module, runs it, compares.
//!
//! No Python oracle — same Rust code that builds the weights also
//! evaluates the network. If the JIT and the host disagree, it's
//! a JIT bug, period.

use std::net::TcpStream;
use std::time::Duration;

use a64_encoder::{compile_module, parse_module_full};
use a64_streamer::{
    auth, parse_hello, parse_result, read_frame, serialize_data, write_frame, DEFAULT_PORT,
    FRAME_CODE, FRAME_DATA, FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
};

const WASM_PATH: &str =
    "examples/wasm-bigmlp/target/wasm32-unknown-unknown/release/bigmlp_wasm.wasm";

// Memory layout — must match the constants in wasm-bigmlp/src/lib.rs.
const BASE: u32 = 0x1000;
const OFF_INPUTS: usize  = 0x000;
const OFF_W1: usize      = 0x020;
const OFF_B1: usize      = 0x220;
const OFF_W2: usize      = 0x260;
const OFF_B2: usize      = 0x460;
const OFF_W3: usize      = 0x480;
const OFF_B3: usize      = 0x4A0;

// Network shape.
const N_IN: usize = 8;
const N_HIDDEN1: usize = 16;
const N_HIDDEN2: usize = 8;
// Output is a single scalar.

const PAYLOAD_LEN: usize = OFF_B3 + 4;

/// Deterministic pseudo-random f32 generator, matched on host and
/// as raw bytes so we don't introduce ordering surprises.
fn pseudo_weights() -> Vec<u8> {
    // Fixed RNG state — produces the same weights every run.
    let mut state: u32 = 0x1234_5678;
    let mut next = || -> f32 {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        // Map to [-0.5, 0.5]
        (state as f32 / u32::MAX as f32) - 0.5
    };

    let mut buf = vec![0u8; PAYLOAD_LEN];

    // Inputs — small distinct values, easy to spot in a debugger.
    let inputs: [f32; N_IN] = [0.1, 0.2, -0.3, 0.4, -0.5, 0.6, -0.7, 0.8];
    for (i, &v) in inputs.iter().enumerate() {
        buf[OFF_INPUTS + i*4..OFF_INPUTS + i*4 + 4].copy_from_slice(&v.to_le_bytes());
    }

    // W1 [N_IN × N_HIDDEN1]
    for k in 0..N_IN { for j in 0..N_HIDDEN1 {
        let off = OFF_W1 + (k * N_HIDDEN1 + j) * 4;
        buf[off..off+4].copy_from_slice(&next().to_le_bytes());
    }}
    for j in 0..N_HIDDEN1 {
        let off = OFF_B1 + j * 4;
        buf[off..off+4].copy_from_slice(&next().to_le_bytes());
    }
    for k in 0..N_HIDDEN1 { for j in 0..N_HIDDEN2 {
        let off = OFF_W2 + (k * N_HIDDEN2 + j) * 4;
        buf[off..off+4].copy_from_slice(&next().to_le_bytes());
    }}
    for j in 0..N_HIDDEN2 {
        let off = OFF_B2 + j * 4;
        buf[off..off+4].copy_from_slice(&next().to_le_bytes());
    }
    for k in 0..N_HIDDEN2 {
        let off = OFF_W3 + k * 4;
        buf[off..off+4].copy_from_slice(&next().to_le_bytes());
    }
    let b3 = next();
    buf[OFF_B3..OFF_B3+4].copy_from_slice(&b3.to_le_bytes());

    buf
}

/// Compute the expected output with the SAME f32 op-order as
/// `wasm-bigmlp/src/lib.rs::entry` so we can compare bit-exact.
fn host_reference(buf: &[u8]) -> i32 {
    let lf = |off: usize| -> f32 {
        f32::from_le_bytes(buf[off..off+4].try_into().unwrap())
    };
    // Layer 1: 8 → 16 + ReLU
    let mut h1 = [0.0f32; N_HIDDEN1];
    for j in 0..N_HIDDEN1 {
        let mut s = lf(OFF_B1 + j * 4);
        for k in 0..N_IN {
            s += lf(OFF_INPUTS + k * 4) * lf(OFF_W1 + (k * N_HIDDEN1 + j) * 4);
        }
        h1[j] = if s > 0.0 { s } else { 0.0 };
    }
    // Layer 2: 16 → 8 + ReLU
    let mut h2 = [0.0f32; N_HIDDEN2];
    for j in 0..N_HIDDEN2 {
        let mut s = lf(OFF_B2 + j * 4);
        for k in 0..N_HIDDEN1 {
            s += h1[k] * lf(OFF_W2 + (k * N_HIDDEN2 + j) * 4);
        }
        h2[j] = if s > 0.0 { s } else { 0.0 };
    }
    // Layer 3: 8 → 1
    let mut out = lf(OFF_B3);
    for k in 0..N_HIDDEN2 {
        out += h2[k] * lf(OFF_W3 + k * 4);
    }
    (out * 1000.0) as i32
}

fn main() {
    println!("[BIG] Loading {WASM_PATH}");
    let wasm = std::fs::read(WASM_PATH).unwrap_or_else(|e| {
        eprintln!("Could not read {WASM_PATH}: {e}");
        std::process::exit(2);
    });
    println!("[BIG] {} bytes WASM module", wasm.len());

    let module = parse_module_full(&wasm).expect("parse_module_full");
    let entry = module.exports.iter()
        .find(|e| e.kind == 0 && e.name == "entry")
        .map(|e| e.index)
        .expect("no 'entry' export");
    println!(
        "[BIG] {} fns ({} types, {} globals); entry = fn[{entry}]",
        module.bodies.len(), module.types.len(), module.globals.len(),
    );
    for (i, b) in module.bodies.iter().enumerate() {
        println!(
            "  fn[{i}]: {} ops, {} locals (params via sig)",
            b.ops.len(), b.num_locals,
        );
    }

    let weights = pseudo_weights();
    let expected = host_reference(&weights);
    println!("[BIG] {} B weight payload, expected result: {}", weights.len(), expected);

    // ── Connect ────────────────────────────────────────────────────
    let raw = std::env::var("PI_HOST").unwrap_or_else(|_| "192.168.68.72".to_string());
    let host_part = raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw);
    let addr = if host_part.contains(':') { host_part.to_string() }
               else { format!("{host_part}:{DEFAULT_PORT}") };

    println!("[BIG] connecting to {addr}");
    let mut sock = TcpStream::connect(&addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_nodelay(true).ok();

    let (ty, payload) = read_frame(&mut sock).expect("HELLO");
    assert_eq!(ty, FRAME_HELLO);
    let hello = parse_hello(&payload).expect("parse HELLO");
    println!(
        "[BIG] HELLO: mem_base=0x{:016x} mem_size={}",
        hello.mem_base, hello.mem_size,
    );

    // ── Compile + ship ─────────────────────────────────────────────
    let layout = compile_module(&module, hello.mem_base, hello.mem_size, entry)
        .expect("compile_module");
    println!("[BIG] compiled to {} bytes AArch64", layout.code.len());
    for (i, off) in layout.function_offsets.iter().enumerate() {
        println!("  fn[{i}] @ 0x{off:x}");
    }

    // Weight DATA frame at BASE.
    let data_payload = serialize_data(BASE, &weights);
    write_frame(&mut sock, FRAME_DATA, &data_payload).expect("write DATA");

    let tag = auth::sign(&layout.code);
    let mut signed = Vec::with_capacity(layout.code.len() + auth::TAG_LEN);
    signed.extend_from_slice(&layout.code);
    signed.extend_from_slice(&tag);
    write_frame(&mut sock, FRAME_CODE, &signed).expect("write CODE");
    write_frame(&mut sock, FRAME_EXEC, &[]).expect("write EXEC");
    let (ty, payload) = read_frame(&mut sock).expect("RESULT");
    assert_eq!(ty, FRAME_RESULT, "expected RESULT got 0x{:02x}", ty);
    let exit = parse_result(&payload).expect("parse RESULT");

    println!("[BIG] Pi result: {exit}  (expected {expected})");
    if exit == expected {
        println!("[BIG] MATCH — multi-function MLP runs end-to-end on AArch64!");
    } else if (exit - expected).abs() <= 1 {
        println!("[BIG] MATCH (within rounding) — expected {expected}");
    } else {
        println!("[BIG] MISMATCH — expected {expected}, got {exit}");
        std::process::exit(1);
    }
}
