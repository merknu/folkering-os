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
use libfolk::sys::{yield_cpu, get_pid, shmem_unmap, shmem_destroy};
use libfolk::sys::ipc::{recv_async, reply_with_token, AsyncIpcMessage};
use libfolk::sys::fs::{read_dir, DirEntry, read_file};
use libfolk::sys::block;
use libfolk::sys::synapse::{
    SYN_OP_PING, SYN_OP_LIST_FILES, SYN_OP_FILE_COUNT, SYN_OP_FILE_BY_INDEX,
    SYN_OP_FILE_INFO, SYN_OP_READ_FILE, SYN_OP_READ_FILE_BY_NAME, SYN_OP_READ_FILE_CHUNK,
    SYN_OP_READ_FILE_SHMEM, SYN_OP_SQL_QUERY,
    SYN_OP_VECTOR_SEARCH, SYN_OP_GET_EMBEDDING, SYN_OP_EMBEDDING_COUNT,
    SYN_OP_WRITE_FILE,
    SYN_STATUS_NOT_FOUND, SYN_STATUS_INVALID, SYN_STATUS_ERROR,
    SYNAPSE_VERSION, hash_name,
};
use libfolk::sys::{shmem_create, shmem_map, shmem_grant};
use libsqlite::{SqliteDb, Value, encode_varint};
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

/// Current IPC caller token (set by main loop before each handle_request)
static mut CURRENT_TOKEN: Option<libfolk::sys::ipc::CallerToken> = None;

/// Reply to the current IPC request using the stored CallerToken
fn reply(payload0: u64, payload1: u64) -> Result<(), libfolk::sys::ipc::IpcError> {
    if let Some(token) = unsafe { CURRENT_TOKEN.take() } {
        reply_with_token(token, payload0, payload1)
    } else {
        Err(libfolk::sys::ipc::IpcError::WouldBlock)
    }
}

fn main() -> ! {
    let pid = get_pid();
    println!("[SYNAPSE] Data Kernel starting (PID: {})", pid);
    println!("[SYNAPSE] Protocol version: {}.{}",
             (SYNAPSE_VERSION >> 16) as u16,
             (SYNAPSE_VERSION & 0xFFFF) as u16);

    // Try to load SQLite database:
    // 1. VirtIO disk (persistent) — preferred
    // 2. Ramdisk FPK (volatile) — fallback
    if try_load_sqlite_from_disk() {
        println!("[SYNAPSE] SQLite loaded from VirtIO disk (persistent!)");
        unsafe { BACKEND = Backend::Sqlite; }
        refresh_sqlite_cache();
        let file_count = unsafe { DIR_CACHE_STATE.count };
        println!("[SYNAPSE] Ready - {} ({} files, VirtIO)", DB_FILENAME, file_count);
    } else if try_load_sqlite() {
        println!("[SYNAPSE] SQLite loaded from ramdisk (volatile)");
        unsafe { BACKEND = Backend::Sqlite; }
        refresh_sqlite_cache();
        let file_count = unsafe { DIR_CACHE_STATE.count };
        println!("[SYNAPSE] Ready - {} ({} files, ramdisk)", DB_FILENAME, file_count);
    } else {
        println!("[SYNAPSE] SQLite not found, using FPK backend");
        unsafe { BACKEND = Backend::Fpk; }
        refresh_fpk_cache();
        println!("[SYNAPSE] Ready - {} files indexed (FPK)", unsafe { DIR_CACHE_STATE.count });
    }

    println!("[SYNAPSE] Entering service loop...\n");

    // Main service loop — use recv_async for full 64-bit payload
    loop {
        match recv_async() {
            Ok(msg) => {
                // Store token for reply
                unsafe { CURRENT_TOKEN = Some(msg.token); }
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

/// Try to load SQLite database from VirtIO block device
///
/// Reads the FOLKDISK header to find synapse_db_sector/size, then reads
/// the database directly from disk sectors into SQLITE_STATE.
fn try_load_sqlite_from_disk() -> bool {
    // Read sector 0 (disk header)
    let mut header_buf = [0u8; block::SECTOR_SIZE];
    match block::read_sector(0, &mut header_buf) {
        Ok(()) => {}
        Err(e) => {
            println!("[SYNAPSE] VirtIO header read failed: {:?}", e);
            return false;
        }
    }

    // Check FOLKDISK magic
    if &header_buf[0..8] != b"FOLKDISK" {
        return false;
    }

    // Parse header fields (little-endian):
    // DiskHeader layout: magic(8) + version(4) + pad(4) + journal_start(8) +
    //   journal_size(8) + data_start(8) + data_size(8) + synapse_db_sector(8) + synapse_db_size(8)
    // offset 48: synapse_db_sector (u64)
    // offset 56: synapse_db_size (u64, in sectors)
    let db_sector = u64::from_le_bytes([
        header_buf[48], header_buf[49], header_buf[50], header_buf[51],
        header_buf[52], header_buf[53], header_buf[54], header_buf[55],
    ]);
    let db_sectors = u64::from_le_bytes([
        header_buf[56], header_buf[57], header_buf[58], header_buf[59],
        header_buf[60], header_buf[61], header_buf[62], header_buf[63],
    ]);

    println!("[SYNAPSE] VirtIO header: db_sector={}, db_sectors={}", db_sector, db_sectors);

    if db_sector == 0 || db_sectors == 0 {
        println!("[SYNAPSE] VirtIO: no DB location in header");
        return false; // No database on disk
    }

    let db_bytes = (db_sectors as usize) * block::SECTOR_SIZE;
    if db_bytes > MAX_DB_SIZE {
        println!("[SYNAPSE] VirtIO DB too large: {} bytes (max {})", db_bytes, MAX_DB_SIZE);
        return false;
    }

    // Read database sectors into SQLITE_STATE (chunked, max 64 sectors per syscall)
    unsafe {
        let chunk_size = 64usize; // sectors per syscall (must be <= 128)
        let mut sectors_remaining = db_sectors as usize;
        let mut current_sector = db_sector;
        let mut buf_offset = 0usize;

        while sectors_remaining > 0 {
            let this_chunk = sectors_remaining.min(chunk_size);
            let chunk_bytes = this_chunk * block::SECTOR_SIZE;
            let buf = &mut SQLITE_STATE.data[buf_offset..buf_offset + chunk_bytes];

            if block::block_read(current_sector, buf, this_chunk).is_err() {
                println!("[SYNAPSE] VirtIO DB read failed at sector {}", current_sector);
                return false;
            }

            current_sector += this_chunk as u64;
            buf_offset += chunk_bytes;
            sectors_remaining -= this_chunk;
        }

        // Verify SQLite magic
        if db_bytes < 100 || &SQLITE_STATE.data[0..16] != b"SQLite format 3\0" {
            println!("[SYNAPSE] VirtIO DB not valid SQLite");
            return false;
        }

        SQLITE_STATE.size = db_bytes;
        SQLITE_STATE.valid = true;
    }

    println!("[SYNAPSE] Loaded {} sectors ({} KB) from VirtIO sector {}",
             db_sectors, db_bytes / 1024, db_sector);
    true
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

/// Populate DIR_CACHE_STATE from SQLite files table (called once at init)
/// This avoids repeated table scans — all handlers use the cached entries.
fn refresh_sqlite_cache() {
    if let Some(db) = get_sqlite_db() {
        if let Ok(scanner) = db.table_scan("files") {
            let mut count = 0;
            for result in scanner {
                if count >= MAX_ENTRIES { break; }
                if let Ok(record) = result {
                    if let Some(Value::Text(name)) = record.get(1) {
                        let name_bytes = name.as_bytes();
                        let name_len = name_bytes.len().min(32);
                        unsafe {
                            let entry = &mut DIR_CACHE_STATE.entries[count];
                            entry.name = [0u8; 32];
                            entry.name[..name_len].copy_from_slice(&name_bytes[..name_len]);
                            entry.id = record.get(0)
                                .and_then(|v| v.as_int())
                                .unwrap_or(count as i64) as u16;
                            entry.size = record.get(3)
                                .and_then(|v| v.as_int())
                                .unwrap_or(0) as u64;
                            entry.entry_type = if record.get(2)
                                .and_then(|v| v.as_int())
                                .unwrap_or(1) == KIND_ELF { 0 } else { 1 };
                            count += 1;
                        }
                    }
                }
            }
            unsafe {
                DIR_CACHE_STATE.count = count;
                DIR_CACHE_STATE.valid = true;
            }
        }
    }
}

/// Handle an incoming IPC request
fn handle_request(msg: AsyncIpcMessage) {
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
        SYN_OP_WRITE_FILE => handle_write_file(msg),
        SYN_OP_VECTOR_SEARCH => handle_vector_search(msg),
        SYN_OP_GET_EMBEDDING => handle_get_embedding(msg),
        SYN_OP_EMBEDDING_COUNT => handle_embedding_count(msg),
        _ => {
            let _ = reply(SYN_STATUS_INVALID, 0);
        }
    }
}

/// Handle PING request
fn handle_ping(_msg: AsyncIpcMessage) {
    let _ = reply(SYNAPSE_VERSION, 0);
}

/// Handle FILE_COUNT request
fn handle_file_count(_msg: AsyncIpcMessage) {
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
fn handle_file_by_index(msg: AsyncIpcMessage) {
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
fn handle_list_files(_msg: AsyncIpcMessage) {
    // Return file entries via shmem from DIR_CACHE_STATE (works for both backends)
    // Format: [name: [u8; 24]][size: u32][type: u32] = 32 bytes each
    if !unsafe { DIR_CACHE_STATE.valid } {
        let _ = reply(0, 0);
        return;
    }

    let count = unsafe { DIR_CACHE_STATE.count };
    if count == 0 {
        let _ = reply(0, 0);
        return;
    }

    let shmem_size = count * 32;
    let handle = match shmem_create(shmem_size) {
        Ok(h) => h,
        Err(_) => { let _ = reply((count as u64) << 32, 0); return; }
    };

    for tid in 2..=8 {
        let _ = shmem_grant(handle, tid);
    }

    if shmem_map(handle, SHMEM_BUFFER_VADDR).is_err() {
        let _ = shmem_destroy(handle);
        let _ = reply((count as u64) << 32, 0);
        return;
    }

    let buf = unsafe {
        core::slice::from_raw_parts_mut(SHMEM_BUFFER_VADDR as *mut u8, shmem_size)
    };
    for i in 0..count {
        let offset = i * 32;
        let entry = unsafe { &DIR_CACHE_STATE.entries[i] };
        // name: [u8; 24] from entry.name[0..24]
        buf[offset..offset+24].copy_from_slice(&entry.name[..24]);
        // size: u32
        buf[offset+24..offset+28].copy_from_slice(&(entry.size as u32).to_le_bytes());
        // type: u32 (0=elf, 1=data)
        buf[offset+28..offset+32].copy_from_slice(&(entry.entry_type as u32).to_le_bytes());
    }

    let _ = shmem_unmap(handle, SHMEM_BUFFER_VADDR);
    let _ = reply(((count as u64) << 32) | (handle as u64), 0);
}

/// Handle FILE_INFO request (by name hash)
fn handle_file_info(_msg: AsyncIpcMessage) {
    // For now, just return not found - need shared memory for string passing
    let _ = reply(SYN_STATUS_NOT_FOUND, 0);
}

/// Handle READ_FILE request (legacy)
fn handle_read_file(msg: AsyncIpcMessage) {
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
fn handle_read_file_by_name(msg: AsyncIpcMessage) {
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
fn handle_read_file_chunk(msg: AsyncIpcMessage) {
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
fn handle_read_file_shmem(msg: AsyncIpcMessage) {
    let request_hash = (msg.payload0 >> 16) as u32;

    match unsafe { BACKEND } {
        Backend::Sqlite => {
            // Step 1: Find file name from cache (fast, no table scan)
            let file_name: Option<&str> = unsafe {
                DIR_CACHE_STATE.entries[..DIR_CACHE_STATE.count]
                    .iter()
                    .find(|e| hash_name(e.name_str()) == request_hash)
                    .map(|e| e.name_str())
            };

            let name = match file_name {
                Some(n) => n,
                None => {
                    let _ = reply(SYN_STATUS_NOT_FOUND, 0);
                    return;
                }
            };

            // Step 2: Read file content — try ramdisk first, fallback to SQLite BLOB
            let mut file_buf = [0u8; 4096];
            let bytes_read = read_file(name, &mut file_buf);
            // If ramdisk has nothing, read BLOB from SQLite directly
            let bytes_read = if bytes_read == 0 {
                read_sqlite_blob(name, &mut file_buf)
            } else {
                bytes_read
            };
            if bytes_read == 0 {
                let _ = reply(SYN_STATUS_NOT_FOUND, 0);
                return;
            }

            // Step 3: Create shmem and copy content
            let buffer_size = ((bytes_read + 4095) / 4096) * 4096;
            let shmem_handle = match shmem_create(buffer_size) {
                Ok(handle) => handle,
                Err(_) => {
                    let _ = reply(SYN_STATUS_ERROR, 0);
                    return;
                }
            };

            for tid in 2..=8 {
                let _ = shmem_grant(shmem_handle, tid);
            }

            if shmem_map(shmem_handle, SHMEM_BUFFER_VADDR).is_err() {
                let _ = reply(SYN_STATUS_ERROR, 0);
                return;
            }

            unsafe {
                core::ptr::copy_nonoverlapping(
                    file_buf.as_ptr(),
                    SHMEM_BUFFER_VADDR as *mut u8,
                    bytes_read,
                );
            }

            let _ = shmem_unmap(shmem_handle, SHMEM_BUFFER_VADDR);
            let response = ((bytes_read as u64) << 32) | (shmem_handle as u64);
            let _ = reply(response, 0);
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

            for tid in 2..=8 {
                let _ = shmem_grant(shmem_handle, tid);
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
fn handle_sql_query(msg: AsyncIpcMessage) {
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
fn handle_vector_search(msg: AsyncIpcMessage) {
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
fn handle_get_embedding(msg: AsyncIpcMessage) {
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
fn handle_embedding_count(_msg: AsyncIpcMessage) {
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

// ============================================================================
// VFS Write Handler (Milestone 7)
// ============================================================================

/// Virtual address for write shmem mapping in Synapse
const WRITE_SHMEM_VADDR: usize = 0x13000000;

/// M12: Overwrite an existing BLOB in-place in SQLITE_STATE.data.
/// Scans the "files" table for a record with matching name, then overwrites
/// the data BLOB directly. Returns false if file not found or new data too large.
fn overwrite_blob_inplace(name: &str, new_data: &[u8]) -> bool {
    unsafe {
        if !SQLITE_STATE.valid {
            return false;
        }
        let db_data = &SQLITE_STATE.data[..SQLITE_STATE.size];
        let db = match SqliteDb::open(db_data) {
            Ok(db) => db,
            Err(_) => return false,
        };
        let scanner = match db.table_scan("files") {
            Ok(s) => s,
            Err(_) => return false,
        };
        for result in scanner {
            if let Ok(record) = result {
                if let Some(Value::Text(rec_name)) = record.get(1) {
                    if rec_name == name {
                        if let Some(Value::Blob(old_blob)) = record.get(4) {
                            if new_data.len() > old_blob.len() {
                                return false; // New data too large for in-place
                            }
                            // Calculate offset into SQLITE_STATE.data
                            let base = SQLITE_STATE.data.as_ptr() as usize;
                            let blob_ptr = old_blob.as_ptr() as usize;
                            let offset = blob_ptr - base;
                            // Overwrite in-place
                            SQLITE_STATE.data[offset..offset + new_data.len()]
                                .copy_from_slice(new_data);
                            // Zero remaining bytes if new data is shorter
                            if new_data.len() < old_blob.len() {
                                for b in &mut SQLITE_STATE.data[offset + new_data.len()..offset + old_blob.len()] {
                                    *b = 0;
                                }
                            }
                            // Increment SQLite change counter (bytes 24..28, big-endian)
                            let cc = u32::from_be_bytes([
                                SQLITE_STATE.data[24], SQLITE_STATE.data[25],
                                SQLITE_STATE.data[26], SQLITE_STATE.data[27],
                            ]);
                            let new_cc = cc.wrapping_add(1).to_be_bytes();
                            SQLITE_STATE.data[24..28].copy_from_slice(&new_cc);
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Handle WRITE_FILE request
/// Protocol: op | (shmem_handle << 16) | (total_size << 32)
/// Shmem format: [name_len: u16 LE][name bytes][content bytes]
fn handle_write_file(msg: AsyncIpcMessage) {
    if unsafe { BACKEND } != Backend::Sqlite {
        println!("[SYNAPSE] write_file: SQLite backend required");
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    let shmem_handle = ((msg.payload0 >> 16) & 0xFFFF) as u32;
    let total_size = (msg.payload0 >> 32) as usize;

    if shmem_handle == 0 || total_size < 3 {
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    // Map the shmem
    if shmem_map(shmem_handle, WRITE_SHMEM_VADDR).is_err() {
        println!("[SYNAPSE] write_file: failed to map shmem {}", shmem_handle);
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    let shmem_slice = unsafe {
        core::slice::from_raw_parts(WRITE_SHMEM_VADDR as *const u8, total_size)
    };

    // Parse: [name_len: u16 LE][name][content]
    let name_len = u16::from_le_bytes([shmem_slice[0], shmem_slice[1]]) as usize;
    if name_len == 0 || 2 + name_len > total_size {
        let _ = shmem_unmap(shmem_handle, WRITE_SHMEM_VADDR);
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    let name_bytes = &shmem_slice[2..2 + name_len];
    let content = &shmem_slice[2 + name_len..total_size];

    // Validate UTF-8
    let name = match core::str::from_utf8(name_bytes) {
        Ok(s) => s,
        Err(_) => {
            let _ = shmem_unmap(shmem_handle, WRITE_SHMEM_VADDR);
            let _ = reply(SYN_STATUS_ERROR, 0);
            return;
        }
    };

    // Check for duplicate in DIR_CACHE_STATE
    let name_hash = hash_name(name);
    let duplicate = unsafe {
        DIR_CACHE_STATE.entries[..DIR_CACHE_STATE.count]
            .iter()
            .any(|e| hash_name(e.name_str()) == name_hash)
    };
    if duplicate {
        // M12: In-place overwrite instead of rejecting
        // Copy content AND name to local buffers before unmapping shmem
        let mut ow_content = [0u8; 4096];
        let ow_len = content.len().min(ow_content.len());
        ow_content[..ow_len].copy_from_slice(&content[..ow_len]);
        let mut ow_name_buf = [0u8; 64];
        let ow_name_len = name_len.min(64);
        ow_name_buf[..ow_name_len].copy_from_slice(&name_bytes[..ow_name_len]);
        let _ = shmem_unmap(shmem_handle, WRITE_SHMEM_VADDR);

        // Use local name copy — `name` pointed into now-unmapped shmem!
        let ow_name = unsafe { core::str::from_utf8_unchecked(&ow_name_buf[..ow_name_len]) };
        if overwrite_blob_inplace(ow_name, &ow_content[..ow_len]) {
            println!("[SYNAPSE] write_file: '{}' overwritten in-place ({} bytes)", ow_name, ow_len);
            flush_sqlite_to_disk();
            let _ = reply(0, 0);
        } else {
            println!("[SYNAPSE] write_file: '{}' overwrite failed", ow_name);
            let _ = reply(SYN_STATUS_ERROR, 0);
        }
        return;
    }

    // Copy content to a local buffer (we need it after unmapping shmem)
    let mut content_buf = [0u8; 4096];
    let content_len = content.len().min(content_buf.len());
    content_buf[..content_len].copy_from_slice(&content[..content_len]);

    // Copy name to local buffer
    let mut name_buf = [0u8; 32];
    let nl = name_len.min(32);
    name_buf[..nl].copy_from_slice(&name_bytes[..nl]);

    let _ = shmem_unmap(shmem_handle, WRITE_SHMEM_VADDR);

    // Determine next rowid by scanning existing records
    let next_rowid = find_max_rowid() + 1;

    // Insert cell into SQLite B-tree
    let name_str = unsafe { core::str::from_utf8_unchecked(&name_buf[..nl]) };
    if !sqlite_insert_file(next_rowid, name_str, &content_buf[..content_len]) {
        println!("[SYNAPSE] write_file: cell insert failed");
        let _ = reply(SYN_STATUS_ERROR, 0);
        return;
    }

    // Update DIR_CACHE_STATE for immediate visibility
    update_dir_cache(next_rowid, &name_buf[..nl], content_len);

    // Flush to VirtIO disk
    if !flush_sqlite_to_disk() {
        println!("[SYNAPSE] write_file: disk flush failed (data in memory only)");
    }

    println!("[SYNAPSE] Wrote '{}' ({} bytes, rowid={})", name_str, content_len, next_rowid);
    let _ = reply(0, 0); // Success
}

/// Find the maximum rowid in the files table
fn find_max_rowid() -> i64 {
    if let Some(db) = get_sqlite_db() {
        if let Ok(scanner) = db.table_scan("files") {
            let mut max_id: i64 = 0;
            for result in scanner {
                if let Ok(record) = result {
                    if record.rowid > max_id {
                        max_id = record.rowid;
                    }
                }
            }
            return max_id;
        }
    }
    0
}

/// Pick the smallest SQLite integer type code for a value
fn pick_integer_type(val: u64) -> u64 {
    if val == 0 { return 8; }  // type 8 = integer constant 0
    if val == 1 { return 9; }  // type 9 = integer constant 1
    if val <= 0xFF { return 1; }          // 1-byte int
    if val <= 0xFFFF { return 2; }        // 2-byte int
    if val <= 0xFFFFFF { return 3; }      // 3-byte int
    if val <= 0xFFFFFFFF { return 4; }    // 4-byte int
    if val <= 0xFFFFFFFFFF { return 5; }  // 6-byte int (type 5 = 6 bytes)
    6  // 8-byte int
}

/// Get byte count for an integer type code
fn integer_type_size(type_code: u64) -> usize {
    match type_code {
        0 => 0,  // NULL
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 => 8,
        8 | 9 => 0,  // Constants 0 and 1
        _ => 0,
    }
}

/// Encode an integer value in big-endian format for the given type code
/// Returns the number of bytes written
fn encode_integer_value(val: u64, type_code: u64, buf: &mut [u8]) -> usize {
    let size = integer_type_size(type_code);
    match size {
        0 => 0,
        1 => { buf[0] = val as u8; 1 }
        2 => { buf[0..2].copy_from_slice(&(val as u16).to_be_bytes()); 2 }
        3 => {
            let bytes = (val as u32).to_be_bytes();
            buf[0..3].copy_from_slice(&bytes[1..4]);
            3
        }
        4 => { buf[0..4].copy_from_slice(&(val as u32).to_be_bytes()); 4 }
        6 => {
            let bytes = val.to_be_bytes();
            buf[0..6].copy_from_slice(&bytes[2..8]);
            6
        }
        8 => { buf[0..8].copy_from_slice(&val.to_be_bytes()); 8 }
        _ => 0,
    }
}

/// Insert a file record as a new cell in the SQLite B-tree leaf page
///
/// Cell format: [payload_size: varint][rowid: varint][record]
/// Record: [header_size: varint][name_type][kind_type][size_type][data_type][embed_type][name_bytes][size_bytes][data_bytes]
///
/// For INTEGER PRIMARY KEY, the id is stored as the cell rowid, not in the record body.
/// Record body columns: name(TEXT), kind(INT), size(INT), data(BLOB), embedding(NULL)
fn sqlite_insert_file(rowid: i64, name: &str, content: &[u8]) -> bool {
    unsafe {
        if !SQLITE_STATE.valid {
            return false;
        }

        let db_data = &SQLITE_STATE.data[..SQLITE_STATE.size];
        let db = match SqliteDb::open(db_data) {
            Ok(db) => db,
            Err(_) => return false,
        };

        let root_page = match db.find_table_root("files") {
            Ok(p) => p,
            Err(_) => return false,
        };

        let page_size = db.page_size() as usize;

        // Traverse B-tree to find the rightmost leaf page
        // Interior pages (type 0x05) have a right-pointer; follow it to get the last leaf
        let mut current_page = root_page;
        for _ in 0..10 {
            // Safety depth limit
            let page_off = (current_page as usize - 1) * page_size;
            let hdr_off = if current_page == 1 { 100 } else { 0 };
            let page_type = SQLITE_STATE.data[page_off + hdr_off];

            if page_type == 0x0d {
                break; // Found leaf page
            } else if page_type == 0x05 {
                // Interior page: right-pointer is at header offset + 8 (BE u32)
                let rp_offset = page_off + hdr_off + 8;
                let right_child = u32::from_be_bytes([
                    SQLITE_STATE.data[rp_offset],
                    SQLITE_STATE.data[rp_offset + 1],
                    SQLITE_STATE.data[rp_offset + 2],
                    SQLITE_STATE.data[rp_offset + 3],
                ]);
                current_page = right_child;
            } else {
                println!("[SYNAPSE] insert: unexpected page type 0x{:02x}", page_type);
                return false;
            }
        }

        let leaf_page = current_page;
        let page_offset = (leaf_page as usize - 1) * page_size;
        let header_offset = if leaf_page == 1 { 100 } else { 0 };

        // Verify it's actually a leaf
        if SQLITE_STATE.data[page_offset + header_offset] != 0x0d {
            println!("[SYNAPSE] insert: traversal failed, not a leaf (0x{:02x})",
                     SQLITE_STATE.data[page_offset + header_offset]);
            return false;
        }

        let hdr = page_offset + header_offset;

        // Read page header fields (all big-endian)
        // Offset 0: page type (1 byte) — already validated
        // Offset 3: cell count (2 bytes)
        // Offset 5: cell content start (2 bytes), 0 means 65536
        let cell_count = u16::from_be_bytes([
            SQLITE_STATE.data[hdr + 3],
            SQLITE_STATE.data[hdr + 4],
        ]) as usize;

        let cell_content_start = {
            let raw = u16::from_be_bytes([
                SQLITE_STATE.data[hdr + 5],
                SQLITE_STATE.data[hdr + 6],
            ]);
            if raw == 0 { page_size } else { raw as usize }
        };

        // Build the record body
        let name_bytes = name.as_bytes();

        // Type codes for record columns (must match existing rows from folk-pack):
        // id: NULL (type 0) — INTEGER PRIMARY KEY stored as rowid, record body has NULL placeholder
        // name: TEXT → 13 + len*2
        // kind: INT constant 1 (DATA file) → type 9
        // size: smallest int type for content.len()
        // data: BLOB → 12 + len*2
        // embedding: NULL → type 0 (reserved for future vector storage)
        let id_type: u64 = 0; // NULL placeholder for INTEGER PRIMARY KEY
        let name_type = 13 + (name_bytes.len() as u64) * 2;
        let kind_type: u64 = 9; // constant 1
        let size_type = pick_integer_type(content.len() as u64);
        let data_type = 12 + (content.len() as u64) * 2;
        let embed_type: u64 = 0; // NULL

        // Build header: [header_size: varint][id_type][name_type][kind_type][size_type][data_type][embed_type]
        let mut hdr_buf = [0u8; 32];
        let mut hdr_pos = 0usize;

        // Reserve space for header_size varint (fill later)
        let hdr_size_pos = 0;
        hdr_pos += 1; // Placeholder — header size is usually 1 byte for small headers

        hdr_pos += encode_varint(id_type, &mut hdr_buf[hdr_pos..]);
        hdr_pos += encode_varint(name_type, &mut hdr_buf[hdr_pos..]);
        hdr_pos += encode_varint(kind_type, &mut hdr_buf[hdr_pos..]);
        hdr_pos += encode_varint(size_type, &mut hdr_buf[hdr_pos..]);
        hdr_pos += encode_varint(data_type, &mut hdr_buf[hdr_pos..]);
        hdr_pos += encode_varint(embed_type, &mut hdr_buf[hdr_pos..]);

        // Header size includes itself
        let header_size = hdr_pos;
        // For small headers (< 128), header_size fits in 1 byte
        if header_size > 127 {
            println!("[SYNAPSE] insert: header too large");
            return false;
        }
        hdr_buf[hdr_size_pos] = header_size as u8;

        // Record body size = header + name + size_int + data + (id=0 bytes, kind=0 bytes, embed=0 bytes)
        let size_int_size = integer_type_size(size_type);
        let record_body_size = header_size + name_bytes.len() + size_int_size + content.len();

        // Build the full cell: [payload_size: varint][rowid: varint][record body]
        let mut cell_buf = [0u8; 8192];
        let mut cell_pos = 0usize;

        cell_pos += encode_varint(record_body_size as u64, &mut cell_buf[cell_pos..]);
        cell_pos += encode_varint(rowid as u64, &mut cell_buf[cell_pos..]);

        // Copy record header
        cell_buf[cell_pos..cell_pos + header_size].copy_from_slice(&hdr_buf[..header_size]);
        cell_pos += header_size;

        // Column values (in order): id, name, kind, size, data, embedding
        // id: NULL — 0 bytes (INTEGER PRIMARY KEY placeholder)
        // name (TEXT)
        cell_buf[cell_pos..cell_pos + name_bytes.len()].copy_from_slice(name_bytes);
        cell_pos += name_bytes.len();

        // kind: type 9 = constant 1, zero value bytes
        // (no bytes to write)

        // size: integer value
        let mut int_buf = [0u8; 8];
        let int_len = encode_integer_value(content.len() as u64, size_type, &mut int_buf);
        cell_buf[cell_pos..cell_pos + int_len].copy_from_slice(&int_buf[..int_len]);
        cell_pos += int_len;

        // data (BLOB)
        cell_buf[cell_pos..cell_pos + content.len()].copy_from_slice(content);
        cell_pos += content.len();

        // embedding: NULL, zero bytes

        let cell_len = cell_pos;

        // Check free space
        // Pointer array ends at: header_offset + 8 + cell_count * 2
        let pointer_array_end = header_offset + 8 + cell_count * 2;
        let free_space = cell_content_start - pointer_array_end;
        if cell_len + 2 > free_space {
            println!("[SYNAPSE] insert: no space (need {}, have {})", cell_len + 2, free_space);
            return false;
        }

        // Write cell at cell_content_start - cell_len
        let new_cell_offset = cell_content_start - cell_len;
        SQLITE_STATE.data[page_offset + new_cell_offset..page_offset + new_cell_offset + cell_len]
            .copy_from_slice(&cell_buf[..cell_len]);

        // Write cell pointer (BE u16) at end of pointer array
        let ptr_offset = page_offset + header_offset + 8 + cell_count * 2;
        let cell_ptr = (new_cell_offset as u16).to_be_bytes();
        SQLITE_STATE.data[ptr_offset] = cell_ptr[0];
        SQLITE_STATE.data[ptr_offset + 1] = cell_ptr[1];

        // Update page header: cell_count += 1
        let new_count = (cell_count + 1) as u16;
        let count_bytes = new_count.to_be_bytes();
        SQLITE_STATE.data[hdr + 3] = count_bytes[0];
        SQLITE_STATE.data[hdr + 4] = count_bytes[1];

        // Update page header: cell_content_start = new_cell_offset
        let start_bytes = (new_cell_offset as u16).to_be_bytes();
        SQLITE_STATE.data[hdr + 5] = start_bytes[0];
        SQLITE_STATE.data[hdr + 6] = start_bytes[1];

        // Increment DB change counter at file offset 24 (BE u32)
        let change_counter = u32::from_be_bytes([
            SQLITE_STATE.data[24], SQLITE_STATE.data[25],
            SQLITE_STATE.data[26], SQLITE_STATE.data[27],
        ]);
        let new_counter = change_counter.wrapping_add(1);
        let counter_bytes = new_counter.to_be_bytes();
        SQLITE_STATE.data[24] = counter_bytes[0];
        SQLITE_STATE.data[25] = counter_bytes[1];
        SQLITE_STATE.data[26] = counter_bytes[2];
        SQLITE_STATE.data[27] = counter_bytes[3];

        true
    }
}

/// Read a file's BLOB data directly from SQLite into buf. Returns bytes read.
fn read_sqlite_blob(name: &str, buf: &mut [u8]) -> usize {
    if let Some(db) = get_sqlite_db() {
        if let Ok(scanner) = db.table_scan("files") {
            for result in scanner {
                if let Ok(record) = result {
                    if let Some(Value::Text(rec_name)) = record.get(1) {
                        if rec_name == name {
                            if let Some(Value::Blob(data)) = record.get(4) {
                                let copy_len = data.len().min(buf.len());
                                buf[..copy_len].copy_from_slice(&data[..copy_len]);
                                return copy_len;
                            }
                        }
                    }
                }
            }
        }
    }
    0
}

/// Update DIR_CACHE_STATE with the newly inserted file
fn update_dir_cache(rowid: i64, name_bytes: &[u8], size: usize) {
    unsafe {
        let count = DIR_CACHE_STATE.count;
        if count >= MAX_ENTRIES {
            return;
        }
        let entry = &mut DIR_CACHE_STATE.entries[count];
        entry.id = rowid as u16;
        entry.name = [0u8; 32];
        let nl = name_bytes.len().min(32);
        entry.name[..nl].copy_from_slice(&name_bytes[..nl]);
        entry.size = size as u64;
        entry.entry_type = 1; // DATA type
        DIR_CACHE_STATE.count = count + 1;
    }
}

/// Flush SQLITE_STATE.data back to VirtIO disk
fn flush_sqlite_to_disk() -> bool {
    // Read FOLKDISK header to find synapse_db_sector
    let mut header_buf = [0u8; block::SECTOR_SIZE];
    if block::read_sector(0, &mut header_buf).is_err() {
        return false;
    }

    if &header_buf[0..8] != b"FOLKDISK" {
        return false;
    }

    let db_sector = u64::from_le_bytes([
        header_buf[48], header_buf[49], header_buf[50], header_buf[51],
        header_buf[52], header_buf[53], header_buf[54], header_buf[55],
    ]);

    if db_sector == 0 {
        return false;
    }

    unsafe {
        let db_size = SQLITE_STATE.size;
        let total_sectors = (db_size + block::SECTOR_SIZE - 1) / block::SECTOR_SIZE;
        let chunk_size = 64usize;
        let mut sectors_remaining = total_sectors;
        let mut current_sector = db_sector;
        let mut buf_offset = 0usize;

        while sectors_remaining > 0 {
            let this_chunk = sectors_remaining.min(chunk_size);
            let chunk_bytes = this_chunk * block::SECTOR_SIZE;
            let buf = &SQLITE_STATE.data[buf_offset..buf_offset + chunk_bytes];

            if block::block_write(current_sector, buf, this_chunk).is_err() {
                println!("[SYNAPSE] flush: write failed at sector {}", current_sector);
                return false;
            }

            current_sector += this_chunk as u64;
            buf_offset += chunk_bytes;
            sectors_remaining -= this_chunk;
        }
    }

    true
}
