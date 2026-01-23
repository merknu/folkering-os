//! IPC (Inter-Process Communication) subsystem
//!
//! Fast message passing with <1000 cycle target.
//!
//! # Message Structure
//!
//! The `IpcMessage` struct is exactly 64 bytes (one cache line) for optimal performance.
//!
//! # Modules
//!
//! - `message`: IPC message structure and types
//! - `queue`: Per-task bounded message queues
//! - `send`: Synchronous and asynchronous send operations
//! - `receive`: Blocking and non-blocking receive operations
//! - `shared_memory`: Zero-copy bulk data transfer
//!
//! # Example
//!
//! ```no_run
//! use folkering_kernel::ipc::*;
//!
//! // Client: send request
//! let request = IpcMessage::new_request([1, 2, 3, 4]);
//! let reply = ipc_send(server_id, &request)?;
//!
//! // Server: receive and reply
//! let request = ipc_receive()?;
//! let reply = IpcMessage::new_reply([42, 0, 0, 0]);
//! ipc_reply(&reply)?;
//! ```

pub mod message;
pub mod queue;
pub mod send;
pub mod receive;
pub mod shared_memory;

// Re-export main types
pub use message::{IpcMessage, IpcType, TaskId, CapabilityId, ShmemId};
pub use queue::MessageQueue;
pub use send::{ipc_send, ipc_send_async, Errno};
pub use receive::{ipc_receive, ipc_try_receive, ipc_reply};
pub use shared_memory::{
    shmem_create, shmem_map, shmem_unmap, shmem_destroy,
    shmem_grant, shmem_revoke, ShmemPerms, ShmemError,
};

use core::sync::atomic::{AtomicU64, Ordering};

/// IPC message identifier
pub type MessageId = u64;

/// Global message ID counter
static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

/// Generate next unique message ID
///
/// Used internally by send/receive operations to assign unique IDs to messages.
#[inline]
pub(crate) fn next_message_id() -> MessageId {
    NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Initialize IPC subsystem
///
/// Called during kernel boot to set up IPC infrastructure.
pub fn init() {
    // IPC subsystem is ready to use
    // Message ID counter is initialized statically
    // Shared memory table is initialized statically
    // No additional setup needed for Phase 1
}
