//! Apples-to-apples benchmark: SDOT (int8) vs scalar f32 matmul
//! on the same input dimensions.
//!
//! For a square N×N matmul:
//!   * f32 path: hand-built JIT loop, scalar FMUL + FADD
//!   * SDOT path: hand-built JIT, 16 i8 MACs per SDOT instruction
//!
//! N is fixed at 64 — large enough that the SDOT pipeline fills
//! and small enough that one EXEC fits comfortably in our memory.
//!
//! Both paths get the same input data interpretation (i8 values
//! in the int test, f32 in the float test) so the throughput
//! difference is the architecture difference.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use a64_encoder::{Lowerer, ValType, WasmOp};
use a64_streamer::{
    auth, parse_hello, parse_result, read_frame, serialize_data, write_frame, DEFAULT_PORT,
    FRAME_CODE, FRAME_DATA, FRAME_EXEC, FRAME_HELLO, FRAME_RESULT,
};

/// Dot-product length: 4 KiB of i8 / 16 KiB of f32. Big enough that
/// SDOT's pipeline fills, small enough to fit in L1 (Cortex-A76 has
/// 64 KiB L1D).
const N: u32 = 4096;

/// How many times we run the inner dot product per EXEC. Without
/// this, the per-EXEC compute is microseconds and the ~3 ms network
/// RTT dominates so badly the speedup is invisible. K=200 makes the
/// compute the dominant cost so we can actually see f32 vs SDOT.
const REPS: u32 = 200;

const ITERATIONS: u32 = 30;

fn build_f32_dot_loop(mem_base: u64) -> (Vec<u8>, u32) {
    // Outer loop runs the inner dot product REPS times, resetting
    // the accumulator each iteration so the final return value
    // equals one dot product (not REPS × it). Locals:
    //   0: k (i32, inner counter)
    //   1: acc (f32)
    //   2: r (i32, outer counter)
    const A_OFF: u32 = 0x1000;
    const B_OFF: u32 = 0x1000 + N * 4; // immediately after a

    let mut ops = Vec::new();
    // r = 0
    ops.push(WasmOp::I32Const(0));
    ops.push(WasmOp::LocalSet(2));

    ops.push(WasmOp::Block);
    ops.push(WasmOp::Loop);
    ops.push(WasmOp::LocalGet(2));
    ops.push(WasmOp::I32Const(REPS as i32));
    ops.push(WasmOp::I32GeS);
    ops.push(WasmOp::BrIf(1));

    // ── inner dot product ──
    ops.push(WasmOp::F32Const(0.0));
    ops.push(WasmOp::LocalSet(1));
    ops.push(WasmOp::I32Const(0));
    ops.push(WasmOp::LocalSet(0));
    ops.push(WasmOp::Block);
    ops.push(WasmOp::Loop);
    ops.push(WasmOp::LocalGet(0));
    ops.push(WasmOp::I32Const(N as i32));
    ops.push(WasmOp::I32GeS);
    ops.push(WasmOp::BrIf(1));
    ops.push(WasmOp::LocalGet(1));
    ops.push(WasmOp::LocalGet(0));
    ops.push(WasmOp::I32Const(4));
    ops.push(WasmOp::I32Mul);
    ops.push(WasmOp::F32Load(A_OFF));
    ops.push(WasmOp::LocalGet(0));
    ops.push(WasmOp::I32Const(4));
    ops.push(WasmOp::I32Mul);
    ops.push(WasmOp::F32Load(B_OFF));
    ops.push(WasmOp::F32Mul);
    ops.push(WasmOp::F32Add);
    ops.push(WasmOp::LocalSet(1));
    ops.push(WasmOp::LocalGet(0));
    ops.push(WasmOp::I32Const(1));
    ops.push(WasmOp::I32Add);
    ops.push(WasmOp::LocalSet(0));
    ops.push(WasmOp::Br(0));
    ops.push(WasmOp::End);
    ops.push(WasmOp::End);

    // r += 1
    ops.push(WasmOp::LocalGet(2));
    ops.push(WasmOp::I32Const(1));
    ops.push(WasmOp::I32Add);
    ops.push(WasmOp::LocalSet(2));
    ops.push(WasmOp::Br(0));
    ops.push(WasmOp::End);
    ops.push(WasmOp::End);

    ops.push(WasmOp::LocalGet(1));
    ops.push(WasmOp::I32TruncF32S);
    ops.push(WasmOp::End);

    let mut lw = Lowerer::new_function_with_memory_typed(
        &[ValType::I32, ValType::F32, ValType::I32],
        Vec::new(),
        mem_base,
    ).unwrap();
    lw.set_mem_size(4 * 1024 * 1024);
    lw.lower_all(&ops).unwrap();
    let elisions = lw.elision_count();
    (lw.finish(), elisions)
}

fn build_sdot_loop(mem_base: u64) -> (Vec<u8>, u32) {
    // Outer loop runs the SDOT inner loop REPS times. Locals:
    //   0: k (i32, inner counter, byte index 0..N stride 16)
    //   1: acc (v128, i32x4 accumulator)
    //   2: r (i32, outer counter)
    const A_OFF: u32 = 0x1000;
    const B_OFF: u32 = 0x1000 + N; // i8 arrays, 1 B per lane

    let mut ops = Vec::new();
    // r = 0
    ops.push(WasmOp::I32Const(0));
    ops.push(WasmOp::LocalSet(2));

    ops.push(WasmOp::Block);
    ops.push(WasmOp::Loop);
    ops.push(WasmOp::LocalGet(2));
    ops.push(WasmOp::I32Const(REPS as i32));
    ops.push(WasmOp::I32GeS);
    ops.push(WasmOp::BrIf(1));

    // ── inner SDOT loop ──
    ops.push(WasmOp::V128Const(0));
    ops.push(WasmOp::LocalSet(1));
    ops.push(WasmOp::I32Const(0));
    ops.push(WasmOp::LocalSet(0));
    ops.push(WasmOp::Block);
    ops.push(WasmOp::Loop);
    ops.push(WasmOp::LocalGet(0));
    ops.push(WasmOp::I32Const(N as i32));
    ops.push(WasmOp::I32GeS);
    ops.push(WasmOp::BrIf(1));
    ops.push(WasmOp::LocalGet(1));            // acc
    ops.push(WasmOp::LocalGet(0));            // k (byte addr)
    ops.push(WasmOp::V128Load(A_OFF));        // a chunk
    ops.push(WasmOp::LocalGet(0));
    ops.push(WasmOp::V128Load(B_OFF));        // b chunk
    ops.push(WasmOp::I32x4DotI8x16Signed);    // 16 i8 MACs in 1 instr
    ops.push(WasmOp::LocalSet(1));
    ops.push(WasmOp::LocalGet(0));
    ops.push(WasmOp::I32Const(16));
    ops.push(WasmOp::I32Add);
    ops.push(WasmOp::LocalSet(0));
    ops.push(WasmOp::Br(0));
    ops.push(WasmOp::End);
    ops.push(WasmOp::End);

    // r += 1
    ops.push(WasmOp::LocalGet(2));
    ops.push(WasmOp::I32Const(1));
    ops.push(WasmOp::I32Add);
    ops.push(WasmOp::LocalSet(2));
    ops.push(WasmOp::Br(0));
    ops.push(WasmOp::End);
    ops.push(WasmOp::End);

    // Reduce the 4 i32 lanes via extract_lane + add.
    ops.push(WasmOp::LocalGet(1));
    ops.push(WasmOp::I32x4ExtractLane(0));
    ops.push(WasmOp::LocalGet(1));
    ops.push(WasmOp::I32x4ExtractLane(1));
    ops.push(WasmOp::I32Add);
    ops.push(WasmOp::LocalGet(1));
    ops.push(WasmOp::I32x4ExtractLane(2));
    ops.push(WasmOp::I32Add);
    ops.push(WasmOp::LocalGet(1));
    ops.push(WasmOp::I32x4ExtractLane(3));
    ops.push(WasmOp::I32Add);
    ops.push(WasmOp::End);

    let mut lw = Lowerer::new_function_with_memory_typed(
        &[ValType::I32, ValType::V128, ValType::I32],
        Vec::new(),
        mem_base,
    ).unwrap();
    lw.set_mem_size(4 * 1024 * 1024);
    lw.lower_all(&ops).unwrap();
    let elisions = lw.elision_count();
    (lw.finish(), elisions)
}

fn run_one(addr: &str, code: &[u8], data: &[u8], iters: u32) -> (i32, Vec<u128>) {
    let mut sock = TcpStream::connect(addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(15))).ok();
    sock.set_nodelay(true).ok();
    let (ty, _payload) = read_frame(&mut sock).expect("HELLO");
    assert_eq!(ty, FRAME_HELLO);

    let data_payload = serialize_data(0x1000, data);
    write_frame(&mut sock, FRAME_DATA, &data_payload).expect("DATA");

    let tag = auth::sign(code);
    let mut signed = Vec::with_capacity(code.len() + auth::TAG_LEN);
    signed.extend_from_slice(code);
    signed.extend_from_slice(&tag);
    write_frame(&mut sock, FRAME_CODE, &signed).expect("CODE");

    let mut last = 0i32;
    let mut samples: Vec<u128> = Vec::with_capacity(iters as usize);
    for _ in 0..iters {
        let t = Instant::now();
        write_frame(&mut sock, FRAME_EXEC, &[]).expect("EXEC");
        let (ty, payload) = read_frame(&mut sock).expect("RESULT");
        assert_eq!(ty, FRAME_RESULT);
        last = parse_result(&payload).expect("RESULT");
        samples.push(t.elapsed().as_micros());
    }
    (last, samples)
}

fn stats(samples: &mut Vec<u128>) -> (u128, u128, u128) {
    samples.sort_unstable();
    let n = samples.len();
    let mean = samples.iter().sum::<u128>() / n as u128;
    let p50 = samples[n / 2];
    let min = samples[0];
    (min, mean, p50)
}

fn main() {
    let raw = std::env::var("PI_HOST").unwrap_or_else(|_| "192.168.68.72".to_string());
    let host_part = raw.split_once('@').map(|(_, h)| h).unwrap_or(&raw);
    let addr = if host_part.contains(':') { host_part.to_string() }
               else { format!("{host_part}:{DEFAULT_PORT}") };

    println!("[SDOT-BENCH] N = {N}, REPS = {REPS} per EXEC, iters = {ITERATIONS}");
    let macs = (N as u64) * (REPS as u64);
    println!("[SDOT-BENCH] MACs per EXEC: {} ({:.2} M)",
             macs,
             macs as f64 / 1_000_000.0);

    // ── Discover mem_base ──────────────────────────────────────────
    let mem_base = {
        let mut sock = TcpStream::connect(&addr).expect("connect");
        sock.set_nodelay(true).ok();
        let (_, p) = read_frame(&mut sock).expect("HELLO");
        parse_hello(&p).expect("hello").mem_base
    };
    println!("[SDOT-BENCH] mem_base = 0x{:016x}", mem_base);

    // ── Build f32 data: a[k] = 1.0, b[k] = 2.0 ─────────────────────
    // Σ (1 * 2) over N elements = 2N. With N=4096: 8192.
    let mut f32_data = vec![0u8; (N * 8) as usize];
    for k in 0..N {
        let a = 1.0f32;
        let b = 2.0f32;
        f32_data[(k*4) as usize..(k*4+4) as usize].copy_from_slice(&a.to_le_bytes());
        f32_data[((N+k)*4) as usize..((N+k)*4+4) as usize].copy_from_slice(&b.to_le_bytes());
    }
    let expected: i32 = (2 * N) as i32;
    println!("[SDOT-BENCH] expected: {expected}");

    // ── Build i8 data: a[k] = 1, b[k] = 2 ──────────────────────────
    let mut i8_data = vec![0u8; (N * 2) as usize];
    for k in 0..N {
        i8_data[k as usize] = 1i8 as u8;
        i8_data[(N + k) as usize] = 2i8 as u8;
    }
    let f32_expected = expected;
    let i8_expected = expected;

    // ── F32 path ───────────────────────────────────────────────────
    let (f32_code, f32_elisions) = build_f32_dot_loop(mem_base);
    println!("[SDOT-BENCH] f32 JIT: {} bytes ({} insns), {} bounds-checks elided",
             f32_code.len(), f32_code.len() / 4, f32_elisions);
    let (got, mut s) = run_one(&addr, &f32_code, &f32_data, ITERATIONS);
    let (f_min, f_mean, f_p50) = stats(&mut s);
    println!(
        "[SDOT-BENCH] f32 result: {got} (expected {f32_expected})  exec min={f_min} mean={f_mean} p50={f_p50} us",
    );

    // ── SDOT path ──────────────────────────────────────────────────
    let (sdot_code, sdot_elisions) = build_sdot_loop(mem_base);
    println!("[SDOT-BENCH] SDOT JIT: {} bytes ({} insns), {} bounds-checks elided",
             sdot_code.len(), sdot_code.len() / 4, sdot_elisions);
    let (got, mut s) = run_one(&addr, &sdot_code, &i8_data, ITERATIONS);
    let (s_min, s_mean, s_p50) = stats(&mut s);
    println!(
        "[SDOT-BENCH] SDOT result: {got} (expected {i8_expected})  exec min={s_min} mean={s_mean} p50={s_p50} us",
    );

    println!();
    println!("─── speedup ───");
    let f64_p50 = f_p50 as f64;
    let s64_p50 = s_p50 as f64;
    if s64_p50 > 0.0 {
        println!("  SDOT vs f32 (p50): {:.2}× faster", f64_p50 / s64_p50);
    }
    let f64_min = f_min as f64;
    let s64_min = s_min as f64;
    if s64_min > 0.0 {
        println!("  SDOT vs f32 (min): {:.2}× faster", f64_min / s64_min);
    }
}
