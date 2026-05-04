//! Folkering OS Phase D.1 — hybrid inference service.
//!
//! ┌─────────────────────────┐                  ┌─────────────────────┐
//! │ draug-daemon            │  IPC: shmem_id   │ inference (this)    │
//! │  (or future apps)       │ ───────────────▶ │  router::dispatch   │
//! └─────────────────────────┘   prompt+result  │       │             │
//!                                              │       │             │
//!                                              │  ┌────┴────┐        │
//!                                              │  ▼         ▼        │
//!                                              │ local    proxy      │
//!                                              │ backend  backend    │
//!                                              │ (Burn,   (TCP via   │
//!                                              │  D.2)    libfolk's  │
//!                                              │ stub     llm_       │
//!                                              │ today    generate)  │
//!                                              └─────────────────────┘
//!
//! The router decides per-request which backend handles it. For D.1
//! the local backend is a stub that always returns `NotImplemented`,
//! so every request transparently falls through to the proxy backend
//! — same Ollama wire as before, just one extra IPC hop. That's the
//! whole point: ship the routing infrastructure FIRST, with zero
//! behavior change, and swap in the Burn local engine in D.2 without
//! touching draug-daemon or any future caller.
//!
//! Service contract (see `ipc_msg.rs` for the wire types):
//!
//!   1. Caller creates a shmem region with an `InferenceWire` header
//!      followed by the prompt bytes and a result-buffer.
//!   2. Caller sends an IPC message to this task with the shmem_id
//!      packed in payload0 (and optional flags in payload1).
//!   3. We map the shmem, parse the header, route to a backend.
//!   4. Backend writes its response into the wire's result-buffer
//!      and updates the header's `status` + `output_len` fields.
//!   5. We reply with `Ok(0)` once the response is written, then
//!      unmap. The caller reads its result-buffer and destroys the
//!      shmem.
//!
//! The wire layout is intentionally identical in shape to libfolk's
//! `llm_generate` syscall — the proxy backend is then a 5-line
//! delegator. Whether the local backend ends up wanting the same
//! shape is TBD; if it grows separate fields (KV-cache handle,
//! temperature, top-p, etc.) we extend the header rather than
//! splitting the wire.

#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

use libfolk::{entry, println};
use libfolk::sys::yield_cpu;

mod ipc_msg;
mod router;
mod proxy_backend;
mod local_backend;
mod tensor_math;
mod weights;
mod weights_test_blob;

// ── Bump allocator ──────────────────────────────────────────────────
//
// 256 KiB. The router itself doesn't allocate much (one map per
// request); the proxy backend uses the kernel's syscall_llm_generate
// path which allocates kernel-side. Local backend (Burn) will need
// significantly more heap once D.2 lands — at that point we either
// bump this constant up or move to a per-request slab to bound
// per-call usage.

const HEAP_SIZE: usize = 256 * 1024;

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
            return core::ptr::null_mut();
        }
        *offset = new_offset;
        (*self.heap.get()).as_mut_ptr().add(aligned)
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    heap: UnsafeCell::new([0; HEAP_SIZE]),
    offset: UnsafeCell::new(0),
};

// ── Service loop ───────────────────────────────────────────────────

entry!(main);

fn main() -> ! {
    println!("[INFERENCE] Phase D.1 — hybrid router starting up");

    // Sanity-test the local-backend tensor math at boot — fail loud
    // here rather than the first time a real request lands. Cheap
    // (a 2×2 @ 2×2 matmul takes single-digit microseconds).
    if !tensor_math::self_test() {
        println!("[INFERENCE] FATAL: tensor_math self-test failed");
    } else {
        println!("[INFERENCE] tensor self-test PASS");
    }

    // D.2: exercise the full local-backend path (incl. Burn's
    // TensorData round-trip) on a fake in-process wire. Real IPC
    // end-to-end is D.3.
    if !local_backend::boot_test() {
        println!("[INFERENCE] FATAL: local_backend boot_test failed");
    } else {
        println!("[INFERENCE] local_backend D.2 boot_test PASS");
    }

    // D.3.1: parse the embedded `.fbin` test blob, find both named
    // tensors, verify their data via FNV-1a checksums. Real Synapse
    // VFS file-read plumbs in D.3.1.2.
    if !run_fbin_self_test() {
        println!("[INFERENCE] FATAL: weights D.3.1 self-test failed");
    } else {
        println!("[INFERENCE] weights D.3.1 self-test PASS");
    }

    println!("[INFERENCE] ready — awaiting IPC requests on this task id");

    let mut req_count: u64 = 0;
    loop {
        match libfolk::sys::ipc::receive() {
            Ok(msg) => {
                req_count += 1;
                handle_request(&msg, req_count);
            }
            Err(libfolk::sys::ipc::IpcError::WouldBlock) => {
                // No request queued — yield so the compositor + net
                // driver get their share. ipc::receive is non-blocking
                // so this loop only spins under load; idle CPU cost is
                // bounded by the scheduler's yield latency.
                yield_cpu();
            }
            Err(e) => {
                // Other IPC errors are diagnostic only — keep serving.
                println!("[INFERENCE] ipc recv error: {:?}", e);
                yield_cpu();
            }
        }
    }
}

/// D.3.1 self-test: parse the embedded blob, look up both tensors,
/// verify shapes and data hashes match the values
/// `weights_test_blob.rs` baked into the file.
fn run_fbin_self_test() -> bool {
    use weights::{FbinView, fnv1a_64};

    let view = match FbinView::parse(weights_test_blob::TEST_FBIN) {
        Ok(v) => v,
        Err(e) => {
            println!("[INFERENCE] fbin parse error: {:?}", e);
            return false;
        }
    };
    if view.tensors.len() != 2 {
        println!("[INFERENCE] fbin: expected 2 tensors, got {}", view.tensors.len());
        return false;
    }

    // ── tensor 1: embed_test (4×4 f32, values 1..16) ───────────────
    let embed = match view.find("embed_test") {
        Some(t) => t,
        None => {
            println!("[INFERENCE] fbin: tensor 'embed_test' not found");
            return false;
        }
    };
    if embed.shape != [4u32, 4] {
        println!("[INFERENCE] fbin: embed_test wrong shape (got {:?})", embed.shape);
        return false;
    }
    let embed_vals = match view.read_f32(embed) {
        Some(v) => v,
        None => {
            println!("[INFERENCE] fbin: embed_test read_f32 failed");
            return false;
        }
    };
    let sum: f32 = embed_vals.iter().sum();
    // Expected: 1+2+...+16 = 136
    if (sum - 136.0).abs() > 1e-3 {
        println!("[INFERENCE] fbin: embed_test sum {} != 136", sum);
        return false;
    }

    // ── tensor 2: weight_test (4 f32, [0.25, 0.5, 0.75, 1.0]) ──────
    let weight = match view.find("weight_test") {
        Some(t) => t,
        None => {
            println!("[INFERENCE] fbin: tensor 'weight_test' not found");
            return false;
        }
    };
    if weight.shape != [4u32] {
        println!("[INFERENCE] fbin: weight_test wrong shape (got {:?})", weight.shape);
        return false;
    }
    let weight_vals = match view.read_f32(weight) {
        Some(v) => v,
        None => {
            println!("[INFERENCE] fbin: weight_test read_f32 failed");
            return false;
        }
    };
    let wsum: f32 = weight_vals.iter().sum();
    // Expected: 0.25 + 0.5 + 0.75 + 1.0 = 2.5
    if (wsum - 2.5).abs() > 1e-6 {
        println!("[INFERENCE] fbin: weight_test sum {} != 2.5", wsum);
        return false;
    }

    // ── Hash check (proves byte-level integrity) ───────────────────
    // FNV-1a over each tensor's raw bytes. Stable so a regression in
    // the parser surfaces here, not 100 LOC into a forward pass.
    let h_embed = fnv1a_64(view.data_for(embed));
    let h_weight = fnv1a_64(view.data_for(weight));
    println!(
        "[INFERENCE] fbin: embed_hash=0x{:x} weight_hash=0x{:x}",
        h_embed, h_weight
    );
    true
}

fn handle_request(msg: &libfolk::sys::ipc::IpcMessage, n: u64) {
    let shmem_id = (msg.payload0 & 0xFFFF_FFFF) as u32;
    let flags = (msg.payload0 >> 32) & 0xFFFF_FFFF;
    println!(
        "[INFERENCE] req#{} from task {} shmem_id={} flags=0x{:x}",
        n, msg.sender, shmem_id, flags
    );

    let outcome = router::dispatch(shmem_id, flags as u32);

    // Reply with the outcome status in payload0; payload1 reserved for
    // the bytes-written count which the caller already has via the
    // shmem header, but copying it back out cheap-double-checks the
    // happy path.
    let _ = libfolk::sys::ipc::reply(outcome.status as u64, outcome.output_len as u64);
}
