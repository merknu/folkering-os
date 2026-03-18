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
//! and allows hot-swapping backends (ramdisk -> SQLite -> Vector DB) without kernel changes.
//!
//! # SQLite Backend (v2)
//!
//! This version supports SQLite databases created with `folk-pack create-sqlite`.
//! The database file is loaded into memory at startup and parsed using libsqlite.
//! File lookups use SQLite B-tree queries instead of linear scans.

#![no_std]
#![no_main]

use libfolk::{entry, println};
use libfolk::sys::{yield_cpu, get_pid};
use libfolk::sys::ipc::{receive, reply, IpcMessage};
use libfolk::sys::fs::{read_dir, DirEntry, read_file};
use libfolk::sys::synapse::{
    SYN_OP_PING, SYN_OP_LIST_FILES, SYN_OP_FILE_COUNT, SYN_OP_FILE_BY_INDEX,
    SYN_OP_FILE_INFO, SYN_OP_READ_FILE, SYN_OP_READ_FILE_BY_NAME, SYN_OP_READ_FILE_CHUNK,
    SYN_OP_READ_FILE_SHMEM, SYN_OP_SQL_QUERY,
    SYN_OP_VECTOR_SEARCH, SYN_OP_GET_EMBEDDING, SYN_OP_EMBEDDING_COUNT,
    SYN_STATUS_NOT_FOUND, SYN_STATUS_INVALID, SYN_STATUS_ERROR,
    SYNAPSE_VERSION, hash_name,
};
use libfolk::sys::{shmem_create, shmem_map, shmem_grant};
use libsqlite::{SqliteDb, Value};
use libsqlite::vector::{
    Embedding, SearchResult, search_similar_auto,
    get_embedding_by_file_id, count_embeddings, EMBEDDING_SIZE
};
use libsqlite::shadow::has_shadow_tables;

entry!(main);

/// Maximum cached directory entries (for FPK fallback)
const MAX_ENTRIES: usize = 16;

/// Maximum SQLite database size (256KB — must fit files.db with all ELFs + data)
const MAX_DB_SIZE: usize = 262144;

/// SQLite database filename
const DB_FILENAME: &str = "files.db";

/// File kind constants (match folk-pack create-sqlite)
const KIND_ELF: i64 = 0;
#[allow(dead_code)]
const KIND_DATA: i64 = 1;

/// Backend type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// Using FPK format (legacy)
    Fpk,
    /// Using SQLite database
    Sqlite,
}

/// Directory cache state for FPK backend - kept in a single struct to ensure memory layout
#[repr(C, align(64))]
struct DirCacheState {
    count: usize,
    valid: bool,
    _padding: [u8; 7],
    entries: [DirEntry; MAX_ENTRIES],
}

/// SQLite backend state
#[repr(C, align(4096))]
struct SqliteState {
    /// Raw database bytes
    data: [u8; MAX_DB_SIZE],
    /// Actual size of loaded database
    size: usize,
    /// Whether the database is valid
    valid: bool,
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

static mut SQLITE_STATE: SqliteState = SqliteState {
    data: [0u8; MAX_DB_SIZE],
    size: 0,
    valid: false,
};

static mut BACKEND: Backend = Backend::Fpk;

fn main() -> ! {
    let pid = get_pid();
    println!("[SYNAPSE] Data Kernel starting (PID: {})", pid);
    println!("[SYNAPSE] Protocol version: {}.{}",
             (SYNAPSE_VERSION >> 16) as u16,
             (SYNAPSE_VERSION & 0xFFFF) as u16);

    // Try to load SQLite database first, fall back to FPK
    if try_load_sqlite() {
        println!("[SYNAPSE] SQLite backend initialized");
        unsafe { BACKEND = Backend::Sqlite; }

        // Count files
        let file_count = count_sqlite_files();

        // Count embeddings - cap at file_count since we have 1:1 file:embedding
        let embedding_count = if let Some(db) = get_sqlite_db() {
            if let Ok(scanner) = db.table_scan("embeddings") {
                let mut cnt = 0;
                for result in scanner {
                    // Only count valid records
                    if result.is_ok() {
                        cnt += 1;
                    }
                    // Cap at file count (1:1 relationship)
                    if cnt >= file_count { break; }
                }
                cnt
            } else {
                0
            }
        } else {
            0
        };

        if embedding_count > 0 {
            // Check for quantized index
            let has_quantized = if let Some(db) = get_sqlite_db() {
                has_shadow_tables(&db)
            } else {
                false
            };

            if has_quantized {
                println!("[SYNAPSE] Ready - {} ({} files, {} embeddings)",
                         DB_FILENAME, file_count, embedding_count);
                println!("[SYNAPSE] Vector search enabled (quantized BQ+SQ8)");
            } else {
                println!("[SYNAPSE] Ready - {} ({} files, {} embeddings)",
                         DB_FILENAME, file_count, embedding_count);
                println!("[SYNAPSE] Vector search enabled (brute-force)");
            }
        } else {
            println!("[SYNAPSE] Ready - {} ({} files)", DB_FILENAME, file_count);
        }
    } else {
        println!("[SYNAPSE] SQLite not found, using FPK backend");
        unsafe { BACKEND = Backend::Fpk; }
        refresh_fpk_cache();
        println!("[SYNAPSE] Ready - {} files indexed (FPK)", unsafe { DIR_CACHE_STATE.count });
    }

    println!("[SYNAPSE] Entering service loop...\n");

    // Main service loop
    loop {
        match receive() {
            Ok(msg) => {
                handle_request(msg);
            }
            Err(_) => {
                yield_cpu();
            }
        }
    }
}

/// Try to load SQLite database from ramdisk
fn try_load_sqlite() -> bool {
    unsafe {
        // Try to read files.db
        let bytes_read = read_file(DB_FILENAME, &mut SQLITE_STATE.data);

        if bytes_read == 0 {
            return false;
        }

        SQLITE_STATE.size = bytes_read;

        // Verify it's a valid SQLite database
        if bytes_read < 100 {
            return false;
        }

        // Check SQLite magic
        if &SQLITE_STATE.data[0..16] != b"SQLite format 3\0" {
            return false;
        }

        SQLITE_STATE.valid = true;
        true
    }
}

/// Count files in SQLite database
fn count_sqlite_files() -> usize {
    unsafe {
        if !SQLITE_STATE.valid {
            return 0;
        }

        let db_data = &SQLITE_STATE.data[..SQLITE_STATE.size];
        let db = match SqliteDb::open(db_data) {
            Ok(db) => db,
            Err(_) => return 0,
        };

        let scanner = match db.table_scan("files") {
            Ok(s) => s,
            Err(_) => return 0,
        };

        let mut count = 0;
        for result in scanner {
            match result {
                Ok(_) => count += 1,
                Err(_) => break,
            }
            if count > 1000 { break; } // Safety cap
        }
        count
    }
}

/// Get a reference to the SQLite database
fn get_sqlite_db<'a>() -> Option<SqliteDb<'a>> {
    unsafe {
        if !SQLITE_STATE.valid {
            return None;
        }

        let db_data = &SQLITE_STATE.data[..SQLITE_STATE.size];
        SqliteDb::open(db_data).ok()
    }
}

/// Refresh the FPK directory cache from the ramdisk
fn refresh_fpk_cache() {
    unsafe {
        let result = read_dir(&mut DIR_CACHE_STATE.entries);
        DIR_CACHE_STATE.count = result;
        DIR_CACHE_STATE.valid = true;
    }
}

/// Handle an incoming IPC request
fn handle_request(msg: IpcMessage) {
    let op = msg.payload0 & 0xFFFF;

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
        SYN_OP_SQL_QUERY => handle_sql_query(msg),
        SYN_OP_VECTOR_SEARCH => handle_vector_search(msg),
        SYN_OP_GET_EMBEDDING => handle_get_embedding(msg),
        SYN_OP_EMBEDDING_COUNT => handle_embedding_count(msg),
        _ => {
            let _ = reply(SYN_STATUS_INVALID, 0);
        }
    }
}

/// Handle PING request
fn handle_ping(_msg: IpcMessage) {
    let _ = reply(SYNAPSE_VERSION, 0);
}

/// Handle FILE_COUNT request
fn handle_file_count(_msg: IpcMessage) {
    let count = match unsafe { BACKEND } {
        Backend::Sqlite => count_sqlite_files(),
        Backend::Fpk => {
            if !unsafe { DIR_CACHE_STATE.valid } {
                refresh_fpk_cache();
            }
            unsafe { DIR_CACHE_STATE.count }
        }
    };
    let _ = reply(count as u64, 0);
}

/// Handle FILE_BY_INDEX request
fn handle_file_by_index(msg: IpcMessage) {
    let index = ((msg.payload0 >> 16) & 0xFFFF) as usize;

    match unsafe { BACKEND } {
        Backend::Sqlite => {
            if let Some(db) = get_sqlite_db() {
                if let Ok(scanner) = db.table_scan("files") {
                    if let Some(Ok(record)) = scanner.skip(index).next() {
                        // files table: id, name, kind, size, data
                        let id = record.get(0).and_then(|v| v.as_int()).unwrap_or(0) as u64;
                        let kind = record.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                        let size = record.get(3).and_then(|v| v.as_int()).unwrap_or(0) as u64;

                        // Pack response: (id << 48) | (size << 16) | type
                        let entry_type = if kind == KIND_ELF { 1u64 } else { 0u64 };
                        let response = (id << 48) | (size << 16) | entry_type;
                        let _ = reply(response, 0);
                        return;
                    }
                }
            }
            let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        }
        Backend::Fpk => {
            if !unsafe { DIR_CACHE_STATE.valid } {
                refresh_fpk_cache();
            }

            let count = unsafe { DIR_CACHE_STATE.count };
            if index >= count {
                let _ = reply(SYN_STATUS_NOT_FOUND, 0);
                return;
            }

            let entry = unsafe { &DIR_CACHE_STATE.entries[index] };
            let response = ((entry.id as u64) << 48)
                         | ((entry.size as u64) << 16)
                         | (entry.entry_type as u64);
            let _ = reply(response, 0);
        }
    }
}

/// Handle LIST_FILES request
fn handle_list_files(_msg: IpcMessage) {
    let count = match unsafe { BACKEND } {
        Backend::Sqlite => count_sqlite_files(),
        Backend::Fpk => {
            if !unsafe { DIR_CACHE_STATE.valid } {
                refresh_fpk_cache();
            }
            unsafe { DIR_CACHE_STATE.count }
        }
    };
    let _ = reply(count as u64, 0);
}

/// Handle FILE_INFO request (by name hash)
fn handle_file_info(_msg: IpcMessage) {
    // For now, just return not found - need shared memory for string passing
    let _ = reply(SYN_STATUS_NOT_FOUND, 0);
}

/// Handle READ_FILE request (legacy)
fn handle_read_file(msg: IpcMessage) {
    let file_id = ((msg.payload0 >> 16) & 0xFFFF) as usize;

    match unsafe { BACKEND } {
        Backend::Sqlite => {
            if let Some(db) = get_sqlite_db() {
                if let Ok(scanner) = db.table_scan("files") {
                    for result in scanner {
                        if let Ok(record) = result {
                            let id = record.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
                            if id as usize == file_id {
                                let kind = record.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                                let size = record.get(3).and_then(|v| v.as_int()).unwrap_or(0);
                                let entry_type = if kind == KIND_ELF { 1u64 } else { 0u64 };
                                let response = ((size as u64) << 32) | entry_type;
                                let _ = reply(response, 0);
                                return;
                            }
                        }
                    }
                }
            }
            let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        }
        Backend::Fpk => {
            if !unsafe { DIR_CACHE_STATE.valid } {
                refresh_fpk_cache();
            }

            let entry = unsafe {
                DIR_CACHE_STATE.entries[..DIR_CACHE_STATE.count]
                    .iter()
                    .find(|e| e.id as usize == file_id)
            };

            match entry {
                Some(e) => {
                    let response = ((e.size as u64) << 32) | (e.entry_type as u64);
                    let _ = reply(response, 0);
                }
                None => {
                    let _ = reply(SYN_STATUS_NOT_FOUND, 0);
                }
            }
        }
    }
}

/// Handle READ_FILE_BY_NAME request
fn handle_read_file_by_name(msg: IpcMessage) {
    let request_hash = (msg.payload0 >> 16) as u32;

    match unsafe { BACKEND } {
        Backend::Sqlite => {
            if let Some(db) = get_sqlite_db() {
                if let Ok(scanner) = db.table_scan("files") {
                    for result in scanner {
                        if let Ok(record) = result {
                            if let Some(Value::Text(name)) = record.get(1) {
                                if hash_name(name) == request_hash {
                                    let id = record.get(0).and_then(|v| v.as_int()).unwrap_or(0);
                                    let size = record.get(3).and_then(|v| v.as_int()).unwrap_or(0);
                                    let response = ((size as u64) << 32) | (id as u64);
                                    let _ = reply(response, 0);
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        }
        Backend::Fpk => {
            if !unsafe { DIR_CACHE_STATE.valid } {
                refresh_fpk_cache();
            }

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
                    let response = ((e.size as u64) << 32) | (e.id as u64);
                    let _ = reply(response, 0);
                }
                None => {
                    let _ = reply(SYN_STATUS_NOT_FOUND, 0);
                }
            }
        }
    }
}

/// Handle READ_FILE_CHUNK request
fn handle_read_file_chunk(msg: IpcMessage) {
    let file_id = ((msg.payload0 >> 16) & 0xFFFF) as u16;
    let offset = (msg.payload0 >> 32) as u32;

    match unsafe { BACKEND } {
        Backend::Sqlite => {
            // For SQLite, read the BLOB data directly
            if let Some(db) = get_sqlite_db() {
                if let Ok(scanner) = db.table_scan("files") {
                    for result in scanner {
                        if let Ok(record) = result {
                            let id = record.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
                            if id as u16 == file_id {
                                if let Some(Value::Blob(data)) = record.get(4) {
                                    let offset = offset as usize;
                                    if offset >= data.len() {
                                        let _ = reply(0, 0); // EOF
                                        return;
                                    }

                                    let chunk_end = (offset + 8).min(data.len());
                                    let mut chunk: u64 = 0;
                                    for (i, &byte) in data[offset..chunk_end].iter().enumerate() {
                                        chunk |= (byte as u64) << (i * 8);
                                    }
                                    let _ = reply(chunk, 0);
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        }
        Backend::Fpk => {
            // FPK implementation (same as before)
            if !unsafe { DIR_CACHE_STATE.valid } {
                refresh_fpk_cache();
            }

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

            if offset as u64 >= entry.size {
                let _ = reply(0, 0);
                return;
            }

            let mut buf = [0u8; 4096];
            let name = entry.name_str();
            let bytes_read = read_file(name, &mut buf);

            if bytes_read == 0 {
                let _ = reply(SYN_STATUS_NOT_FOUND, 0);
                return;
            }

            let chunk_start = offset as usize;
            let chunk_end = (chunk_start + 8).min(bytes_read);

            if chunk_start >= bytes_read {
                let _ = reply(0, 0);
                return;
            }

            let mut chunk: u64 = 0;
            for (i, &byte) in buf[chunk_start..chunk_end].iter().enumerate() {
                chunk |= (byte as u64) << (i * 8);
            }
            let _ = reply(chunk, 0);
        }
    }
}

/// Virtual address for Synapse's shared memory buffer mapping
const SHMEM_BUFFER_VADDR: usize = 0x10000000;

/// Handle READ_FILE_SHMEM request (zero-copy file read)
fn handle_read_file_shmem(msg: IpcMessage) {
    let request_hash = (msg.payload0 >> 16) as u32;
    let requester_task = msg.sender;

    match unsafe { BACKEND } {
        Backend::Sqlite => {
            // For SQLite, we read the BLOB data directly
            if let Some(db) = get_sqlite_db() {
                if let Ok(scanner) = db.table_scan("files") {
                    for result in scanner {
                        if let Ok(record) = result {
                            if let Some(Value::Text(name)) = record.get(1) {
                                if hash_name(name) == request_hash {
                                    if let Some(Value::Blob(data)) = record.get(4) {
                                        let file_size = data.len();
                                        let buffer_size = ((file_size + 4095) / 4096) * 4096;
                                        let buffer_size = if buffer_size == 0 { 4096 } else { buffer_size };

                                        let shmem_handle = match shmem_create(buffer_size) {
                                            Ok(handle) => handle,
                                            Err(_) => {
                                                let _ = reply(SYN_STATUS_ERROR, 0);
                                                return;
                                            }
                                        };

                                        if shmem_grant(shmem_handle, requester_task).is_err() {
                                            let _ = reply(SYN_STATUS_ERROR, 0);
                                            return;
                                        }

                                        if shmem_map(shmem_handle, SHMEM_BUFFER_VADDR).is_err() {
                                            let _ = reply(SYN_STATUS_ERROR, 0);
                                            return;
                                        }

                                        // Copy BLOB data to shared memory
                                        let buffer_ptr = SHMEM_BUFFER_VADDR as *mut u8;
                                        unsafe {
                                            core::ptr::copy_nonoverlapping(
                                                data.as_ptr(),
                                                buffer_ptr,
                                                file_size
                                            );
                                        }

                                        let response = ((file_size as u64) << 32) | (shmem_handle as u64);
                                        let _ = reply(response, 0);
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let _ = reply(SYN_STATUS_NOT_FOUND, 0);
        }
        Backend::Fpk => {
            // FPK implementation (same as before)
            if !unsafe { DIR_CACHE_STATE.valid } {
                refresh_fpk_cache();
            }

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

            let buffer_size = ((file_size + 4095) / 4096) * 4096;
            let buffer_size = if buffer_size == 0 { 4096 } else { buffer_size };

            let shmem_handle = match shmem_create(buffer_size) {
                Ok(handle) => handle,
                Err(_) => {
                    let _ = reply(SYN_STATUS_ERROR, 0);
                    return;
                }
            };

            if shmem_grant(shmem_handle, requester_task).is_err() {
                let _ = reply(SYN_STATUS_ERROR, 0);
                return;
            }

            if shmem_map(shmem_handle, SHMEM_BUFFER_VADDR).is_err() {
                let _ = reply(SYN_STATUS_ERROR, 0);
                return;
            }

            let buffer_ptr = SHMEM_BUFFER_VADDR as *mut u8;
            let buffer_slice = unsafe {
                core::slice::from_raw_parts_mut(buffer_ptr, buffer_size)
            };

            let bytes_read = read_file(file_name, buffer_slice);

            if bytes_read == 0 {
                let _ = reply(SYN_STATUS_ERROR, 0);
                return;
            }

            let response = ((bytes_read as u64) << 32) | (shmem_handle as u64);
            let _ = reply(response, 0);
        }
    }
}

/// Handle SQL_QUERY request
/// For now, this returns file info for the SQL query type
fn handle_sql_query(msg: IpcMessage) {
    let query_type = ((msg.payload0 >> 16) & 0xFF) as u8;

    // Query types:
    // 0 = Get file count
    // 1 = List file names (returns count, names via shmem)

    match unsafe { BACKEND } {
        Backend::Sqlite => {
            match query_type {
                0 => {
                    // Get file count
                    let count = count_sqlite_files();
                    let _ = reply(count as u64, 0);
                }
                1 => {
                    // List file names - for now just return count
                    // Full implementation would use shared memory for the names
                    let count = count_sqlite_files();
                    let _ = reply(count as u64, 0);
                }
                _ => {
                    let _ = reply(SYN_STATUS_INVALID, 0);
                }
            }
        }
        Backend::Fpk => {
            // FPK doesn't support SQL queries
            let _ = reply(SYN_STATUS_INVALID, 0);
        }
    }
}

// ============================================================================
// Vector Search Handlers (Phase 5)
// ============================================================================

/// Virtual address for vector search query embedding mapping
const VECTOR_QUERY_VADDR: usize = 0x11000000;

/// Virtual address for vector search results mapping
const VECTOR_RESULTS_VADDR: usize = 0x12000000;

/// Handle VECTOR_SEARCH request
/// Request format: op | (k << 16) | (shmem_handle << 32)
/// Reply: (result_count << 32) | shmem_handle_with_results
fn handle_vector_search(msg: IpcMessage) {
    let k = ((msg.payload0 >> 16) & 0xFF) as usize;
    let query_shmem = (msg.payload0 >> 32) as u32;
    let requester_task = msg.sender;

    // Only SQLite backend supports vector search
    if unsafe { BACKEND } != Backend::Sqlite {
        let _ = reply(SYN_STATUS_INVALID, 0);
        return;
    }

    // Validate k
    if k == 0 || k > 100 {
        let _ = reply(SYN_STATUS_INVALID, 0);
        return;
    }

    // Map the query embedding from shared memory
    if query_shmem == 0 {
        let _ = reply(SYN_STATUS_INVALID, 0);
        return;
    }

    if shmem_map(query_shmem, VECTOR_QUERY_VADDR).is_err() {
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    // Read the query embedding from shared memory
    let query_ptr = VECTOR_QUERY_VADDR as *const u8;
    let query_slice = unsafe { core::slice::from_raw_parts(query_ptr, EMBEDDING_SIZE) };

    let query_embedding = match Embedding::from_blob(query_slice) {
        Ok(e) => e,
        Err(_) => {
            let _ = reply(SYN_STATUS_INVALID, 0);
            return;
        }
    };

    // Perform the search
    let db = match get_sqlite_db() {
        Some(db) => db,
        None => {
            let _ = reply(SYN_STATUS_ERROR, 0);
            return;
        }
    };

    // Stack-allocated results buffer (max 100 results)
    let mut results = [SearchResult::default(); 100];
    // Use auto search: quantized if available, brute-force otherwise
    let result_count = match search_similar_auto(&db, &query_embedding, k, &mut results) {
        Ok(count) => count,
        Err(_) => {
            let _ = reply(SYN_STATUS_ERROR, 0);
            return;
        }
    };

    if result_count == 0 {
        // Return zero results (no shmem needed)
        let _ = reply(0, 0);
        return;
    }

    // Create shared memory for results
    // Each result is 8 bytes (4 bytes file_id + 4 bytes similarity)
    let results_size = result_count * 8;
    let buffer_size = ((results_size + 4095) / 4096) * 4096;
    let buffer_size = if buffer_size == 0 { 4096 } else { buffer_size };

    let result_shmem = match shmem_create(buffer_size) {
        Ok(handle) => handle,
        Err(_) => {
            let _ = reply(SYN_STATUS_ERROR, 0);
            return;
        }
    };

    if shmem_grant(result_shmem, requester_task).is_err() {
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    if shmem_map(result_shmem, VECTOR_RESULTS_VADDR).is_err() {
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    // Copy results to shared memory
    let results_ptr = VECTOR_RESULTS_VADDR as *mut u8;
    unsafe {
        for (i, result) in results[..result_count].iter().enumerate() {
            let offset = i * 8;
            // Write file_id (4 bytes, little-endian)
            let file_id_bytes = result.file_id.to_le_bytes();
            core::ptr::copy_nonoverlapping(
                file_id_bytes.as_ptr(),
                results_ptr.add(offset),
                4
            );
            // Write similarity (4 bytes, little-endian f32)
            let sim_bytes = result.similarity.to_le_bytes();
            core::ptr::copy_nonoverlapping(
                sim_bytes.as_ptr(),
                results_ptr.add(offset + 4),
                4
            );
        }
    }

    // Reply with count and shmem handle
    let response = ((result_count as u64) << 32) | (result_shmem as u64);
    let _ = reply(response, 0);
}

/// Handle GET_EMBEDDING request
/// Request format: op | (file_id << 16)
/// Reply: (size << 32) | shmem_handle
fn handle_get_embedding(msg: IpcMessage) {
    let file_id = ((msg.payload0 >> 16) & 0xFFFF) as u32;
    let requester_task = msg.sender;

    // Only SQLite backend supports embeddings
    if unsafe { BACKEND } != Backend::Sqlite {
        let _ = reply(SYN_STATUS_INVALID, 0);
        return;
    }

    let db = match get_sqlite_db() {
        Some(db) => db,
        None => {
            let _ = reply(SYN_STATUS_ERROR, 0);
            return;
        }
    };

    // Look up the embedding
    let embedding = match get_embedding_by_file_id(&db, file_id) {
        Ok(Some(e)) => e,
        Ok(None) => {
            let _ = reply(SYN_STATUS_NOT_FOUND, 0);
            return;
        }
        Err(_) => {
            let _ = reply(SYN_STATUS_ERROR, 0);
            return;
        }
    };

    // Create shared memory for the embedding
    let shmem_handle = match shmem_create(4096) {
        Ok(handle) => handle,
        Err(_) => {
            let _ = reply(SYN_STATUS_ERROR, 0);
            return;
        }
    };

    if shmem_grant(shmem_handle, requester_task).is_err() {
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    if shmem_map(shmem_handle, VECTOR_QUERY_VADDR).is_err() {
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    // Copy embedding to shared memory
    let buffer_ptr = VECTOR_QUERY_VADDR as *mut u8;
    let buffer_slice = unsafe {
        core::slice::from_raw_parts_mut(buffer_ptr, EMBEDDING_SIZE)
    };
    embedding.to_blob(buffer_slice);

    // Reply with size and shmem handle
    let response = ((EMBEDDING_SIZE as u64) << 32) | (shmem_handle as u64);
    let _ = reply(response, 0);
}

/// Handle EMBEDDING_COUNT request
fn handle_embedding_count(_msg: IpcMessage) {
    // Only SQLite backend has embeddings
    if unsafe { BACKEND } != Backend::Sqlite {
        let _ = reply(0, 0);
        return;
    }

    let db = match get_sqlite_db() {
        Some(db) => db,
        None => {
            let _ = reply(0, 0);
            return;
        }
    };

    let count = count_embeddings(&db).unwrap_or(0);
    let _ = reply(count as u64, 0);
}
