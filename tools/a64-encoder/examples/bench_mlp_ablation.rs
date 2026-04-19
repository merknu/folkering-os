//! Ablation benchmark across three MLP scales.
//!
//! For each test (A: 16→32→16, B: 64→128→64, C: 256→512→256):
//!   1. Generate deterministic pseudo-random weights + inputs
//!   2. Compute reference output in Rust (same f32 op-order)
//!   3. Compile WASM module → AArch64 via the multi-function linker
//!   4. Connect, ship CODE + DATA, run N warm iterations
//!   5. Report compile time, exec time stats, throughput, MAC rate
//!
//! Output is one summary block per test, plus a final comparison
//! table so you can see the sweet-spot at a glance.
//!
//! Usage:
//!   PI_HOST=192.168.68.72:7700 cargo run --release --example bench_mlp_ablation [iters]
//!
//! Default iters = 50 per test (Test C is heavy; lower if needed).

use std::net::TcpStream;
use std::time::{Duration, Instant};

use a64_encoder::{compile_module, parse_module_full};
use a64_streamer::{
    auth, parse_hello, parse_result, read_frame, serialize_data, write_frame, DEFAULT_PORT,
    FRAME_CODE, FRAME_DATA, FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
};

#[derive(Clone, Copy)]
struct Spec {
    name: &'static str,
    label: &'static str,
    wasm_path: &'static str,
    n_in: usize,
    n_h1: usize,
    n_h2: usize,
    // Memory layout offsets (relative to BASE = 0x1000):
    off_inputs: usize,
    off_w1: usize,
    off_b1: usize,
    off_w2: usize,
    off_b2: usize,
    off_w3: usize,
    off_b3: usize,
}

const SPECS: &[Spec] = &[
    Spec {
        name: "A", label: "16 → 32 → 16 → 1",
        wasm_path: "examples/wasm-mlp-a/target/wasm32-unknown-unknown/release/mlp_a_wasm.wasm",
        n_in: 16, n_h1: 32, n_h2: 16,
        off_inputs: 0x0000, off_w1: 0x0040, off_b1: 0x0840,
        off_w2: 0x08C0, off_b2: 0x10C0,
        off_w3: 0x1100, off_b3: 0x1140,
    },
    Spec {
        name: "B", label: "64 → 128 → 64 → 1",
        wasm_path: "examples/wasm-mlp-b/target/wasm32-unknown-unknown/release/mlp_b_wasm.wasm",
        n_in: 64, n_h1: 128, n_h2: 64,
        off_inputs: 0x00000, off_w1: 0x00100, off_b1: 0x08100,
        off_w2: 0x08300, off_b2: 0x10300,
        off_w3: 0x10400, off_b3: 0x10500,
    },
    Spec {
        name: "C", label: "256 → 512 → 256 → 1",
        wasm_path: "examples/wasm-mlp-c/target/wasm32-unknown-unknown/release/mlp_c_wasm.wasm",
        n_in: 256, n_h1: 512, n_h2: 256,
        off_inputs: 0x000000, off_w1: 0x000400, off_b1: 0x080400,
        off_w2: 0x080C00, off_b2: 0x100C00,
        off_w3: 0x101000, off_b3: 0x101400,
    },
];

const BASE: u32 = 0x1000;

fn pseudo_weights(spec: &Spec) -> Vec<u8> {
    let mut state: u32 = 0x1234_5678;
    let mut next = || -> f32 {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        (state as f32 / u32::MAX as f32) - 0.5
    };
    let total_size = spec.off_b3 + 4;
    let mut buf = vec![0u8; total_size];

    for k in 0..spec.n_in {
        let off = spec.off_inputs + k * 4;
        // Use a deterministic input pattern (small distinct values).
        let v = (k as f32 / spec.n_in as f32) * 2.0 - 1.0;
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    for k in 0..spec.n_in { for j in 0..spec.n_h1 {
        let off = spec.off_w1 + (k * spec.n_h1 + j) * 4;
        buf[off..off + 4].copy_from_slice(&next().to_le_bytes());
    }}
    for j in 0..spec.n_h1 {
        let off = spec.off_b1 + j * 4;
        buf[off..off + 4].copy_from_slice(&next().to_le_bytes());
    }
    for k in 0..spec.n_h1 { for j in 0..spec.n_h2 {
        let off = spec.off_w2 + (k * spec.n_h2 + j) * 4;
        buf[off..off + 4].copy_from_slice(&next().to_le_bytes());
    }}
    for j in 0..spec.n_h2 {
        let off = spec.off_b2 + j * 4;
        buf[off..off + 4].copy_from_slice(&next().to_le_bytes());
    }
    for k in 0..spec.n_h2 {
        let off = spec.off_w3 + k * 4;
        buf[off..off + 4].copy_from_slice(&next().to_le_bytes());
    }
    let b3 = next();
    buf[spec.off_b3..spec.off_b3 + 4].copy_from_slice(&b3.to_le_bytes());
    buf
}

fn host_reference(spec: &Spec, buf: &[u8]) -> i32 {
    let lf = |off: usize| -> f32 {
        f32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    };
    let mut h1 = vec![0.0f32; spec.n_h1];
    for j in 0..spec.n_h1 {
        let mut s = lf(spec.off_b1 + j * 4);
        for k in 0..spec.n_in {
            s += lf(spec.off_inputs + k * 4) * lf(spec.off_w1 + (k * spec.n_h1 + j) * 4);
        }
        h1[j] = if s > 0.0 { s } else { 0.0 };
    }
    let mut h2 = vec![0.0f32; spec.n_h2];
    for j in 0..spec.n_h2 {
        let mut s = lf(spec.off_b2 + j * 4);
        for k in 0..spec.n_h1 {
            s += h1[k] * lf(spec.off_w2 + (k * spec.n_h2 + j) * 4);
        }
        h2[j] = if s > 0.0 { s } else { 0.0 };
    }
    let mut out = lf(spec.off_b3);
    for k in 0..spec.n_h2 {
        out += h2[k] * lf(spec.off_w3 + k * 4);
    }
    (out * 1000.0) as i32
}

#[derive(Clone)]
struct Result {
    name: &'static str,
    label: &'static str,
    wasm_bytes: usize,
    weight_bytes: usize,
    aarch64_bytes: usize,
    macs: u64,
    compile_us: u128,
    exec_min_us: u128,
    exec_mean_us: u128,
    exec_p50_us: u128,
    exec_p99_us: u128,
    throughput: u64,
    gflops: f64,
    matched: bool,
    expected: i32,
    got: i32,
}

fn run_test(spec: &Spec, addr: &str, iters: u32) -> std::result::Result<Result, String> {
    let wasm = std::fs::read(spec.wasm_path).map_err(|e| format!("read wasm: {e}"))?;
    let module = parse_module_full(&wasm).map_err(|e| format!("parse: {e:?}"))?;
    let entry = module.exports.iter()
        .find(|e| e.kind == 0 && e.name == "entry")
        .map(|e| e.index)
        .ok_or("no 'entry' export")?;

    let weights = pseudo_weights(spec);
    let expected = host_reference(spec, &weights);

    // ── Connect (per-test fresh session) ───────────────────────────
    let mut sock = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_secs(60))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(60))).ok();
    sock.set_nodelay(true).ok();
    let (ty, payload) = read_frame(&mut sock).map_err(|e| format!("HELLO: {e}"))?;
    if ty != FRAME_HELLO { return Err(format!("expected HELLO got 0x{ty:02x}")); }
    let hello = parse_hello(&payload).map_err(|e| format!("parse HELLO: {e:?}"))?;

    // ── Compile (timed) ────────────────────────────────────────────
    let t = Instant::now();
    let layout = compile_module(&module, hello.mem_base, hello.mem_size, entry)
        .map_err(|e| format!("compile_module: {e:?}"))?;
    let compile_us = t.elapsed().as_micros();

    // ── Ship CODE once ─────────────────────────────────────────────
    let tag = auth::sign(&layout.code);
    let mut signed = Vec::with_capacity(layout.code.len() + auth::TAG_LEN);
    signed.extend_from_slice(&layout.code);
    signed.extend_from_slice(&tag);
    write_frame(&mut sock, FRAME_CODE, &signed).map_err(|e| format!("CODE: {e}"))?;

    // ── Warm-up: ship DATA once + one EXEC to validate result ──────
    let data_payload = serialize_data(BASE, &weights);
    write_frame(&mut sock, FRAME_DATA, &data_payload).map_err(|e| format!("DATA: {e}"))?;
    write_frame(&mut sock, FRAME_EXEC, &[]).map_err(|e| format!("EXEC: {e}"))?;
    let (ty, payload) = read_frame(&mut sock).map_err(|e| format!("RESULT: {e}"))?;
    if ty != FRAME_RESULT { return Err(format!("expected RESULT got 0x{ty:02x}")); }
    let got = parse_result(&payload).map_err(|e| format!("parse RESULT: {e:?}"))?;
    let matched = (got - expected).abs() <= 1;

    // ── Bench loop: re-ship DATA + EXEC each iteration ─────────────
    let mut samples: Vec<u128> = Vec::with_capacity(iters as usize);
    let warm_start = Instant::now();
    for _ in 0..iters {
        let t = Instant::now();
        write_frame(&mut sock, FRAME_DATA, &data_payload).map_err(|e| format!("DATA: {e}"))?;
        write_frame(&mut sock, FRAME_EXEC, &[]).map_err(|e| format!("EXEC: {e}"))?;
        let (ty, payload) = read_frame(&mut sock).map_err(|e| format!("RESULT: {e}"))?;
        if ty != FRAME_RESULT { return Err(format!("expected RESULT got 0x{ty:02x}")); }
        let _ = parse_result(&payload).map_err(|e| format!("parse RESULT: {e:?}"))?;
        samples.push(t.elapsed().as_micros());
    }
    let warm_total = warm_start.elapsed().as_micros();
    samples.sort_unstable();
    let n = samples.len();
    let exec_min = samples[0];
    let exec_mean = samples.iter().sum::<u128>() / n as u128;
    let exec_p50 = samples[n / 2];
    let exec_p99 = samples[((n as f64) * 0.99) as usize];
    let throughput = if warm_total > 0 {
        (iters as u64 * 1_000_000) / warm_total as u64
    } else { 0 };

    // MAC count: layer1 (n_in × n_h1) + layer2 (n_h1 × n_h2) + layer3 (n_h2)
    let macs = (spec.n_in * spec.n_h1 + spec.n_h1 * spec.n_h2 + spec.n_h2) as u64;
    let gflops = if exec_mean > 0 {
        (2.0 * macs as f64) / (exec_mean as f64 * 1e3)
    } else { 0.0 };

    Ok(Result {
        name: spec.name, label: spec.label,
        wasm_bytes: wasm.len(),
        weight_bytes: weights.len(),
        aarch64_bytes: layout.code.len(),
        macs,
        compile_us,
        exec_min_us: exec_min,
        exec_mean_us: exec_mean,
        exec_p50_us: exec_p50,
        exec_p99_us: exec_p99,
        throughput,
        gflops,
        matched,
        expected, got,
    })
}

fn main() {
    let iters: u32 = std::env::args().nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    let raw = std::env::var("PI_HOST").unwrap_or_else(|_| "192.168.68.72".to_string());
    let host_part = raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw);
    let addr = if host_part.contains(':') { host_part.to_string() }
               else { format!("{host_part}:{DEFAULT_PORT}") };

    println!("[ABLATION] target: {addr}, iters per test: {iters}");
    println!();

    let mut results: Vec<Result> = Vec::new();
    for spec in SPECS {
        println!("─── Test {} ({}) ───", spec.name, spec.label);
        match run_test(spec, &addr, iters) {
            Ok(r) => {
                println!(
                    "  WASM: {:>6} B  weights: {:>7} B  AArch64: {:>7} B  MACs: {:>7}",
                    r.wasm_bytes, r.weight_bytes, r.aarch64_bytes, r.macs,
                );
                println!("  compile      {:>8} us", r.compile_us);
                println!(
                    "  EXEC  min={:>6}  mean={:>6}  p50={:>6}  p99={:>6} us",
                    r.exec_min_us, r.exec_mean_us, r.exec_p50_us, r.exec_p99_us,
                );
                println!(
                    "  throughput   {:>5} ops/sec   ≈ {:.3} GFLOPS",
                    r.throughput, r.gflops,
                );
                println!(
                    "  result       {} (expected {}) {}",
                    r.got, r.expected,
                    if r.matched { "✓" } else { "✗ MISMATCH" },
                );
                results.push(r);
            }
            Err(e) => {
                println!("  FAILED: {e}");
            }
        }
        println!();
    }

    // Final comparison table
    println!("─── Ablation summary ───");
    println!(
        "{:>4}  {:<22}  {:>7}  {:>9}  {:>6}  {:>9}  {:>6}",
        "Test", "shape", "weights", "AArch64", "MACs", "exec_p50", "GFLOPS",
    );
    for r in &results {
        println!(
            "{:>4}  {:<22}  {:>7}  {:>9}  {:>6}  {:>9}  {:>6.3}",
            r.name, r.label, r.weight_bytes, r.aarch64_bytes,
            r.macs, r.exec_p50_us, r.gflops,
        );
    }
}
