//! End-to-end test of multi-function WASM → JIT → Pi.
//!
//! Uses the new `compile_module` (two-pass linker) to compile a
//! real Rust crate with two functions calling each other:
//!
//!   entry()         calls helper_mul3(14)
//!   helper_mul3(x)  = x + x + x = 42
//!
//! Rust's wasm linker usually places the exported function after
//! the helpers, so `entry` typically has fn_idx = 1. We look up
//! the right index from the Export section and feed it to
//! `compile_module` as the entrypoint.

use std::net::TcpStream;
use std::time::Duration;

use a64_encoder::{compile_module, parse_module_full};
use a64_streamer::{
    auth, parse_hello, parse_result, read_frame, write_frame, DEFAULT_PORT,
    FRAME_CODE, FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
};

const WASM_PATH: &str =
    "examples/wasm-multifn/target/wasm32-unknown-unknown/release/multifn_wasm.wasm";

fn main() {
    println!("[MULTI] Loading {WASM_PATH}");
    let wasm = std::fs::read(WASM_PATH).unwrap_or_else(|e| {
        eprintln!("Could not read {WASM_PATH}: {e}");
        std::process::exit(2);
    });
    println!("[MULTI] {} bytes WASM module", wasm.len());

    let module = parse_module_full(&wasm).expect("parse_module_full");

    // Resolve `entry` from the Export section → fn_idx.
    let entry = module.exports.iter()
        .find(|e| e.kind == 0 && e.name == "entry")
        .map(|e| e.index)
        .unwrap_or_else(|| {
            eprintln!("no function export named 'entry'");
            std::process::exit(3);
        });
    println!("[MULTI] {} fns; entry = fn[{entry}]", module.bodies.len());
    for (i, b) in module.bodies.iter().enumerate() {
        println!(
            "  fn[{i}]: {} ops, {} locals",
            b.ops.len(), b.num_locals,
        );
    }

    // ── Connect to daemon ─────────────────────────────────────────
    let raw = std::env::var("PI_HOST").unwrap_or_else(|_| "192.168.68.72".to_string());
    let host_part = raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw);
    let addr = if host_part.contains(':') { host_part.to_string() }
               else { format!("{host_part}:{DEFAULT_PORT}") };

    println!("[MULTI] connecting to {addr}");
    let mut sock = TcpStream::connect(&addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_nodelay(true).ok();
    let (ty, payload) = read_frame(&mut sock).expect("HELLO");
    assert_eq!(ty, FRAME_HELLO);
    let hello = parse_hello(&payload).expect("parse HELLO");
    println!(
        "[MULTI] HELLO: mem_base=0x{:016x} mem_size={}",
        hello.mem_base, hello.mem_size,
    );

    // ── Compile the whole module ──────────────────────────────────
    let layout = compile_module(&module, hello.mem_base, hello.mem_size, entry)
        .expect("compile_module");
    println!(
        "[MULTI] compiled to {} bytes AArch64",
        layout.code.len(),
    );
    for (i, off) in layout.function_offsets.iter().enumerate() {
        println!("  fn[{i}] @ 0x{off:x}");
    }
    println!("  entrypoint @ 0x{:x}", layout.entrypoint_offset);

    // ── Ship + exec ────────────────────────────────────────────────
    let tag = auth::sign(&layout.code);
    let mut signed = Vec::with_capacity(layout.code.len() + auth::TAG_LEN);
    signed.extend_from_slice(&layout.code);
    signed.extend_from_slice(&tag);
    write_frame(&mut sock, FRAME_CODE, &signed).expect("write CODE");
    write_frame(&mut sock, FRAME_EXEC, &[]).expect("write EXEC");
    let (ty, payload) = read_frame(&mut sock).expect("RESULT");
    assert_eq!(ty, FRAME_RESULT, "expected RESULT got 0x{:02x}", ty);
    let exit = parse_result(&payload).expect("parse RESULT");

    let expected: i32 = 42;
    println!("[MULTI] Pi result: {exit}  (expected {expected})");
    if exit == expected {
        println!("[MULTI] MATCH — multi-function WASM lands on AArch64!");
    } else {
        println!("[MULTI] MISMATCH — expected {expected}, got {exit}");
        std::process::exit(1);
    }
}
