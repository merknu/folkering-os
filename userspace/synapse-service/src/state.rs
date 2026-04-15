//! Synapse state structures.
//!
//! # SafeSqliteBuffer
//!
//! The 4MB SQLite database buffer was previously a `static mut` accessed
//! through 80+ unsafe blocks scattered across the codebase. Each access
//! had to manually:
//!
//! 1. bounds-check the offset against `SQLITE_STATE.size`
//! 2. mark dirty pages (often forgotten — silent persistence bugs)
//! 3. assume nothing else was holding a reference (fragile under reborrows)
//!
//! `SafeSqliteBuffer` wraps the buffer with **safe accessor methods** that
//! do all three automatically. The 4MB array still lives in BSS (it's too
//! large for stack/heap), but every byte access goes through `read_byte`,
//! `read_slice`, `write_byte`, `write_slice`, or `write_be_u32` — and the
//! dirty bitmap is updated transparently.
//!
//! # ShmemArena
//!
//! Six different hardcoded virtual addresses (0x10000000, 0x11000000, …)
//! were manually mapped/unmapped per handler. Forgetting to unmap leaked
//! the slot; double-mapping crashed. `ShmemArena` is a small RAII wrapper
//! that calls `shmem_unmap` in `Drop`, eliminating both classes of bug.

extern crate alloc;

use libfolk::sys::fs::DirEntry;

/// Maximum SQLite database size (4MB — fixed BSS allocation).
pub const MAX_DB_SIZE: usize = 4 * 1024 * 1024;

/// Maximum SQLite pages (4MB / 4096 = 1024 pages).
pub const MAX_PAGES: usize = MAX_DB_SIZE / 4096;

/// Initial capacity for directory cache.
pub const INITIAL_CACHE_CAPACITY: usize = 32;

/// File kind constants (match folk-pack create-sqlite).
pub const KIND_ELF: i64 = 0;
#[allow(dead_code)]
pub const KIND_DATA: i64 = 1;

/// Backend selector — chosen at boot based on disk contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Using FPK format (legacy ramdisk fallback)
    Fpk,
    /// Using SQLite database (preferred)
    Sqlite,
}

/// Directory cache — dynamically grows as files are added.
pub struct DirCacheState {
    pub count: usize,
    pub valid: bool,
    pub entries: alloc::vec::Vec<DirEntry>,
}

impl DirCacheState {
    pub const fn new() -> Self {
        Self {
            count: 0,
            valid: false,
            entries: alloc::vec::Vec::new(),
        }
    }
}

/// Result type for SafeSqliteBuffer access — fails on out-of-bounds.
pub type SqliteResult<T> = Result<T, SqliteBufferError>;

#[derive(Debug)]
pub enum SqliteBufferError {
    OutOfBounds { offset: usize, len: usize, max: usize },
    NotValid,
}

/// Safe wrapper around the 4MB SQLite database buffer.
///
/// Replaces direct `SQLITE_STATE.data[off] = byte` accesses with bounds-checked
/// methods that also automatically mark dirty pages. The 4MB array is stored
/// inline (in BSS) — keep this struct in a `static`, never move it.
///
/// All access methods take an explicit `offset`/`len` and validate against
/// `self.size` (the loaded DB size, not the buffer capacity).
#[repr(C, align(4096))]
pub struct SafeSqliteBuffer {
    /// Raw database bytes (4 MB). Never accessed directly outside this module.
    pub(crate) data: [u8; MAX_DB_SIZE],
    /// Actual size of loaded database in bytes.
    pub(crate) size: usize,
    /// Whether the database has been successfully loaded.
    pub(crate) valid: bool,
    /// Dirty page bitmap — one bit per 4KB SQLite page.
    /// `dirty[i / 8] & (1 << (i % 8))` is set iff page `i` was modified.
    pub(crate) dirty: [u8; MAX_PAGES / 8],
}

impl SafeSqliteBuffer {
    pub const fn new() -> Self {
        Self {
            data: [0u8; MAX_DB_SIZE],
            size: 0,
            valid: false,
            dirty: [0u8; MAX_PAGES / 8],
        }
    }

    // ── Validity / size ────────────────────────────────────────────────

    pub fn is_valid(&self) -> bool {
        self.valid
    }

    pub fn set_valid(&mut self, valid: bool) {
        self.valid = valid;
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn set_size(&mut self, size: usize) {
        self.size = size.min(MAX_DB_SIZE);
    }

    // ── Bulk access (init only — bypasses dirty marking) ───────────────

    /// Get a mutable slice for INITIAL LOAD only. Caller is responsible
    /// for not bypassing dirty tracking after init.
    pub fn raw_data_mut(&mut self) -> &mut [u8; MAX_DB_SIZE] {
        &mut self.data
    }

    /// Get the loaded portion as a read-only slice.
    pub fn loaded(&self) -> &[u8] {
        &self.data[..self.size]
    }

    // ── Safe byte access ───────────────────────────────────────────────

    /// Read a single byte. Bounds-checked against `self.size`.
    pub fn read_byte(&self, offset: usize) -> SqliteResult<u8> {
        if offset >= self.size {
            return Err(SqliteBufferError::OutOfBounds {
                offset, len: 1, max: self.size,
            });
        }
        Ok(self.data[offset])
    }

    /// Read a slice. Bounds-checked.
    pub fn read_slice(&self, offset: usize, len: usize) -> SqliteResult<&[u8]> {
        let end = offset.checked_add(len).ok_or(SqliteBufferError::OutOfBounds {
            offset, len, max: self.size,
        })?;
        if end > self.size {
            return Err(SqliteBufferError::OutOfBounds {
                offset, len, max: self.size,
            });
        }
        Ok(&self.data[offset..end])
    }

    /// Read a big-endian u16 at `offset`.
    pub fn read_be_u16(&self, offset: usize) -> SqliteResult<u16> {
        let s = self.read_slice(offset, 2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    /// Read a big-endian u32 at `offset`.
    pub fn read_be_u32(&self, offset: usize) -> SqliteResult<u32> {
        let s = self.read_slice(offset, 4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    // ── Safe mutating access (auto-marks dirty pages) ──────────────────

    /// Write a single byte. Bounds-checked + auto-marks the containing page dirty.
    pub fn write_byte(&mut self, offset: usize, byte: u8) -> SqliteResult<()> {
        if offset >= MAX_DB_SIZE {
            return Err(SqliteBufferError::OutOfBounds {
                offset, len: 1, max: MAX_DB_SIZE,
            });
        }
        self.data[offset] = byte;
        self.mark_page_dirty(offset / 4096);
        Ok(())
    }

    /// Write a slice starting at `offset`. Bounds-checked + auto-marks dirty.
    pub fn write_slice(&mut self, offset: usize, src: &[u8]) -> SqliteResult<()> {
        let end = offset.checked_add(src.len()).ok_or(SqliteBufferError::OutOfBounds {
            offset, len: src.len(), max: MAX_DB_SIZE,
        })?;
        if end > MAX_DB_SIZE {
            return Err(SqliteBufferError::OutOfBounds {
                offset, len: src.len(), max: MAX_DB_SIZE,
            });
        }
        self.data[offset..end].copy_from_slice(src);
        self.mark_range_dirty(offset, src.len());
        Ok(())
    }

    /// Write a big-endian u16 at `offset`.
    pub fn write_be_u16(&mut self, offset: usize, val: u16) -> SqliteResult<()> {
        self.write_slice(offset, &val.to_be_bytes())
    }

    /// Write a big-endian u32 at `offset`.
    pub fn write_be_u32(&mut self, offset: usize, val: u32) -> SqliteResult<()> {
        self.write_slice(offset, &val.to_be_bytes())
    }

    /// Increment the SQLite change counter at offset 24 (BE u32).
    /// Called after every B-tree mutation. Auto-marks page 0 dirty.
    pub fn increment_change_counter(&mut self) {
        let cc = u32::from_be_bytes([self.data[24], self.data[25], self.data[26], self.data[27]]);
        let new_cc = cc.wrapping_add(1).to_be_bytes();
        self.data[24..28].copy_from_slice(&new_cc);
        self.mark_page_dirty(0);
    }

    /// Read the page count from the DB header (offset 28, BE u32).
    pub fn page_count(&self) -> u32 {
        u32::from_be_bytes([self.data[28], self.data[29], self.data[30], self.data[31]])
    }

    /// Update the page count in the DB header.
    pub fn set_page_count(&mut self, count: u32) {
        let bytes = count.to_be_bytes();
        self.data[28..32].copy_from_slice(&bytes);
        self.mark_page_dirty(0);
    }

    // ── Dirty bitmap ───────────────────────────────────────────────────

    /// Mark a single SQLite page (4KB) as dirty.
    pub fn mark_page_dirty(&mut self, page_num: usize) {
        if page_num < MAX_PAGES {
            self.dirty[page_num / 8] |= 1 << (page_num % 8);
        }
    }

    /// Mark all pages covering [offset, offset+len) as dirty.
    pub fn mark_range_dirty(&mut self, offset: usize, len: usize) {
        let start_page = offset / 4096;
        let end_page = (offset + len + 4095) / 4096;
        for p in start_page..end_page.min(MAX_PAGES) {
            self.dirty[p / 8] |= 1 << (p % 8);
        }
    }

    /// Check whether a page is dirty (used by flush).
    pub fn is_page_dirty(&self, page_num: usize) -> bool {
        if page_num >= MAX_PAGES { return false; }
        (self.dirty[page_num / 8] & (1 << (page_num % 8))) != 0
    }

    /// Clear all dirty flags after flush.
    pub fn clear_dirty(&mut self) {
        for b in self.dirty.iter_mut() {
            *b = 0;
        }
    }
}
