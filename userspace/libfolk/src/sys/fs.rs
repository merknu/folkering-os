//! Filesystem syscall wrappers
//!
//! Provides access to the kernel's filesystem (ramdisk) from userspace.

use crate::syscall::{syscall2, syscall3, syscall4};

const SYS_FS_READ_DIR: u64 = 13;
const SYS_FS_READ_FILE: u64 = 14;

// Mutable VFS (tmpfs) — see `kernel/src/fs/mvfs.rs`.
const SYS_MVFS_WRITE: u64 = 0x27;
const SYS_MVFS_READ: u64 = 0x28;
const SYS_MVFS_DELETE: u64 = 0x29;
const SYS_MVFS_LIST: u64 = 0x2A;

/// Max file name length enforced by the MVFS kernel module.
pub const MVFS_MAX_NAME: usize = 32;
/// Max single-file size enforced by the MVFS kernel module.
pub const MVFS_MAX_FILE_SIZE: usize = 4096;

/// Directory entry returned by the kernel (matches kernel's DirEntry layout).
/// Note: NOT packed to avoid alignment issues. 48 bytes total.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirEntry {
    pub id: u16,
    pub entry_type: u16,
    pub name: [u8; 32],
    // 4 bytes implicit padding here for u64 alignment
    pub size: u64,
}

impl DirEntry {
    /// Get the entry name as a string slice (up to first null byte).
    pub fn name_str(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(32);
        // Safety: name bytes are ASCII, written by folk-pack tool.
        unsafe { core::str::from_utf8_unchecked(&self.name[..len]) }
    }

    /// Check if this entry is an ELF executable.
    pub fn is_elf(&self) -> bool {
        self.entry_type == 0
    }
}

/// Read directory entries from the ramdisk.
///
/// Fills the provided buffer with DirEntry structs and returns the
/// number of entries written. Returns 0 if no ramdisk or on error.
pub fn read_dir(buf: &mut [DirEntry]) -> usize {
    let ptr = buf.as_mut_ptr() as u64;
    let size = (buf.len() * core::mem::size_of::<DirEntry>()) as u64;
    let ret = unsafe { syscall2(SYS_FS_READ_DIR, ptr, size) };
    if ret == u64::MAX { 0 } else { ret as usize }
}

/// Read a file's contents into the provided buffer.
/// Returns the number of bytes read, or 0 on error/not found.
pub fn read_file(name: &str, buf: &mut [u8]) -> usize {
    let mut name_buf = [0u8; 32];
    let len = name.len().min(31);
    name_buf[..len].copy_from_slice(&name.as_bytes()[..len]);
    // name_buf[len] is already 0 (null terminator)

    let ret = unsafe {
        syscall3(
            SYS_FS_READ_FILE,
            name_buf.as_ptr() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    };
    if ret == u64::MAX { 0 } else { ret as usize }
}

// ── Mutable VFS (tmpfs) ────────────────────────────────────────────────
//
// Lives parallel to the read-only ramdisk: separate namespace, writable
// from userspace, but entries die on reboot. Use this for scratch data
// that doesn't justify the Synapse IPC round-trip. Phase 2 will back
// MVFS with disk sectors for reboot-persistence.

/// Write (create or overwrite) a file in the MVFS. `name` must be
/// non-empty and ≤ 32 bytes; `data` must be ≤ 4 KiB.
///
/// Returns `true` on success, `false` if the kernel rejected the
/// request (bad name, too large, or the MVFS table is already full
/// with 16 distinct entries).
pub fn mvfs_write(name: &str, data: &[u8]) -> bool {
    if name.is_empty() || name.len() > MVFS_MAX_NAME { return false; }
    if data.len() > MVFS_MAX_FILE_SIZE { return false; }
    let ret = unsafe {
        syscall4(
            SYS_MVFS_WRITE,
            name.as_ptr() as u64,
            name.len() as u64,
            data.as_ptr() as u64,
            data.len() as u64,
        )
    };
    ret != u64::MAX
}

/// Read an MVFS file into `buf`. Returns `Some(bytes_copied)` on
/// success or `None` if the name doesn't exist.
pub fn mvfs_read(name: &str, buf: &mut [u8]) -> Option<usize> {
    if name.is_empty() || name.len() > MVFS_MAX_NAME || buf.is_empty() {
        return None;
    }
    let ret = unsafe {
        syscall4(
            SYS_MVFS_READ,
            name.as_ptr() as u64,
            name.len() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    };
    if ret == u64::MAX { None } else { Some(ret as usize) }
}

/// Delete an MVFS file. Returns `true` if a file was removed, `false`
/// if the name wasn't found.
pub fn mvfs_delete(name: &str) -> bool {
    if name.is_empty() || name.len() > MVFS_MAX_NAME { return false; }
    let ret = unsafe {
        syscall2(SYS_MVFS_DELETE, name.as_ptr() as u64, name.len() as u64)
    };
    ret != u64::MAX
}

/// Write the MVFS directory listing into `buf` as a flat byte stream
/// of `[name_len: u8][name bytes]` pairs. Returns the number of bytes
/// written. Caller must iterate the stream to pull out names.
///
/// If `prefix` is non-empty, only entries whose name starts with the
/// prefix are included — use this to simulate subdirectories.
pub fn mvfs_list(prefix: &str, buf: &mut [u8]) -> usize {
    if buf.is_empty() { return 0; }
    if prefix.len() > MVFS_MAX_NAME { return 0; }
    let ret = unsafe {
        syscall4(
            SYS_MVFS_LIST,
            prefix.as_ptr() as u64,
            prefix.len() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    };
    if ret == u64::MAX { 0 } else { ret as usize }
}
