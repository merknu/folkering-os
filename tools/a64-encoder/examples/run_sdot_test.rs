//! Hardware test of SDOT (ARMv8.4-A int8 dot product) on Pi 5.
//!
//! Hand-builds a small JIT that:
//!   1. Loads three v128 vectors from linear memory:
//!        acc[i32x4]  (initial accumulator, e.g. [10, 20, 30, 40])
//!        a  [i8x16]  (first source)
//!        b  [i8x16]  (second source)
//!   2. Calls SDOT — for each output lane i ∈ 0..4:
//!        result[i] = acc[i] + Σ_{j=0..4} a[i*4+j] * b[i*4+j]
//!   3. Extracts lane 0 of the result, returns as i32.
//!
//! With our test inputs:
//!   acc = [10, 20, 30, 40]
//!   a   = [1, 2, 3, 4,    5, 6, 7, 8,    9,10,11,12,   13,14,15,16]
//!   b   = [1, 1, 1, 1,    2, 2, 2, 2,    3, 3, 3, 3,    4, 4, 4, 4]
//!
//! Lane 0: dot([1,2,3,4], [1,1,1,1]) = 10  → result = 10 + 10 = 20
//! Lane 1: dot([5,6,7,8], [2,2,2,2]) = 52  → result = 20 + 52 = 72
//! Lane 2: dot([9..12],   [3,3,3,3]) = 126 → result = 30 + 126 = 156
//! Lane 3: dot([13..16],  [4,4,4,4]) = 232 → result = 40 + 232 = 272
//!
//! We extract lane 0 and expect 20.

use std::net::TcpStream;
use std::time::Duration;

use a64_encoder::{Lowerer, ValType, WasmOp};
use a64_streamer::{
    auth, parse_hello, parse_result, read_frame, serialize_data, write_frame, DEFAULT_PORT,
    FRAME_CODE, FRAME_DATA, FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
};

fn main() {
    let raw = std::env::var("PI_HOST").unwrap_or_else(|_| "192.168.68.72".to_string());
    let host_part = raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw);
    let addr = if host_part.contains(':') { host_part.to_string() }
               else { format!("{host_part}:{DEFAULT_PORT}") };

    println!("[SDOT] connecting to {addr}");
    let mut sock = TcpStream::connect(&addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_nodelay(true).ok();

    let (ty, payload) = read_frame(&mut sock).expect("HELLO");
    assert_eq!(ty, FRAME_HELLO);
    let hello = parse_hello(&payload).expect("parse HELLO");
    println!(
        "[SDOT] HELLO: mem_base=0x{:016x} mem_size={}",
        hello.mem_base, hello.mem_size,
    );

    // ── Build the test data ─────────────────────────────────────────
    // Layout in memory (relative to BASE = 0x1000):
    //   0x00 acc[i32x4]  — 16 B
    //   0x10 a  [i8x16]  — 16 B
    //   0x20 b  [i8x16]  — 16 B
    let mut data = vec![0u8; 0x30];
    // acc = [10i32, 20, 30, 40]
    for (i, v) in [10i32, 20, 30, 40].iter().enumerate() {
        data[i*4..i*4+4].copy_from_slice(&v.to_le_bytes());
    }
    // a = [1..16] as i8
    for i in 0..16 { data[0x10 + i] = (i + 1) as i8 as u8; }
    // b = chunks of [1,1,1,1], [2,2,2,2], [3,3,3,3], [4,4,4,4]
    for chunk in 0..4 {
        for k in 0..4 {
            data[0x20 + chunk*4 + k] = (chunk + 1) as i8 as u8;
        }
    }

    // ── Hand-build the JIT op stream ────────────────────────────────
    //   i32.const 0x1000;  v128.load 0    → acc
    //   i32.const 0x1010;  v128.load 0    → a (on stack: acc, a)
    //   i32.const 0x1020;  v128.load 0    → b (on stack: acc, a, b)
    //   i32x4.dot_i8x16_signed             → result (on stack: result)
    //   i32x4.extract_lane 0               → i32
    //   end
    let ops = vec![
        WasmOp::I32Const(0x1000),
        WasmOp::V128Load(0),
        WasmOp::I32Const(0x1010),
        WasmOp::V128Load(0),
        WasmOp::I32Const(0x1020),
        WasmOp::V128Load(0),
        WasmOp::I32x4DotI8x16Signed,
        WasmOp::I32x4ExtractLane(0),
        WasmOp::End,
    ];

    let mut lw = Lowerer::new_function_with_memory_typed(
        &[],          // no locals
        Vec::new(),   // no external call_targets
        hello.mem_base,
    ).expect("Lowerer");
    lw.set_mem_size(hello.mem_size);
    lw.lower_all(&ops).expect("lower_all");
    let code = lw.finish();
    println!("[SDOT] JIT-compiled to {} bytes AArch64", code.len());

    // ── Ship DATA + CODE + EXEC ────────────────────────────────────
    let data_payload = serialize_data(0x1000, &data);
    write_frame(&mut sock, FRAME_DATA, &data_payload).expect("DATA");

    let tag = auth::sign(&code);
    let mut signed = Vec::with_capacity(code.len() + auth::TAG_LEN);
    signed.extend_from_slice(&code);
    signed.extend_from_slice(&tag);
    write_frame(&mut sock, FRAME_CODE, &signed).expect("CODE");
    write_frame(&mut sock, FRAME_EXEC, &[]).expect("EXEC");
    let (ty, payload) = read_frame(&mut sock).expect("RESULT");
    assert_eq!(ty, FRAME_RESULT, "got 0x{:02x}", ty);
    let exit = parse_result(&payload).expect("parse RESULT");

    let expected: i32 = 20;
    println!("[SDOT] Pi result: {exit}  (expected {expected})");
    if exit == expected {
        println!("[SDOT] MATCH — SDOT (ARMv8.4-A int8 dot product) works!");
    } else {
        println!("[SDOT] MISMATCH");
        std::process::exit(1);
    }
}
