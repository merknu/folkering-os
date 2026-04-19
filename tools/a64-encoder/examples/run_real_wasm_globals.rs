//! End-to-end: real Rust crate using `__stack_pointer` global +
//! stack-allocated array → JIT'd → executed on Pi.
//!
//! Validates the Phase 1 + Phase 2 work:
//!   * Module parser extracts globals (3 of them, all i32)
//!   * Init values get clamped to fit in our 64 KiB linear memory
//!   * Lowerer initialises globals at function entry
//!   * `global.get`/`global.set` lower to LDR/STR through X28
//!   * Stack-allocated array (`let mut arr = [0i32; 8]`) actually
//!     reads/writes the right memory cells
//!
//! Expected: `test_globals()` returns 1 + 2 + ... + 8 = 36.

use std::net::TcpStream;
use std::time::Duration;

use a64_encoder::{parse_module_full, Lowerer, ValType};
use a64_streamer::{
    auth, parse_hello, parse_result, read_frame, write_frame, DEFAULT_PORT,
    FRAME_CODE, FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
};

const WASM_PATH: &str =
    "examples/wasm-globals/target/wasm32-unknown-unknown/release/globals_wasm.wasm";

fn vt(b: u8) -> ValType {
    match b {
        0x7F => ValType::I32, 0x7E => ValType::I64,
        0x7D => ValType::F32, 0x7C => ValType::F64,
        _ => panic!("unsupported valtype 0x{b:02x}"),
    }
}

fn main() {
    println!("[GLOBALS] Loading {WASM_PATH}");
    let wasm = std::fs::read(WASM_PATH).unwrap_or_else(|e| {
        eprintln!("Could not read {WASM_PATH}: {e}");
        std::process::exit(2);
    });
    println!("[GLOBALS] {} bytes WASM module", wasm.len());

    let module = parse_module_full(&wasm).expect("parse_module_full");
    let body = module.bodies.into_iter().next().expect("at least one fn");

    println!(
        "[GLOBALS] parsed: {} ops, {} locals (types {:?})",
        body.ops.len(), body.num_locals, body.local_types,
    );
    println!("[GLOBALS] declared {} globals:", module.globals.len());
    for (i, g) in module.globals.iter().enumerate() {
        let v = i32::from_le_bytes(g.init_bytes[..4].try_into().unwrap());
        println!(
            "  global[{i}]: type=0x{:02x} mut={} init=0x{:x} ({})",
            g.valtype, g.mutable, v as u32, v,
        );
    }

    let global_types: Vec<ValType> = module.globals.iter().map(|g| vt(g.valtype)).collect();
    let global_mut: Vec<bool> = module.globals.iter().map(|g| g.mutable).collect();
    let global_inits: Vec<[u8; 8]> = module.globals.iter().map(|g| g.init_bytes).collect();
    let local_types: Vec<ValType> = body.local_types.iter().map(|&b| vt(b)).collect();

    // ── Connect to daemon ─────────────────────────────────────────
    let raw = std::env::var("PI_HOST").unwrap_or_else(|_| "192.168.68.72".to_string());
    let host_part = raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw);
    let addr = if host_part.contains(':') { host_part.to_string() }
               else { format!("{host_part}:{DEFAULT_PORT}") };

    println!("[GLOBALS] connecting to {addr}");
    let mut sock = TcpStream::connect(&addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_nodelay(true).ok();

    let (ty, payload) = read_frame(&mut sock).expect("HELLO");
    assert_eq!(ty, FRAME_HELLO);
    let hello = parse_hello(&payload).expect("parse HELLO");
    println!(
        "[GLOBALS] HELLO: mem_base=0x{:016x} mem_size={}",
        hello.mem_base, hello.mem_size,
    );

    // ── JIT: typed-locals constructor + globals registered ─────────
    let mut lw = Lowerer::new_function_with_memory_typed(
        &local_types, Vec::new(), hello.mem_base,
    ).expect("Lowerer");
    lw.set_mem_size(hello.mem_size);
    lw.set_globals(global_types.clone(), global_mut.clone()).expect("set_globals");
    // Emit code that initialises all globals to their declared
    // values (Rust expects __stack_pointer to be ready before the
    // function body runs).
    lw.emit_global_inits(&global_inits).expect("emit_global_inits");
    lw.lower_all(&body.ops).expect("lower_all");
    let code = lw.finish();
    println!("[GLOBALS] JIT-compiled to {} bytes AArch64", code.len());

    // ── HMAC-signed CODE + EXEC ───────────────────────────────────
    let tag = auth::sign(&code);
    let mut signed = Vec::with_capacity(code.len() + auth::TAG_LEN);
    signed.extend_from_slice(&code);
    signed.extend_from_slice(&tag);
    write_frame(&mut sock, FRAME_CODE, &signed).expect("write CODE");
    write_frame(&mut sock, FRAME_EXEC, &[]).expect("write EXEC");
    let (ty, payload) = read_frame(&mut sock).expect("RESULT");
    assert_eq!(ty, FRAME_RESULT, "expected RESULT got 0x{:02x}", ty);
    let exit = parse_result(&payload).expect("parse RESULT");

    let expected: i32 = 36; // sum of 1..=8
    println!("[GLOBALS] Pi result: {exit}  (expected {expected})");

    if exit == expected {
        println!("[GLOBALS] MATCH — globals + __stack_pointer infrastructure works!");
    } else {
        println!("[GLOBALS] MISMATCH — expected {expected}, got {exit}");
        std::process::exit(1);
    }
}
