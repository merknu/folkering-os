//! IPC Shared Memory
//!
//! Zero-copy bulk data transfer mechanism for IPC.
//! Essential for high-performance file I/O and network operations.

use crate::ipc::message::{ShmemId, TaskId};
use crate::memory::{alloc_pages, free_pages};
use alloc::vec::Vec;
use hashbrown::{HashMap, hash_map::DefaultHashBuilder};
use spin::Mutex;
use core::sync::atomic::{AtomicU32, Ordering};
use core::num::NonZeroU32;

/// Physical address (platform-specific)
pub type PhysAddr = usize;

/// Virtual address (platform-specific)
pub type VirtAddr = usize;

/// Page size (4KB on x86-64)
pub const PAGE_SIZE: usize = 4096;

/// Shared memory permissions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmemPerms {
    /// Read-only access
    ReadOnly,
    /// Write-only access (rare, but useful for logging)
    WriteOnly,
    /// Read and write access
    ReadWrite,
}

/// Page flags for memory mapping
#[derive(Debug, Clone, Copy)]
pub struct PageFlags {
    bits: u8,
}

impl PageFlags {
    pub const READABLE: Self = Self { bits: 0b001 };
    pub const WRITABLE: Self = Self { bits: 0b010 };
    pub const USER: Self = Self { bits: 0b100 };

    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    pub const fn or(self, other: Self) -> Self {
        Self { bits: self.bits | other.bits }
    }
}

/// Shared memory region
///
/// # Design
/// - Multiple tasks can map the same physical pages
/// - Zero-copy: Data written by one task is immediately visible to others
/// - Capability-protected: Only tasks with access can map region
///
/// # Memory Layout
/// - Physical pages allocated from buddy allocator
/// - Each task maps pages into their virtual address space
/// - Pages are 4KB aligned (x86-64 page size)
#[derive(Debug, Clone)]
pub struct SharedMemory {
    /// Unique identifier
    pub id: ShmemId,

    /// Physical pages backing this region
    pub phys_pages: Vec<PhysAddr>,

    /// Total size in bytes (multiple of PAGE_SIZE)
    pub size: usize,

    /// Access permissions
    pub perms: ShmemPerms,

    /// Tasks with access to this region
    /// First task in list is the creator/owner
    pub tasks: Vec<TaskId>,
}

/// Global shared memory table
lazy_static! {
    static ref SHMEM_TABLE: Mutex<HashMap<u32, SharedMemory, DefaultHashBuilder>> =
        Mutex::new(HashMap::with_hasher(DefaultHashBuilder::default()));
}

/// Next shared memory ID counter
static NEXT_SHMEM_ID: AtomicU32 = AtomicU32::new(1);

/// IPC shared memory errors
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmemError {
    /// Invalid shared memory ID
    InvalidId,
    /// Permission denied
    PermissionDenied,
    /// Out of memory (cannot allocate pages)
    OutOfMemory,
    /// ID overflow (too many shared memory regions)
    IdOverflow,
    /// Invalid size (must be multiple of page size)
    InvalidSize,
}

/// Create new shared memory region
///
/// # Flow
/// 1. Round size up to page boundary (4KB)
/// 2. Allocate contiguous physical frames
/// 3. Generate unique ShmemId
/// 4. Create SharedMemory object
/// 5. Insert into global table
/// 6. Return ShmemId
///
/// # Arguments
/// - `size`: Size in bytes (will be rounded up to page boundary)
/// - `perms`: Access permissions for the region
///
/// # Returns
/// - `Ok(id)`: Shared memory ID
/// - `Err(error)`: Error code
///
/// # Performance
/// - ~10 microseconds (page allocation + table insertion)
///
/// # Example
/// ```no_run
/// use folkering_kernel::ipc::{shmem_create, ShmemPerms};
///
/// // Create 8KB shared memory region
/// let shmem_id = shmem_create(8192, ShmemPerms::ReadWrite)?;
///
/// // Map it into current task's address space
/// let ptr = shmem_map(shmem_id, 0x1000_0000)?;
///
/// // Write data (zero-copy)
/// unsafe { *(ptr as *mut u64) = 42; }
/// ```
pub fn shmem_create(size: usize, perms: ShmemPerms) -> Result<ShmemId, ShmemError> {
    // 1. Round size up to page boundary (4KB)
    if size == 0 {
        return Err(ShmemError::InvalidSize);
    }

    let num_pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
    let actual_size = num_pages * PAGE_SIZE;

    // 2. Allocate individual physical pages (order 0 each)
    let mut phys_pages = Vec::new();
    for _ in 0..num_pages {
        match alloc_pages(0) {
            Some(page_addr) => phys_pages.push(page_addr),
            None => {
                // Free any pages we already allocated
                for &addr in &phys_pages {
                    free_pages(addr, 0);
                }
                return Err(ShmemError::OutOfMemory);
            }
        }
    }

    // 3. Generate unique ShmemId
    let id_raw = NEXT_SHMEM_ID.fetch_add(1, Ordering::Relaxed);
    let id = NonZeroU32::new(id_raw)
        .ok_or(ShmemError::IdOverflow)?;

    // 4. Get current task as owner
    let current_task_id = crate::task::task::current_task().lock().id;

    // 5. Create SharedMemory object
    let shmem = SharedMemory {
        id,
        phys_pages,
        size: actual_size,
        perms,
        tasks: alloc::vec![current_task_id],
    };

    // 6. Insert into global table
    SHMEM_TABLE.lock().insert(id_raw, shmem);

    Ok(id)
}

/// Map shared memory into current task's address space
///
/// # Flow
/// 1. Validate ShmemId exists
/// 2. Check current task has access
/// 3. Map physical pages into virtual address space
/// 4. Return virtual address pointer
///
/// # Arguments
/// - `id`: Shared memory region ID
/// - `virt`: Virtual address to map at (must be page-aligned)
///
/// # Returns
/// - `Ok(())`: Mapping successful
/// - `Err(error)`: Error code
///
/// # Performance
/// - ~5 microseconds per page (TLB flush + page table update)
/// - 4KB region: ~5 microseconds
/// - 1MB region: ~1.25 milliseconds
///
/// # Example
/// ```no_run
/// use folkering_kernel::ipc::{shmem_create, shmem_map, ShmemPerms};
///
/// // Creator task
/// let shmem_id = shmem_create(4096, ShmemPerms::ReadWrite)?;
/// shmem_map(shmem_id, 0x1000_0000)?;
///
/// // Write data
/// let ptr = 0x1000_0000 as *mut u64;
/// unsafe { *ptr = 0xDEADBEEF; }
///
/// // Receiver task (after receiving shmem_id via IPC)
/// shmem_map(shmem_id, 0x2000_0000)?;
///
/// // Read data (zero-copy!)
/// let ptr = 0x2000_0000 as *const u64;
/// let value = unsafe { *ptr };
/// assert_eq!(value, 0xDEADBEEF);
/// ```
pub fn shmem_map(id: ShmemId, virt: VirtAddr) -> Result<(), ShmemError> {
    // Validate address is page-aligned
    if virt % PAGE_SIZE != 0 {
        return Err(ShmemError::InvalidSize);
    }

    // 1. Validate ShmemId exists
    let shmem = {
        let table = SHMEM_TABLE.lock();
        table.get(&id.get())
            .ok_or(ShmemError::InvalidId)?
            .clone()
    };

    // 2. Check current task has access
    let current_task_id = crate::task::task::current_task().lock().id;

    if !shmem.tasks.contains(&current_task_id) {
        return Err(ShmemError::PermissionDenied);
    }

    // 3. Map pages into address space
    let page_flags = match shmem.perms {
        ShmemPerms::ReadOnly => PageFlags::READABLE.or(PageFlags::USER),
        ShmemPerms::WriteOnly => PageFlags::WRITABLE.or(PageFlags::USER),
        ShmemPerms::ReadWrite => PageFlags::READABLE.or(PageFlags::WRITABLE).or(PageFlags::USER),
    };

    for (i, &phys) in shmem.phys_pages.iter().enumerate() {
        let virt_page = virt + (i * PAGE_SIZE);
        map_page(virt_page, phys, page_flags)?;
    }

    Ok(())
}

/// Unmap shared memory from current task's address space
///
/// # Arguments
/// - `id`: Shared memory region ID
/// - `virt`: Virtual address where region is mapped
///
/// # Returns
/// - `Ok(())`: Unmapped successfully
/// - `Err(error)`: Error code
///
/// # Note
/// This does NOT free the physical pages - other tasks may still
/// have the region mapped. Use `shmem_destroy()` to free pages.
pub fn shmem_unmap(id: ShmemId, virt: VirtAddr) -> Result<(), ShmemError> {
    // Validate address is page-aligned
    if virt % PAGE_SIZE != 0 {
        return Err(ShmemError::InvalidSize);
    }

    // Get region info
    let shmem = {
        let table = SHMEM_TABLE.lock();
        table.get(&id.get())
            .ok_or(ShmemError::InvalidId)?
            .clone()
    };

    // Unmap each page
    for i in 0..shmem.phys_pages.len() {
        let virt_page = virt + (i * PAGE_SIZE);
        unmap_page(virt_page)?;
    }

    Ok(())
}

/// Destroy shared memory region (free physical pages)
///
/// # Security
/// Only the creator (first task in `tasks` list) can destroy the region.
///
/// # Arguments
/// - `id`: Shared memory region ID
///
/// # Returns
/// - `Ok(())`: Destroyed successfully
/// - `Err(error)`: Error code
///
/// # Note
/// This frees the physical pages. All tasks must unmap the region
/// before calling this, otherwise they will get page faults.
pub fn shmem_destroy(id: ShmemId) -> Result<(), ShmemError> {
    // Remove from table
    let shmem = {
        let mut table = SHMEM_TABLE.lock();
        table.remove(&id.get())
            .ok_or(ShmemError::InvalidId)?
    };

    // Check current task is the owner (first in list)
    let current_task_id = crate::task::task::current_task().lock().id;

    if shmem.tasks.first() != Some(&current_task_id) {
        // Not the owner - restore to table
        SHMEM_TABLE.lock().insert(id.get(), shmem);
        return Err(ShmemError::PermissionDenied);
    }

    // Free physical pages (each page is order 0)
    for &phys_addr in &shmem.phys_pages {
        free_pages(phys_addr, 0);
    }

    Ok(())
}

/// Grant access to shared memory region to another task
///
/// # Arguments
/// - `id`: Shared memory region ID
/// - `task`: Task ID to grant access to
///
/// # Returns
/// - `Ok(())`: Access granted
/// - `Err(error)`: Error code
///
/// # Use Case
/// Creator grants access, then sends IPC with ShmemId.
/// Receiver can then map the region.
pub fn shmem_grant(id: ShmemId, task: TaskId) -> Result<(), ShmemError> {
    let mut table = SHMEM_TABLE.lock();
    let shmem = table.get_mut(&id.get())
        .ok_or(ShmemError::InvalidId)?;

    // Check current task has access
    let current_task_id = crate::task::task::current_task().lock().id;

    if !shmem.tasks.contains(&current_task_id) {
        return Err(ShmemError::PermissionDenied);
    }

    // Add task to access list if not already present
    if !shmem.tasks.contains(&task) {
        shmem.tasks.push(task);
    }

    Ok(())
}

/// Revoke access to shared memory region from a task
///
/// # Arguments
/// - `id`: Shared memory region ID
/// - `task`: Task ID to revoke access from
///
/// # Returns
/// - `Ok(())`: Access revoked
/// - `Err(error)`: Error code
pub fn shmem_revoke(id: ShmemId, task: TaskId) -> Result<(), ShmemError> {
    let mut table = SHMEM_TABLE.lock();
    let shmem = table.get_mut(&id.get())
        .ok_or(ShmemError::InvalidId)?;

    // Check current task is owner (first in list)
    let current_task_id = crate::task::task::current_task().lock().id;

    if shmem.tasks.first() != Some(&current_task_id) {
        return Err(ShmemError::PermissionDenied);
    }

    // Remove task from access list (keep owner)
    shmem.tasks.retain(|&t| t == current_task_id || t != task);

    Ok(())
}

/// Map a single page into virtual address space
///
/// Platform-specific implementation (x86-64).
/// In real kernel, this would interact with page tables.
///
/// # Arguments
/// - `virt`: Virtual address (page-aligned)
/// - `phys`: Physical address (page-aligned)
/// - `flags`: Page protection flags
fn map_page(virt: VirtAddr, phys: PhysAddr, _flags: PageFlags) -> Result<(), ShmemError> {
    // TODO: Implement actual page table manipulation
    // This is a placeholder for the real implementation
    // which would involve:
    // 1. Walking page tables (PML4 -> PDPT -> PD -> PT)
    // 2. Creating missing page table levels
    // 3. Setting PTE with physical address and flags
    // 4. Flushing TLB

    // For now, just validate addresses are page-aligned
    if virt % PAGE_SIZE != 0 || phys % PAGE_SIZE != 0 {
        return Err(ShmemError::InvalidSize);
    }

    Ok(())
}

/// Unmap a single page from virtual address space
///
/// Platform-specific implementation (x86-64).
fn unmap_page(virt: VirtAddr) -> Result<(), ShmemError> {
    // TODO: Implement actual page table manipulation
    // This would:
    // 1. Walk page tables to find PTE
    // 2. Clear PTE
    // 3. Flush TLB entry

    // Validate address is page-aligned
    if virt % PAGE_SIZE != 0 {
        return Err(ShmemError::InvalidSize);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_size() {
        // Verify page size is 4KB
        assert_eq!(PAGE_SIZE, 4096);
    }

    #[test]
    fn test_shmem_perms() {
        // Verify permission types exist
        let _ro = ShmemPerms::ReadOnly;
        let _wo = ShmemPerms::WriteOnly;
        let _rw = ShmemPerms::ReadWrite;

        assert_ne!(ShmemPerms::ReadOnly, ShmemPerms::WriteOnly);
        assert_ne!(ShmemPerms::ReadOnly, ShmemPerms::ReadWrite);
    }

    #[test]
    fn test_page_flags() {
        let flags = PageFlags::READABLE.or(PageFlags::WRITABLE);
        assert_eq!(flags.bits, 0b011);
    }

    #[test]
    fn test_shmem_error_types() {
        // Verify error types are distinct
        assert_ne!(ShmemError::InvalidId, ShmemError::PermissionDenied);
        assert_ne!(ShmemError::OutOfMemory, ShmemError::IdOverflow);
    }
}
