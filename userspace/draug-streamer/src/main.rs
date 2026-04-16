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
use libfolk::sys::{get_pid, yield_cpu};

use a64_encoder::{Lowerer, WasmOp};

mod protocol;
mod tcp;

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

/// Pi-side `a64-stream-daemon` address. Smoltcp installs a default
/// IPv4 route from the DHCP offer (gateway 10.0.2.2), so off-subnet
/// destinations should NAT out through SLIRP automatically. If that
/// proves unreliable, fall back to the host relay at `10.0.2.2:14712`
/// (see `tools/a64-streamer/src/bin/relay.rs`).
const DAEMON_IP: [u8; 4] = [192, 168, 68, 72];
const DAEMON_PORT: u16 = 14712;

entry!(main);

fn main() -> ! {
    println!("[DRAUG-STREAMER] === ENTRY === (PID {})", get_pid());
    match run() {
        Ok(()) => println!("[DRAUG-STREAMER] stream complete — idle."),
        Err(e) => println!("[DRAUG-STREAMER] fatal: {:?}", e),
    }
    loop {
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
    println!("[DRAUG-STREAMER] JIT produced {} bytes", code.len());
    send_frame(&mut sess, FRAME_CODE, &code)?;

    // ── Stream 5 samples through DATA + EXEC ──────────────────────
    for sample in &[1i32, 2, 3, 7, 21] {
        let data = build_data_payload(0, &sample.to_le_bytes());
        send_frame(&mut sess, FRAME_DATA, &data)?;
        send_frame(&mut sess, FRAME_EXEC, &[])?;
        let rv = recv_result(&mut sess)?;
        println!(
            "[DRAUG-STREAMER]   sample {:>3} → result {:>3} (expected {})",
            sample,
            rv,
            sample.wrapping_mul(2)
        );
    }

    // ── BYE — tell daemon we're done ──────────────────────────────
    send_frame(&mut sess, FRAME_BYE, &[])?;
    Ok(())
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
