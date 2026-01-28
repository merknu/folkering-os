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
    SYN_OP_FILE_INFO, SYN_OP_READ_FILE, SYN_OP_READ_FILE_BY_NAME, SYN_OP_READ_FILE_CHUNK,
    SYN_OP_READ_FILE_SHMEM,
    SYN_STATUS_NOT_FOUND, SYN_STATUS_INVALID, SYN_STATUS_ERROR,
    SYNAPSE_VERSION, hash_name,
};
use libfolk::sys::fs::read_file;
use libfolk::sys::{shmem_create, shmem_map, shmem_grant};

entry!(main);

/// Maximum cached directory entries
const MAX_ENTRIES: usize = 16;

/// Directory cache state - kept in a single struct to ensure memory layout
/// doesn't cause overlapping static variables.
/// Aligned to 64 bytes (cache line) to prevent any overlap with adjacent data.
#[repr(C, align(64))]
struct DirCacheState {
    count: usize,
    valid: bool,
    _padding: [u8; 7], // Align entries to 8 bytes
    entries: [DirEntry; MAX_ENTRIES],
}

static mut DIR_CACHE_STATE: DirCacheState = DirCacheState {
    count: 0,
    valid: false,
    _padding: [0; 7],
    entries: [DirEntry {
        id: 0,
        entry_type: 0,
        name: [0u8; 32],
        size: 0,
    }; MAX_ENTRIES],
};

fn main() -> ! {
    let pid = get_pid();
    println!("[SYNAPSE] Data Kernel starting (PID: {})", pid);
    println!("[SYNAPSE] Protocol version: {}.{}",
             (SYNAPSE_VERSION >> 16) as u16,
             (SYNAPSE_VERSION & 0xFFFF) as u16);

    // Load directory cache on startup
    refresh_cache();

    println!("[SYNAPSE] Ready - {} files indexed", unsafe { DIR_CACHE_STATE.count });
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
        let buf_ptr = DIR_CACHE_STATE.entries.as_mut_ptr() as usize;
        println!("[SYNAPSE] Calling read_dir with buf={:#x}", buf_ptr);
        let result = read_dir(&mut DIR_CACHE_STATE.entries);
        println!("[SYNAPSE] read_dir returned: {}", result);
        DIR_CACHE_STATE.count = result;
        DIR_CACHE_STATE.valid = true;
    }
}

/// Handle an incoming IPC request
fn handle_request(msg: IpcMessage) {
    // Operation code is in the low 16 bits of payload0
    let op = msg.payload0 & 0xFFFF;

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
        SYN_OP_READ_FILE_BY_NAME => handle_read_file_by_name(msg),
        SYN_OP_READ_FILE_CHUNK => handle_read_file_chunk(msg),
        SYN_OP_READ_FILE_SHMEM => handle_read_file_shmem(msg),
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
    if !unsafe { DIR_CACHE_STATE.valid } {
        refresh_cache();
    }

    let count = unsafe { DIR_CACHE_STATE.count };
    let _ = reply(count as u64, 0);
}

/// Handle FILE_BY_INDEX request
fn handle_file_by_index(msg: IpcMessage) {
    // Index is encoded in the upper bits of payload0
    let index = ((msg.payload0 >> 16) & 0xFFFF) as usize;

    if !unsafe { DIR_CACHE_STATE.valid } {
        refresh_cache();
    }

    let count = unsafe { DIR_CACHE_STATE.count };
    if index >= count {
        let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        return;
    }

    let entry = unsafe { &DIR_CACHE_STATE.entries[index] };

    // Pack response: (id << 48) | (size << 16) | type
    let response = ((entry.id as u64) << 48)
                 | ((entry.size as u64) << 16)
                 | (entry.entry_type as u64);

    let _ = reply(response, 0);
}

/// Handle LIST_FILES request (returns count, details via repeated FILE_BY_INDEX)
fn handle_list_files(_msg: IpcMessage) {
    if !unsafe { DIR_CACHE_STATE.valid } {
        refresh_cache();
    }

    let count = unsafe { DIR_CACHE_STATE.count };
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

/// Handle READ_FILE request (legacy - returns metadata only)
fn handle_read_file(msg: IpcMessage) {
    // File ID is encoded in the parameter
    let file_id = ((msg.payload0 >> 16) & 0xFFFF) as usize;

    if !unsafe { DIR_CACHE_STATE.valid } {
        refresh_cache();
    }

    // Find file by ID
    let entry = unsafe {
        DIR_CACHE_STATE.entries[..DIR_CACHE_STATE.count]
            .iter()
            .find(|e| e.id as usize == file_id)
    };

    match entry {
        Some(e) => {
            // Return file size and type
            // Actual content reading requires shared memory (future)
            let response = ((e.size as u64) << 32) | (e.entry_type as u64);
            let _ = reply(response, 0);
        }
        None => {
            let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        }
    }
}

/// Handle READ_FILE_BY_NAME request
/// Looks up a file by name hash and returns (size, file_id)
fn handle_read_file_by_name(msg: IpcMessage) {
    // Name hash is in upper bits of payload0
    let request_hash = (msg.payload0 >> 16) as u32;

    if !unsafe { DIR_CACHE_STATE.valid } {
        refresh_cache();
    }

    // Search for file by name hash
    let entry = unsafe {
        DIR_CACHE_STATE.entries[..DIR_CACHE_STATE.count]
            .iter()
            .find(|e| {
                let name = e.name_str();
                hash_name(name) == request_hash
            })
    };

    match entry {
        Some(e) => {
            // Pack response: (size << 32) | file_id
            let response = ((e.size as u64) << 32) | (e.id as u64);
            let _ = reply(response, 0);
        }
        None => {
            let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        }
    }
}

/// Handle READ_FILE_CHUNK request
/// Reads 8 bytes from a file at the given offset
fn handle_read_file_chunk(msg: IpcMessage) {
    // File ID is in upper bits of payload0
    let file_id = ((msg.payload0 >> 16) & 0xFFFF) as u16;

    // Offset is passed as the second argument via IPC
    // In current simple IPC, this comes as upper bits of the packed message
    // Actually, looking at how IPC_SEND works: syscall3(IPC_SEND, target, payload0, payload1)
    // payload1 would be the offset... but we only receive payload0 in IpcMessage!
    // For now, we need to encode offset differently.

    // WORKAROUND: Since we can't get payload1, let's pass offset in msg.sender upper bits
    // Actually, let's check how the message is structured...
    // The sender field has the sender task ID, so we can't use that.

    // NEW APPROACH: For Phase 3.1, we'll use a simpler protocol:
    // The shell will make repeated FILE_BY_INDEX calls to get filenames,
    // then use READ_FILE_BY_NAME which we just implemented.
    // For chunk reading, we need to store the "current file" state.

    // For now, just implement a simple version that reads from start
    // Offset will be encoded in the lower bits above the opcode
    // payload0 format: (offset_high << 32) | (file_id << 16) | op
    // Actually, let's use a different encoding:
    // We receive payload0 = (param << 32) | sender from IPC receive
    // But that's the kernel side... userspace send uses: syscall3(target, op|params, extra)

    // Let me check what we actually receive...
    // From ipc.rs receive(): "Return sender ID in lower 32 bits, first payload in upper 32 bits"
    // So payload0 = what the sender sent as their first argument

    // The sender sends: syscall3(IPC_SEND, SYNAPSE_TASK_ID, request, offset)
    // So we should be getting request in payload0... but where's offset?

    // Looking at syscall_ipc_send: it takes (target, payload0, payload1) and creates IpcMessage
    // But ipc_receive only returns payload[0] packed with sender ID!

    // This is a limitation. For now, let's just return error and document that
    // we need to fix the IPC protocol for Phase 3.2.

    // ACTUALLY - let's use a different approach:
    // Store the current file content in a buffer, and use FILE_BY_INDEX-style offset encoding:
    // payload0 = (offset << 32) | (file_id << 16) | op

    let offset = (msg.payload0 >> 32) as u32;

    if !unsafe { DIR_CACHE_STATE.valid } {
        refresh_cache();
    }

    // Find file by ID
    let entry = unsafe {
        DIR_CACHE_STATE.entries[..DIR_CACHE_STATE.count]
            .iter()
            .find(|e| e.id == file_id)
    };

    let entry = match entry {
        Some(e) => e,
        None => {
            let _ = reply(SYN_STATUS_NOT_FOUND, 0);
            return;
        }
    };

    // Check if offset is beyond file
    if offset as u64 >= entry.size {
        let _ = reply(0, 0); // EOF - return 0 bytes
        return;
    }

    // Read file content using syscall 14
    let mut buf = [0u8; 4096]; // Max 4KB file read
    let name = entry.name_str();
    let bytes_read = read_file(name, &mut buf);

    if bytes_read == 0 {
        let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        return;
    }

    // Extract the 8-byte chunk at offset
    let chunk_start = offset as usize;
    let chunk_end = (chunk_start + 8).min(bytes_read);

    if chunk_start >= bytes_read {
        let _ = reply(0, 0); // EOF
        return;
    }

    // Pack up to 8 bytes into a u64 (little-endian)
    let mut chunk: u64 = 0;
    for (i, &byte) in buf[chunk_start..chunk_end].iter().enumerate() {
        chunk |= (byte as u64) << (i * 8);
    }

    let _ = reply(chunk, 0);
}

/// Virtual address for Synapse's shared memory buffer mapping
/// Using a fixed address in userspace range that won't conflict with code/stack
const SHMEM_BUFFER_VADDR: usize = 0x10000000;

/// Handle READ_FILE_SHMEM request (zero-copy file read)
/// Creates shared memory, grants access to requester, loads file, returns handle
fn handle_read_file_shmem(msg: IpcMessage) {
    // Name hash is in upper bits of payload0
    let request_hash = (msg.payload0 >> 16) as u32;
    let requester_task = msg.sender;

    if !unsafe { DIR_CACHE_STATE.valid } {
        refresh_cache();
    }

    // Search for file by name hash
    let entry = unsafe {
        DIR_CACHE_STATE.entries[..DIR_CACHE_STATE.count]
            .iter()
            .find(|e| {
                let name = e.name_str();
                hash_name(name) == request_hash
            })
    };

    let entry = match entry {
        Some(e) => e,
        None => {
            let _ = reply(SYN_STATUS_NOT_FOUND, 0);
            return;
        }
    };

    let file_size = entry.size as usize;
    let file_name = entry.name_str();

    // Step 1: Create shared memory buffer (page-aligned size)
    let buffer_size = ((file_size + 4095) / 4096) * 4096; // Round up to page size
    let buffer_size = if buffer_size == 0 { 4096 } else { buffer_size }; // Min 1 page

    let shmem_handle = match shmem_create(buffer_size) {
        Ok(handle) => handle,
        Err(_) => {
            let _ = reply(SYN_STATUS_ERROR, 0);
            return;
        }
    };

    // Step 2: Grant access to the requesting task
    if shmem_grant(shmem_handle, requester_task).is_err() {
        // Failed to grant - can't continue
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    // Step 3: Map the buffer into Synapse's address space
    if shmem_map(shmem_handle, SHMEM_BUFFER_VADDR).is_err() {
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    // Step 4: Read file directly into the mapped buffer
    let buffer_ptr = SHMEM_BUFFER_VADDR as *mut u8;
    let buffer_slice = unsafe {
        core::slice::from_raw_parts_mut(buffer_ptr, buffer_size)
    };

    let bytes_read = read_file(file_name, buffer_slice);

    if bytes_read == 0 {
        // File read failed (but we already found it in cache, so this is weird)
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    // Step 5: Return (size << 32) | shmem_handle
    let response = ((bytes_read as u64) << 32) | (shmem_handle as u64);
    let _ = reply(response, 0);
}
