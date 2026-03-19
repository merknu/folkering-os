//! Block device I/O syscall wrappers
//!
//! Provides sector-level read/write access to the VirtIO block device.
//! Requires Hardware(BLOCK_DEVICE) capability (currently granted to all tasks).

use crate::syscall::{syscall3, SYS_BLOCK_READ, SYS_BLOCK_WRITE};

/// Sector size in bytes
pub const SECTOR_SIZE: usize = 512;

/// Data area start sector (after header + journal)
pub const DATA_START_SECTOR: u64 = 2048;

/// Block device error
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// No block device available
    NoDevice,
    /// I/O error
    IoError,
    /// Invalid arguments
    InvalidArgs,
}

/// Read sectors from the block device
///
/// # Arguments
/// * `sector` - Starting sector number
/// * `buf` - Buffer to read into (must be sector_count * 512 bytes)
/// * `sector_count` - Number of sectors to read
pub fn block_read(sector: u64, buf: &mut [u8], sector_count: usize) -> Result<(), BlockError> {
    if buf.len() < sector_count * SECTOR_SIZE || sector_count == 0 {
        return Err(BlockError::InvalidArgs);
    }

    let result = unsafe {
        syscall3(SYS_BLOCK_READ, sector, buf.as_mut_ptr() as u64, sector_count as u64)
    };

    if result == 0 {
        Ok(())
    } else {
        Err(BlockError::IoError)
    }
}

/// Write sectors to the block device
///
/// # Arguments
/// * `sector` - Starting sector number
/// * `buf` - Buffer to write from (must be sector_count * 512 bytes)
/// * `sector_count` - Number of sectors to write
pub fn block_write(sector: u64, buf: &[u8], sector_count: usize) -> Result<(), BlockError> {
    if buf.len() < sector_count * SECTOR_SIZE || sector_count == 0 {
        return Err(BlockError::InvalidArgs);
    }

    let result = unsafe {
        syscall3(SYS_BLOCK_WRITE, sector, buf.as_ptr() as u64, sector_count as u64)
    };

    if result == 0 {
        Ok(())
    } else {
        Err(BlockError::IoError)
    }
}

/// Read a single sector
pub fn read_sector(sector: u64, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), BlockError> {
    block_read(sector, buf, 1)
}

/// Write a single sector
pub fn write_sector(sector: u64, buf: &[u8; SECTOR_SIZE]) -> Result<(), BlockError> {
    block_write(sector, buf, 1)
}
