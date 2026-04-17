//! Host-side bench mirror of `kernel::jit::bench` — gives us
//! numbers immediately without needing to boot Folkering OS.
//!
//! Same protocol as `run_real_wasm_attention.rs`, but instead of
//! one shot, opens ONE TCP session, sends CODE once, then loops
//! N iterations of {DATA, EXEC, RESULT} measuring each phase
//! with `Instant::now()`. Reports min/mean/p50/p99/max in
//! microseconds, plus throughput.
//!
//! Usage:
//!   PI_HOST=192.168.68.72:7700 cargo run --release --example bench_real_wasm \
//!     -- examples/wasm-attention/target/wasm32-unknown-unknown/release/attention_wasm.wasm \
//!     200
//!
//! Or omit args for defaults (attention_wasm.wasm, 100 iters).

use std::net::TcpStream;
use std::time::{Duration, Instant};

use a64_encoder::{parse_module, Lowerer, ValType};
use a64_streamer::{
    auth, parse_hello, parse_result, read_frame, serialize_data, write_frame, DEFAULT_PORT,
    FRAME_CODE, FRAME_DATA, FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
};

const DEFAULT_WASM: &str =
    "examples/wasm-attention/target/wasm32-unknown-unknown/release/attention_wasm.wasm";

const BASE: u32 = 0x1000;
const S: usize = 4;
const D: usize = 4;

fn build_attention_data() -> Vec<u8> {
    let inputs: [[f32; D]; S] = [
        [ 1.0,  0.5, -0.3,  0.8],
        [ 0.2, -0.7,  0.4,  0.1],
        [-0.5,  0.6,  0.9, -0.2],
        [ 0.3,  0.1, -0.4,  0.7],
    ];
    let wq: [[f32; D]; D] = [
        [ 0.5, -0.3,  0.8,  0.1], [-0.2,  0.7,  0.4, -0.6],
        [ 0.9, -0.1, -0.5,  0.3], [ 0.1,  0.4,  0.6, -0.2],
    ];
    let wk: [[f32; D]; D] = [
        [ 0.3, -0.2,  0.5,  0.1], [-0.1,  0.4,  0.2, -0.3],
        [ 0.2, -0.5,  0.1,  0.4], [ 0.4,  0.1, -0.2,  0.3],
    ];
    let wv: [[f32; D]; D] = [
        [ 0.5,  0.2, -0.3,  0.4], [-0.1,  0.3,  0.5,  0.2],
        [ 0.4, -0.2,  0.1, -0.5], [ 0.2,  0.5, -0.4,  0.3],
    ];
    let mut buf = Vec::with_capacity(256);
    let push = |b: &mut Vec<u8>, m: &[[f32; D]]| {
        for r in m { for &v in r { b.extend_from_slice(&v.to_le_bytes()); } }
    };
    push(&mut buf, &inputs);
    push(&mut buf, &wq);
    push(&mut buf, &wk);
    push(&mut buf, &wv);
    buf
}

fn vt(b: u8) -> ValType {
    match b {
        0x7F => ValType::I32, 0x7E => ValType::I64,
        0x7D => ValType::F32, 0x7C => ValType::F64,
        _ => panic!("unsupported valtype 0x{b:02x}"),
    }
}

#[derive(Clone, Copy, Debug)]
struct Stats {
    min: u64,
    mean: u64,
    p50: u64,
    p99: u64,
    max: u64,
}

impl Stats {
    fn from_unsorted(samples: &mut [u64]) -> Self {
        if samples.is_empty() {
            return Stats { min: 0, mean: 0, p50: 0, p99: 0, max: 0 };
        }
        samples.sort_unstable();
        let n = samples.len();
        let sum: u64 = samples.iter().sum();
        let p99_idx = (((n as u64 * 99 + 99) / 100).saturating_sub(1) as usize).min(n - 1);
        Stats {
            min: samples[0],
            mean: sum / n as u64,
            p50: samples[n / 2],
            p99: samples[p99_idx],
            max: samples[n - 1],
        }
    }
}

fn fmt_stats(label: &str, s: &Stats) {
    println!(
        "  {label:8} min={:>6}  mean={:>6}  p50={:>6}  p99={:>6}  max={:>6} us",
        s.min, s.mean, s.p50, s.p99, s.max,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let wasm_path = args.get(0).cloned().unwrap_or_else(|| DEFAULT_WASM.to_string());
    let iterations: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);

    println!("[BENCH] WASM:       {wasm_path}");
    println!("[BENCH] iterations: {iterations}");

    let wasm = std::fs::read(&wasm_path).unwrap_or_else(|e| {
        eprintln!("Could not read {wasm_path}: {e}");
        std::process::exit(2);
    });

    // ── parse ──
    let t0 = Instant::now();
    let bodies = parse_module(&wasm).expect("parse_module");
    let body = bodies.into_iter().next().expect("at least one fn");
    let parse_us = t0.elapsed().as_micros() as u64;

    let local_types: Vec<ValType> = body.local_types.iter().map(|&b| vt(b)).collect();
    let weights = build_attention_data();

    // ── connect + HELLO ──
    let raw = std::env::var("PI_HOST").unwrap_or_else(|_| "192.168.68.72".to_string());
    let host_part = raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw);
    let addr = if host_part.contains(':') { host_part.to_string() }
               else { format!("{host_part}:{DEFAULT_PORT}") };

    println!("[BENCH] connecting to {addr}");
    let t0 = Instant::now();
    let mut sock = TcpStream::connect(&addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();
    let (ty, payload) = read_frame(&mut sock).expect("HELLO");
    assert_eq!(ty, FRAME_HELLO);
    let hello = parse_hello(&payload).expect("parse HELLO");
    let connect_us = t0.elapsed().as_micros() as u64;

    // ── compile against real mem_base ──
    let t0 = Instant::now();
    let mut lw = Lowerer::new_function_with_memory_typed(
        &local_types, Vec::new(), hello.mem_base,
    ).expect("Lowerer");
    lw.set_mem_size(hello.mem_size);
    lw.lower_all(&body.ops).expect("lower_all");
    let code = lw.finish();
    let compile_us = t0.elapsed().as_micros() as u64;

    // ── send signed CODE (one-time) ──
    let tag = auth::sign(&code);
    let mut signed = Vec::with_capacity(code.len() + auth::TAG_LEN);
    signed.extend_from_slice(&code);
    signed.extend_from_slice(&tag);
    let t0 = Instant::now();
    write_frame(&mut sock, FRAME_CODE, &signed).expect("CODE");
    let code_send_us = t0.elapsed().as_micros() as u64;

    // ── warm streaming loop ──
    let mut data_samples: Vec<u64> = Vec::with_capacity(iterations as usize);
    let mut exec_samples: Vec<u64> = Vec::with_capacity(iterations as usize);
    let mut iter_samples: Vec<u64> = Vec::with_capacity(iterations as usize);
    let mut first_result: Option<i32> = None;
    let mut mismatches = 0u32;

    let warm_start = Instant::now();
    for _ in 0..iterations {
        let iter_start = Instant::now();

        let t = Instant::now();
        let data_payload = serialize_data(BASE, &weights);
        write_frame(&mut sock, FRAME_DATA, &data_payload).expect("DATA");
        let data_us = t.elapsed().as_micros() as u64;

        let t = Instant::now();
        write_frame(&mut sock, FRAME_EXEC, &[]).expect("EXEC");
        let (ty, payload) = read_frame(&mut sock).expect("RESULT");
        assert_eq!(ty, FRAME_RESULT);
        let result = parse_result(&payload).expect("parse RESULT");
        let exec_us = t.elapsed().as_micros() as u64;

        let total_us = iter_start.elapsed().as_micros() as u64;

        match first_result {
            None => first_result = Some(result),
            Some(r) if r != result => mismatches += 1,
            _ => {}
        }

        data_samples.push(data_us);
        exec_samples.push(exec_us);
        iter_samples.push(total_us);
    }
    let warm_total_us = warm_start.elapsed().as_micros() as u64;

    let data_stats = Stats::from_unsorted(&mut data_samples);
    let exec_stats = Stats::from_unsorted(&mut exec_samples);
    let iter_stats = Stats::from_unsorted(&mut iter_samples);
    let throughput = if warm_total_us > 0 {
        (iterations as u64 * 1_000_000) / warm_total_us
    } else { 0 };

    // ── report ──
    println!();
    println!("=== one-time costs ===");
    println!("  parse        {parse_us:>6} us  ({} B WASM)", wasm.len());
    println!("  compile      {compile_us:>6} us  ({} B AArch64)", code.len());
    println!("  connect+HELLO {connect_us:>6} us");
    println!("  CODE send    {code_send_us:>6} us  (HMAC-signed, {} B)", signed.len());
    println!();
    println!("=== warm streaming loop ({iterations} iterations) ===");
    println!("  DATA payload: {} B at offset 0x{BASE:x}", weights.len());
    fmt_stats("DATA", &data_stats);
    fmt_stats("EXEC", &exec_stats);
    fmt_stats("ITER", &iter_stats);
    println!();
    println!("throughput: {throughput} ops/sec  (warm-loop wall time {warm_total_us} us)");
    println!(
        "result: {}  ({} mismatches across {} iterations)",
        first_result.unwrap_or(-1),
        mismatches,
        iterations,
    );
}
