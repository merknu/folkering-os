//! IPC Message Structure
//!
//! The `IpcMessage` struct is **exactly 64 bytes** (one cache line) for optimal performance.
//! This is a critical architecture requirement - any changes must maintain this size.

use core::num::NonZeroU32;

// ============================================================================
// CallerToken - Reply-Later IPC Support (Phase 6)
// ============================================================================

/// Opaque token representing a suspended client waiting for reply.
///
/// Used for async/deferred reply pattern where server can stash the token,
/// do long-running work (e.g., LLM inference), then reply later.
///
/// # Security
/// The token encodes sender_pid + request_id with a simple obfuscation.
/// On reply, the kernel verifies the sender is still waiting for this exact request.
///
/// # Example
/// ```no_run
/// // Server receives request, gets token
/// let (token, msg_id, payload_len) = ipc_recv_async()?;
///
/// // Server can now do other work, or spawn async task
/// spawn_inference_task(payload, token);
///
/// // Later, when ready to reply:
/// ipc_reply_token(token, &reply_data)?;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct CallerToken(pub u64);

impl CallerToken {
    /// Create a new CallerToken from sender PID and request ID.
    ///
    /// Encoding: (sender_pid << 32) | (request_id & 0xFFFFFFFF) ^ OBFUSCATION_KEY
    /// This is NOT cryptographically secure, but prevents accidental misuse.
    #[inline]
    pub fn new(sender_pid: TaskId, request_id: u64) -> Self {
        const OBFUSCATION_KEY: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let raw = ((sender_pid as u64) << 32) | (request_id & 0xFFFF_FFFF);
        Self(raw ^ OBFUSCATION_KEY)
    }

    /// Decode the token to extract sender PID and request ID.
    ///
    /// Returns None if the token appears corrupted (reserved for future validation).
    #[inline]
    pub fn decode(self) -> Option<(TaskId, u64)> {
        const OBFUSCATION_KEY: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let raw = self.0 ^ OBFUSCATION_KEY;
        let sender_pid = (raw >> 32) as TaskId;
        let request_id = raw & 0xFFFF_FFFF;

        // Basic sanity check: sender_pid should be non-zero
        if sender_pid == 0 {
            return None;
        }

        Some((sender_pid, request_id))
    }

    /// Get the raw token value (for syscall transfer).
    #[inline]
    pub fn as_raw(self) -> u64 {
        self.0
    }

    /// Create from raw value (from syscall).
    #[inline]
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// Task identifier (u32)
pub type TaskId = u32;

/// Capability identifier (NonZeroU32 for Option optimization)
pub type CapabilityId = NonZeroU32;

/// Shared memory region identifier (NonZeroU32 for Option optimization)
pub type ShmemId = NonZeroU32;

/// IPC message type
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IpcType {
    /// Synchronous request (wait for reply)
    Request = 0,
    /// Reply to request
    Reply = 1,
    /// Asynchronous notification (no reply expected)
    Notification = 2,
}

/// IPC message structure (cache-line optimized)
///
/// # Size: 64 bytes (exactly one cache line)
///
/// # Alignment: 8 bytes (natural, from u64 fields)
///
/// # Layout:
/// - `sender` (u32): 4 bytes, offset 0
/// - `msg_type` (u8): 1 byte, offset 4
/// - `_padding1` ([u8; 3]): 3 bytes, offset 5-7 (align payload)
/// - `payload` ([u64; 4]): 32 bytes, offset 8-39 (8-byte aligned)
/// - `cap` (Option<NonZeroU32>): 8 bytes, offset 40-47
/// - `shmem` (Option<NonZeroU32>): 8 bytes, offset 48-55
/// - `msg_id` (u64): 8 bytes, offset 56-63
///
/// **Total: 64 bytes**
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct IpcMessage {
    /// Sender task ID (set by kernel)
    pub sender: TaskId,

    /// Message type (request, reply, notification)
    pub msg_type: IpcType,

    /// Explicit padding to align payload to 8-byte boundary
    _padding1: [u8; 3],

    /// Small message payload (inline, no copy overhead)
    /// Can hold 4×u64 = 256 bits of data
    pub payload: [u64; 4],

    /// Capability to transfer (optional)
    /// Uses NonZeroU32 optimization: Option<NonZeroU32> is 4 bytes
    pub cap: Option<CapabilityId>,

    /// Padding to maintain 8-byte alignment
    _padding2: [u8; 4],

    /// Shared memory region (for bulk data >32 bytes)
    /// Uses NonZeroU32 optimization: Option<NonZeroU32> is 4 bytes
    pub shmem: Option<ShmemId>,

    /// Padding to maintain 8-byte alignment
    _padding3: [u8; 4],

    /// Message ID (assigned by kernel, for tracking/debugging)
    pub msg_id: u64,
}

impl IpcMessage {
    /// Create new IPC request message
    pub const fn new_request(payload: [u64; 4]) -> Self {
        Self {
            sender: 0, // Set by kernel
            msg_type: IpcType::Request,
            _padding1: [0; 3],
            payload,
            cap: None,
            _padding2: [0; 4],
            shmem: None,
            _padding3: [0; 4],
            msg_id: 0, // Set by kernel
        }
    }

    /// Create new IPC reply message
    pub const fn new_reply(payload: [u64; 4]) -> Self {
        Self {
            sender: 0,
            msg_type: IpcType::Reply,
            _padding1: [0; 3],
            payload,
            cap: None,
            _padding2: [0; 4],
            shmem: None,
            _padding3: [0; 4],
            msg_id: 0,
        }
    }

    /// Create new IPC notification message
    pub const fn new_notification(payload: [u64; 4]) -> Self {
        Self {
            sender: 0,
            msg_type: IpcType::Notification,
            _padding1: [0; 3],
            payload,
            cap: None,
            _padding2: [0; 4],
            shmem: None,
            _padding3: [0; 4],
            msg_id: 0,
        }
    }
}

// CRITICAL: Compile-time assertion to ensure IpcMessage is exactly 64 bytes
const _: () = {
    if core::mem::size_of::<IpcMessage>() != 64 {
        panic!("IpcMessage must be exactly 64 bytes!");
    }
};


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ipc_message_size() {
        assert_eq!(core::mem::size_of::<IpcMessage>(), 64);
        assert_eq!(core::mem::align_of::<IpcMessage>(), 8);
    }

    #[test]
    fn test_ipc_message_layout() {
        use core::mem::offset_of;

        // Verify field offsets
        assert_eq!(offset_of!(IpcMessage, sender), 0);
        assert_eq!(offset_of!(IpcMessage, msg_type), 4);
        assert_eq!(offset_of!(IpcMessage, payload), 8);
        assert_eq!(offset_of!(IpcMessage, cap), 40);
        assert_eq!(offset_of!(IpcMessage, shmem), 48);
        assert_eq!(offset_of!(IpcMessage, msg_id), 56);
    }

    #[test]
    fn test_option_nonzero_optimization() {
        // Verify Option<NonZeroU32> uses null optimization
        assert_eq!(core::mem::size_of::<Option<NonZeroU32>>(), 4);
    }
}
