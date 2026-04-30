//! draug-daemon — Folkering OS autonomous agent (binary entry point).
//!
//! Phase A skeleton (2026-05-01). The daemon boots, prints its task
//! id, and handles the wire protocol defined in `libfolk::sys::draug`
//! (PING, USER_INPUT, WASM_CRASH, INSTALL_REFACTOR_TASKS). Real Draug
//! logic still lives inside the compositor process — A.4 is where the
//! 7000 lines move over.
//!
//! Layout mirrors `synapse-service/main.rs`: heap allocator + minimal
//! daemon state + boot sequence + IPC dispatch loop.

#![no_std]
#![no_main]

extern crate alloc;

// ── Heap allocator ─────────────────────────────────────────────────────

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

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

// ── Imports ────────────────────────────────────────────────────────────

use libfolk::{entry, println};
use libfolk::sys::{yield_cpu, get_pid};
use libfolk::sys::ipc::{recv_async, reply_with_token};
use libfolk::sys::draug::{
    unpack_op, unpack_data48, unpack_shmem_size,
    DRAUG_OP_PING, DRAUG_OP_USER_INPUT, DRAUG_OP_WASM_CRASH,
    DRAUG_OP_INSTALL_REFACTOR_TASKS,
    DRAUG_STATUS_OK, DRAUG_STATUS_ERR, DRAUG_VERSION,
};

entry!(main);

// ── Minimal daemon state ───────────────────────────────────────────────
//
// Phase A.1+A.2: just enough to acknowledge wire-protocol commands.
// The real DraugDaemon struct (1844 LoC) moves over in A.4. Until
// then, this state is a smoke-test signal — the counters prove the
// IPC round-trip actually delivers data, nothing more.

struct DaemonState {
    /// Last user-input timestamp recorded over IPC. Will become the
    /// authoritative source of `last_input_ms()` once compositor
    /// stops owning it.
    last_input_ms: u64,
    /// Number of WASM crashes received over IPC since boot.
    crash_count: u32,
    /// Number of refactor-task install attempts received.
    install_count: u32,
}

impl DaemonState {
    const fn new() -> Self {
        Self { last_input_ms: 0, crash_count: 0, install_count: 0 }
    }
}

static mut STATE: DaemonState = DaemonState::new();

fn main() -> ! {
    let pid = get_pid();
    println!("[DRAUG-DAEMON] starting (PID: {})", pid);
    println!("[DRAUG-DAEMON] Phase A skeleton — protocol v{}.{}",
             (DRAUG_VERSION >> 16) as u16,
             (DRAUG_VERSION & 0xFFFF) as u16);

    loop {
        match recv_async() {
            Ok(msg) => {
                let reply = handle_command(msg.payload0);
                let _ = reply_with_token(msg.token, reply, 0);
            }
            Err(_) => {
                yield_cpu();
            }
        }
    }
}

/// Dispatch a single IPC command. Returns the value to put in the
/// reply's payload0. Errors stay in-process — never panic out of the
/// service loop, since that would defeat the whole point of moving
/// Draug to its own task.
fn handle_command(payload0: u64) -> u64 {
    match unpack_op(payload0) {
        DRAUG_OP_PING => DRAUG_VERSION,

        DRAUG_OP_USER_INPUT => {
            let ms = unpack_data48(payload0);
            unsafe { STATE.last_input_ms = ms; }
            DRAUG_STATUS_OK
        }

        DRAUG_OP_WASM_CRASH => {
            // Hash captured for future use; for now just count.
            let _key_hash = unpack_data48(payload0);
            unsafe {
                STATE.crash_count = STATE.crash_count.saturating_add(1);
            }
            DRAUG_STATUS_OK
        }

        DRAUG_OP_INSTALL_REFACTOR_TASKS => {
            // A.4 implements actual task installation (map shmem
            // handle, parse serialised list, install into the
            // DraugDaemon struct's task table). For now the skeleton
            // logs the call and replies OK so boot doesn't fail.
            let (handle, size) = unpack_shmem_size(payload0);
            unsafe {
                STATE.install_count = STATE.install_count.saturating_add(1);
            }
            println!("[DRAUG-DAEMON] INSTALL_REFACTOR_TASKS handle={} size={} (skeleton stub)",
                     handle, size);
            DRAUG_STATUS_OK
        }

        _ => DRAUG_STATUS_ERR,
    }
}
