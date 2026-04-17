//! Built-in benchmark suite for the JIT pipeline.
//!
//! Measures every phase of the WASM → AArch64 → Pi pipeline with
//! TSC-precision (~0.5 ns) timestamps so we can answer simple but
//! load-bearing questions:
//!
//!   - How long does a parse + JIT compile take for this WASM?
//!   - What's our per-iteration EXEC latency once the connection
//!     is warm? (the number that matters for streaming inference)
//!   - How much of end-to-end latency is network vs JIT vs Pi?
//!   - Are results consistent across iterations?
//!
//! The model: open ONE TCP session, send CODE+initial-DATA once,
//! then loop {DATA, EXEC, RESULT} N times measuring each phase.
//! This mirrors a real streaming-inference workload — first-token
//! latency is paid once, every subsequent inference is just
//! "data in, result out".
//!
//! All output is via the kernel serial logger, formatted as a
//! human-readable report. Statistics are computed in-place over
//! a fixed-size sample buffer (no heap allocation in the hot path).

extern crate alloc;
use alloc::vec::Vec;

use a64_encoder::{parse_module, Lowerer, ValType};

use super::{
    hmac_sign, valtype_from_byte_pub as valtype_from_byte,
    DEFAULT_DATA_BASE, HMAC_TAG_LEN,
};
use crate::drivers::iqe::{rdtsc, tsc_ticks_per_us};
use crate::net::a64_stream::A64Session;

/// Maximum iterations per benchmark run. Cap is mostly to bound
/// the on-stack sample buffer; raise if you need more.
pub const MAX_ITERATIONS: usize = 1024;

/// Per-iteration timing, captured during the warm streaming loop.
#[derive(Clone, Copy)]
struct IterSample {
    data_us: u64,
    exec_us: u64,
    total_us: u64,
    result: i32,
}

/// Aggregated min/mean/p50/p99/max over a list of microsecond
/// samples. Computed by sorting once and indexing percentiles.
pub struct Stats {
    pub min: u64,
    pub mean: u64,
    pub p50: u64,
    pub p99: u64,
    pub max: u64,
}

impl Stats {
    fn from_sorted(sorted: &[u64]) -> Self {
        if sorted.is_empty() {
            return Stats { min: 0, mean: 0, p50: 0, p99: 0, max: 0 };
        }
        let n = sorted.len();
        let sum: u64 = sorted.iter().sum();
        let p50_idx = n / 2;
        // p99 by ceiling: cover the 99th-percentile bucket even
        // when n is small (n=10 → idx 9 = max, which is fine).
        let p99_idx = ((n as u64 * 99 + 99) / 100).saturating_sub(1) as usize;
        let p99_idx = p99_idx.min(n - 1);
        Stats {
            min: sorted[0],
            mean: sum / n as u64,
            p50: sorted[p50_idx],
            p99: sorted[p99_idx],
            max: sorted[n - 1],
        }
    }
}

/// Final benchmark report — printed to serial after the run completes.
pub struct BenchReport {
    pub iterations: u32,
    pub wasm_bytes: usize,
    pub data_bytes: usize,
    pub code_bytes: usize,

    // One-time costs (paid before the warm loop)
    pub parse_us: u64,
    pub compile_us: u64,
    pub connect_us: u64,
    pub code_send_us: u64,

    // Per-iteration warm-loop statistics
    pub data_stats: Stats,
    pub exec_stats: Stats,
    pub iter_stats: Stats,

    // Throughput across the whole warm loop
    pub throughput_per_sec: u64,

    // Correctness: every iteration's RESULT should be identical.
    pub all_results_match: bool,
    pub first_result: i32,
    pub mismatch_count: u32,
}

/// Run a full benchmark: parse, compile, connect, send CODE, then
/// loop N iterations of (DATA, EXEC, RESULT) measuring each phase.
///
/// Returns Err if any I/O step fails (e.g. daemon unreachable);
/// returns Ok with the report otherwise.
pub fn run_bench(
    wasm_bytes: &[u8],
    data_bytes: Option<&[u8]>,
    data_offset: u32,
    pi_ip: [u8; 4],
    pi_port: u16,
    iterations: u32,
) -> Result<BenchReport, &'static str> {
    if iterations == 0 {
        return Err("iterations must be > 0");
    }
    let iters = (iterations as usize).min(MAX_ITERATIONS);

    let ticks_per_us = tsc_ticks_per_us();
    if ticks_per_us == 0 {
        return Err("TSC not calibrated");
    }
    let to_us = |delta: u64| -> u64 { delta / ticks_per_us };

    // ── Phase 1: parse the WASM module ────────────────────────────
    let t0 = rdtsc();
    let bodies = parse_module(wasm_bytes).map_err(|_| "parse_module failed")?;
    let body = bodies.into_iter().next().ok_or("no function in module")?;
    let parse_us = to_us(rdtsc() - t0);

    let mut local_types: Vec<ValType> = Vec::with_capacity(body.local_types.len());
    for &b in &body.local_types {
        local_types.push(valtype_from_byte(b)?);
    }

    // ── Phase 2: open TCP session (HELLO round-trip) ──────────────
    let t0 = rdtsc();
    let session = A64Session::connect(pi_ip, pi_port)?;
    let connect_us = to_us(rdtsc() - t0);

    // ── Phase 3: JIT compile against the daemon's actual mem_base ─
    let t0 = rdtsc();
    let mut lw = Lowerer::new_function_with_memory_typed(
        &local_types,
        Vec::new(),
        session.hello.mem_base,
    )
    .map_err(|_| "Lowerer construction failed")?;
    lw.set_mem_size(session.hello.mem_size);
    lw.lower_all(&body.ops).map_err(|_| "lower_all failed")?;
    let code = lw.finish();
    let compile_us = to_us(rdtsc() - t0);

    // ── Phase 4: HMAC-sign + send CODE (one-time) ─────────────────
    let tag = hmac_sign(&code);
    let mut signed = Vec::with_capacity(code.len() + HMAC_TAG_LEN);
    signed.extend_from_slice(&code);
    signed.extend_from_slice(&tag);

    let t0 = rdtsc();
    session.send_code(&signed)?;
    let code_send_us = to_us(rdtsc() - t0);

    // ── Phase 5: warm streaming loop ──────────────────────────────
    //
    // For each iteration: optionally re-send DATA (so the daemon's
    // linear memory is fresh each call), then EXEC, then read the
    // RESULT. Everything timed.
    let mut samples: Vec<IterSample> = Vec::with_capacity(iters);
    let data_len = data_bytes.map(|d| d.len()).unwrap_or(0);

    let warm_loop_start = rdtsc();
    for _ in 0..iters {
        let iter_start = rdtsc();

        let data_us = if let Some(d) = data_bytes {
            let t = rdtsc();
            session.send_data(data_offset, d)?;
            to_us(rdtsc() - t)
        } else {
            0
        };

        let t = rdtsc();
        let result = session.exec()?;
        let exec_us = to_us(rdtsc() - t);

        let total_us = to_us(rdtsc() - iter_start);
        samples.push(IterSample { data_us, exec_us, total_us, result });
    }
    let warm_loop_total_us = to_us(rdtsc() - warm_loop_start);

    // Best-effort close — don't fail the whole bench if BYE drops.
    let _ = session.close();

    // ── Phase 6: aggregate statistics ─────────────────────────────
    let mut data_vec: Vec<u64> = samples.iter().map(|s| s.data_us).collect();
    let mut exec_vec: Vec<u64> = samples.iter().map(|s| s.exec_us).collect();
    let mut iter_vec: Vec<u64> = samples.iter().map(|s| s.total_us).collect();
    data_vec.sort_unstable();
    exec_vec.sort_unstable();
    iter_vec.sort_unstable();

    let first_result = samples[0].result;
    let mismatch_count =
        samples.iter().filter(|s| s.result != first_result).count() as u32;

    // throughput = iterations / total warm-loop time
    // Express as ops/sec: 1_000_000 µs/sec ÷ µs/op = ops/sec
    let throughput_per_sec = if warm_loop_total_us > 0 {
        (iters as u64 * 1_000_000) / warm_loop_total_us
    } else {
        0
    };

    Ok(BenchReport {
        iterations: iters as u32,
        wasm_bytes: wasm_bytes.len(),
        data_bytes: data_len,
        code_bytes: code.len(),
        parse_us,
        compile_us,
        connect_us,
        code_send_us,
        data_stats: Stats::from_sorted(&data_vec),
        exec_stats: Stats::from_sorted(&exec_vec),
        iter_stats: Stats::from_sorted(&iter_vec),
        throughput_per_sec,
        all_results_match: mismatch_count == 0,
        first_result,
        mismatch_count,
    })
}
