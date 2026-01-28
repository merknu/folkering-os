//! Filesystem syscall wrappers
//!
//! Provides access to the kernel's filesystem (ramdisk) from userspace.

use crate::syscall::{syscall2, syscall3};

const SYS_FS_READ_DIR: u64 = 13;
const SYS_FS_READ_FILE: u64 = 14;

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
