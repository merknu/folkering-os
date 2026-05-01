//! Draug Protocol — IPC interface to the Draug autonomous-agent daemon.
//!
//! The Draug daemon (`draug-daemon` binary) runs as its own userspace
//! task so that a parse-error or panic in agent code can never take
//! down the compositor / desktop. This module is the wire-protocol
//! definition + thin client wrappers; the server side lives in the
//! `draug-daemon` crate.
//!
//! # Architecture
//!
//! ```text
//! ┌────────────┐    IPC commands    ┌──────────────┐
//! │ Compositor │ ─────────────────► │ Draug daemon │
//! │ (task 4)   │                    │ (task 7)     │
//! │            │ ◄──── status ──────│              │
//! │            │     (shmem)        │              │
//! └────────────┘                    └──────────────┘
//! ```
//!
//! High-frequency events (input timestamps, WASM crashes) flow over
//! IPC as fire-and-forget commands. Live status counters that the
//! compositor reads every render tick (refactor_iter, task_levels,
//! etc.) live in a shared-memory region — see Phase A.3.
//!
//! # Wire format
//!
//! `recv_async()` only delivers `payload0` to the receiver — `payload1`
//! is dropped on the floor by the kernel's async receive path
//! (`SYS_IPC_GET_RECV_PAYLOAD` only fetches the first word). All
//! Draug commands therefore pack their data into `payload0`:
//!
//! ```text
//! ┌────────────┬──────────────────────────────────────────────────┐
//! │ op (16b)   │ op-specific data (48b)                           │
//! └────────────┴──────────────────────────────────────────────────┘
//! ```
//!
//! 48 bits of timestamp ≈ 8.9 years of uptime in milliseconds, which
//! is more than the OS will ever see in one boot. Same budget for
//! WASM-crash key hashes (we use only the low 48 bits — collision
//! probability over a million crashes is ~10⁻⁷, fine for friction
//! accounting).
//!
//! Replies use the standard send-returns-payload0 convention:
//! `DRAUG_STATUS_OK` (0) or `DRAUG_STATUS_ERR` (`u64::MAX`), or a
//! version constant for `PING`.

use crate::sys::ipc;
use crate::sys::memory::{shmem_map, ShmemError};
use crate::sys::system::task_list_detailed;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

// ============================================================================
// Well-Known Task ID
// ============================================================================

/// Pinned draug-daemon task ID. Phase A.6 added an explicit
/// kernel-side spawn between shell (task 3) and the generic ramdisk
/// loop, so the daemon now lands on task 4 deterministically. The
/// `daemon_task_id()` discovery helper still scans `task_list_detailed`
/// as a fallback in case spawn order changes again, but the seed
/// here matches the actual ID on a fresh boot.
pub const DRAUG_TASK_ID: u32 = 4;

/// Cached daemon task ID. Seeded with `DRAUG_TASK_ID`; if the first
/// IPC fails (`Err(IpcError::Unknown)` from `send`), the next
/// `daemon_task_id()` call re-discovers via task scan and updates
/// the cache. Subsequent fast-path calls cost one relaxed load.
static CACHED_DAEMON_TASK_ID: AtomicU32 = AtomicU32::new(DRAUG_TASK_ID);

/// Cached "we already discovered" flag. `false` means the const seed
/// hasn't been validated yet (or a previous discovery failed); `true`
/// means we successfully sent at least one IPC to the cached ID.
static DAEMON_TASK_ID_VALIDATED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Return the daemon's task ID. First call may scan `task_list_detailed`
/// to find a task named "draug-daemon"; subsequent calls are a single
/// relaxed atomic load.
pub fn daemon_task_id() -> u32 {
    if DAEMON_TASK_ID_VALIDATED.load(Ordering::Acquire) {
        return CACHED_DAEMON_TASK_ID.load(Ordering::Relaxed);
    }
    // First-call slow path: try the const seed via PING. If it works,
    // mark validated and we're done. Otherwise rescan.
    let seed = CACHED_DAEMON_TASK_ID.load(Ordering::Relaxed);
    if ipc::send(seed, DRAUG_OP_PING, 0).is_ok() {
        DAEMON_TASK_ID_VALIDATED.store(true, Ordering::Release);
        return seed;
    }
    if let Some(found) = scan_for_daemon() {
        CACHED_DAEMON_TASK_ID.store(found, Ordering::Relaxed);
        DAEMON_TASK_ID_VALIDATED.store(true, Ordering::Release);
        return found;
    }
    // Discovery failed — return the seed so callers fail loudly via
    // their existing `Unreachable` error path.
    seed
}

/// Walk `task_list_detailed` looking for a task whose name is
/// `"draug-daemon"`. Returns `Some(task_id)` on hit. Used as a boot-
/// ordering fallback so libfolk callers don't have to know the
/// daemon's spawn position.
fn scan_for_daemon() -> Option<u32> {
    // 64 tasks × 32 bytes per entry = 2 KiB stack buffer. Folkering
    // doesn't run anywhere near 64 concurrent userspace tasks today,
    // so this is comfortably oversized.
    let mut buf = [0u8; 64 * 32];
    let count = task_list_detailed(&mut buf) as usize;
    let target = b"draug-daemon";
    for i in 0..count.min(64) {
        let off = i * 32;
        // Layout: [task_id: u32][state: u32][name: [u8; 16]][cpu_time_ms: u64]
        let task_id = u32::from_le_bytes([
            buf[off], buf[off + 1], buf[off + 2], buf[off + 3]
        ]);
        let name = &buf[off + 8..off + 24];
        // Names are zero-padded; trim to first NUL.
        let nul = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        if &name[..nul] == target {
            return Some(task_id);
        }
    }
    None
}

// ============================================================================
// Operation Codes (low 16 bits of payload0)
// ============================================================================

/// Liveness probe.
/// Request:  payload0 = OP_PING
/// Reply:    DRAUG_VERSION on success, `u64::MAX` if daemon down
pub const DRAUG_OP_PING: u64 = 0x0000;

/// Friction-sensor input pulse. Sent from compositor's keyboard /
/// mouse drain loops on every event.
/// Request:  payload0 = OP_USER_INPUT | (timestamp_ms << 16)
/// Reply:    DRAUG_STATUS_OK
pub const DRAUG_OP_USER_INPUT: u64 = 0x0001;

/// WASM-app crash notification. Friction-sensor weight signal.
/// Request:  payload0 = OP_WASM_CRASH | (key_hash_low_48 << 16)
/// Reply:    DRAUG_STATUS_OK
pub const DRAUG_OP_WASM_CRASH: u64 = 0x0002;

/// Boot-time refactor-task install. Compositor reads the merged task
/// list from disk during boot, hands it off to the daemon.
/// Request:  payload0 = OP_INSTALL_REFACTOR_TASKS
///                    | (shmem_handle << 16)        // 24 bits
///                    | (size << 40)                 // 24 bits
/// Reply:    DRAUG_STATUS_OK
pub const DRAUG_OP_INSTALL_REFACTOR_TASKS: u64 = 0x0003;

/// Fetch the shmem handle of the status region. Compositor calls
/// this once at boot, maps the handle read-only, and reads the
/// `DraugStatus` struct via atomics on every render frame instead
/// of round-tripping over IPC.
/// Request:  payload0 = OP_GET_STATUS_HANDLE
/// Reply:    shmem_handle (low 32 bits) on success, `u64::MAX` if
///           the daemon has not yet allocated the region.
pub const DRAUG_OP_GET_STATUS_HANDLE: u64 = 0x0004;

/// Friction-sensor signal — compositor's input handlers detect
/// frustration patterns (ESC <3s after open, rage-click bursts) and
/// forward them so the daemon's friction map sees the same signals
/// the local DraugDaemon does. Until this op landed, daemon's
/// friction sensor only saw `WASM_CRASH` events; autodream's
/// "is this app problematic?" gating in compositor-local Draug had
/// a complete picture, daemon-local Draug did not.
/// Request:  payload0 = OP_FRICTION_SIGNAL
///                    | (key_hash << 16)        // 32 bits
///                    | (weight  << 48)         // 16 bits
/// Reply:    DRAUG_STATUS_OK
pub const DRAUG_OP_FRICTION_SIGNAL: u64 = 0x0005;

/// Autodream decision query. Compositor publishes the current
/// `wasm.cache` key list into a freshly-allocated shmem region,
/// grants the daemon access, then sends this op with the handle and
/// the user-idle timestamp. Daemon maps the shmem, runs its own
/// `should_dream` + `start_dream` logic on the supplied keys, and
/// replies with the decision. Compositor still owns the dream
/// EXECUTION (memory snapshot, MCP send, chunk reassembly) — this
/// op only moves the DECISION (which app + which mode) to the
/// daemon, where Draug's friction sensor and idle accounting live.
///
/// Request: payload0 = OP_DREAM_DECIDE
///                   | (shmem_handle << 16)   // 24 bits
///                   | (idle_seconds << 40)   // 24 bits, ≈ 6 months
/// Shmem layout:
///   [u32 num_keys][u32 reserved]
///   per key: [u16 len][u16 reserved][len bytes UTF-8]
///
/// Reply payload0:
///   bits  0..8   action (DREAM_ACTION_SKIP / DREAM_ACTION_DREAM)
///   bits  8..16  mode (DREAM_MODE_*)
///   bits 16..32  target_index (which key in the list)
///   bits 32..64  daemon's dream_count (for compositor HUD)
pub const DRAUG_OP_DREAM_DECIDE: u64 = 0x0006;

/// Autodream lifecycle event from compositor — "the dream that
/// daemon picked just finished". `status` = 0 means success (daemon
/// records `on_dream_complete`, bumps `dream_count`, updates
/// `last_dream_ms`); `status` = 1 means cancel (daemon calls
/// `wake_up`, no count bump). Compositor sends this after evaluation
/// or on send-failure / wake-on-input, then drops its local
/// `current_dream` context.
///
/// Request:  payload0 = OP_DREAM_RESULT | (status << 16)
/// Reply:    DRAUG_STATUS_OK
pub const DRAUG_OP_DREAM_RESULT: u64 = 0x0007;

/// Refactor-failure strike against an app, identified by its key
/// hash. Compositor's V1-vs-V2 dream evaluator runs in the WASM
/// runtime (compositor-only), so the strike signal flows from
/// compositor *into* the daemon — without this IPC, daemon's
/// `start_dream` priority-3 ("skip perfected apps") would never see
/// any strikes and would re-pick the same broken app forever.
///
/// Request:  payload0 = OP_STRIKE_ADD | (key_hash << 16)
/// Reply:    DRAUG_STATUS_OK
pub const DRAUG_OP_STRIKE_ADD: u64 = 0x0008;

/// Reset strikes for an app — sent by compositor when a dream
/// successfully evolves the app (V2 faster than V1, fuzz passes).
///
/// Request:  payload0 = OP_STRIKE_RESET | (key_hash << 16)
/// Reply:    DRAUG_STATUS_OK
pub const DRAUG_OP_STRIKE_RESET: u64 = 0x0009;

// DreamMode wire encoding — must match the order of variants in
// `draug_daemon::draug::DreamMode`. Stable ABI.
pub const DREAM_MODE_REFACTOR: u8 = 0;
pub const DREAM_MODE_CREATIVE: u8 = 1;
pub const DREAM_MODE_NIGHTMARE: u8 = 2;
pub const DREAM_MODE_DRIVER_REFACTOR: u8 = 3;
pub const DREAM_MODE_DRIVER_NIGHTMARE: u8 = 4;

pub const DREAM_ACTION_SKIP: u8 = 0;
pub const DREAM_ACTION_DREAM: u8 = 1;

/// `DRAUG_OP_DREAM_RESULT` status byte values.
pub const DREAM_RESULT_COMPLETE: u8 = 0;
pub const DREAM_RESULT_CANCEL: u8 = 1;

// ============================================================================
// Status Codes (reply payload0)
// ============================================================================

pub const DRAUG_STATUS_OK: u64 = 0;
pub const DRAUG_STATUS_ERR: u64 = u64::MAX;

/// Protocol version. Bumped whenever the wire format changes
/// incompatibly. PING returns this so clients can refuse to talk to a
/// daemon that's older than they expect.
pub const DRAUG_VERSION: u64 = 0x0001_0000; // major.minor packed; 1.0

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraugError {
    /// daemon task not running, or IPC send refused.
    Unreachable,
    /// daemon replied with an unexpected value.
    Protocol(u64),
}

pub type DraugResult<T> = Result<T, DraugError>;

// ============================================================================
// Client wrappers
// ============================================================================

/// Liveness check. Returns `Ok(version)` if the daemon answered, `Err`
/// otherwise. Useful for boot-order readiness probing.
pub fn ping() -> DraugResult<u64> {
    let reply = ipc::send(daemon_task_id(), DRAUG_OP_PING, 0)
        .map_err(|_| DraugError::Unreachable)?;

    if reply == DRAUG_STATUS_ERR {
        return Err(DraugError::Unreachable);
    }
    Ok(reply)
}

/// Notify Draug of a user-input pulse for friction-sensor accounting.
///
/// This is a fire-and-forget event. We still wait for the reply
/// (synchronous IPC is the only kind we have) but ignore its value.
/// Send failure is silently dropped — losing one timestamp does not
/// affect correctness, only the staleness of the idle estimate.
#[inline]
pub fn send_user_input(timestamp_ms: u64) {
    let payload = DRAUG_OP_USER_INPUT | ((timestamp_ms & 0xFFFF_FFFF_FFFF) << 16);
    let _ = ipc::send(daemon_task_id(), payload, 0);
}

/// Notify Draug of a WASM-app crash, identified by its key hash.
/// Friction-sensor weight signal. Same fire-and-forget semantics as
/// `send_user_input`.
#[inline]
pub fn record_crash(key_hash: u64) {
    let payload = DRAUG_OP_WASM_CRASH | ((key_hash & 0xFFFF_FFFF_FFFF) << 16);
    let _ = ipc::send(daemon_task_id(), payload, 0);
}

/// Forward a friction-sensor signal to the daemon. Compositor's
/// input handlers detect ESC-quick-close and rage-click patterns;
/// without this forwarding the daemon's friction map sees only
/// `WASM_CRASH` events.
///
/// Fire-and-forget — the IPC reply is ignored. Losing one signal
/// just means the daemon's friction estimate is one tick stale,
/// which the decay path absorbs.
#[inline]
pub fn send_friction_signal(key_hash: u32, weight: u16) {
    let payload = DRAUG_OP_FRICTION_SIGNAL
        | ((key_hash as u64) << 16)
        | ((weight as u64) << 48);
    let _ = ipc::send(daemon_task_id(), payload, 0);
}

/// Decoded autodream decision from `request_dream_decision`.
#[derive(Debug, Clone, Copy)]
pub struct DreamDecision {
    /// `DREAM_ACTION_SKIP` or `DREAM_ACTION_DREAM`.
    pub action: u8,
    /// `DREAM_MODE_*` (only meaningful when action == DREAM).
    pub mode: u8,
    /// Index into the key list compositor sent (only meaningful
    /// when action == DREAM).
    pub target_index: u16,
    /// Daemon's running dream_count, for compositor's HUD.
    pub dream_count: u32,
}

impl DreamDecision {
    pub fn should_dream(&self) -> bool { self.action == DREAM_ACTION_DREAM }
}

/// Ask the daemon whether to start a dream cycle right now and, if
/// yes, which app + mode to target. Compositor must have already
/// allocated `shmem_handle` and written the key list there in the
/// format documented for `DRAUG_OP_DREAM_DECIDE`. The handle's read
/// permission must already be granted to the daemon
/// (`shmem_grant(handle, daemon_task_id())`).
///
/// `idle_seconds` is the time since last user input as compositor
/// observed it (rdtsc → seconds since boot would also work — the
/// daemon only uses the value via subtraction against its own
/// timestamps).
///
/// Returns `None` on transport failure or shmem-handle overflow.
pub fn request_dream_decision(shmem_handle: u32, idle_seconds: u32) -> Option<DreamDecision> {
    if shmem_handle >> 24 != 0 || idle_seconds >> 24 != 0 {
        return None;
    }
    let payload = DRAUG_OP_DREAM_DECIDE
        | ((shmem_handle as u64) << 16)
        | ((idle_seconds as u64) << 40);
    let reply = ipc::send(daemon_task_id(), payload, 0).ok()?;
    if reply == DRAUG_STATUS_ERR {
        return None;
    }
    Some(DreamDecision {
        action: (reply & 0xFF) as u8,
        mode: ((reply >> 8) & 0xFF) as u8,
        target_index: ((reply >> 16) & 0xFFFF) as u16,
        dream_count: ((reply >> 32) & 0xFFFF_FFFF) as u32,
    })
}

/// Hand the boot-time refactor-task list to the daemon. The caller
/// owns a shmem region containing the serialised tasks; this function
/// transfers a handle.
///
/// `shmem_handle` is capped at 24 bits (16M handles) and `total_size`
/// at 24 bits (16 MiB) — both more than enough headroom for the
/// merged task list, which is typically <100 KiB.
///
/// The caller is responsible for granting the daemon access to the
/// shmem before calling (`shmem_grant`).
pub fn install_refactor_tasks(shmem_handle: u32, total_size: u32) -> DraugResult<()> {
    if shmem_handle >> 24 != 0 || total_size >> 24 != 0 {
        return Err(DraugError::Protocol(DRAUG_STATUS_ERR));
    }
    let payload = DRAUG_OP_INSTALL_REFACTOR_TASKS
        | ((shmem_handle as u64) << 16)
        | ((total_size as u64) << 40);
    let reply = ipc::send(daemon_task_id(), payload, 0)
        .map_err(|_| DraugError::Unreachable)?;

    if reply == DRAUG_STATUS_OK {
        Ok(())
    } else {
        Err(DraugError::Protocol(reply))
    }
}

// ============================================================================
// Server-side helpers (for draug-daemon)
// ============================================================================

/// Decode the operation code from a received payload0.
#[inline]
pub fn unpack_op(payload0: u64) -> u64 {
    payload0 & 0xFFFF
}

/// Decode the 48-bit data field above the op code.
#[inline]
pub fn unpack_data48(payload0: u64) -> u64 {
    (payload0 >> 16) & 0xFFFF_FFFF_FFFF
}

/// Decode an `(shmem_handle, total_size)` pair from a payload0 that
/// uses the `INSTALL_REFACTOR_TASKS` packing.
#[inline]
pub fn unpack_shmem_size(payload0: u64) -> (u32, u32) {
    let shmem_handle = ((payload0 >> 16) & 0xFF_FFFF) as u32;
    let total_size = ((payload0 >> 40) & 0xFF_FFFF) as u32;
    (shmem_handle, total_size)
}

/// Decode `(shmem_handle, idle_seconds)` from a payload0 that uses
/// the `DREAM_DECIDE` packing. Both fields are 24 bits.
#[inline]
pub fn unpack_dream_decide(payload0: u64) -> (u32, u32) {
    let shmem = ((payload0 >> 16) & 0xFF_FFFF) as u32;
    let idle = ((payload0 >> 40) & 0xFF_FFFF) as u32;
    (shmem, idle)
}

/// Decode the status byte from a `DREAM_RESULT` payload0.
#[inline]
pub fn unpack_dream_result_status(payload0: u64) -> u8 {
    ((payload0 >> 16) & 0xFF) as u8
}

/// Decode a 32-bit key hash from a payload0 that uses the `STRIKE_*`
/// packing (`OP | (key_hash << 16)`).
#[inline]
pub fn unpack_key_hash(payload0: u64) -> u32 {
    ((payload0 >> 16) & 0xFFFF_FFFF) as u32
}

/// Notify the daemon that a dream cycle finished. `status` is one of
/// `DREAM_RESULT_COMPLETE` / `DREAM_RESULT_CANCEL`. Fire-and-forget;
/// reply is ignored. A dropped notification leaves the daemon's
/// `dreaming` flag stuck on `true` until the next `DREAM_DECIDE`
/// overwrites it — annoying but not catastrophic, and rare enough
/// that we don't need a retry loop here.
#[inline]
pub fn notify_dream_result(status: u8) {
    let payload = DRAUG_OP_DREAM_RESULT | ((status as u64) << 16);
    let _ = ipc::send(daemon_task_id(), payload, 0);
}

/// Bump the daemon's strike counter for an app, keyed by its hash.
/// Fire-and-forget. Sent from compositor's V1-vs-V2 evaluator after
/// a refactor attempt fails sanity / benchmark / fuzz.
#[inline]
pub fn notify_strike_add(key_hash: u32) {
    let payload = DRAUG_OP_STRIKE_ADD | ((key_hash as u64) << 16);
    let _ = ipc::send(daemon_task_id(), payload, 0);
}

/// Clear the daemon's strike counter for an app. Sent from compositor
/// when a dream successfully evolves the app.
#[inline]
pub fn notify_strike_reset(key_hash: u32) {
    let payload = DRAUG_OP_STRIKE_RESET | ((key_hash as u64) << 16);
    let _ = ipc::send(daemon_task_id(), payload, 0);
}

/// Encode the daemon-side reply for `DRAUG_OP_DREAM_DECIDE`.
#[inline]
pub fn pack_dream_decision(action: u8, mode: u8, target_idx: u16, dream_count: u32) -> u64 {
    (action as u64)
        | ((mode as u64) << 8)
        | ((target_idx as u64) << 16)
        | ((dream_count as u64) << 32)
}

/// Decode `(key_hash, weight)` from a payload0 that uses the
/// `FRICTION_SIGNAL` packing.
#[inline]
pub fn unpack_friction(payload0: u64) -> (u32, u16) {
    let hash = ((payload0 >> 16) & 0xFFFF_FFFF) as u32;
    let weight = ((payload0 >> 48) & 0xFFFF) as u16;
    (hash, weight)
}

// ============================================================================
// Status shmem region (read by compositor, written by daemon)
// ============================================================================

/// Layout version of the `DraugStatus` struct. The compositor refuses
/// to read a region whose version doesn't match what it was compiled
/// against, so adding/reordering fields requires bumping this so old
/// readers fail loudly instead of silently returning garbage.
///
/// v2 (Phase A.5 step 4): added `DRAUG_FLAG_WAITING_FOR_LLM`,
/// `DRAUG_FLAG_DREAM_READY`, `DRAUG_FLAG_SKILL_TREE_HAS_WORK`. Struct
/// layout itself is unchanged — only flag-bit semantics — but readers
/// of the new flags need the new version to interpret them correctly,
/// and old readers ignoring the bits are still safe.
pub const DRAUG_STATUS_LAYOUT_VERSION: u32 = 2;

/// Size of the status shmem region in bytes. Round number that fits
/// the current `DraugStatus` and leaves headroom for additions.
pub const DRAUG_STATUS_SHMEM_SIZE: usize = 256;

/// Stable virtual address where the compositor maps the status
/// region. Picked to sit alongside the existing per-shmem vaddrs
/// (0x30000000 / 0x32000000) without overlap. Daemon-side mapping
/// uses a different vaddr — see `draug-daemon` for `DRAUG_STATUS_DAEMON_VADDR`.
pub const DRAUG_STATUS_COMPOSITOR_VADDR: usize = 0x33000000;

/// Bit flags packed into `DraugStatus::flags`.
pub const DRAUG_FLAG_PLAN_MODE_ACTIVE: u32     = 1 << 0;
pub const DRAUG_FLAG_REFACTOR_HIBERNATING: u32 = 1 << 1;
/// Daemon's `DraugDaemon::is_waiting()` — set while an LLM round-trip
/// is in flight. Compositor consults this to decide whether to keep
/// polling MCP for a response.
pub const DRAUG_FLAG_WAITING_FOR_LLM: u32      = 1 << 2;
/// Daemon's `should_dream(now_ms)` returned true at last publish —
/// idle long enough, dream budget not exhausted, not currently
/// dreaming, not waiting for LLM. Compositor uses this as the
/// authoritative gate for `start_dream_cycle`; replaces the
/// compositor-local `draug.should_dream` call which was returning
/// stale results because the local instance no longer ticks dream
/// state (since #76 / #78).
pub const DRAUG_FLAG_DREAM_READY: u32          = 1 << 3;
/// Daemon has a skill-tree task at L1 / L2 / L3 still un-PASSed.
/// Compositor uses this to gate AutoDream behind the refactor loop:
/// when there's still skill-tree work, dreaming is suppressed.
pub const DRAUG_FLAG_SKILL_TREE_HAS_WORK: u32  = 1 << 4;
pub const DRAUG_FLAG_INITIALISED: u32          = 1 << 31;

/// Live status snapshot. Daemon updates fields with `Ordering::Release`,
/// compositor reads with `Ordering::Acquire`. Field reads are
/// individually consistent; cross-field totals (e.g. passed + failed
/// vs iter) can briefly disagree by ~1 — acceptable for HUD display.
///
/// Layout is fixed at 128 bytes with explicit padding so adding a new
/// field forces a layout-version bump (= the old size no longer fits
/// the struct, so the compiler complains).
#[repr(C, align(64))]
pub struct DraugStatus {
    /// `DRAUG_STATUS_LAYOUT_VERSION`. Read first; if it doesn't
    /// match, treat all other fields as invalid.
    pub layout_version: AtomicU32,
    /// Bit-OR of `DRAUG_FLAG_*` constants.
    pub flags: AtomicU32,

    pub refactor_iter: AtomicU32,
    pub refactor_passed: AtomicU32,
    pub refactor_failed: AtomicU32,
    pub refactor_retries: AtomicU32,

    pub complex_task_idx: AtomicU32,
    pub crash_count: AtomicU32,

    pub last_input_ms: AtomicU64,
    pub last_skill_ms: AtomicU64,

    pub tasks_at_l1: AtomicU32,
    pub tasks_at_l2: AtomicU32,
    pub tasks_at_l3: AtomicU32,
    pub consecutive_skips: AtomicU32,

    /// Per-task skill levels (0..=3). Index = task slot.
    pub task_levels: [AtomicU8; 20],

    pub _padding: [u8; 28],
}

const _: () = {
    // Compile-time size guard: bump the layout version if this fires.
    assert!(core::mem::size_of::<DraugStatus>() <= DRAUG_STATUS_SHMEM_SIZE);
};

impl DraugStatus {
    /// Const-friendly initialiser used by the daemon when it carves
    /// out the shmem region. Every counter starts at zero.
    pub const fn zeroed() -> Self {
        const A8: AtomicU8 = AtomicU8::new(0);
        Self {
            layout_version: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            refactor_iter: AtomicU32::new(0),
            refactor_passed: AtomicU32::new(0),
            refactor_failed: AtomicU32::new(0),
            refactor_retries: AtomicU32::new(0),
            complex_task_idx: AtomicU32::new(0),
            crash_count: AtomicU32::new(0),
            last_input_ms: AtomicU64::new(0),
            last_skill_ms: AtomicU64::new(0),
            tasks_at_l1: AtomicU32::new(0),
            tasks_at_l2: AtomicU32::new(0),
            tasks_at_l3: AtomicU32::new(0),
            consecutive_skips: AtomicU32::new(0),
            task_levels: [A8; 20],
            _padding: [0; 28],
        }
    }
}

/// Fetch the daemon's status shmem handle over IPC. Compositor calls
/// this once at boot, then maps the handle and stops talking IPC for
/// status reads.
pub fn get_status_handle() -> DraugResult<u32> {
    let reply = ipc::send(daemon_task_id(), DRAUG_OP_GET_STATUS_HANDLE, 0)
        .map_err(|_| DraugError::Unreachable)?;
    if reply == DRAUG_STATUS_ERR {
        return Err(DraugError::Protocol(reply));
    }
    Ok(reply as u32)
}

/// Errors specific to attaching the status region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    /// Daemon not yet ready or unreachable.
    Daemon(DraugError),
    /// `shmem_map` failed.
    Map(ShmemError),
    /// Layout version mismatch — daemon and client compiled against
    /// different versions of `DraugStatus`.
    LayoutMismatch { expected: u32, found: u32 },
}

/// Bootstrap a read-only view of the daemon's status region.
///
/// This is a one-shot call: the returned reference is valid for the
/// lifetime of the process (or until daemon teardown). Subsequent
/// reads use plain atomic loads with no IPC overhead.
///
/// Returns `Err(LayoutMismatch)` if the daemon's layout version does
/// not match what this compositor was built against — the right
/// response is to skip Draug status display rather than read garbage.
pub fn attach_status() -> Result<&'static DraugStatus, AttachError> {
    let handle = get_status_handle().map_err(AttachError::Daemon)?;
    shmem_map(handle, DRAUG_STATUS_COMPOSITOR_VADDR).map_err(AttachError::Map)?;

    let status: &'static DraugStatus = unsafe {
        &*(DRAUG_STATUS_COMPOSITOR_VADDR as *const DraugStatus)
    };

    let found = status.layout_version.load(Ordering::Acquire);
    if found != DRAUG_STATUS_LAYOUT_VERSION {
        return Err(AttachError::LayoutMismatch {
            expected: DRAUG_STATUS_LAYOUT_VERSION,
            found,
        });
    }
    Ok(status)
}

/// `attach_status` with retry. Phase A.6 (#84) made the kernel spawn
/// draug-daemon before compositor, but "spawn order" doesn't
/// guarantee "init order" — when the scheduler hands compositor a
/// time slice before daemon has finished `boot_status_shmem`, the
/// `GET_STATUS_HANDLE` IPC returns ERR (or `Unreachable` if the
/// daemon's IPC loop hasn't started yet) and the gate stays closed
/// for the rest of the session.
///
/// This wrapper retries with `yield_cpu()` between attempts, giving
/// the daemon a chance to catch up. `max_attempts = 50` × 1 ms-ish
/// scheduler ticks puts a ~50 ms cap on boot delay, which is well
/// below human perception and well above any plausible boot-race
/// window.
pub fn attach_status_with_retry(
    max_attempts: u32,
) -> Result<&'static DraugStatus, AttachError> {
    let mut last_err = AttachError::Daemon(DraugError::Unreachable);
    for _ in 0..max_attempts {
        match attach_status() {
            Ok(status) => return Ok(status),
            Err(e @ AttachError::LayoutMismatch { .. }) => return Err(e),
            // For Daemon (IPC unreachable / status err) and Map
            // errors: retry. The first happens during the boot race;
            // the second can happen if the handle came back stale —
            // either way, yielding lets the daemon drive its boot
            // sequence forward.
            Err(e) => {
                last_err = e;
                crate::sys::yield_cpu();
            }
        }
    }
    Err(last_err)
}
