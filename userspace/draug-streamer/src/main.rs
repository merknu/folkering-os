//! draug-streamer — WASM Streaming Service client.
//!
//! Runs inside Folkering OS userspace. Connects to the Pi-side
//! `a64-stream-daemon` over TCP, JITs AArch64 machine code on the
//! x86 side using `a64-encoder`, streams CODE + DATA + EXEC frames,
//! and prints RESULT values as they come back.
//!
//! This is Fase B of the WASM Streaming Service — cutting the SSH
//! umbilical means the entire pipeline (JIT → wire format → remote
//! execution → result) happens autonomously from inside the x86
//! kernel's async TCP stack, with zero Linux tools in the loop.
//!
//! Target daemon: `192.168.68.72:14712` (see `tools/a64-streamer`).
//! Protocol: see `tools/a64-streamer/src/lib.rs` for the canonical
//! spec; this binary's `protocol.rs` ports the pure parts to no_std.

#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

use alloc::vec::Vec;

use libfolk::{entry, println};
use libfolk::sys::{get_pid, uptime, yield_cpu};

use a64_encoder::{Lowerer, WasmOp};

mod protocol;
mod tcp;

// Share the HMAC auth module with the Pi-side daemon so both agree
// on the secret key + sign/verify algorithm. The #[path] attribute
// lets us embed the same source file directly without forcing the
// daemon's crate to become a dependency of our no_std userspace
// binary (which would pull std via transitive-feature issues).
#[path = "../../../tools/a64-streamer/src/auth.rs"]
mod auth;

use protocol::{
    build_data_payload, build_frame, parse_hello, parse_result, take_frame, Hello,
    FRAME_BYE, FRAME_CODE, FRAME_DATA, FRAME_EXEC, FRAME_HEADER_LEN, FRAME_HELLO, FRAME_RESULT,
};
use tcp::{TcpError, TcpSession};

// ── Bump allocator ──────────────────────────────────────────────────
//
// 64 KiB heap in BSS — enough for the JIT code buffer, protocol
// scratch, and HELLO payload. No deallocation (bump-only) matches the
// synapse-service / inference-server pattern: this is a linear,
// short-running streaming client, not a long-lived service.

const HEAP_SIZE: usize = 64 * 1024;

struct BumpAllocator {
    heap: UnsafeCell<[u8; HEAP_SIZE]>,
    offset: UnsafeCell<usize>,
}

unsafe impl Sync for BumpAllocator {}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let offset = &mut *self.offset.get();
        let align = layout.align();
        let aligned = (*offset + align - 1) & !(align - 1);
        let new_offset = aligned + layout.size();
        if new_offset > HEAP_SIZE {
            core::ptr::null_mut()
        } else {
            *offset = new_offset;
            (*self.heap.get()).as_mut_ptr().add(aligned)
        }
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    heap: UnsafeCell::new([0; HEAP_SIZE]),
    offset: UnsafeCell::new(0),
};

// ── Target ──────────────────────────────────────────────────────────

/// Pi-side `a64-stream-daemon` address, configured at compile time
/// via `FOLKERING_STREAMER_IP` and `FOLKERING_STREAMER_PORT`. Default
/// is the SLIRP gateway (`10.0.2.2:14712`) where the host relay runs;
/// override with `FOLKERING_STREAMER_IP=192.168.68.72` to talk to a
/// physical Pi on the LAN.
///
/// Pre-cleanup the streamer hardcoded `[192, 168, 68, 72]:14712` and
/// ARPed it forever on boot — when the target was offline this
/// pegged smoltcp's ARP cache and starved Phase 17's outbound TCP.
/// Now combined with `BACKOFF_*` below: at most `MAX_ATTEMPTS`
/// connect attempts, then we exit to idle yield without burning more
/// network resources.
const DAEMON_IP: [u8; 4] = match option_env!("FOLKERING_STREAMER_IP") {
    Some(s) => parse_ipv4(s),
    None => [10, 0, 2, 2],
};
const DAEMON_PORT: u16 = match option_env!("FOLKERING_STREAMER_PORT") {
    Some(s) => parse_u16(s),
    None => 14712,
};

/// Hard cap on connect / reconnect attempts. After this many
/// transport-layer failures we stop trying and idle the task — the
/// kernel's smoltcp stack stops being woken by our connect calls,
/// which un-pegs ARP for everything else (notably the Phase 17
/// outbound TCP that the proxy depends on).
const MAX_ATTEMPTS: u32 = 5;

/// Initial backoff before the second attempt, in milliseconds.
/// Doubles each attempt: 2s, 4s, 8s, 16s, 32s (capped). Total wait
/// across `MAX_ATTEMPTS = 5` is ~62s, after which we give up.
const BACKOFF_INITIAL_MS: u64 = 2_000;
/// Cap backoff so very-long sessions don't sleep silently for
/// minutes. 32 s is the largest doubling under MAX_ATTEMPTS = 5.
const BACKOFF_CAP_MS: u64 = 32_000;

const fn parse_ipv4(s: &str) -> [u8; 4] {
    let bytes = s.as_bytes();
    let mut out = [0u8; 4];
    let mut octet: usize = 0;
    let mut acc: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'.' {
            if octet < 4 { out[octet] = acc as u8; }
            octet += 1;
            acc = 0;
        } else if b >= b'0' && b <= b'9' {
            acc = acc * 10 + (b - b'0') as u32;
        }
        i += 1;
    }
    if octet < 4 { out[octet] = acc as u8; }
    out
}

const fn parse_u16(s: &str) -> u16 {
    let bytes = s.as_bytes();
    let mut acc: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b >= b'0' && b <= b'9' {
            acc = acc * 10 + (b - b'0') as u32;
        }
        i += 1;
    }
    acc as u16
}

entry!(main);

fn main() -> ! {
    println!("[DRAUG-STREAMER] === ENTRY === (PID {}) target={}.{}.{}.{}:{}",
        get_pid(),
        DAEMON_IP[0], DAEMON_IP[1], DAEMON_IP[2], DAEMON_IP[3], DAEMON_PORT);

    let mut attempt: u32 = 0;
    let mut backoff_ms: u64 = BACKOFF_INITIAL_MS;
    loop {
        attempt += 1;
        match run() {
            Ok(()) => {
                println!("[DRAUG-STREAMER] stream complete — idle.");
                break;
            }
            Err(e) => {
                println!("[DRAUG-STREAMER] attempt {}/{} failed: {:?}",
                    attempt, MAX_ATTEMPTS, e);
                if attempt >= MAX_ATTEMPTS {
                    println!("[DRAUG-STREAMER] giving up after {} attempts — idle.",
                        MAX_ATTEMPTS);
                    break;
                }
                println!("[DRAUG-STREAMER] backing off {} ms before retry...", backoff_ms);
                sleep_ms_yielding(backoff_ms);
                backoff_ms = (backoff_ms * 2).min(BACKOFF_CAP_MS);
            }
        }
    }
    loop {
        yield_cpu();
    }
}

/// Yield-loop sleep. Folkering doesn't have a kernel-side `sleep_ms`
/// syscall, so we busy-wait on `uptime()` and yield the CPU on every
/// pass. The granularity is whatever the scheduler tick is; for the
/// 2 s – 32 s ranges we use that's plenty.
fn sleep_ms_yielding(ms: u64) {
    let target = uptime().saturating_add(ms);
    while uptime() < target {
        yield_cpu();
    }
}

#[derive(Debug)]
enum StreamError {
    Tcp(TcpError),
    Protocol,
    UnexpectedFrame(u8),
    Jit,
}

impl From<TcpError> for StreamError {
    fn from(e: TcpError) -> Self {
        StreamError::Tcp(e)
    }
}

fn run() -> Result<(), StreamError> {
    println!(
        "[DRAUG-STREAMER] connecting to {}.{}.{}.{}:{}",
        DAEMON_IP[0], DAEMON_IP[1], DAEMON_IP[2], DAEMON_IP[3], DAEMON_PORT
    );
    let mut sess = TcpSession::connect(DAEMON_IP, DAEMON_PORT)?;

    // ── Handshake: receive HELLO ───────────────────────────────────
    let hello = recv_hello(&mut sess)?;
    println!(
        "[DRAUG-STREAMER] HELLO received: mem_base=0x{:016x} mem_size={} helpers={}",
        hello.mem_base,
        hello.mem_size,
        hello.helpers.len()
    );
    for h in &hello.helpers {
        println!("[DRAUG-STREAMER]   helper {} = 0x{:016x}", h.name, h.addr);
    }

    // ── JIT the sensor-model once ──────────────────────────────────
    //
    // Program: read i32 from mem[0], multiply by 2. Same shape as
    // the smoke test's Case 3/4 — simple but exercises the full
    // toolchain (memory-mode prologue, I32Load, I32Const, I32Mul, End).
    // The result is returned in X0.
    let code = jit_sensor_model(hello.mem_base)?;
    println!(
        "[DRAUG-STREAMER] JIT produced {} bytes (+ {}-byte HMAC tag)",
        code.len(),
        auth::TAG_LEN,
    );
    send_code_signed(&mut sess, &code)?;

    // ── Live sensor stream — uptime_ms in an infinite loop ────────
    //
    // Each iteration reads the kernel's monotonic uptime counter
    // (libfolk::sys::uptime — backed by TSC via SYS_UPTIME),
    // truncates to i32, ships it via DATA + EXEC, and prints the
    // Pi-side RESULT. The JIT model is still "load × 2" so the
    // result should always be exactly 2× the sample, and the fact
    // that every cycle's sample is larger than the last is the
    // proof that we're looking at real-time data.
    //
    // Cadence control: `yield_cpu` between cycles hands the CPU
    // back to the scheduler so we don't hog a core. At ~60 Hz
    // scheduler tick, that puts the stream rate around the tick
    // frequency — fast enough to feel "live" on the serial log,
    // slow enough that the Pi daemon and relay/LAN aren't
    // saturated. Adjust by inserting more yields if needed.
    //
    // This is an **infinite** stream — the function never returns
    // Ok. Process termination only happens on a TCP error (peer
    // reset, daemon restart, kernel net stack reset) which bubbles
    // up as a StreamError and gets logged by main().
    println!("[DRAUG-STREAMER] streaming live uptime_ms (∞ — Ctrl+Alt+G to halt QEMU)");
    let mut cycle: u64 = 0;
    loop {
        // Truncating a u64 uptime to i32 gives wrap-around after
        // ~24.8 days (2^31 ms) which is well beyond any plausible
        // demo. For shorter demos the low 32 bits are monotonic and
        // visibly incrementing between cycles.
        let sample = uptime() as i32;
        let data = build_data_payload(0, &sample.to_le_bytes());
        send_frame(&mut sess, FRAME_DATA, &data)?;
        send_frame(&mut sess, FRAME_EXEC, &[])?;
        let rv = recv_result(&mut sess)?;
        let ok = rv == sample.wrapping_mul(2);
        println!(
            "[DRAUG-STREAMER]   t+{:>8} ms   sample={:>10}   result={:>10}   {}",
            sample as u32,
            sample,
            rv,
            if ok { "OK" } else { "MISMATCH" }
        );
        cycle = cycle.wrapping_add(1);
        // Occasional heartbeat so even if the print stream is busy
        // we know draug-streamer is alive.
        if cycle % 64 == 0 {
            println!("[DRAUG-STREAMER] heartbeat — {} cycles streamed", cycle);
        }
        yield_cpu();
    }
}

/// JIT the "load mem[0], multiply by 2, return" program. Takes
/// `mem_base` reported by the daemon so the prologue points X28 at
/// the daemon's linear-memory buffer.
fn jit_sensor_model(mem_base: u64) -> Result<Vec<u8>, StreamError> {
    let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), mem_base)
        .map_err(|_| StreamError::Jit)?;
    lw.lower_all(&[
        WasmOp::I32Const(0),
        WasmOp::I32Load(0),
        WasmOp::I32Const(2),
        WasmOp::I32Mul,
        WasmOp::End,
    ])
    .map_err(|_| StreamError::Jit)?;
    Ok(lw.finish())
}

// ── Framed I/O helpers ─────────────────────────────────────────────

fn send_frame(sess: &mut TcpSession, ty: u8, payload: &[u8]) -> Result<(), StreamError> {
    let frame = build_frame(ty, payload);
    sess.send_all(&frame)?;
    Ok(())
}

/// Send a CODE frame with its HMAC-SHA256 tag appended. The Pi-side
/// daemon refuses any CODE frame whose tag doesn't verify under the
/// shared secret, so every place that ships code must go through
/// this helper — keeps the "only signed code gets executed" invariant
/// local and obvious.
fn send_code_signed(sess: &mut TcpSession, code: &[u8]) -> Result<(), StreamError> {
    let tag = auth::sign(code);
    let mut payload: Vec<u8> = Vec::with_capacity(code.len() + auth::TAG_LEN);
    payload.extend_from_slice(code);
    payload.extend_from_slice(&tag);
    send_frame(sess, FRAME_CODE, &payload)
}

/// Read one complete frame: 5-byte header, then `length` bytes of
/// payload. Returns (type, payload).
fn recv_frame(sess: &mut TcpSession) -> Result<(u8, Vec<u8>), StreamError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    sess.recv_exact(&mut header)?;
    // Reuse peek_header via take_frame on a just-the-header view:
    // length needs to come out of the header. Parse inline.
    let ty = header[0];
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut payload = alloc::vec![0u8; len];
    if len > 0 {
        sess.recv_exact(&mut payload)?;
    }
    Ok((ty, payload))
}

fn recv_hello(sess: &mut TcpSession) -> Result<Hello, StreamError> {
    let (ty, payload) = recv_frame(sess)?;
    if ty != FRAME_HELLO {
        return Err(StreamError::UnexpectedFrame(ty));
    }
    // `take_frame` is for buffered parsing; for a direct
    // recv_frame that already split header+payload we parse the
    // payload directly.
    let _ = take_frame; // silence unused-import-if-only-in-cfg scare
    parse_hello(&payload).map_err(|_| StreamError::Protocol)
}

fn recv_result(sess: &mut TcpSession) -> Result<i32, StreamError> {
    let (ty, payload) = recv_frame(sess)?;
    if ty != FRAME_RESULT {
        return Err(StreamError::UnexpectedFrame(ty));
    }
    parse_result(&payload).map_err(|_| StreamError::Protocol)
}
