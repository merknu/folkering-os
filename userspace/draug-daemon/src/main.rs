//! draug-daemon — Folkering OS autonomous agent (binary entry point).
//!
//! Phase A skeleton (2026-05-01). The daemon boots, allocates the
//! shared-memory status region (so compositor can read live counters
//! without IPC overhead), and runs an IPC service loop that handles
//! the wire protocol defined in `libfolk::sys::draug`. Real Draug
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
use core::sync::atomic::Ordering;

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
use libfolk::sys::{yield_cpu, get_pid, shmem_create, shmem_map, shmem_grant};
use libfolk::sys::compositor::COMPOSITOR_TASK_ID;
use libfolk::sys::ipc::{recv_async, reply_with_token};
use libfolk::sys::draug::{
    unpack_op, unpack_data48, unpack_shmem_size,
    DRAUG_OP_PING, DRAUG_OP_USER_INPUT, DRAUG_OP_WASM_CRASH,
    DRAUG_OP_INSTALL_REFACTOR_TASKS, DRAUG_OP_GET_STATUS_HANDLE,
    DRAUG_STATUS_OK, DRAUG_STATUS_ERR, DRAUG_VERSION,
    DRAUG_STATUS_LAYOUT_VERSION, DRAUG_STATUS_SHMEM_SIZE,
    DRAUG_FLAG_INITIALISED,
    DraugStatus,
};

entry!(main);

// ── Status shmem region ────────────────────────────────────────────────
//
// We map the daemon's view of the status region at this vaddr. It
// must NOT collide with the bump heap above (which lives in the
// daemon's BSS) or with anything libfolk maps. 0x40000000 is well
// above any heap we plausibly allocate; matches the pattern used by
// other Folkering daemons that map shmem above 0x30000000.

const DRAUG_STATUS_DAEMON_VADDR: usize = 0x40000000;

/// Handle of the daemon's status shmem region. Set during `boot()`,
/// returned to compositor on `DRAUG_OP_GET_STATUS_HANDLE`.
static mut STATUS_HANDLE: u32 = 0;

/// Pointer to the mapped status struct (daemon-side, writable). All
/// updates go through atomic stores on this struct's fields; the
/// compositor maps the same shmem read-only at a different vaddr and
/// sees the writes via cache coherency.
static mut STATUS_PTR: *mut DraugStatus = core::ptr::null_mut();

// ── Minimal daemon state ───────────────────────────────────────────────
//
// Phase A.1+A.2+A.3: just enough to acknowledge wire-protocol commands
// and own the status region. The real DraugDaemon struct (1844 LoC)
// moves over in A.4. Until then, the counters in `DaemonState` are a
// smoke-test signal — they prove the IPC round-trip actually delivers
// data. Once A.4 lands, these mirror into the shmem fields above.

struct DaemonState {
    last_input_ms: u64,
    crash_count: u32,
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

    if !boot_status_shmem() {
        // We can still serve the other commands without the status
        // region, but compositor's HUD will degrade to "Draug status
        // unavailable". Logged so it's visible during debugging.
        println!("[DRAUG-DAEMON] WARNING: status shmem boot failed — IPC fallback only");
    }

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

/// Allocate, map, initialise, and grant the compositor read access
/// to the status shmem region. Returns `true` on success. On failure
/// `STATUS_HANDLE` stays 0 so `DRAUG_OP_GET_STATUS_HANDLE` returns
/// `DRAUG_STATUS_ERR`.
fn boot_status_shmem() -> bool {
    // 1. Allocate.
    let handle = match shmem_create(DRAUG_STATUS_SHMEM_SIZE) {
        Ok(h) => h,
        Err(_) => {
            println!("[DRAUG-DAEMON] shmem_create({}) failed", DRAUG_STATUS_SHMEM_SIZE);
            return false;
        }
    };

    // 2. Map locally so we can write to it.
    if shmem_map(handle, DRAUG_STATUS_DAEMON_VADDR).is_err() {
        println!("[DRAUG-DAEMON] shmem_map daemon-side failed");
        return false;
    }

    // 3. Initialise the struct in place. shmem_create returns a
    // page-zeroed region, so all atomic counters already start at 0;
    // we only need to set the layout version and INITIALISED flag so
    // compositor's `attach_status()` accepts the region.
    let ptr = DRAUG_STATUS_DAEMON_VADDR as *mut DraugStatus;
    unsafe {
        // Cannot call DraugStatus::zeroed() because that would
        // overwrite the kernel-zeroed atomics with their default-
        // construct values, which is the same thing but goes through
        // user-space init. Skip the redundant write and just set the
        // version + flags directly.
        let status = &*ptr;
        status.layout_version.store(DRAUG_STATUS_LAYOUT_VERSION, Ordering::Release);
        status.flags.store(DRAUG_FLAG_INITIALISED, Ordering::Release);

        STATUS_PTR = ptr;
        STATUS_HANDLE = handle;
    }

    // 4. Grant compositor (task 4) read access. Compositor will call
    // shmem_map at its own vaddr in `attach_status()`.
    if shmem_grant(handle, COMPOSITOR_TASK_ID).is_err() {
        println!("[DRAUG-DAEMON] shmem_grant compositor failed");
        // Mapping survives, but compositor can't reach it. Leave the
        // handle 0 to make this state observable to clients.
        unsafe { STATUS_HANDLE = 0; }
        return false;
    }

    println!("[DRAUG-DAEMON] status shmem ready (handle={}, size={})",
             handle, DRAUG_STATUS_SHMEM_SIZE);
    true
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
            unsafe {
                STATE.last_input_ms = ms;
                if !STATUS_PTR.is_null() {
                    (*STATUS_PTR).last_input_ms.store(ms, Ordering::Release);
                }
            }
            DRAUG_STATUS_OK
        }

        DRAUG_OP_WASM_CRASH => {
            let _key_hash = unpack_data48(payload0);
            unsafe {
                STATE.crash_count = STATE.crash_count.saturating_add(1);
                if !STATUS_PTR.is_null() {
                    (*STATUS_PTR).crash_count.store(STATE.crash_count, Ordering::Release);
                }
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

        DRAUG_OP_GET_STATUS_HANDLE => {
            let h = unsafe { STATUS_HANDLE };
            if h == 0 {
                DRAUG_STATUS_ERR
            } else {
                h as u64
            }
        }

        _ => DRAUG_STATUS_ERR,
    }
}
