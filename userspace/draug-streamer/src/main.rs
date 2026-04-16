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
//! spec. This binary duplicates the *pure* parts of that module
//! (frame constants + serialize/parse helpers) since the upstream
//! lib's framed-I/O wrappers depend on `std::io::{Read, Write}`.

#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

use libfolk::{entry, println};
use libfolk::sys::{get_pid, yield_cpu};

// ── Bump allocator ──────────────────────────────────────────────────
//
// 64 KiB heap in BSS. Enough for:
//   * a small `Vec<u8>` code buffer from the JIT (≤ a few hundred B)
//   * a `Vec<u8>` receive buffer for the HELLO payload (≤ 1 KiB)
//   * the protocol scratch vectors
// No-op `dealloc` — matches the synapse-service / inference-server
// pattern, since this is a short-running streaming client that
// doesn't churn allocations. If the heap fills up the allocator
// returns null and the next `alloc`-using call panics into
// libfolk's panic handler (which logs and calls `task::exit(1)`).

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

entry!(main);

fn main() -> ! {
    println!("[DRAUG-STREAMER] === ENTRY === (PID {})", get_pid());
    println!("[DRAUG-STREAMER] scaffold up — no work yet; Fase B/2b next.");

    // Idle until the next sprint wires in protocol + TCP loop.
    loop {
        yield_cpu();
    }
}
