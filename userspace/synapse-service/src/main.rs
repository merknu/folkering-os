//! Synapse - The Data Kernel for Folkering OS
//!
//! Synapse is a userspace service that manages all data operations for the system.
//! It provides a unified IPC interface for file access, queries, and (eventually)
//! AI-powered semantic search.
//!
//! # Architecture
//!
//! Synapse runs as Task 2 at system boot. Other tasks send IPC messages to request
//! data operations. This decouples the filesystem implementation from the kernel
//! and allows hot-swapping backends (ramdisk → SQLite → Vector DB) without kernel changes.
//!
//! # Current Implementation (v1)
//!
//! - Wraps the kernel's ramdisk syscalls
//! - Provides file listing and reading
//! - Stateless (no caching yet)
//!
//! # Future (v2+)
//!
//! - SQLite backend for structured data
//! - Vector embeddings for semantic search
//! - Write support
//! - Caching and indexing

#![no_std]
#![no_main]

use libfolk::{entry, println};
use libfolk::sys::{yield_cpu, get_pid};
use libfolk::sys::ipc::{receive, reply, IpcMessage};
use libfolk::sys::fs::{read_dir, DirEntry};
use libfolk::sys::synapse::{
    SYN_OP_PING, SYN_OP_LIST_FILES, SYN_OP_FILE_COUNT, SYN_OP_FILE_BY_INDEX,
    SYN_OP_FILE_INFO, SYN_OP_READ_FILE,
    SYN_STATUS_OK, SYN_STATUS_NOT_FOUND, SYN_STATUS_INVALID,
    SYNAPSE_VERSION,
};

entry!(main);

/// Maximum cached directory entries
const MAX_ENTRIES: usize = 16;

/// Cached directory entries (loaded on first LIST request)
static mut DIR_CACHE: [DirEntry; MAX_ENTRIES] = [DirEntry {
    id: 0,
    entry_type: 0,
    name: [0u8; 32],
    size: 0,
}; MAX_ENTRIES];

static mut DIR_CACHE_COUNT: usize = 0;
static mut DIR_CACHE_VALID: bool = false;

fn main() -> ! {
    let pid = get_pid();
    println!("[SYNAPSE] Data Kernel starting (PID: {})", pid);
    println!("[SYNAPSE] Protocol version: {}.{}",
             (SYNAPSE_VERSION >> 16) as u16,
             (SYNAPSE_VERSION & 0xFFFF) as u16);

    // Skip cache loading for now - causes GPF in kernel copy path
    // TODO: Fix kernel userspace copy, then re-enable
    // refresh_cache();

    println!("[SYNAPSE] Ready (lazy loading mode)");
    println!("[SYNAPSE] Entering service loop...\n");

    // Main service loop
    loop {
        match receive() {
            Ok(msg) => {
                handle_request(msg);
            }
            Err(_) => {
                // No messages, yield CPU
                yield_cpu();
            }
        }
    }
}

/// Refresh the directory cache from the ramdisk
fn refresh_cache() {
    unsafe {
        DIR_CACHE_COUNT = read_dir(&mut DIR_CACHE);
        DIR_CACHE_VALID = true;
    }
}

/// Handle an incoming IPC request
fn handle_request(msg: IpcMessage) {
    let op = msg.payload0;

    // The second payload word contains the parameter (packed in upper bits of return)
    // For now, we only use payload0 for the operation code
    // Future: use full IpcMessage with 4 payload slots

    match op {
        SYN_OP_PING => handle_ping(msg),
        SYN_OP_FILE_COUNT => handle_file_count(msg),
        SYN_OP_FILE_BY_INDEX => handle_file_by_index(msg),
        SYN_OP_LIST_FILES => handle_list_files(msg),
        SYN_OP_FILE_INFO => handle_file_info(msg),
        SYN_OP_READ_FILE => handle_read_file(msg),
        _ => {
            // Unknown operation
            let _ = reply(SYN_STATUS_INVALID, 0);
        }
    }
}

/// Handle PING request
fn handle_ping(msg: IpcMessage) {
    // Extract magic from the packed return value
    // In the simple protocol, the "parameter" is in the upper bits
    // For ping, we just return a transformed magic
    let magic = msg.payload0 >> 16; // Parameter is in upper bits?

    // Actually, with current simple IPC, we don't have separate payload slots
    // The shell sends: syscall3(IPC_SEND, target, op, param)
    // We receive: payload0 = (param << 32) | sender... actually it's more complex

    // For now, just reply with version and success
    let response = SYNAPSE_VERSION;
    let _ = reply(response, 0);
}

/// Handle FILE_COUNT request
fn handle_file_count(_msg: IpcMessage) {
    // Lazy load cache on first access
    if !unsafe { DIR_CACHE_VALID } {
        // For now, return 0 to avoid GPF in kernel copy path
        // TODO: Fix kernel userspace copy, then enable refresh_cache()
        let _ = reply(0, 0);
        return;
    }

    let count = unsafe { DIR_CACHE_COUNT };
    let _ = reply(count as u64, 0);
}

/// Handle FILE_BY_INDEX request
fn handle_file_by_index(msg: IpcMessage) {
    // Index is encoded in the upper bits of payload0
    let index = ((msg.payload0 >> 16) & 0xFFFF) as usize;

    if !unsafe { DIR_CACHE_VALID } {
        // Cache not loaded, return not found
        // TODO: Enable lazy loading once kernel copy is fixed
        let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        return;
    }

    let count = unsafe { DIR_CACHE_COUNT };
    if index >= count {
        let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        return;
    }

    let entry = unsafe { &DIR_CACHE[index] };

    // Pack response: (id << 48) | (size << 16) | type
    let response = ((entry.id as u64) << 48)
                 | ((entry.size as u64) << 16)
                 | (entry.entry_type as u64);

    let _ = reply(response, 0);
}

/// Handle LIST_FILES request (returns count, details via repeated FILE_BY_INDEX)
fn handle_list_files(_msg: IpcMessage) {
    if !unsafe { DIR_CACHE_VALID } {
        // Cache not loaded
        let _ = reply(0, 0);
        return;
    }

    let count = unsafe { DIR_CACHE_COUNT };
    let _ = reply(count as u64, 0);
}

/// Handle FILE_INFO request (by name hash)
fn handle_file_info(msg: IpcMessage) {
    // Name hash is in the parameter
    let _name_hash = msg.payload0 >> 16;

    // For now, just return not found - we need the full name to look up
    // This will be implemented when we have shared memory for string passing
    let _ = reply(SYN_STATUS_NOT_FOUND, 0);
}

/// Handle READ_FILE request
fn handle_read_file(msg: IpcMessage) {
    // File ID is encoded in the parameter
    let _file_id = ((msg.payload0 >> 16) & 0xFFFF) as usize;

    if !unsafe { DIR_CACHE_VALID } {
        // Cache not loaded, return not found
        let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        return;
    }

    // TODO: Implement file reading once kernel copy is fixed
    let _ = reply(SYN_STATUS_NOT_FOUND, 0);
}
