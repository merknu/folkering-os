//! Directory cache management.
//!
//! Synapse maintains an in-memory `DirCacheState` so that file lookups
//! don't need a SQLite table scan on every IPC request. The cache is
//! populated at boot from either the FPK ramdisk or the SQLite `files`
//! table, and updated incrementally on each `WRITE_FILE`.

extern crate alloc;

use libfolk::sys::fs::{read_dir, DirEntry};
use libsqlite::{SqliteDb, Value};

use crate::state::{Backend, DirCacheState, KIND_ELF, SafeSqliteBuffer};

/// Open a SqliteDb view over the loaded portion of `buf`. Returns None if
/// the buffer is not valid or SqliteDb::open fails.
pub fn open_db<'a>(buf: &'a SafeSqliteBuffer) -> Option<SqliteDb<'a>> {
    if !buf.is_valid() {
        return None;
    }
    SqliteDb::open(buf.loaded()).ok()
}

/// Count files in the SQLite `files` table. Used by FILE_COUNT handler.
pub fn count_sqlite_files(buf: &SafeSqliteBuffer) -> usize {
    let db = match open_db(buf) {
        Some(db) => db,
        None => return 0,
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

/// Refresh the FPK directory cache from the ramdisk.
pub fn refresh_fpk_cache(cache: &mut DirCacheState) {
    cache.entries.resize(64, DirEntry {
        id: 0, entry_type: 0, name: [0u8; 32], size: 0,
    });
    let result = read_dir(&mut cache.entries);
    cache.entries.truncate(result);
    cache.count = result;
    cache.valid = true;
}

/// Populate `cache` from SQLite `files` table (called once at init).
/// Avoids repeated table scans — all handlers use the cached entries.
pub fn refresh_sqlite_cache(buf: &SafeSqliteBuffer, cache: &mut DirCacheState) {
    let Some(db) = open_db(buf) else { return; };
    let Ok(scanner) = db.table_scan("files") else { return; };

    cache.entries.clear();
    cache.count = 0;

    for result in scanner {
        if let Ok(record) = result {
            if let Some(Value::Text(name)) = record.get(1) {
                let name_bytes = name.as_bytes();
                let name_len = name_bytes.len().min(32);

                let mut entry = DirEntry {
                    id: 0, entry_type: 0, name: [0u8; 32], size: 0,
                };
                entry.name[..name_len].copy_from_slice(&name_bytes[..name_len]);
                entry.id = record.get(0)
                    .and_then(|v| v.as_int())
                    .unwrap_or(0) as u16;
                entry.size = record.get(3)
                    .and_then(|v| v.as_int())
                    .unwrap_or(0) as u64;
                entry.entry_type = if record.get(2)
                    .and_then(|v| v.as_int())
                    .unwrap_or(1) == KIND_ELF { 0 } else { 1 };

                cache.entries.push(entry);
                cache.count = cache.entries.len();
            }
        }
    }
    cache.valid = true;
}

/// Update the directory cache after a successful WRITE_FILE.
/// Appends a new DirEntry without re-scanning the database.
pub fn update_dir_cache(cache: &mut DirCacheState, rowid: i64, name_bytes: &[u8], size: usize) {
    let mut entry = DirEntry {
        id: rowid as u16,
        entry_type: 1, // DATA type
        name: [0u8; 32],
        size: size as u64,
    };
    let nl = name_bytes.len().min(32);
    entry.name[..nl].copy_from_slice(&name_bytes[..nl]);
    cache.entries.push(entry);
    cache.count = cache.entries.len();
}

/// Look up a file's size by rowid from the SQLite `files` table.
pub fn get_file_size(buf: &SafeSqliteBuffer, file_id: i64) -> u32 {
    let Some(db) = open_db(buf) else { return 0; };
    let Ok(scanner) = db.table_scan("files") else { return 0; };
    for result in scanner {
        if let Ok(record) = result {
            if record.rowid == file_id {
                if let Some(Value::Integer(size)) = record.get(3) {
                    return size as u32;
                }
            }
        }
    }
    0
}

/// Find the maximum rowid currently in the `files` table (for assigning
/// the next insert).
pub fn find_max_rowid(buf: &SafeSqliteBuffer) -> i64 {
    let Some(db) = open_db(buf) else { return 0; };
    let Ok(scanner) = db.table_scan("files") else { return 0; };
    let mut max_id: i64 = 0;
    for result in scanner {
        if let Ok(record) = result {
            if record.rowid > max_id {
                max_id = record.rowid;
            }
        }
    }
    max_id
}
