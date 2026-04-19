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
//!   PI_HOST=192.168.68.72:7700 cargo run --release --example bench_real_wasm
//!   cargo run --release --example bench_real_wasm -- <wasm> [iters [runs]]
//!
//! Defaults: attention_wasm.wasm, 100 iterations, 1 run.
//! With runs > 1, reports mean ± stdev across runs (essential for
//! understanding whether a perf delta between commits is real or
//! just measurement noise — a single number can swing 2x on a
//! noisy network).

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

/// One full bench run: open session, install CODE, loop N EXECs,
/// return per-iteration timing + summary throughput.
struct RunResult {
    parse_us: u64,
    compile_us: u64,
    connect_us: u64,
    code_send_us: u64,
    code_bytes: usize,
    data_stats: Stats,
    exec_stats: Stats,
    iter_stats: Stats,
    throughput_per_sec: u64,
    first_result: i32,
    mismatches: u32,
}

fn one_run(addr: &str, wasm: &[u8], weights: &[u8], iterations: u32) -> RunResult {
    // Parse
    let t = Instant::now();
    let bodies = parse_module(wasm).expect("parse_module");
    let body = bodies.into_iter().next().expect("at least one fn");
    let parse_us = t.elapsed().as_micros() as u64;

    let local_types: Vec<ValType> = body.local_types.iter().map(|&b| vt(b)).collect();

    // Connect
    let t = Instant::now();
    let mut sock = TcpStream::connect(addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_nodelay(true).ok();
    let (ty, payload) = read_frame(&mut sock).expect("HELLO");
    assert_eq!(ty, FRAME_HELLO);
    let hello = parse_hello(&payload).expect("parse HELLO");
    let connect_us = t.elapsed().as_micros() as u64;

    // Compile
    let t = Instant::now();
    let mut lw = Lowerer::new_function_with_memory_typed(
        &local_types, Vec::new(), hello.mem_base,
    ).expect("Lowerer");
    lw.set_mem_size(hello.mem_size);
    lw.lower_all(&body.ops).expect("lower_all");
    let code = lw.finish();
    let compile_us = t.elapsed().as_micros() as u64;

    // CODE
    let tag = auth::sign(&code);
    let mut signed = Vec::with_capacity(code.len() + auth::TAG_LEN);
    signed.extend_from_slice(&code);
    signed.extend_from_slice(&tag);
    let t = Instant::now();
    write_frame(&mut sock, FRAME_CODE, &signed).expect("CODE");
    let code_send_us = t.elapsed().as_micros() as u64;

    // Warm loop
    let mut data_samples: Vec<u64> = Vec::with_capacity(iterations as usize);
    let mut exec_samples: Vec<u64> = Vec::with_capacity(iterations as usize);
    let mut iter_samples: Vec<u64> = Vec::with_capacity(iterations as usize);
    let mut first_result: Option<i32> = None;
    let mut mismatches = 0u32;

    let warm_start = Instant::now();
    for _ in 0..iterations {
        let iter_start = Instant::now();

        let t = Instant::now();
        let data_payload = serialize_data(BASE, weights);
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
    let throughput = if warm_total_us > 0 {
        (iterations as u64 * 1_000_000) / warm_total_us
    } else { 0 };

    RunResult {
        parse_us, compile_us, connect_us, code_send_us,
        code_bytes: code.len(),
        data_stats: Stats::from_unsorted(&mut data_samples),
        exec_stats: Stats::from_unsorted(&mut exec_samples),
        iter_stats: Stats::from_unsorted(&mut iter_samples),
        throughput_per_sec: throughput,
        first_result: first_result.unwrap_or(-1),
        mismatches,
    }
}

/// Statistics across multiple runs of the same bench. A single
/// number is meaningless without spread — use this to report
/// mean ± stdev over K independent runs.
struct RunsStats {
    n: usize,
    mean: f64,
    stdev: f64,
    min: u64,
    max: u64,
}

fn runs_stats(samples: &[u64]) -> RunsStats {
    let n = samples.len();
    if n == 0 {
        return RunsStats { n: 0, mean: 0.0, stdev: 0.0, min: 0, max: 0 };
    }
    let sum: u64 = samples.iter().sum();
    let mean = sum as f64 / n as f64;
    let var: f64 = samples.iter()
        .map(|&v| { let d = v as f64 - mean; d * d })
        .sum::<f64>() / n as f64;
    let stdev = var.sqrt();
    RunsStats {
        n,
        mean,
        stdev,
        min: *samples.iter().min().unwrap(),
        max: *samples.iter().max().unwrap(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let wasm_path = args.get(0).cloned().unwrap_or_else(|| DEFAULT_WASM.to_string());
    let iterations: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
    let runs: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);

    println!("[BENCH] WASM:       {wasm_path}");
    println!("[BENCH] iterations: {iterations}  per run");
    println!("[BENCH] runs:       {runs}");
    println!();

    let wasm = std::fs::read(&wasm_path).unwrap_or_else(|e| {
        eprintln!("Could not read {wasm_path}: {e}");
        std::process::exit(2);
    });
    let weights = build_attention_data();

    let raw = std::env::var("PI_HOST").unwrap_or_else(|_| "192.168.68.72".to_string());
    let host_part = raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw);
    let addr = if host_part.contains(':') { host_part.to_string() }
               else { format!("{host_part}:{DEFAULT_PORT}") };
    println!("[BENCH] target:     {addr}");
    println!();

    let mut all_throughput: Vec<u64> = Vec::with_capacity(runs);
    let mut all_exec_mean: Vec<u64> = Vec::with_capacity(runs);
    let mut all_exec_p50: Vec<u64> = Vec::with_capacity(runs);
    let mut all_exec_p99: Vec<u64> = Vec::with_capacity(runs);
    let mut all_compile_us: Vec<u64> = Vec::with_capacity(runs);
    let mut total_mismatches = 0u32;
    let mut first_result_overall: Option<i32> = None;

    for run_idx in 1..=runs {
        if runs > 1 {
            print!("[run {:>2}/{:>2}] ", run_idx, runs);
        }
        let r = one_run(&addr, &wasm, &weights, iterations);

        if runs == 1 {
            // Single-run mode — print full per-phase report.
            println!("=== one-time costs ===");
            println!("  parse        {:>6} us  ({} B WASM)", r.parse_us, wasm.len());
            println!("  compile      {:>6} us  ({} B AArch64)", r.compile_us, r.code_bytes);
            println!("  connect+HELLO {:>6} us", r.connect_us);
            println!("  CODE send    {:>6} us  (HMAC-signed)", r.code_send_us);
            println!();
            println!("=== warm streaming loop ({iterations} iterations) ===");
            println!("  DATA payload: {} B at offset 0x{BASE:x}", weights.len());
            fmt_stats("DATA", &r.data_stats);
            fmt_stats("EXEC", &r.exec_stats);
            fmt_stats("ITER", &r.iter_stats);
            println!();
            println!("throughput: {} ops/sec", r.throughput_per_sec);
            println!(
                "result: {}  ({} mismatches across {} iterations)",
                r.first_result, r.mismatches, iterations,
            );
        } else {
            // Multi-run mode — one compact line per run.
            println!(
                "throughput={:>5} ops/sec  exec_mean={:>5} us  p50={:>5}  p99={:>5}  result={}",
                r.throughput_per_sec, r.exec_stats.mean,
                r.exec_stats.p50, r.exec_stats.p99,
                r.first_result,
            );
        }

        all_throughput.push(r.throughput_per_sec);
        all_exec_mean.push(r.exec_stats.mean);
        all_exec_p50.push(r.exec_stats.p50);
        all_exec_p99.push(r.exec_stats.p99);
        all_compile_us.push(r.compile_us);
        total_mismatches += r.mismatches;
        match first_result_overall {
            None => first_result_overall = Some(r.first_result),
            Some(prev) if prev != r.first_result => total_mismatches += 1,
            _ => {}
        }
    }

    // Multi-run summary
    if runs > 1 {
        println!();
        println!("=== summary across {runs} runs ===");
        let tp = runs_stats(&all_throughput);
        let em = runs_stats(&all_exec_mean);
        let ep50 = runs_stats(&all_exec_p50);
        let ep99 = runs_stats(&all_exec_p99);
        let cp = runs_stats(&all_compile_us);

        println!(
            "  throughput   {:>7.1} ± {:>5.1} ops/sec  (min={}, max={})",
            tp.mean, tp.stdev, tp.min, tp.max,
        );
        println!(
            "  exec_mean    {:>7.1} ± {:>5.1} us       (min={}, max={})",
            em.mean, em.stdev, em.min, em.max,
        );
        println!(
            "  exec_p50     {:>7.1} ± {:>5.1} us       (min={}, max={})",
            ep50.mean, ep50.stdev, ep50.min, ep50.max,
        );
        println!(
            "  exec_p99     {:>7.1} ± {:>5.1} us       (min={}, max={})",
            ep99.mean, ep99.stdev, ep99.min, ep99.max,
        );
        println!(
            "  compile_time {:>7.1} ± {:>5.1} us       (min={}, max={})",
            cp.mean, cp.stdev, cp.min, cp.max,
        );

        let cv = if tp.mean > 0.0 { tp.stdev / tp.mean * 100.0 } else { 0.0 };
        println!(
            "  throughput coefficient of variation: {:.1}%   ({} mismatches across all runs)",
            cv, total_mismatches,
        );
    }
}
