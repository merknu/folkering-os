//! draug-daemon — Folkering OS autonomous agent (binary entry point).
//!
//! Phase A.5 (2026-05-01) flips this binary from "passive container"
//! to "actively driving the Draug tick loop". Compositor still keeps
//! a local `DraugDaemon` for transitional UI reads (subsequent A.5
//! steps move those to the status shmem and drop the local instance);
//! both ticks until then. The daemon is now the authoritative driver
//! of agent work — knowledge hunts, refactor iterations, analysis
//! cycles all run in this address space, so a panic in agent code can
//! no longer take down the desktop.
//!
//! Layout mirrors `synapse-service/main.rs`: heap allocator + state
//! declaration + boot sequence + IPC dispatch loop.

#![no_std]
#![no_main]

extern crate alloc;

// ── Heap allocator ─────────────────────────────────────────────────────

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::Ordering;

const HEAP_SIZE: usize = 2 * 1024 * 1024;

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
use libfolk::sys::{yield_cpu, get_pid, shmem_create, shmem_map, shmem_grant, uptime};
use libfolk::sys::compositor::COMPOSITOR_TASK_ID;
use libfolk::sys::ipc::{recv_async, reply_with_token};
use libfolk::sys::draug::{
    unpack_op, unpack_data48, unpack_shmem_size,
    DRAUG_OP_PING, DRAUG_OP_USER_INPUT, DRAUG_OP_WASM_CRASH,
    DRAUG_OP_INSTALL_REFACTOR_TASKS, DRAUG_OP_GET_STATUS_HANDLE,
    DRAUG_STATUS_OK, DRAUG_STATUS_ERR, DRAUG_VERSION,
    DRAUG_STATUS_LAYOUT_VERSION, DRAUG_STATUS_SHMEM_SIZE,
    DRAUG_FLAG_INITIALISED, DRAUG_FLAG_PLAN_MODE_ACTIVE,
    DRAUG_FLAG_REFACTOR_HIBERNATING,
    DraugStatus,
};

use draug_daemon::draug::{DraugDaemon, AsyncPhase};
use draug_daemon::draug_async;
use draug_daemon::knowledge_hunt;

entry!(main);

// ── Status shmem region ────────────────────────────────────────────────
//
// Mapped at this vaddr inside the daemon (well above the bump heap).

const DRAUG_STATUS_DAEMON_VADDR: usize = 0x40000000;

static mut STATUS_HANDLE: u32 = 0;
static mut STATUS_PTR: *mut DraugStatus = core::ptr::null_mut();

fn main() -> ! {
    let pid = get_pid();
    println!("[DRAUG-DAEMON] starting (PID: {})", pid);
    println!("[DRAUG-DAEMON] protocol v{}.{}",
             (DRAUG_VERSION >> 16) as u16,
             (DRAUG_VERSION & 0xFFFF) as u16);

    if !boot_status_shmem() {
        println!("[DRAUG-DAEMON] WARNING: status shmem boot failed — IPC fallback only");
    }

    // ── Instantiate Draug ──────────────────────────────────────────────
    let mut draug = DraugDaemon::new();
    if draug.restore_state() {
        println!("[DRAUG-DAEMON] restored state: iter={}, complex_idx={}",
                 draug.refactor_iter, draug.complex_task_idx);
    } else {
        println!("[DRAUG-DAEMON] cold-boot — fresh DraugDaemon instance");
    }

    // ── Service loop ───────────────────────────────────────────────────
    //
    // Each iteration:
    //   1. Drain any pending IPC commands (USER_INPUT pulses etc.)
    //   2. Run Draug-side ticks (telemetry, analysis, refactor steps)
    //   3. Mirror state into the status shmem
    //   4. Yield the CPU
    //
    // The compositor's local DraugDaemon still ticks too during the
    // A.5 transition window, so until the next A.5 step lands the
    // status shmem may report values DIFFERENT from compositor's HUD.
    // That's expected and harmless.

    loop {
        // 1. IPC drain — handle every queued message before ticking.
        loop {
            match recv_async() {
                Ok(msg) => {
                    let reply = handle_command(msg.payload0, &mut draug);
                    let _ = reply_with_token(msg.token, reply, 0);
                }
                Err(_) => break, // no more messages this round
            }
        }

        // 2. Draug ticks.
        let now_ms = uptime();
        run_draug_tick(&mut draug, now_ms);

        // 3. Mirror state to status shmem so compositor reads see fresh
        //    counters. Cheap (a handful of relaxed atomic stores).
        publish_status(&draug);

        // 4. Yield. The kernel scheduler will run other tasks; we get
        //    re-scheduled when the timer ISR sees us as runnable.
        yield_cpu();
    }
}

/// Allocate, map, initialise, and grant compositor read access to
/// the status shmem region. Returns `true` on success.
fn boot_status_shmem() -> bool {
    let handle = match shmem_create(DRAUG_STATUS_SHMEM_SIZE) {
        Ok(h) => h,
        Err(_) => {
            println!("[DRAUG-DAEMON] shmem_create({}) failed", DRAUG_STATUS_SHMEM_SIZE);
            return false;
        }
    };

    if shmem_map(handle, DRAUG_STATUS_DAEMON_VADDR).is_err() {
        println!("[DRAUG-DAEMON] shmem_map daemon-side failed");
        return false;
    }

    let ptr = DRAUG_STATUS_DAEMON_VADDR as *mut DraugStatus;
    unsafe {
        let status = &*ptr;
        status.layout_version.store(DRAUG_STATUS_LAYOUT_VERSION, Ordering::Release);
        status.flags.store(DRAUG_FLAG_INITIALISED, Ordering::Release);
        STATUS_PTR = ptr;
        STATUS_HANDLE = handle;
    }

    if shmem_grant(handle, COMPOSITOR_TASK_ID).is_err() {
        println!("[DRAUG-DAEMON] shmem_grant compositor failed");
        unsafe { STATUS_HANDLE = 0; }
        return false;
    }

    println!("[DRAUG-DAEMON] status shmem ready (handle={}, size={})",
             handle, DRAUG_STATUS_SHMEM_SIZE);
    true
}

/// Run one round of Draug-side ticks.
///
/// Ports the Draug-relevant subset of `compositor::mcp_handler::agent_logic::tick`
/// over to the daemon. The bits NOT ported are the ones that touched
/// compositor-only state (active_agent, mcp.async_tool_gen, WASM driver
/// runtime); those stay in the compositor process. The "is the user busy
/// with another agent right now?" gate is gone for the moment — its
/// successor will be a `DRAUG_FLAG_COMPOSITOR_BUSY` bit set by compositor
/// in the status shmem (later A.5 step).
fn run_draug_tick(draug: &mut DraugDaemon, now_ms: u64) {
    // Telemetry tick.
    if draug.should_tick(now_ms) {
        draug.tick(now_ms);
    }

    // Analysis cycle (LLM call). Currently ungated — see comment above.
    if draug.should_analyze(now_ms) {
        let _ = draug.start_analysis(now_ms);
    }

    // Wait-for-LLM timeout housekeeping.
    let _ = draug.check_waiting_timeout(now_ms);

    // Pattern mining — periodic insight extraction from telemetry.
    if draug.should_mine_patterns(now_ms)
        && !draug.should_yield_tokens(false, now_ms)
    {
        let _ = draug.mine_patterns(now_ms);
    }

    // Phase 7 — Knowledge Hunt. One-shot Wikipedia/source fetch.
    if draug.should_hunt_knowledge(now_ms) {
        knowledge_hunt::run(draug);
    }

    // Phase 13 — Async refactor loop. Either we're already mid-flight
    // (poll the EAGAIN state machine) or we should kick off a new
    // iteration.
    if draug.async_phase != AsyncPhase::Idle {
        let _ = draug_async::tick_async(draug, now_ms);
    } else if draug.should_run_refactor_step(now_ms) {
        let _ = draug_async::tick_async(draug, now_ms);
    }
}

/// Mirror the current `DraugDaemon` state into the status shmem so
/// compositor reads see fresh counters. Cheap: a handful of relaxed
/// atomic stores. Called from the bottom of every service-loop
/// iteration.
fn publish_status(draug: &DraugDaemon) {
    let ptr = unsafe { STATUS_PTR };
    if ptr.is_null() {
        return;
    }
    let status = unsafe { &*ptr };

    // Counters.
    status.refactor_iter.store(draug.refactor_iter, Ordering::Relaxed);
    status.refactor_passed.store(draug.refactor_passed, Ordering::Relaxed);
    status.refactor_failed.store(draug.refactor_failed, Ordering::Relaxed);
    status.refactor_retries.store(draug.refactor_retries, Ordering::Relaxed);
    status.complex_task_idx.store(draug.complex_task_idx as u32, Ordering::Relaxed);
    status.consecutive_skips.store(draug.consecutive_skips, Ordering::Relaxed);
    status.last_input_ms.store(draug.last_input_ms(), Ordering::Relaxed);

    // Skill-tree counts.
    status.tasks_at_l1.store(draug.tasks_at_level(1) as u32, Ordering::Relaxed);
    status.tasks_at_l2.store(draug.tasks_at_level(2) as u32, Ordering::Relaxed);
    status.tasks_at_l3.store(draug.tasks_at_level(3) as u32, Ordering::Relaxed);

    // Per-task levels.
    for (i, level) in draug.task_levels.iter().enumerate().take(20) {
        status.task_levels[i].store(*level, Ordering::Relaxed);
    }

    // Flags. Daemon owns the lower bits; compositor (later A.5 step)
    // will own bit 16 = COMPOSITOR_BUSY.
    let mut flags = DRAUG_FLAG_INITIALISED;
    if draug.plan_mode_active        { flags |= DRAUG_FLAG_PLAN_MODE_ACTIVE; }
    if draug.refactor_hibernating    { flags |= DRAUG_FLAG_REFACTOR_HIBERNATING; }
    // Preserve any compositor-owned upper bits via fetch_update equivalents.
    // For now compositor doesn't write here, so a plain store is fine;
    // when the busy flag lands we'll switch to fetch_or / fetch_and.
    status.flags.store(flags, Ordering::Release);
}

/// Dispatch a single IPC command. Returns the value to put in the
/// reply's payload0. Errors stay in-process — never panic out of the
/// service loop, since that would defeat the whole point of moving
/// Draug to its own task.
fn handle_command(payload0: u64, draug: &mut DraugDaemon) -> u64 {
    match unpack_op(payload0) {
        DRAUG_OP_PING => DRAUG_VERSION,

        DRAUG_OP_USER_INPUT => {
            let ms = unpack_data48(payload0);
            draug.on_user_input(ms);
            DRAUG_STATUS_OK
        }

        DRAUG_OP_WASM_CRASH => {
            // 48-bit hash on the wire; friction sensor's table is
            // u32-keyed so we fold the high 16 bits into the low half.
            // Same fold the compositor's existing `key_hash_pub`
            // helper would produce on a fresh string, so as long as
            // both sides agree on the fold we keep collision behaviour
            // unchanged.
            let h48 = unpack_data48(payload0);
            let key_hash = (h48 as u32) ^ ((h48 >> 32) as u32);
            draug.friction.record_signal(
                key_hash,
                draug_daemon::draug::FRICTION_QUICK_CLOSE,
            );
            DRAUG_STATUS_OK
        }

        DRAUG_OP_INSTALL_REFACTOR_TASKS => {
            let (handle, size) = unpack_shmem_size(payload0);
            println!("[DRAUG-DAEMON] INSTALL_REFACTOR_TASKS handle={} size={} (skeleton stub)",
                     handle, size);
            DRAUG_STATUS_OK
        }

        DRAUG_OP_GET_STATUS_HANDLE => {
            let h = unsafe { STATUS_HANDLE };
            if h == 0 { DRAUG_STATUS_ERR } else { h as u64 }
        }

        _ => DRAUG_STATUS_ERR,
    }
}
