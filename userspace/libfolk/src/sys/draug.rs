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

// ============================================================================
// Well-Known Task ID
// ============================================================================

/// draug-daemon task ID. Reserved at boot by special-case spawn order
/// (see `kernel/src/lib.rs`). Must come AFTER compositor (task 4),
/// intent (task 5), and inference (task 6, currently skipped) to keep
/// the existing well-known task IDs stable.
pub const DRAUG_TASK_ID: u32 = 7;

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
    let reply = ipc::send(DRAUG_TASK_ID, DRAUG_OP_PING, 0)
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
    let _ = ipc::send(DRAUG_TASK_ID, payload, 0);
}

/// Notify Draug of a WASM-app crash, identified by its key hash.
/// Friction-sensor weight signal. Same fire-and-forget semantics as
/// `send_user_input`.
#[inline]
pub fn record_crash(key_hash: u64) {
    let payload = DRAUG_OP_WASM_CRASH | ((key_hash & 0xFFFF_FFFF_FFFF) << 16);
    let _ = ipc::send(DRAUG_TASK_ID, payload, 0);
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
    let reply = ipc::send(DRAUG_TASK_ID, payload, 0)
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
