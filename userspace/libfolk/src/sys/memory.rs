//! Shared memory syscalls
//!
//! Functions for creating and mapping shared memory regions.

use crate::syscall::{syscall1, syscall2, SYS_SHMEM_CREATE, SYS_SHMEM_MAP};

/// Error codes for shared memory operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmemError {
    /// Invalid size or parameters
    InvalidParam,
    /// Out of memory
    OutOfMemory,
    /// Region not found
    NotFound,
    /// Unknown error
    Unknown,
}

/// Create a new shared memory region
///
/// # Arguments
/// * `size` - Size of the region in bytes (will be page-aligned)
///
/// # Returns
/// * `Ok(shmem_id)` - The shared memory region ID
/// * `Err(error)` - Error code on failure
pub fn shmem_create(size: usize) -> Result<u32, ShmemError> {
    let ret = unsafe { syscall1(SYS_SHMEM_CREATE, size as u64) };
    if ret == u64::MAX {
        Err(ShmemError::Unknown)
    } else {
        Ok(ret as u32)
    }
}

/// Map a shared memory region into the current task's address space
///
/// # Arguments
/// * `shmem_id` - The shared memory region ID
/// * `virt_addr` - Virtual address to map at
///
/// # Returns
/// * `Ok(())` - Mapping successful
/// * `Err(error)` - Error code on failure
pub fn shmem_map(shmem_id: u32, virt_addr: usize) -> Result<(), ShmemError> {
    let ret = unsafe { syscall2(SYS_SHMEM_MAP, shmem_id as u64, virt_addr as u64) };
    if ret == u64::MAX {
        Err(ShmemError::Unknown)
    } else {
        Ok(())
    }
}
