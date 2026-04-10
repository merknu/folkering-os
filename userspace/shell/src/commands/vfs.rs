//! VFS commands: cat, sql, save, load.

use libfolk::{print, println};
use libfolk::sys::block::{self, SECTOR_SIZE, DATA_START_SECTOR};
use libfolk::sys::fs::DirEntry;
use libfolk::sys::synapse::{file_count, read_file_shmem, write_file};
use libfolk::sys::{shmem_map, shmem_unmap};

use crate::ui::SHELL_SHMEM_VADDR;

/// User data sector for legacy load command
const USER_DATA_SECTOR: u64 = DATA_START_SECTOR + 200;

pub fn cmd_cat<'a>(mut args: impl Iterator<Item = &'a str>) {
    let filename = match args.next() {
        Some(f) => f,
        None => { println!("usage: cat <filename>"); return; }
    };

    let response = match read_file_shmem(filename) {
        Ok(r) => r,
        Err(_) => { println!("cat: {}: not found", filename); return; }
    };

    if response.size == 0 {
        println!("cat: {}: empty file", filename);
        return;
    }

    if shmem_map(response.shmem_handle, SHELL_SHMEM_VADDR).is_err() {
        println!("cat: failed to map file buffer");
        return;
    }

    let buffer = unsafe {
        core::slice::from_raw_parts(SHELL_SHMEM_VADDR as *const u8, response.size as usize)
    };

    for &b in buffer {
        if b == b'\n' || b == b'\r' || b == b'\t' || (b >= 0x20 && b < 0x7F) {
            print!("{}", b as char);
        } else if b == 0 {
            break;
        } else {
            print!(".");
        }
    }
    println!();

    let _ = shmem_unmap(response.shmem_handle, SHELL_SHMEM_VADDR);
}

pub fn cmd_save<'a>(mut parts: impl Iterator<Item = &'a str>) {
    let filename = match parts.next() {
        Some(f) => f,
        None => { println!("Usage: save <filename> <text>"); return; }
    };

    let mut buf = [0u8; 4096];
    let mut pos = 0usize;
    let mut first = true;
    for word in parts {
        if !first && pos < buf.len() {
            buf[pos] = b' ';
            pos += 1;
        }
        first = false;
        let bytes = word.as_bytes();
        let copy_len = bytes.len().min(buf.len() - pos);
        if copy_len == 0 { break; }
        buf[pos..pos + copy_len].copy_from_slice(&bytes[..copy_len]);
        pos += copy_len;
    }

    if pos == 0 {
        println!("Usage: save <filename> <text>");
        return;
    }

    match write_file(filename, &buf[..pos]) {
        Ok(()) => println!("[VFS] Saved '{}' ({} bytes) to SQLite", filename, pos),
        Err(e) => println!("[VFS] Write failed: {:?}", e),
    }
}

pub fn cmd_load() {
    let mut buf = [0u8; SECTOR_SIZE];
    match block::read_sector(USER_DATA_SECTOR, &mut buf) {
        Ok(()) => {
            let text_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
            if text_len == 0 || text_len > SECTOR_SIZE - 4 {
                println!("[STORAGE] No saved data (or corrupted)");
                return;
            }
            if let Ok(text) = core::str::from_utf8(&buf[4..4 + text_len]) {
                println!("{}", text);
                println!("[STORAGE] Loaded {} bytes from sector {}", text_len, USER_DATA_SECTOR);
            } else {
                println!("[STORAGE] Data is not valid UTF-8");
            }
        }
        Err(e) => {
            println!("[STORAGE] Read failed: {:?}", e);
        }
    }
}

/// Execute a simple SELECT query on the files table.
pub fn cmd_sql(full_cmd: &str) {
    let query = if let Some(start) = full_cmd.find('"') {
        if let Some(end) = full_cmd[start + 1..].find('"') {
            &full_cmd[start + 1..start + 1 + end]
        } else {
            println!("sql: missing closing quote");
            return;
        }
    } else {
        let trimmed = full_cmd.strip_prefix("sql ").unwrap_or("");
        if trimmed.is_empty() {
            println!("usage: sql \"SELECT ... FROM files\"");
            return;
        }
        trimmed
    };

    if !query.to_uppercase_simple().starts_with("SELECT ") {
        println!("sql: only SELECT queries are supported");
        return;
    }

    if !query.to_uppercase_simple().contains("FROM FILES") {
        println!("sql: only 'files' table is available");
        return;
    }

    // Parse columns
    let columns_part = &query[7..];
    let from_pos = columns_part.to_uppercase_simple().find(" FROM");
    let columns_str = match from_pos {
        Some(pos) => columns_part[..pos].trim(),
        None => { println!("sql: invalid query syntax"); return; }
    };

    let show_name = columns_str == "*" ||
                    columns_str.to_uppercase_simple().contains("NAME");
    let show_size = columns_str == "*" ||
                    columns_str.to_uppercase_simple().contains("SIZE");
    let show_kind = columns_str == "*" ||
                    columns_str.to_uppercase_simple().contains("KIND") ||
                    columns_str.to_uppercase_simple().contains("TYPE");

    let count = match file_count() {
        Ok(c) => c,
        Err(_) => { println!("sql: Synapse not available"); return; }
    };
    if count == 0 { println!("(0 rows)"); return; }

    let mut entries = [DirEntry { id: 0, entry_type: 0, name: [0u8; 32], size: 0 }; 16];
    let dir_count = libfolk::sys::fs::read_dir(&mut entries);

    println!();
    for i in 0..dir_count.min(count) {
        let entry = &entries[i];
        let name = entry.name_str();

        if show_name && show_size && show_kind {
            let kind = if entry.is_elf() { "elf" } else { "data" };
            println!("{:<16} {:>8} {}", name, entry.size, kind);
        } else if show_name && show_size {
            println!("{:<16} {:>8}", name, entry.size);
        } else if show_name && show_kind {
            let kind = if entry.is_elf() { "elf" } else { "data" };
            println!("{:<16} {}", name, kind);
        } else if show_name {
            println!("{}", name);
        } else if show_size {
            println!("{}", entry.size);
        }
    }
    println!("\n({} rows)", dir_count.min(count));
}

// ── Allocation-free uppercase comparison helpers ──────────────────────

/// Trait that wraps a `&str` in a lazy uppercase view supporting
/// `starts_with`, `contains`, and `find` without allocating.
trait ToUppercaseSimple {
    fn to_uppercase_simple(&self) -> SimpleUpper;
}

impl ToUppercaseSimple for &str {
    fn to_uppercase_simple(&self) -> SimpleUpper {
        SimpleUpper { s: self }
    }
}

struct SimpleUpper<'a> {
    s: &'a str,
}

impl<'a> SimpleUpper<'a> {
    fn starts_with(&self, prefix: &str) -> bool {
        if self.s.len() < prefix.len() {
            return false;
        }
        for (a, b) in self.s.bytes().zip(prefix.bytes()) {
            let a_upper = if a >= b'a' && a <= b'z' { a - 32 } else { a };
            if a_upper != b {
                return false;
            }
        }
        true
    }

    fn contains(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        for i in 0..=self.s.len().saturating_sub(needle.len()) {
            let slice = &self.s[i..];
            if (SimpleUpper { s: slice }).starts_with(needle) {
                return true;
            }
        }
        false
    }

    fn find(&self, needle: &str) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        for i in 0..=self.s.len().saturating_sub(needle.len()) {
            let slice = &self.s[i..];
            if (SimpleUpper { s: slice }).starts_with(needle) {
                return Some(i);
            }
        }
        None
    }
}
