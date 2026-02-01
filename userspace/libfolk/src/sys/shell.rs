//! Shell Service Protocol - Command Execution IPC Interface
//!
//! The Shell service provides command execution capabilities to other tasks,
//! particularly the Compositor for omnibar command handling.
//!
//! # Architecture
//!
//! ```text
//! ┌────────────┐     IPC      ┌─────────┐
//! │ Compositor │◄────────────►│  Shell  │
//! │  (Omnibar) │              │ (Exec)  │
//! └────────────┘              └─────────┘
//! ```
//!
//! # Protocol
//!
//! Commands are sent via IPC with opcode in low 8 bits:
//! - SHELL_OP_LIST_FILES (0x80): List files in ramdisk
//! - SHELL_OP_CAT_FILE (0x81): Read file contents
//! - SHELL_OP_SEARCH (0x82): Keyword search
//! - SHELL_OP_PS (0x83): List processes
//! - SHELL_OP_UPTIME (0x84): Get system uptime

use crate::syscall::{syscall3, SYS_IPC_SEND};

// ============================================================================
// Well-Known Task IDs
// ============================================================================

/// Shell service task ID (spawned at boot as Task 3)
pub const SHELL_TASK_ID: u32 = 3;

// ============================================================================
// Operation Codes
// ============================================================================

/// List files in ramdisk
/// Request: opcode only (0x80)
/// Reply: (count << 32) | 0  (files follow via shmem if > 0)
pub const SHELL_OP_LIST_FILES: u64 = 0x80;

/// Read file by name hash
/// Request: op | (name_hash << 8)
/// Reply: (size << 32) | shmem_handle, or SHELL_STATUS_NOT_FOUND
pub const SHELL_OP_CAT_FILE: u64 = 0x81;

/// Keyword search
/// Request: op | (keyword_hash << 8)
/// Reply: (count << 32) | shmem_handle (results in shared memory)
pub const SHELL_OP_SEARCH: u64 = 0x82;

/// List running processes
/// Request: opcode only
/// Reply: task count in low 32 bits
pub const SHELL_OP_PS: u64 = 0x83;

/// Get system uptime
/// Request: opcode only
/// Reply: uptime in milliseconds
pub const SHELL_OP_UPTIME: u64 = 0x84;

/// Execute arbitrary command (via shared memory)
/// Request: op | (shmem_handle << 8)
/// Reply: (status << 32) | result_shmem_handle
pub const SHELL_OP_EXEC: u64 = 0x85;

// ============================================================================
// Status Codes
// ============================================================================

/// Operation succeeded
pub const SHELL_STATUS_OK: u64 = 0;

/// Resource not found
pub const SHELL_STATUS_NOT_FOUND: u64 = 1;

/// Invalid operation or parameter
pub const SHELL_STATUS_INVALID: u64 = 2;

/// Service busy
pub const SHELL_STATUS_BUSY: u64 = 3;

/// Internal error
pub const SHELL_STATUS_ERROR: u64 = 0xFFFF;

// ============================================================================
// Result Types
// ============================================================================

/// Result type for Shell operations
pub type ShellResult<T> = Result<T, ShellError>;

/// Shell error types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellError {
    /// Shell service not responding
    ServiceUnavailable,
    /// Resource not found
    NotFound,
    /// Invalid request
    InvalidRequest,
    /// IPC error
    IpcFailed,
    /// Unknown error
    Unknown(u64),
}

/// File entry from list_files
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct ShellFileEntry {
    /// Entry type (0=data, 1=elf)
    pub entry_type: u8,
    /// File size in bytes
    pub size: u32,
    /// File name (null-terminated, max 23 chars)
    pub name: [u8; 24],
}

impl ShellFileEntry {
    pub fn name_str(&self) -> &str {
        let len = self.name.iter().position(|&c| c == 0).unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..len]).unwrap_or("")
    }
}

/// Search result entry
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct ShellSearchResult {
    /// File name (null-terminated)
    pub name: [u8; 24],
    /// Match score (for ranking)
    pub score: u16,
}

/// Response from list_files
#[derive(Debug, Clone, Copy)]
pub struct ListFilesResponse {
    /// Number of files
    pub count: usize,
    /// Shared memory handle containing file entries (if count > 0)
    pub shmem_handle: u32,
}

/// Response from cat_file
#[derive(Debug, Clone, Copy)]
pub struct CatFileResponse {
    /// File size in bytes
    pub size: u32,
    /// Shared memory handle containing file contents
    pub shmem_handle: u32,
}

/// Response from search
#[derive(Debug, Clone, Copy)]
pub struct SearchResponse {
    /// Number of matching files
    pub count: usize,
    /// Shared memory handle containing results
    pub shmem_handle: u32,
}

// ============================================================================
// Client API
// ============================================================================

/// Simple hash function for names (same as Synapse)
pub fn hash_name(name: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in name.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

/// List files in ramdisk
pub fn list_files() -> ShellResult<ListFilesResponse> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SHELL_TASK_ID as u64, SHELL_OP_LIST_FILES, 0)
    };

    if ret == u64::MAX {
        return Err(ShellError::ServiceUnavailable);
    }

    let count = ((ret >> 32) & 0xFFFFFFFF) as usize;
    let shmem_handle = (ret & 0xFFFFFFFF) as u32;

    Ok(ListFilesResponse { count, shmem_handle })
}

/// Read a file by name
pub fn cat_file(name: &str) -> ShellResult<CatFileResponse> {
    let name_hash = hash_name(name);
    let request = SHELL_OP_CAT_FILE | ((name_hash as u64) << 8);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SHELL_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        return Err(ShellError::ServiceUnavailable);
    }

    if ret == SHELL_STATUS_NOT_FOUND {
        return Err(ShellError::NotFound);
    }

    let size = ((ret >> 32) & 0xFFFFFFFF) as u32;
    let shmem_handle = (ret & 0xFFFFFFFF) as u32;

    if shmem_handle == 0 {
        return Err(ShellError::IpcFailed);
    }

    Ok(CatFileResponse { size, shmem_handle })
}

/// Search for files matching keyword
pub fn search(keyword: &str) -> ShellResult<SearchResponse> {
    let keyword_hash = hash_name(keyword);
    let request = SHELL_OP_SEARCH | ((keyword_hash as u64) << 8);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SHELL_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        return Err(ShellError::ServiceUnavailable);
    }

    let count = ((ret >> 32) & 0xFFFFFFFF) as usize;
    let shmem_handle = (ret & 0xFFFFFFFF) as u32;

    Ok(SearchResponse { count, shmem_handle })
}

/// Get number of running tasks
pub fn ps() -> ShellResult<usize> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SHELL_TASK_ID as u64, SHELL_OP_PS, 0)
    };

    if ret == u64::MAX {
        return Err(ShellError::ServiceUnavailable);
    }

    Ok((ret & 0xFFFFFFFF) as usize)
}

/// Get system uptime in milliseconds
pub fn get_uptime() -> ShellResult<u64> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SHELL_TASK_ID as u64, SHELL_OP_UPTIME, 0)
    };

    if ret == u64::MAX {
        return Err(ShellError::ServiceUnavailable);
    }

    Ok(ret)
}
