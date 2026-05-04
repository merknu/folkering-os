//! IPC request handlers + dispatcher.
//!
//! Each handler operates on a `&mut SafeSqliteBuffer` (via the global
//! accessor in `main`) plus a `&mut DirCacheState`. Replies are sent
//! through the global `CURRENT_TOKEN` set by the main loop.

extern crate alloc;

use libfolk::println;
use libfolk::sys::fs::read_file;
use libfolk::sys::ipc::{reply_with_token, AsyncIpcMessage, CallerToken, IpcError};
use libfolk::sys::synapse::{
    SYN_OP_PING, SYN_OP_LIST_FILES, SYN_OP_FILE_COUNT, SYN_OP_FILE_BY_INDEX,
    SYN_OP_FILE_INFO, SYN_OP_READ_FILE, SYN_OP_READ_FILE_BY_NAME, SYN_OP_READ_FILE_CHUNK,
    SYN_OP_READ_FILE_SHMEM, SYN_OP_SQL_QUERY,
    SYN_OP_VECTOR_SEARCH, SYN_OP_GET_EMBEDDING, SYN_OP_EMBEDDING_COUNT,
    SYN_OP_WRITE_FILE, SYN_OP_DELETE_FILE,
    SYN_OP_WRITE_INTENT, SYN_OP_READ_INTENT, SYN_OP_QUERY_MIME,
    SYN_OP_QUERY_INTENT,
    SYN_OP_UPSERT_ENTITY, SYN_OP_UPSERT_EDGE, SYN_OP_GRAPH_WALK,
    SYN_STATUS_NOT_FOUND, SYN_STATUS_INVALID, SYN_STATUS_ERROR,
    SYNAPSE_VERSION, hash_name,
};
use libfolk::sys::{shmem_create, shmem_destroy, shmem_grant, shmem_map, shmem_unmap};
use libsqlite::Value;
use libsqlite::vector::{
    Embedding, SearchResult, search_similar_auto,
    get_embedding_by_file_id, count_embeddings, EMBEDDING_SIZE,
};

use crate::btree::{
    overwrite_blob_inplace, sqlite_insert_file, sqlite_insert_intent, sqlite_read_intent,
    sqlite_insert_entity, sqlite_insert_edge, sqlite_expire_edge, sqlite_graph_walk,
    sqlite_delete_file_by_rowid,
};
use crate::cache::{
    count_sqlite_files, find_max_rowid, get_file_size, open_db, refresh_fpk_cache,
    refresh_sqlite_cache, update_dir_cache,
};
use crate::mime::auto_detect_mime;
use crate::shmem::{vaddr, ShmemArena};
use crate::sqlite_io::{flush_sqlite_to_disk, read_sqlite_blob_large};
use crate::state::{Backend, DirCacheState, SafeSqliteBuffer};

/// Global IPC reply token, set by the main loop before each `handle_request`.
/// Stored in main.rs as a `static mut`; we access it through these helpers.
pub trait TokenSource {
    fn take_token(&mut self) -> Option<CallerToken>;
}

/// Convenience: send a reply for the current request.
pub fn reply<T: TokenSource>(src: &mut T, payload0: u64, payload1: u64) -> Result<(), IpcError> {
    if let Some(token) = src.take_token() {
        reply_with_token(token, payload0, payload1)
    } else {
        Err(IpcError::WouldBlock)
    }
}

/// Top-level IPC dispatcher. Routes to the appropriate handler based on opcode.
pub fn handle_request<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &mut SafeSqliteBuffer,
    cache: &mut DirCacheState,
    backend: Backend,
) {
    let op = msg.payload0 & 0xFFFF;

    match op {
        SYN_OP_PING => { let _ = reply(src, SYNAPSE_VERSION, 0); }
        SYN_OP_FILE_COUNT => handle_file_count(src, sqlite, cache, backend),
        SYN_OP_FILE_BY_INDEX => handle_file_by_index(msg, src, sqlite, cache, backend),
        SYN_OP_LIST_FILES => handle_list_files(src, cache),
        SYN_OP_FILE_INFO => { let _ = reply(src, SYN_STATUS_NOT_FOUND, 0); }
        SYN_OP_READ_FILE => handle_read_file(msg, src, sqlite, cache, backend),
        SYN_OP_READ_FILE_BY_NAME => handle_read_file_by_name(msg, src, sqlite, cache, backend),
        SYN_OP_READ_FILE_CHUNK => handle_read_file_chunk(msg, src, sqlite, cache, backend),
        SYN_OP_READ_FILE_SHMEM => handle_read_file_shmem(msg, src, sqlite, cache, backend),
        SYN_OP_SQL_QUERY => handle_sql_query(msg, src, sqlite, backend),
        SYN_OP_WRITE_FILE => handle_write_file(msg, src, sqlite, cache, backend),
        SYN_OP_DELETE_FILE => handle_delete_file(msg, src, sqlite, cache, backend),
        SYN_OP_WRITE_INTENT => handle_write_intent(msg, src, sqlite, backend),
        SYN_OP_READ_INTENT => handle_read_intent(msg, src, sqlite),
        SYN_OP_QUERY_MIME => handle_query_mime(msg, src, sqlite),
        SYN_OP_QUERY_INTENT => handle_query_intent(msg, src, sqlite),
        SYN_OP_VECTOR_SEARCH => handle_vector_search(msg, src, sqlite, backend),
        SYN_OP_GET_EMBEDDING => handle_get_embedding(msg, src, sqlite, backend),
        SYN_OP_EMBEDDING_COUNT => handle_embedding_count(src, sqlite, backend),
        // Phase 9: Bi-Temporal Knowledge Graph
        SYN_OP_UPSERT_ENTITY => handle_upsert_entity(msg, src, sqlite, backend),
        SYN_OP_UPSERT_EDGE => handle_upsert_edge(msg, src, sqlite, backend),
        SYN_OP_GRAPH_WALK => handle_graph_walk(msg, src, sqlite, backend),
        _ => { let _ = reply(src, SYN_STATUS_INVALID, 0); }
    }
}

// ── File query handlers ───────────────────────────────────────────────

fn handle_file_count<T: TokenSource>(
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
    cache: &mut DirCacheState,
    backend: Backend,
) {
    let count = match backend {
        Backend::Sqlite => count_sqlite_files(sqlite),
        Backend::Fpk => {
            if !cache.valid {
                refresh_fpk_cache(cache);
            }
            cache.count
        }
    };
    let _ = reply(src, count as u64, 0);
}

fn handle_file_by_index<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
    cache: &mut DirCacheState,
    backend: Backend,
) {
    let index = ((msg.payload0 >> 16) & 0xFFFF) as usize;

    match backend {
        Backend::Sqlite => {
            if let Some(db) = open_db(sqlite) {
                if let Ok(scanner) = db.table_scan("files") {
                    if let Some(Ok(record)) = scanner.skip(index).next() {
                        let id = record.get(0).and_then(|v| v.as_int()).unwrap_or(0) as u64;
                        let kind = record.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                        let size = record.get(3).and_then(|v| v.as_int()).unwrap_or(0) as u64;
                        let entry_type = if kind == crate::state::KIND_ELF { 1u64 } else { 0u64 };
                        let response = (id << 48) | (size << 16) | entry_type;
                        let _ = reply(src, response, 0);
                        return;
                    }
                }
            }
            let _ = reply(src, SYN_STATUS_NOT_FOUND, 0);
        }
        Backend::Fpk => {
            if !cache.valid { refresh_fpk_cache(cache); }
            if index >= cache.count {
                let _ = reply(src, SYN_STATUS_NOT_FOUND, 0);
                return;
            }
            let entry = &cache.entries[index];
            let response = ((entry.id as u64) << 48)
                         | ((entry.size as u64) << 16)
                         | (entry.entry_type as u64);
            let _ = reply(src, response, 0);
        }
    }
}

fn handle_list_files<T: TokenSource>(src: &mut T, cache: &DirCacheState) {
    if !cache.valid {
        let _ = reply(src, 0, 0);
        return;
    }

    let count = cache.count;
    if count == 0 {
        let _ = reply(src, 0, 0);
        return;
    }

    let shmem_size = count * 32;
    let handle = match shmem_create(shmem_size) {
        Ok(h) => h,
        Err(_) => { let _ = reply(src, (count as u64) << 32, 0); return; }
    };
    for tid in 2..=8 { let _ = shmem_grant(handle, tid); }

    {
        let mut arena = match ShmemArena::map(handle, vaddr::SHMEM_BUFFER) {
            Ok(a) => a,
            Err(_) => {
                let _ = shmem_destroy(handle);
                let _ = reply(src, (count as u64) << 32, 0);
                return;
            }
        };
        let buf = unsafe { arena.as_mut_slice(shmem_size) };
        for i in 0..count {
            let offset = i * 32;
            let entry = &cache.entries[i];
            buf[offset..offset + 24].copy_from_slice(&entry.name[..24]);
            buf[offset + 24..offset + 28].copy_from_slice(&(entry.size as u32).to_le_bytes());
            buf[offset + 28..offset + 32].copy_from_slice(&(entry.entry_type as u32).to_le_bytes());
        }
    } // arena dropped here → auto unmap

    let _ = reply(src, ((count as u64) << 32) | (handle as u64), 0);
}

fn handle_read_file<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
    cache: &mut DirCacheState,
    backend: Backend,
) {
    let file_id = ((msg.payload0 >> 16) & 0xFFFF) as usize;

    match backend {
        Backend::Sqlite => {
            if let Some(db) = open_db(sqlite) {
                if let Ok(scanner) = db.table_scan("files") {
                    for result in scanner {
                        if let Ok(record) = result {
                            let id = record.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
                            if id as usize == file_id {
                                let kind = record.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                                let size = record.get(3).and_then(|v| v.as_int()).unwrap_or(0);
                                let entry_type = if kind == crate::state::KIND_ELF { 1u64 } else { 0u64 };
                                let response = ((size as u64) << 32) | entry_type;
                                let _ = reply(src, response, 0);
                                return;
                            }
                        }
                    }
                }
            }
            let _ = reply(src, SYN_STATUS_NOT_FOUND, 0);
        }
        Backend::Fpk => {
            if !cache.valid { refresh_fpk_cache(cache); }
            let entry = cache.entries[..cache.count].iter().find(|e| e.id as usize == file_id);
            match entry {
                Some(e) => {
                    let response = ((e.size as u64) << 32) | (e.entry_type as u64);
                    let _ = reply(src, response, 0);
                }
                None => { let _ = reply(src, SYN_STATUS_NOT_FOUND, 0); }
            }
        }
    }
}

fn handle_read_file_by_name<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
    cache: &mut DirCacheState,
    backend: Backend,
) {
    let request_hash = (msg.payload0 >> 16) as u32;

    match backend {
        Backend::Sqlite => {
            if let Some(db) = open_db(sqlite) {
                if let Ok(scanner) = db.table_scan("files") {
                    for result in scanner {
                        if let Ok(record) = result {
                            if let Some(Value::Text(name)) = record.get(1) {
                                if hash_name(name) == request_hash {
                                    let id = record.get(0).and_then(|v| v.as_int()).unwrap_or(0);
                                    let size = record.get(3).and_then(|v| v.as_int()).unwrap_or(0);
                                    let response = ((size as u64) << 32) | (id as u64);
                                    let _ = reply(src, response, 0);
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            let _ = reply(src, SYN_STATUS_NOT_FOUND, 0);
        }
        Backend::Fpk => {
            if !cache.valid { refresh_fpk_cache(cache); }
            let entry = cache.entries[..cache.count].iter().find(|e| {
                let name = e.name_str();
                hash_name(name) == request_hash
            });
            match entry {
                Some(e) => {
                    let response = ((e.size as u64) << 32) | (e.id as u64);
                    let _ = reply(src, response, 0);
                }
                None => { let _ = reply(src, SYN_STATUS_NOT_FOUND, 0); }
            }
        }
    }
}

fn handle_read_file_chunk<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
    cache: &mut DirCacheState,
    backend: Backend,
) {
    let file_id = ((msg.payload0 >> 16) & 0xFFFF) as u16;
    let offset = (msg.payload0 >> 32) as u32;

    match backend {
        Backend::Sqlite => {
            if let Some(db) = open_db(sqlite) {
                if let Ok(scanner) = db.table_scan("files") {
                    for result in scanner {
                        if let Ok(record) = result {
                            let id = record.get(0).and_then(|v| v.as_int()).unwrap_or(-1);
                            if id as u16 == file_id {
                                if let Some(Value::Blob(data)) = record.get(4) {
                                    let offset = offset as usize;
                                    if offset >= data.len() { let _ = reply(src, 0, 0); return; }
                                    let chunk_end = (offset + 8).min(data.len());
                                    let mut chunk: u64 = 0;
                                    for (i, &byte) in data[offset..chunk_end].iter().enumerate() {
                                        chunk |= (byte as u64) << (i * 8);
                                    }
                                    let _ = reply(src, chunk, 0);
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            let _ = reply(src, SYN_STATUS_NOT_FOUND, 0);
        }
        Backend::Fpk => {
            if !cache.valid { refresh_fpk_cache(cache); }
            let entry = cache.entries[..cache.count].iter().find(|e| e.id == file_id);
            let entry = match entry {
                Some(e) => e,
                None => { let _ = reply(src, SYN_STATUS_NOT_FOUND, 0); return; }
            };
            if offset as u64 >= entry.size { let _ = reply(src, 0, 0); return; }
            let mut buf = [0u8; 4096];
            let name = entry.name_str();
            let bytes_read = read_file(name, &mut buf);
            if bytes_read == 0 { let _ = reply(src, SYN_STATUS_NOT_FOUND, 0); return; }
            let chunk_start = offset as usize;
            let chunk_end = (chunk_start + 8).min(bytes_read);
            if chunk_start >= bytes_read { let _ = reply(src, 0, 0); return; }
            let mut chunk: u64 = 0;
            for (i, &byte) in buf[chunk_start..chunk_end].iter().enumerate() {
                chunk |= (byte as u64) << (i * 8);
            }
            let _ = reply(src, chunk, 0);
        }
    }
}

fn handle_read_file_shmem<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
    cache: &mut DirCacheState,
    backend: Backend,
) {
    let request_hash = (msg.payload0 >> 16) as u32;

    match backend {
        Backend::Sqlite => {
            // Find file name + size from cache
            let cache_entry = cache.entries[..cache.count]
                .iter()
                .find(|e| hash_name(e.name_str()) == request_hash);

            // Debug logging for first few lookups
            static mut SHMEM_DBG_COUNT: u32 = 0;
            unsafe {
                SHMEM_DBG_COUNT += 1;
                if SHMEM_DBG_COUNT <= 3 {
                    println!("[SYNAPSE] read_file_shmem: looking for hash={:#010x}, cache has {} entries",
                        request_hash, cache.count);
                    for i in 0..cache.count {
                        let e = &cache.entries[i];
                        let n = e.name_str();
                        println!("[SYNAPSE]   [{}] '{}' hash={:#010x} size={}",
                            i, n, hash_name(n), e.size);
                    }
                }
            }

            let (name_buf, name_len, file_size) = match cache_entry {
                Some(e) => {
                    let mut buf = [0u8; 32];
                    let n = e.name_str();
                    let nb = n.as_bytes();
                    let nl = nb.len().min(32);
                    buf[..nl].copy_from_slice(&nb[..nl]);
                    (buf, nl, e.size as usize)
                }
                None => {
                    let _ = reply(src, SYN_STATUS_NOT_FOUND, 0);
                    return;
                }
            };
            let name = unsafe { core::str::from_utf8_unchecked(&name_buf[..name_len]) };

            let buffer_size = ((file_size + 4095) / 4096) * 4096;
            let shmem_handle = match shmem_create(buffer_size) {
                Ok(h) => h,
                Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
            };
            // Grant to the actual sender plus the historical 2..=8
            // fan-out (kept so existing callers in that range — shell,
            // compositor, draug-daemon, etc. — keep working without a
            // re-test pass). New tasks (inference task 10+, future
            // userspace) get the grant via msg.sender.
            for tid in 2..=8 { let _ = shmem_grant(shmem_handle, tid); }
            if msg.sender > 8 {
                let _ = shmem_grant(shmem_handle, msg.sender);
            }

            let bytes_read = {
                let mut arena = match ShmemArena::map(shmem_handle, vaddr::SHMEM_BUFFER) {
                    Ok(a) => a,
                    Err(_) => {
                        let _ = shmem_destroy(shmem_handle);
                        let _ = reply(src, SYN_STATUS_ERROR, 0);
                        return;
                    }
                };
                let shmem_buf = unsafe { arena.as_mut_slice(buffer_size) };
                let r = read_file(name, shmem_buf);
                if r == 0 {
                    read_sqlite_blob_large(sqlite, name, shmem_buf)
                } else {
                    r
                }
            }; // arena drops → auto unmap

            if bytes_read == 0 {
                let _ = shmem_destroy(shmem_handle);
                let _ = reply(src, SYN_STATUS_NOT_FOUND, 0);
                return;
            }

            let response = ((bytes_read as u64) << 32) | (shmem_handle as u64);
            let _ = reply(src, response, 0);
        }
        Backend::Fpk => {
            if !cache.valid { refresh_fpk_cache(cache); }
            let entry = cache.entries[..cache.count].iter().find(|e| {
                hash_name(e.name_str()) == request_hash
            });
            let entry = match entry {
                Some(e) => e,
                None => { let _ = reply(src, SYN_STATUS_NOT_FOUND, 0); return; }
            };
            let file_size = entry.size as usize;
            // Copy name out of cache to drop the borrow
            let mut name_buf = [0u8; 32];
            let nb = entry.name_str().as_bytes();
            let nl = nb.len().min(32);
            name_buf[..nl].copy_from_slice(&nb[..nl]);
            let file_name = unsafe { core::str::from_utf8_unchecked(&name_buf[..nl]) };

            let buffer_size = ((file_size + 4095) / 4096) * 4096;
            let buffer_size = if buffer_size == 0 { 4096 } else { buffer_size };

            let shmem_handle = match shmem_create(buffer_size) {
                Ok(h) => h,
                Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
            };
            // Same grant fan-out as the SQLite branch above — keep
            // tasks 2..=8 working, plus grant to the actual sender
            // when it lives outside that range (e.g. inference at
            // task 10+).
            for tid in 2..=8 { let _ = shmem_grant(shmem_handle, tid); }
            if msg.sender > 8 {
                let _ = shmem_grant(shmem_handle, msg.sender);
            }

            let bytes_read = {
                let mut arena = match ShmemArena::map(shmem_handle, vaddr::SHMEM_BUFFER) {
                    Ok(a) => a,
                    Err(_) => {
                        let _ = shmem_destroy(shmem_handle);
                        let _ = reply(src, SYN_STATUS_ERROR, 0);
                        return;
                    }
                };
                let buf = unsafe { arena.as_mut_slice(buffer_size) };
                read_file(file_name, buf)
            };

            if bytes_read == 0 {
                let _ = shmem_destroy(shmem_handle);
                let _ = reply(src, SYN_STATUS_ERROR, 0);
                return;
            }

            let response = ((bytes_read as u64) << 32) | (shmem_handle as u64);
            let _ = reply(src, response, 0);
        }
    }
}

fn handle_sql_query<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
    backend: Backend,
) {
    let query_type = ((msg.payload0 >> 16) & 0xFF) as u8;
    match backend {
        Backend::Sqlite => match query_type {
            0 | 1 => {
                let count = count_sqlite_files(sqlite);
                let _ = reply(src, count as u64, 0);
            }
            _ => { let _ = reply(src, SYN_STATUS_INVALID, 0); }
        },
        Backend::Fpk => { let _ = reply(src, SYN_STATUS_INVALID, 0); }
    }
}

// ── Delete handler ────────────────────────────────────────────────────

/// Phase C / Issue #100: delete a file row from the `files` table by
/// name hash. Resolves the hash to a rowid via `table_scan` (same
/// path `read_file_by_name` uses), then calls `sqlite_delete_file_by_rowid`
/// to remove the leaf-page cell pointer + decrement cell_count.
///
/// Wire shape: `op | (name_hash << 16)`. Reply is one of:
///   - 0          — success
///   - NOT_FOUND  — no row with matching name hash
///   - ERROR      — btree corruption or backend mismatch
///
/// After a successful delete the dir cache is invalidated so
/// subsequent listings reflect the smaller set; refresh happens
/// lazily on the next file-listing request.
fn handle_delete_file<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &mut SafeSqliteBuffer,
    cache: &mut DirCacheState,
    backend: Backend,
) {
    if backend != Backend::Sqlite {
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }
    let name_hash = ((msg.payload0 >> 16) & 0xFFFFFFFF) as u32;

    // Phase 1: resolve name hash → rowid via a table_scan view.
    // Hash collisions are technically possible (FNV-1a 32-bit) but the
    // file count is small and the cost of a wrong delete is low (the
    // operator can re-write the file). First match wins, same policy
    // `read_file_by_name` already follows.
    let rowid: i64 = {
        let db = match open_db(sqlite) {
            Some(db) => db,
            None => {
                let _ = reply(src, SYN_STATUS_ERROR, 0);
                return;
            }
        };
        let scanner = match db.table_scan("files") {
            Ok(s) => s,
            Err(_) => {
                let _ = reply(src, SYN_STATUS_ERROR, 0);
                return;
            }
        };
        let mut found: Option<i64> = None;
        for result in scanner {
            if let Ok(record) = result {
                if let Some(Value::Text(rec_name)) = record.get(1) {
                    if hash_name(rec_name) == name_hash {
                        if let Some(id) = record.get(0).and_then(|v| v.as_int()) {
                            found = Some(id);
                            break;
                        }
                    }
                }
            }
        }
        match found {
            Some(id) => id,
            None => {
                let _ = reply(src, SYN_STATUS_NOT_FOUND, 0);
                return;
            }
        }
    };

    // Phase 2: remove the cell. (sqlite_delete_file_by_rowid takes
    // &mut SafeSqliteBuffer, so the table_scan borrow above had to
    // drop before this call — hence the inner scope.)
    if !sqlite_delete_file_by_rowid(sqlite, rowid) {
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    // Phase 3: invalidate the dir cache so the next listing picks
    // up the smaller set. Lazy refresh happens on first list/scan.
    cache.valid = false;

    println!(
        "[SYNAPSE] delete_file: removed rowid={} (name_hash=0x{:x})",
        rowid, name_hash
    );
    let _ = reply(src, 0, 0);
}

// ── Write handler ─────────────────────────────────────────────────────

fn handle_write_file<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &mut SafeSqliteBuffer,
    cache: &mut DirCacheState,
    backend: Backend,
) {
    if backend != Backend::Sqlite {
        println!("[SYNAPSE] write_file: SQLite backend required");
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    let shmem_handle = ((msg.payload0 >> 16) & 0xFFFF) as u32;
    let total_size = (msg.payload0 >> 32) as usize;

    if shmem_handle == 0 || total_size < 3 {
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    // Map and parse, then drop the arena before any DB mutation
    const MAX_CONTENT: usize = 16384;
    let mut content_buf = [0u8; MAX_CONTENT];
    let mut name_buf = [0u8; 32];
    let mut content_len = 0usize;
    let mut name_len = 0usize;
    let mut ok = false;
    {
        let arena = match ShmemArena::map(shmem_handle, vaddr::WRITE_SHMEM) {
            Ok(a) => a,
            Err(_) => {
                println!("[SYNAPSE] write_file: failed to map shmem {}", shmem_handle);
                let _ = reply(src, SYN_STATUS_ERROR, 0);
                return;
            }
        };
        let shmem_slice = unsafe { arena.as_slice(total_size) };

        let nl_field = u16::from_le_bytes([shmem_slice[0], shmem_slice[1]]) as usize;
        if nl_field == 0 || 2 + nl_field > total_size {
            let _ = reply(src, SYN_STATUS_ERROR, 0);
            return;
        }
        let name_bytes = &shmem_slice[2..2 + nl_field];
        let content = &shmem_slice[2 + nl_field..total_size];

        if core::str::from_utf8(name_bytes).is_err() {
            let _ = reply(src, SYN_STATUS_ERROR, 0);
            return;
        }

        // Copy out before arena drops
        content_len = content.len().min(MAX_CONTENT);
        content_buf[..content_len].copy_from_slice(&content[..content_len]);
        if content.len() > MAX_CONTENT {
            println!("[SYNAPSE] WARNING: file truncated from {} to {} bytes", content.len(), MAX_CONTENT);
        }
        name_len = nl_field.min(32);
        name_buf[..name_len].copy_from_slice(&name_bytes[..name_len]);
        ok = true;
    } // arena drops here → auto unmap

    if !ok { return; }

    let name = unsafe { core::str::from_utf8_unchecked(&name_buf[..name_len]) };
    let name_hash = hash_name(name);

    // Check for duplicate in cache
    let duplicate = cache.entries[..cache.count]
        .iter()
        .any(|e| hash_name(e.name_str()) == name_hash);
    if duplicate {
        // M12: in-place overwrite
        if overwrite_blob_inplace(sqlite, name, &content_buf[..content_len]) {
            println!("[SYNAPSE] write_file: '{}' overwritten in-place ({} bytes)", name, content_len);
            flush_sqlite_to_disk(sqlite);
            let _ = reply(src, 0, 0);
        } else {
            println!("[SYNAPSE] write_file: '{}' overwrite failed", name);
            let _ = reply(src, SYN_STATUS_ERROR, 0);
        }
        return;
    }

    // New file: insert via B-tree
    let next_rowid = find_max_rowid(sqlite) + 1;

    if !sqlite_insert_file(sqlite, next_rowid, name, &content_buf[..content_len]) {
        println!("[SYNAPSE] write_file: cell insert failed");
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    update_dir_cache(cache, next_rowid, &name_buf[..name_len], content_len);

    // Auto-detect MIME and create intent record
    let mime = auto_detect_mime(name, &content_buf[..content_len]);
    let _ = sqlite_insert_intent(sqlite, next_rowid, mime, "");

    if !flush_sqlite_to_disk(sqlite) {
        println!("[SYNAPSE] write_file: disk flush failed (data in memory only)");
    }

    println!("[SYNAPSE] Wrote '{}' ({} bytes, rowid={}, mime={})",
        name, content_len, next_rowid, mime);
    let _ = reply(src, next_rowid as u64, 0);
}

// ── Intent handlers ───────────────────────────────────────────────────

fn handle_write_intent<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &mut SafeSqliteBuffer,
    backend: Backend,
) {
    if backend != Backend::Sqlite {
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    let shmem_handle = ((msg.payload0 >> 16) & 0xFFFF) as u32;
    let total_size = (msg.payload0 >> 32) as usize;

    if shmem_handle == 0 || total_size < 7 {
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    let mut mime_buf = [0u8; 128];
    let mut json_buf = [0u8; 1024];
    let mut ml = 0usize;
    let mut jl = 0usize;
    let mut file_id = 0u32;
    {
        let arena = match ShmemArena::map(shmem_handle, vaddr::INTENT_SHMEM) {
            Ok(a) => a,
            Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        let shmem_slice = unsafe { arena.as_slice(total_size) };
        file_id = u32::from_le_bytes([shmem_slice[0], shmem_slice[1], shmem_slice[2], shmem_slice[3]]);
        let mime_len = u16::from_le_bytes([shmem_slice[4], shmem_slice[5]]) as usize;
        if 6 + mime_len > total_size {
            let _ = reply(src, SYN_STATUS_ERROR, 0);
            return;
        }
        ml = mime_len.min(128);
        mime_buf[..ml].copy_from_slice(&shmem_slice[6..6 + ml]);
        let json_start = 6 + mime_len;
        let json_len = total_size - json_start;
        jl = json_len.min(1024);
        if jl > 0 {
            json_buf[..jl].copy_from_slice(&shmem_slice[json_start..json_start + jl]);
        }
    } // auto unmap

    let mime_str = unsafe { core::str::from_utf8_unchecked(&mime_buf[..ml]) };
    let json_str = unsafe { core::str::from_utf8_unchecked(&json_buf[..jl]) };

    if sqlite_insert_intent(sqlite, file_id as i64, mime_str, json_str) {
        flush_sqlite_to_disk(sqlite);
        println!("[SYNAPSE] Intent stored for file_id={} mime={}", file_id, mime_str);
        let _ = reply(src, 0, 0);
    } else {
        println!("[SYNAPSE] Intent insert failed for file_id={}", file_id);
        let _ = reply(src, SYN_STATUS_ERROR, 0);
    }
}

fn handle_read_intent<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
) {
    let file_id = ((msg.payload0 >> 16) & 0xFFFF) as i64;
    let mut buf = [0u8; 2048];
    match sqlite_read_intent(sqlite, file_id, &mut buf) {
        Some((mime_len, json_len)) => {
            let total = 2 + mime_len + json_len;
            let shmem_size = 4096;
            match shmem_create(shmem_size) {
                Ok(handle) => {
                    let _ = shmem_grant(handle, msg.sender);
                    let mapped = ShmemArena::map(handle, vaddr::INTENT_SHMEM);
                    if let Ok(mut arena) = mapped {
                        let dst = unsafe { arena.as_mut_slice(shmem_size) };
                        let ml = (mime_len as u16).to_le_bytes();
                        dst[0] = ml[0];
                        dst[1] = ml[1];
                        dst[2..2 + mime_len].copy_from_slice(&buf[..mime_len]);
                        dst[2 + mime_len..2 + mime_len + json_len]
                            .copy_from_slice(&buf[mime_len..mime_len + json_len]);
                        drop(arena);
                        let _ = reply(src, ((total as u64) << 32) | handle as u64, 0);
                    } else {
                        let _ = shmem_destroy(handle);
                        let _ = reply(src, SYN_STATUS_ERROR, 0);
                    }
                }
                Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); }
            }
        }
        None => { let _ = reply(src, SYN_STATUS_NOT_FOUND, 0); }
    }
}

fn handle_query_mime<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
) {
    let mime_hash = ((msg.payload0 >> 32) & 0xFFFFFFFF) as u32;
    if let Some(db) = open_db(sqlite) {
        if let Ok(scanner) = db.table_scan("file_intents") {
            for result in scanner {
                if let Ok(record) = result {
                    if let Some(Value::Text(mime)) = record.get(1) {
                        if hash_name(mime) == mime_hash {
                            let file_id = record.rowid as u16;
                            let size = get_file_size(sqlite, file_id as i64);
                            let _ = reply(src, (size as u64) << 32 | file_id as u64, 0);
                            return;
                        }
                    }
                }
            }
        }
    }
    let _ = reply(src, SYN_STATUS_NOT_FOUND, 0);
}

fn handle_query_intent<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
) {
    let shmem_handle = ((msg.payload0 >> 16) & 0xFFFF) as u32;
    let query_len = (msg.payload0 >> 32) as usize;

    if shmem_handle == 0 || query_len == 0 || query_len > 256 {
        let _ = reply(src, SYN_STATUS_NOT_FOUND, 0);
        return;
    }

    let mut query_buf = [0u8; 256];
    let ql;
    {
        let arena = match ShmemArena::map(shmem_handle, vaddr::QUERY_INTENT) {
            Ok(a) => a,
            Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        let query_slice = unsafe { arena.as_slice(query_len) };
        ql = query_len.min(256);
        query_buf[..ql].copy_from_slice(&query_slice[..ql]);
    }

    let query = match core::str::from_utf8(&query_buf[..ql]) {
        Ok(s) => s,
        Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
    };

    let mut query_lower = [0u8; 256];
    for (i, b) in query.bytes().enumerate().take(256) {
        query_lower[i] = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
    }
    let query_lc = unsafe { core::str::from_utf8_unchecked(&query_lower[..ql]) };

    let mut best_file_id: i64 = -1;
    let mut best_score: usize = 0;

    if let Some(db) = open_db(sqlite) {
        if let Ok(scanner) = db.table_scan("file_intents") {
            for result in scanner {
                if let Ok(record) = result {
                    let file_id = record.rowid;
                    if let Some(Value::Text(json)) = record.get(2) {
                        let mut json_lower = [0u8; 1024];
                        let jl = json.len().min(1024);
                        for (i, b) in json.bytes().enumerate().take(jl) {
                            json_lower[i] = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
                        }
                        let json_lc = unsafe { core::str::from_utf8_unchecked(&json_lower[..jl]) };
                        let mut score = 0;
                        for word in query_lc.split_whitespace() {
                            if json_lc.contains(word) { score += 1; }
                        }
                        if score > best_score {
                            best_score = score;
                            best_file_id = file_id;
                        }
                    }
                    if let Some(Value::Text(mime)) = record.get(1) {
                        if mime.contains(query_lc) || query_lc.contains(mime) {
                            if best_score == 0 {
                                best_file_id = file_id;
                                best_score = 1;
                            }
                        }
                    }
                }
            }
        }
    }

    if best_file_id >= 0 {
        let size = get_file_size(sqlite, best_file_id);
        println!("[SYNAPSE] Intent query '{}' → file_id={} (score={})", query, best_file_id, best_score);
        let _ = reply(src, (size as u64) << 32 | best_file_id as u64, 0);
    } else {
        println!("[SYNAPSE] Intent query '{}' → not found", query);
        let _ = reply(src, SYN_STATUS_NOT_FOUND, 0);
    }
}

// ── Vector handlers ───────────────────────────────────────────────────

fn handle_vector_search<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
    backend: Backend,
) {
    let k = ((msg.payload0 >> 16) & 0xFF) as usize;
    let query_shmem = (msg.payload0 >> 32) as u32;
    let requester_task = msg.sender;

    if backend != Backend::Sqlite {
        let _ = reply(src, SYN_STATUS_INVALID, 0);
        return;
    }
    if k == 0 || k > 100 {
        let _ = reply(src, SYN_STATUS_INVALID, 0);
        return;
    }
    if query_shmem == 0 {
        let _ = reply(src, SYN_STATUS_INVALID, 0);
        return;
    }

    let query_embedding = {
        let arena = match ShmemArena::map(query_shmem, vaddr::VECTOR_QUERY) {
            Ok(a) => a,
            Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        let query_slice = unsafe { arena.as_slice(EMBEDDING_SIZE) };
        match Embedding::from_blob(query_slice) {
            Ok(e) => e,
            Err(_) => { let _ = reply(src, SYN_STATUS_INVALID, 0); return; }
        }
    };

    let db = match open_db(sqlite) {
        Some(db) => db,
        None => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
    };

    let mut results = [SearchResult::default(); 100];
    let result_count = match search_similar_auto(&db, &query_embedding, k, &mut results) {
        Ok(c) => c,
        Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
    };

    if result_count == 0 {
        let _ = reply(src, 0, 0);
        return;
    }

    let results_size = result_count * 8;
    let buffer_size = ((results_size + 4095) / 4096) * 4096;
    let buffer_size = if buffer_size == 0 { 4096 } else { buffer_size };

    let result_shmem = match shmem_create(buffer_size) {
        Ok(h) => h,
        Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
    };
    if shmem_grant(result_shmem, requester_task).is_err() {
        let _ = shmem_destroy(result_shmem);
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    {
        let mut arena = match ShmemArena::map(result_shmem, vaddr::VECTOR_RESULTS) {
            Ok(a) => a,
            Err(_) => {
                let _ = shmem_destroy(result_shmem);
                let _ = reply(src, SYN_STATUS_ERROR, 0);
                return;
            }
        };
        let dst = unsafe { arena.as_mut_slice(buffer_size) };
        for (i, result) in results[..result_count].iter().enumerate() {
            let off = i * 8;
            dst[off..off + 4].copy_from_slice(&result.file_id.to_le_bytes());
            dst[off + 4..off + 8].copy_from_slice(&result.similarity.to_le_bytes());
        }
    } // auto unmap

    let response = ((result_count as u64) << 32) | (result_shmem as u64);
    let _ = reply(src, response, 0);
}

fn handle_get_embedding<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
    backend: Backend,
) {
    let file_id = ((msg.payload0 >> 16) & 0xFFFF) as u32;
    let requester_task = msg.sender;

    if backend != Backend::Sqlite {
        let _ = reply(src, SYN_STATUS_INVALID, 0);
        return;
    }

    let db = match open_db(sqlite) {
        Some(db) => db,
        None => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
    };

    let embedding = match get_embedding_by_file_id(&db, file_id) {
        Ok(Some(e)) => e,
        Ok(None) => { let _ = reply(src, SYN_STATUS_NOT_FOUND, 0); return; }
        Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
    };

    let shmem_handle = match shmem_create(4096) {
        Ok(h) => h,
        Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
    };
    if shmem_grant(shmem_handle, requester_task).is_err() {
        let _ = shmem_destroy(shmem_handle);
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    {
        let mut arena = match ShmemArena::map(shmem_handle, vaddr::VECTOR_QUERY) {
            Ok(a) => a,
            Err(_) => {
                let _ = shmem_destroy(shmem_handle);
                let _ = reply(src, SYN_STATUS_ERROR, 0);
                return;
            }
        };
        let dst = unsafe { arena.as_mut_slice(EMBEDDING_SIZE) };
        embedding.to_blob(dst);
    }

    let response = ((EMBEDDING_SIZE as u64) << 32) | (shmem_handle as u64);
    let _ = reply(src, response, 0);
}

fn handle_embedding_count<T: TokenSource>(
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
    backend: Backend,
) {
    if backend != Backend::Sqlite {
        let _ = reply(src, 0, 0);
        return;
    }
    let db = match open_db(sqlite) {
        Some(db) => db,
        None => { let _ = reply(src, 0, 0); return; }
    };
    let count = count_embeddings(&db).unwrap_or(0);
    let _ = reply(src, count as u64, 0);
}

// ════════════════════════════════════════════════════════════════════════
// Phase 9: Bi-Temporal Knowledge Graph handlers
// ════════════════════════════════════════════════════════════════════════

/// Decode a length-prefixed string from a shared-memory request buffer.
/// Layout: [len: u16 LE][bytes]. Advances `pos` past the field.
/// Returns `None` if the slice would run past `end`.
fn read_prefixed_str<'a>(slice: &'a [u8], pos: &mut usize, end: usize) -> Option<&'a str> {
    if *pos + 2 > end { return None; }
    let len = u16::from_le_bytes([slice[*pos], slice[*pos + 1]]) as usize;
    *pos += 2;
    if *pos + len > end { return None; }
    let s = core::str::from_utf8(&slice[*pos..*pos + len]).ok()?;
    *pos += len;
    Some(s)
}

/// Monotonic timestamp for valid_from / valid_to fields. We can't
/// easily call into the kernel uptime syscall from synapse-service
/// without pulling more crates in, so we use a process-local counter
/// that ticks on every operation. The important guarantee for
/// temporal supersession is ORDERING, not wall-clock accuracy.
fn next_timestamp() -> i64 {
    use core::sync::atomic::{AtomicI64, Ordering};
    static CLOCK: AtomicI64 = AtomicI64::new(1);
    CLOCK.fetch_add(1, Ordering::Relaxed)
}

fn handle_upsert_entity<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &mut SafeSqliteBuffer,
    backend: Backend,
) {
    if backend != Backend::Sqlite {
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    let shmem_handle = ((msg.payload0 >> 16) & 0xFFFF) as u32;
    let total_size = (msg.payload0 >> 32) as usize;
    if shmem_handle == 0 || total_size < 6 {
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    // Copy out into owned local buffers before touching sqlite (the
    // ShmemArena borrow conflicts with the &mut SafeSqliteBuffer).
    let mut eid_buf = [0u8; 128];
    let mut name_buf = [0u8; 128];
    let mut type_buf = [0u8; 64];
    let mut eid_len = 0usize;
    let mut name_len = 0usize;
    let mut type_len = 0usize;
    {
        let arena = match ShmemArena::map(shmem_handle, vaddr::ENTITY_SHMEM) {
            Ok(a) => a,
            Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        let slice = unsafe { arena.as_slice(total_size) };
        let mut pos = 0usize;
        let eid = match read_prefixed_str(slice, &mut pos, total_size) {
            Some(s) => s,
            None => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        let name = match read_prefixed_str(slice, &mut pos, total_size) {
            Some(s) => s,
            None => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        let etype = match read_prefixed_str(slice, &mut pos, total_size) {
            Some(s) => s,
            None => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        eid_len = eid.len().min(eid_buf.len());
        name_len = name.len().min(name_buf.len());
        type_len = etype.len().min(type_buf.len());
        eid_buf[..eid_len].copy_from_slice(&eid.as_bytes()[..eid_len]);
        name_buf[..name_len].copy_from_slice(&name.as_bytes()[..name_len]);
        type_buf[..type_len].copy_from_slice(&etype.as_bytes()[..type_len]);
    }

    let eid = unsafe { core::str::from_utf8_unchecked(&eid_buf[..eid_len]) };
    let name = unsafe { core::str::from_utf8_unchecked(&name_buf[..name_len]) };
    let etype = unsafe { core::str::from_utf8_unchecked(&type_buf[..type_len]) };

    let ts = next_timestamp();
    match sqlite_insert_entity(sqlite, eid, name, etype, ts) {
        Some(rowid) => {
            flush_sqlite_to_disk(sqlite);
            // Shift rowid into bits 16..48 so a value of exactly
            // 0xFFFF (= `SYN_STATUS_ERROR`) can't be mistaken for a
            // failure reply. The libfolk wrapper unpacks via
            // `(ret >> 16) & 0xFFFFFFFF`.
            let _ = reply(src, (rowid as u64) << 16, 0);
        }
        None => { let _ = reply(src, SYN_STATUS_ERROR, 0); }
    }
}

fn handle_upsert_edge<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &mut SafeSqliteBuffer,
    backend: Backend,
) {
    if backend != Backend::Sqlite {
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    let shmem_handle = ((msg.payload0 >> 16) & 0xFFFF) as u32;
    let total_size = (msg.payload0 >> 32) as usize;
    if shmem_handle == 0 || total_size < 8 {
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    let mut eid_buf = [0u8; 128];
    let mut subj_buf = [0u8; 128];
    let mut pred_buf = [0u8; 64];
    let mut obj_buf = [0u8; 128];
    let (mut eid_len, mut subj_len, mut pred_len, mut obj_len) = (0usize, 0usize, 0usize, 0usize);
    {
        let arena = match ShmemArena::map(shmem_handle, vaddr::EDGE_SHMEM) {
            Ok(a) => a,
            Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        let slice = unsafe { arena.as_slice(total_size) };
        let mut pos = 0usize;
        let eid = match read_prefixed_str(slice, &mut pos, total_size) {
            Some(s) => s, None => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        let subj = match read_prefixed_str(slice, &mut pos, total_size) {
            Some(s) => s, None => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        let pred = match read_prefixed_str(slice, &mut pos, total_size) {
            Some(s) => s, None => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        let obj = match read_prefixed_str(slice, &mut pos, total_size) {
            Some(s) => s, None => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        eid_len = eid.len().min(eid_buf.len());
        subj_len = subj.len().min(subj_buf.len());
        pred_len = pred.len().min(pred_buf.len());
        obj_len = obj.len().min(obj_buf.len());
        eid_buf[..eid_len].copy_from_slice(&eid.as_bytes()[..eid_len]);
        subj_buf[..subj_len].copy_from_slice(&subj.as_bytes()[..subj_len]);
        pred_buf[..pred_len].copy_from_slice(&pred.as_bytes()[..pred_len]);
        obj_buf[..obj_len].copy_from_slice(&obj.as_bytes()[..obj_len]);
    }

    let eid = unsafe { core::str::from_utf8_unchecked(&eid_buf[..eid_len]) };
    let subj = unsafe { core::str::from_utf8_unchecked(&subj_buf[..subj_len]) };
    let pred = unsafe { core::str::from_utf8_unchecked(&pred_buf[..pred_len]) };
    let obj = unsafe { core::str::from_utf8_unchecked(&obj_buf[..obj_len]) };

    // Step 1: expire any existing active edge for the same (subject, predicate).
    let ts = next_timestamp();
    let expired = sqlite_expire_edge(sqlite, subj, pred, ts);
    if expired > 0 {
        println!("[SYNAPSE] upsert_edge: superseded {} prior edge(s)", expired);
    }

    // Step 2: insert the new edge with valid_to = 0 (active).
    match sqlite_insert_edge(sqlite, eid, subj, pred, obj, ts, 0, 1.0) {
        Some(rowid) => {
            flush_sqlite_to_disk(sqlite);
            // Same shift pattern as `handle_upsert_entity` — keeps
            // rowid out of the low-16-bit status-code window.
            let _ = reply(src, (rowid as u64) << 16, 0);
        }
        None => { let _ = reply(src, SYN_STATUS_ERROR, 0); }
    }
}

fn handle_graph_walk<T: TokenSource>(
    msg: AsyncIpcMessage,
    src: &mut T,
    sqlite: &SafeSqliteBuffer,
    backend: Backend,
) {
    if backend != Backend::Sqlite {
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    let shmem_handle = ((msg.payload0 >> 16) & 0xFFFF) as u32;
    let max_depth = ((msg.payload0 >> 32) & 0xFFFFFFFF) as u32;
    if shmem_handle == 0 {
        let _ = reply(src, SYN_STATUS_ERROR, 0);
        return;
    }

    // Copy the start entity id out of shmem into a local buffer.
    let mut start_buf = [0u8; 128];
    let mut start_len = 0usize;
    {
        let arena = match ShmemArena::map(shmem_handle, vaddr::GRAPH_WALK) {
            Ok(a) => a,
            Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        let slice = unsafe { arena.as_slice(4096) };
        let mut pos = 0usize;
        let s = match read_prefixed_str(slice, &mut pos, slice.len()) {
            Some(s) => s,
            None => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
        };
        start_len = s.len().min(start_buf.len());
        start_buf[..start_len].copy_from_slice(&s.as_bytes()[..start_len]);
    }
    let start = unsafe { core::str::from_utf8_unchecked(&start_buf[..start_len]) };

    // Run the BFS.
    let hops = sqlite_graph_walk(sqlite, start, max_depth);

    println!("[SYNAPSE] graph_walk from '{}' max_depth={} found {} hop(s)",
        start, max_depth, hops.len());

    // Write results back into the same shmem region.
    // Layout: [hop_count: u16 LE] repeated [eid_len:u16 LE][eid][depth:u16 LE]
    let mut arena = match ShmemArena::map(shmem_handle, vaddr::GRAPH_WALK) {
        Ok(a) => a,
        Err(_) => { let _ = reply(src, SYN_STATUS_ERROR, 0); return; }
    };
    let slice = unsafe { arena.as_mut_slice(4096) };
    let hop_count = (hops.len() as u16).min(255);
    slice[0..2].copy_from_slice(&hop_count.to_le_bytes());
    let mut pos = 2usize;
    for hop in hops.iter().take(hop_count as usize) {
        let eid_bytes = hop.entity_id.as_bytes();
        let eid_len = eid_bytes.len().min(128);
        if pos + 2 + eid_len + 2 > slice.len() { break; }
        slice[pos..pos + 2].copy_from_slice(&(eid_len as u16).to_le_bytes());
        pos += 2;
        slice[pos..pos + eid_len].copy_from_slice(&eid_bytes[..eid_len]);
        pos += eid_len;
        slice[pos..pos + 2].copy_from_slice(&(hop.depth as u16).to_le_bytes());
        pos += 2;
        println!("[SYNAPSE]   hop: {} (depth={})", hop.entity_id, hop.depth);
    }
    drop(arena);

    // Shift the hop count into bits 16..31 so it can't collide with
    // the low-valued status sentinels (SYN_STATUS_NOT_FOUND=1,
    // SYN_STATUS_INVALID=2, ...). The libfolk wrapper unpacks via
    // `(ret >> 16) & 0xFFFF`.
    let _ = reply(src, (hop_count as u64) << 16, 0);
}
