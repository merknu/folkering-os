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
use libfolk::sys::{yield_cpu, get_pid, shmem_create, shmem_map, shmem_unmap, shmem_grant, uptime};
use libfolk::sys::compositor::COMPOSITOR_TASK_ID;
use libfolk::sys::ipc::{recv_async, reply_with_token};
use libfolk::sys::draug::{
    unpack_op, unpack_data48, unpack_shmem_size, unpack_friction,
    unpack_dream_decide, pack_dream_decision, unpack_dream_result_status,
    DRAUG_OP_PING, DRAUG_OP_USER_INPUT, DRAUG_OP_WASM_CRASH,
    DRAUG_OP_INSTALL_REFACTOR_TASKS, DRAUG_OP_GET_STATUS_HANDLE,
    DRAUG_OP_FRICTION_SIGNAL, DRAUG_OP_DREAM_DECIDE, DRAUG_OP_DREAM_RESULT,
    DREAM_ACTION_SKIP, DREAM_ACTION_DREAM,
    DREAM_MODE_REFACTOR, DREAM_MODE_CREATIVE, DREAM_MODE_NIGHTMARE,
    DREAM_MODE_DRIVER_REFACTOR, DREAM_MODE_DRIVER_NIGHTMARE,
    DREAM_RESULT_COMPLETE, DREAM_RESULT_CANCEL,
    DRAUG_STATUS_OK, DRAUG_STATUS_ERR, DRAUG_VERSION,
    DRAUG_STATUS_LAYOUT_VERSION, DRAUG_STATUS_SHMEM_SIZE,
    DRAUG_FLAG_INITIALISED, DRAUG_FLAG_PLAN_MODE_ACTIVE,
    DRAUG_FLAG_REFACTOR_HIBERNATING, DRAUG_FLAG_WAITING_FOR_LLM,
    DRAUG_FLAG_DREAM_READY, DRAUG_FLAG_SKILL_TREE_HAS_WORK,
    DraugStatus,
};

use draug_daemon::draug::{DraugDaemon, AsyncPhase, DreamMode};
use draug_daemon::draug_async;
use draug_daemon::knowledge_hunt;

entry!(main);

// ── Status shmem region ────────────────────────────────────────────────
//
// Mapped at this vaddr inside the daemon (well above the bump heap).

const DRAUG_STATUS_DAEMON_VADDR: usize = 0x40000000;

/// Daemon-side scratch vaddr for `DREAM_DECIDE` shmem mappings.
/// Compositor allocates a fresh shmem per request, hands it over, we
/// map here, parse, then unmap. One slot is enough — `DREAM_DECIDE`
/// is dispatched serially from `handle_command` so there's no overlap.
const DREAM_DECIDE_DAEMON_VADDR: usize = 0x41000000;

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

    // Phase A.5 (Path A): analysis cycle now runs in the daemon
    // over direct TCP via `start_analysis_via_tcp`. Replaces the
    // MCP/COM2 path that used to live in compositor. We only kick
    // off a new cycle when the async pipeline is idle — refactor /
    // knowledge-hunt work takes priority.
    if draug.async_phase == AsyncPhase::Idle && draug.should_analyze(now_ms) {
        let _ = draug_async::start_analysis_via_tcp(draug, now_ms);
    }

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
    let now_ms = uptime();
    let mut flags = DRAUG_FLAG_INITIALISED;
    if draug.plan_mode_active     { flags |= DRAUG_FLAG_PLAN_MODE_ACTIVE; }
    if draug.refactor_hibernating { flags |= DRAUG_FLAG_REFACTOR_HIBERNATING; }
    if draug.is_waiting()         { flags |= DRAUG_FLAG_WAITING_FOR_LLM; }
    if draug.should_dream(now_ms) { flags |= DRAUG_FLAG_DREAM_READY; }
    if draug.next_task_and_level().is_some() {
        flags |= DRAUG_FLAG_SKILL_TREE_HAS_WORK;
    }
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

        DRAUG_OP_FRICTION_SIGNAL => {
            let (hash, weight) = unpack_friction(payload0);
            draug.friction.record_signal(hash, weight);
            DRAUG_STATUS_OK
        }

        DRAUG_OP_DREAM_DECIDE => {
            let (handle, _idle_seconds) = unpack_dream_decide(payload0);
            handle_dream_decide(handle, draug)
        }

        DRAUG_OP_DREAM_RESULT => {
            let status = unpack_dream_result_status(payload0);
            let now_ms = uptime();
            match status {
                DREAM_RESULT_COMPLETE => draug.on_dream_complete(now_ms),
                DREAM_RESULT_CANCEL   => draug.wake_up(),
                _ => return DRAUG_STATUS_ERR,
            }
            DRAUG_STATUS_OK
        }

        _ => DRAUG_STATUS_ERR,
    }
}

/// Hard cap on key-list size — sized for the autodream call site, not
/// for general shmem framing. Compositor's wasm.cache rarely exceeds a
/// dozen apps; 32 leaves comfortable headroom and keeps stack costs low.
const DREAM_DECIDE_MAX_KEYS: usize = 32;
/// Hard cap on per-key length, matching the compositor's cache-key
/// conventions (app names + short tweak hashes). Anything longer is a
/// bug or hostile compositor.
const DREAM_DECIDE_MAX_KEY_LEN: usize = 64;
/// Bytes the daemon will read from the shmem region. Larger than the
/// worst-case 32 × (4 + 64) = 2176, smaller than a 4 KiB page so we
/// never run off the end of the smallest shmem the kernel can give us.
const DREAM_DECIDE_MAX_SHMEM_BYTES: usize = 4096;

/// DREAM_DECIDE handler — maps the compositor-supplied shmem, parses
/// the key list, asks `DraugDaemon::start_dream` for a decision, and
/// packs the reply in the wire format documented next to
/// `DRAUG_OP_DREAM_DECIDE` in `libfolk::sys::draug`.
///
/// All failure paths return `DRAUG_STATUS_ERR`; the caller treats that
/// as "skip this dream cycle". We never panic out — corrupting the
/// daemon would defeat Phase A's whole point.
fn handle_dream_decide(handle: u32, draug: &mut DraugDaemon) -> u64 {
    if handle == 0 {
        return DRAUG_STATUS_ERR;
    }
    if shmem_map(handle, DREAM_DECIDE_DAEMON_VADDR).is_err() {
        return DRAUG_STATUS_ERR;
    }

    // From here we MUST unmap before returning. Wrap the body so the
    // unmap is single-pathed.
    let reply = parse_and_decide(draug);
    let _ = shmem_unmap(handle, DREAM_DECIDE_DAEMON_VADDR);
    reply
}

/// Inner half of `handle_dream_decide`, run with the shmem mapped at
/// `DREAM_DECIDE_DAEMON_VADDR`. Splitting this out keeps the unmap
/// path single-exit.
fn parse_and_decide(draug: &mut DraugDaemon) -> u64 {
    // Snapshot bytes from shmem into a stack buffer. Avoids holding
    // raw pointers into shmem across the `start_dream` call (which
    // can allocate, friction-decay, etc.) and bounds the reads
    // tightly to what we'll consult.
    let mut buf = [0u8; DREAM_DECIDE_MAX_SHMEM_BYTES];
    unsafe {
        core::ptr::copy_nonoverlapping(
            DREAM_DECIDE_DAEMON_VADDR as *const u8,
            buf.as_mut_ptr(),
            DREAM_DECIDE_MAX_SHMEM_BYTES,
        );
    }

    // Header: [u32 num_keys][u32 reserved]
    if buf.len() < 8 {
        return DRAUG_STATUS_ERR;
    }
    let num_keys = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if num_keys == 0 || num_keys > DREAM_DECIDE_MAX_KEYS {
        return DRAUG_STATUS_ERR;
    }

    // Owned copy of each key string. `start_dream` takes &[&str]; we
    // build the &str slice from these owned Strings just before the
    // call so lifetimes work out cleanly.
    let mut owned: alloc::vec::Vec<alloc::string::String> =
        alloc::vec::Vec::with_capacity(num_keys);

    let mut off = 8usize;
    for _ in 0..num_keys {
        if off + 4 > buf.len() {
            return DRAUG_STATUS_ERR;
        }
        let len = u16::from_le_bytes([buf[off], buf[off + 1]]) as usize;
        off += 4; // u16 len + u16 reserved
        if len == 0 || len > DREAM_DECIDE_MAX_KEY_LEN || off + len > buf.len() {
            return DRAUG_STATUS_ERR;
        }
        let bytes = &buf[off..off + len];
        let s = match core::str::from_utf8(bytes) {
            Ok(s) => alloc::string::String::from(s),
            Err(_) => return DRAUG_STATUS_ERR,
        };
        owned.push(s);
        off += len;
    }

    let keys: alloc::vec::Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
    let now_ms = uptime();
    let dream_count = draug.dream_count();

    let Some((target, mode)) = draug.start_dream(&keys, now_ms) else {
        // No dream — reply with SKIP. dream_count stays current so
        // compositor's HUD doesn't lose sync.
        return pack_dream_decision(DREAM_ACTION_SKIP, 0, 0, dream_count);
    };

    // Find the target's index in the original key list. Linear scan;
    // num_keys ≤ 32 so this is trivial.
    let mut target_index: u16 = 0;
    let mut found = false;
    for (i, k) in keys.iter().enumerate() {
        if *k == target.as_str() {
            target_index = i as u16;
            found = true;
            break;
        }
    }
    if !found {
        // start_dream returned a key we didn't ship. Treat as protocol
        // error: roll back the dream so compositor doesn't try to
        // execute a target it can't reference, and surface ERR.
        draug.on_dream_complete(now_ms);
        return DRAUG_STATUS_ERR;
    }

    let mode_u8 = match mode {
        DreamMode::Refactor        => DREAM_MODE_REFACTOR,
        DreamMode::Creative        => DREAM_MODE_CREATIVE,
        DreamMode::Nightmare       => DREAM_MODE_NIGHTMARE,
        DreamMode::DriverRefactor  => DREAM_MODE_DRIVER_REFACTOR,
        DreamMode::DriverNightmare => DREAM_MODE_DRIVER_NIGHTMARE,
    };

    pack_dream_decision(DREAM_ACTION_DREAM, mode_u8, target_index, draug.dream_count())
}
