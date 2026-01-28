//! Synapse Protocol - Data Kernel IPC Interface
//!
//! Synapse is the "Data Kernel" of Folkering OS. It manages all data operations
//! and provides a unified interface for file access, queries, and eventually
//! AI-powered semantic search.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────┐     IPC      ┌─────────┐
//! │  Shell  │◄────────────►│ Synapse │
//! │  (App)  │              │ (Data)  │
//! └─────────┘              └─────────┘
//! ```
//!
//! # Protocol
//!
//! All operations use the standard IPC message format:
//! - payload[0]: Operation code (SYN_OP_*)
//! - payload[1]: Parameter 1 (operation-specific)
//! - payload[2]: Parameter 2 (operation-specific)
//! - payload[3]: Parameter 3 (operation-specific)
//!
//! Replies use the same format with status in payload[0].

use crate::syscall::{syscall3, SYS_IPC_SEND, SYS_IPC_RECEIVE};
use crate::sys::ipc::{IpcError, IpcMessage, receive, reply};

// ============================================================================
// Well-Known Task IDs
// ============================================================================

/// Synapse service task ID (spawned at boot as Task 2)
///
/// Note: Task 1 is the idle/dummy task, Task 2 is IPC receiver (now Synapse),
/// Shell gets Task 4+
pub const SYNAPSE_TASK_ID: u32 = 2;

// ============================================================================
// Operation Codes
// ============================================================================

/// List files in the data store
/// Request: [OP, offset, limit, 0]
/// Reply: [count, 0, 0, 0] (entries follow via shared memory in v2)
pub const SYN_OP_LIST_FILES: u64 = 0x0001;

/// Read file metadata by name hash
/// Request: [OP, name_hash_lo, name_hash_hi, 0]
/// Reply: [size, entry_type, 0, 0]
pub const SYN_OP_FILE_INFO: u64 = 0x0002;

/// Read file contents
/// Request: [OP, file_id, offset, length]
/// Reply: [bytes_read, data_lo, data_hi, 0] (for small reads)
pub const SYN_OP_READ_FILE: u64 = 0x0003;

/// Get file count
/// Request: [OP, 0, 0, 0]
/// Reply: [count, 0, 0, 0]
pub const SYN_OP_FILE_COUNT: u64 = 0x0004;

/// Get file entry by index
/// Request: [OP, index, 0, 0]
/// Reply: [id, size, type, name_hash]
pub const SYN_OP_FILE_BY_INDEX: u64 = 0x0005;

/// Ping - check if Synapse is alive
/// Request: [OP, magic, 0, 0]
/// Reply: [magic ^ 0x5959, version, 0, 0]
pub const SYN_OP_PING: u64 = 0x0000;

// Future operations (Phase 2+)
// pub const SYN_OP_QUERY: u64 = 0x0010;      // SQL-like query
// pub const SYN_OP_VECTOR_SEARCH: u64 = 0x0020; // Semantic search
// pub const SYN_OP_WRITE_FILE: u64 = 0x0030;  // Write (when we have writable FS)

// ============================================================================
// Status Codes
// ============================================================================

/// Operation succeeded
pub const SYN_STATUS_OK: u64 = 0;

/// File/resource not found
pub const SYN_STATUS_NOT_FOUND: u64 = 1;

/// Invalid operation or parameter
pub const SYN_STATUS_INVALID: u64 = 2;

/// Would block (try again)
pub const SYN_STATUS_BUSY: u64 = 3;

/// Internal error
pub const SYN_STATUS_ERROR: u64 = 0xFFFF;

// ============================================================================
// Version
// ============================================================================

/// Synapse protocol version (major.minor as u32: 0x00010000 = v1.0)
pub const SYNAPSE_VERSION: u64 = 0x0001_0000;

// ============================================================================
// Client API
// ============================================================================

/// Result type for Synapse operations
pub type SynapseResult<T> = Result<T, SynapseError>;

/// Synapse error types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynapseError {
    /// Synapse service not responding
    ServiceUnavailable,
    /// File or resource not found
    NotFound,
    /// Invalid request
    InvalidRequest,
    /// IPC error
    IpcFailed,
    /// Unknown error
    Unknown(u64),
}

/// File entry returned from Synapse
#[derive(Debug, Clone, Copy)]
pub struct FileEntry {
    pub id: u32,
    pub size: u32,
    pub entry_type: u8,
    pub name_hash: u32,
}

impl FileEntry {
    /// Check if this entry is an ELF executable
    pub fn is_elf(&self) -> bool {
        self.entry_type == 1
    }
}

/// Ping Synapse to check if it's alive
pub fn ping() -> SynapseResult<u64> {
    let magic: u64 = 0x464F4C4B; // "FOLK"

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, SYN_OP_PING, magic)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    // Check response magic
    let expected = magic ^ 0x5959;
    if ret != expected {
        return Err(SynapseError::Unknown(ret));
    }

    Ok(ret)
}

/// Get the number of files in the data store
pub fn file_count() -> SynapseResult<usize> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, SYN_OP_FILE_COUNT, 0)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    Ok(ret as usize)
}

/// Get file entry by index
pub fn file_by_index(index: usize) -> SynapseResult<FileEntry> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, SYN_OP_FILE_BY_INDEX, index as u64)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    if ret == SYN_STATUS_NOT_FOUND {
        return Err(SynapseError::NotFound);
    }

    // Decode: id in low 16 bits, size in next 16, type in next 8, hash in high 24
    // Actually, let's use a simpler encoding for now:
    // ret = (id << 48) | (size << 16) | type
    let id = ((ret >> 48) & 0xFFFF) as u32;
    let size = ((ret >> 16) & 0xFFFF_FFFF) as u32;
    let entry_type = (ret & 0xFF) as u8;

    Ok(FileEntry {
        id,
        size,
        entry_type,
        name_hash: 0, // Not returned in simple encoding
    })
}

// ============================================================================
// Simple hash function for file names
// ============================================================================

/// FNV-1a hash for file names (32-bit)
pub fn hash_name(name: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in name.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}
