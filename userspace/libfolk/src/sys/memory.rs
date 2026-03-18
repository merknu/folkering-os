//! Shared memory syscalls
//!
//! Functions for creating and mapping shared memory regions.

use crate::syscall::{syscall1, syscall2, syscall3, SYS_SHMEM_CREATE, SYS_SHMEM_MAP, SYS_SHMEM_GRANT, SYS_SHMEM_UNMAP, SYS_SHMEM_DESTROY, SYS_MMAP};

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

/// Grant another task access to a shared memory region
///
/// This enables zero-copy data transfer between tasks. The granting task
/// must be in the shared memory's access list (typically the creator).
///
/// # Arguments
/// * `shmem_id` - The shared memory region ID
/// * `target_task` - The task ID to grant access to
///
/// # Returns
/// * `Ok(())` - Access granted successfully
/// * `Err(error)` - Error code on failure
pub fn shmem_grant(shmem_id: u32, target_task: u32) -> Result<(), ShmemError> {
    let ret = unsafe { syscall2(SYS_SHMEM_GRANT, shmem_id as u64, target_task as u64) };
    if ret == u64::MAX {
        Err(ShmemError::Unknown)
    } else {
        Ok(())
    }
}

/// Unmap a shared memory region from the current task's address space
///
/// This removes the virtual address mapping but does NOT free the physical
/// pages. Other tasks may still have the region mapped. Use `shmem_destroy()`
/// to actually free the memory.
///
/// # Arguments
/// * `shmem_id` - The shared memory region ID
/// * `virt_addr` - Virtual address where the region is mapped
///
/// # Returns
/// * `Ok(())` - Unmapped successfully
/// * `Err(error)` - Error code on failure
pub fn shmem_unmap(shmem_id: u32, virt_addr: usize) -> Result<(), ShmemError> {
    let ret = unsafe { syscall2(SYS_SHMEM_UNMAP, shmem_id as u64, virt_addr as u64) };
    if ret == u64::MAX {
        Err(ShmemError::Unknown)
    } else {
        Ok(())
    }
}

/// Destroy a shared memory region and free its physical pages
///
/// Only the creator (owner) of the shared memory region can destroy it.
/// All tasks should unmap the region before calling this, otherwise
/// they will get page faults when accessing the unmapped addresses.
///
/// # Arguments
/// * `shmem_id` - The shared memory region ID
///
/// # Returns
/// * `Ok(())` - Destroyed successfully
/// * `Err(error)` - Error code on failure (e.g., not the owner)
pub fn shmem_destroy(shmem_id: u32) -> Result<(), ShmemError> {
    let ret = unsafe { syscall1(SYS_SHMEM_DESTROY, shmem_id as u64) };
    if ret == u64::MAX {
        Err(ShmemError::Unknown)
    } else {
        Ok(())
    }
}

// ===== Anonymous Memory Mapping (mmap) =====

/// Protection flags for mmap
pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
pub const PROT_EXEC: u64 = 0x4;

/// Error type for mmap operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmapError {
    /// Invalid size or flags
    InvalidParam,
    /// Out of physical memory
    OutOfMemory,
    /// Address space exhausted
    NoSpace,
}

/// Map anonymous (zero-filled) pages into the current task's address space.
///
/// # Arguments
/// * `size` - Number of bytes to allocate (rounded up to 4KB page boundary)
/// * `flags` - Protection flags (PROT_READ | PROT_WRITE | PROT_EXEC)
///
/// # Returns
/// * `Ok(ptr)` - Pointer to the start of the mapped region
/// * `Err(...)` - Allocation failed
///
/// # Example
/// ```no_run
/// let ptr = mmap(8192, PROT_READ | PROT_WRITE)?;
/// unsafe { *ptr = 42; }
/// ```
pub fn mmap(size: usize, flags: u64) -> Result<*mut u8, MmapError> {
    if size == 0 {
        return Err(MmapError::InvalidParam);
    }
    let ret = unsafe { syscall3(SYS_MMAP, 0, size as u64, flags) };
    if ret == u64::MAX {
        Err(MmapError::OutOfMemory)
    } else {
        Ok(ret as *mut u8)
    }
}

/// Map anonymous pages at a specific virtual address.
pub fn mmap_at(addr: usize, size: usize, flags: u64) -> Result<*mut u8, MmapError> {
    if size == 0 || addr == 0 {
        return Err(MmapError::InvalidParam);
    }
    let ret = unsafe { syscall3(SYS_MMAP, addr as u64, size as u64, flags) };
    if ret == u64::MAX {
        Err(MmapError::OutOfMemory)
    } else {
        Ok(ret as *mut u8)
    }
}
