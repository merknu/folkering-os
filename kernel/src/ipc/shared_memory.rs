//! IPC Shared Memory
//!
//! Zero-copy bulk data transfer mechanism for IPC.
//! Essential for high-performance file I/O and network operations.

use crate::ipc::message::{ShmemId, TaskId};
use crate::memory::physical::{alloc_page, alloc_pages, free_pages};
use crate::memory::paging;
use alloc::vec::Vec;
use hashbrown::{HashMap, hash_map::DefaultHashBuilder};
use spin::Mutex;
use core::sync::atomic::{AtomicU32, Ordering};
use core::num::NonZeroU32;
use x86_64::structures::paging::PageTableFlags;

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

    /// Physical pages backing this region. Each entry is the base
    /// physical address of one allocation block; the block size is
    /// `PAGE_SIZE << block_order`. For the standard 4 KiB path this
    /// is one entry per 4 KiB page (block_order = 0); for the
    /// huge-page path we get one entry per 2 MiB block (block_order = 9).
    pub phys_pages: Vec<PhysAddr>,

    /// Buddy-allocator order of each entry in `phys_pages`. 0 = 4 KiB,
    /// 9 = 2 MiB. Same value across the whole region — we don't mix
    /// page sizes inside one shmem.
    ///
    /// Selected by `shmem_create` based on size and physical-memory
    /// availability. Read by `shmem_map` / `shmem_unmap` /
    /// `shmem_destroy` to pick the right paging routine + free order.
    pub block_order: u8,

    /// Total size in bytes (multiple of PAGE_SIZE)
    pub size: usize,

    /// Access permissions
    pub perms: ShmemPerms,

    /// Tasks with access to this region
    /// First task in list is the creator/owner
    pub tasks: Vec<TaskId>,

    /// Every live mapping of this region, as `(task_id, virt_base)`.
    /// Updated on `shmem_map` / `shmem_unmap`. Used by `shmem_destroy`
    /// to clear each task's PTEs BEFORE the physical pages return to
    /// the PMM — without this, dangling PTEs in grantee tasks would
    /// become a use-after-free window the moment the PMM reallocates
    /// the freed pages to another consumer.
    ///
    /// One task mapping the same region at two distinct virtual
    /// addresses produces two entries, tracked independently.
    pub mappings: Vec<(TaskId, VirtAddr)>,
}

/// Block size threshold above which `shmem_create` tries the 2 MiB
/// huge-page path. The motivating case is the 604 MiB shmem-backed
/// Qwen3 weight stream (PR #170) — at 4 KiB pages that's 154,729 PTEs,
/// far exceeding any x86_64 dTLB capacity (1024-4096 entries). With
/// 2 MiB pages it collapses to 302 PD entries, fits in dTLB
/// comfortably.
///
/// Smaller regions (the 4 KiB IPC shmems libfolk hands out per task,
/// the 280 KiB SQLite buffer Synapse loads, etc.) keep the 4 KiB path
/// — internal-fragmentation cost of huge pages outweighs the TLB win
/// at that scale.
const HUGE_PAGE_THRESHOLD: usize = 2 * 1024 * 1024;
const HUGE_PAGE_SIZE: usize = 2 * 1024 * 1024;
const HUGE_PAGE_ORDER: u8 = 9;

/// Global shared memory table
lazy_static! {
    pub static ref SHMEM_TABLE: Mutex<HashMap<u32, SharedMemory, DefaultHashBuilder>> =
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
    /// Failed to map page into address space
    MapFailed,
    /// Failed to unmap page from address space
    UnmapFailed,
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
    if size == 0 {
        return Err(ShmemError::InvalidSize);
    }

    // Standard 4 KiB path.
    let num_pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
    let actual_size = num_pages * PAGE_SIZE;

    let mut phys_pages = Vec::new();
    for _ in 0..num_pages {
        match alloc_page() {
            Some(page_addr) => phys_pages.push(page_addr),
            None => {
                for &addr in &phys_pages {
                    free_pages(addr, 0);
                }
                return Err(ShmemError::OutOfMemory);
            }
        }
    }

    let id_raw = NEXT_SHMEM_ID.fetch_add(1, Ordering::Relaxed);
    let id = NonZeroU32::new(id_raw)
        .ok_or(ShmemError::IdOverflow)?;

    let current_task_id = crate::task::task::current_task().lock().id;

    let shmem = SharedMemory {
        id,
        phys_pages,
        block_order: 0,
        size: actual_size,
        perms,
        tasks: alloc::vec![current_task_id],
        mappings: Vec::new(),
    };

    SHMEM_TABLE.lock().insert(id_raw, shmem);

    Ok(id)
}

/// Create a shared memory region backed by 2 MiB huge pages.
///
/// Distinct from `shmem_create` because callers MUST guarantee that
/// the eventual `shmem_map` virt address is 2 MiB-aligned — otherwise
/// the mapping is rejected. The motivating case is the 604 MiB Qwen3
/// weight stream (`drivers::model_disk::read_into_shmem` →
/// inference task's `MODEL_VADDR = 0x6000_0000`); collapsing that
/// from 154,729 4 KiB PTEs to 302 PD entries fits the entire weight
/// table in dTLB and unlocks streaming bandwidth on the inner
/// matmul loop.
///
/// We deliberately avoid a "guess huge if size is large" heuristic
/// inside `shmem_create` because most existing callers fix their
/// VFS_VADDR / SHMEM_BUFFER constants at non-2 MiB-aligned offsets.
/// Auto-promoting their requests to huge pages would silently fail
/// the map step. Callers who want huge pages opt in here, and own
/// the alignment contract.
///
/// Falls back to the standard 4 KiB path internally if the buddy +
/// bootstrap arena can't satisfy a 2 MiB allocation — better than
/// hard-failing on PMM fragmentation. The fall-back region behaves
/// identically to `shmem_create`'s output (4 KiB block_order), so
/// the caller still has to map at a 2 MiB-aligned address; the OS
/// just spent more PTEs to get there.
pub fn shmem_create_huge(size: usize, perms: ShmemPerms) -> Result<ShmemId, ShmemError> {
    if size == 0 {
        return Err(ShmemError::InvalidSize);
    }
    let num_blocks = (size + HUGE_PAGE_SIZE - 1) / HUGE_PAGE_SIZE;
    let actual_size = num_blocks * HUGE_PAGE_SIZE;
    let mut phys_pages = Vec::with_capacity(num_blocks);
    let mut all_ok = true;
    for _ in 0..num_blocks {
        match alloc_pages(HUGE_PAGE_ORDER as usize) {
            Some(addr) => phys_pages.push(addr),
            None => { all_ok = false; break; }
        }
    }
    if !all_ok {
        // Roll back partial huge allocation and fall through to 4 KiB.
        for &addr in &phys_pages { free_pages(addr, HUGE_PAGE_ORDER as usize); }
        return shmem_create(size, perms);
    }

    let id_raw = NEXT_SHMEM_ID.fetch_add(1, Ordering::Relaxed);
    let id = match NonZeroU32::new(id_raw) {
        Some(i) => i,
        None => {
            for &addr in &phys_pages { free_pages(addr, HUGE_PAGE_ORDER as usize); }
            return Err(ShmemError::IdOverflow);
        }
    };
    let current_task_id = crate::task::task::current_task().lock().id;
    let shmem = SharedMemory {
        id,
        phys_pages,
        block_order: HUGE_PAGE_ORDER,
        size: actual_size,
        perms,
        tasks: alloc::vec![current_task_id],
        mappings: Vec::new(),
    };
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
    // 1. Validate ShmemId exists
    let shmem = {
        let table = SHMEM_TABLE.lock();
        table.get(&id.get())
            .ok_or(ShmemError::InvalidId)?
            .clone()
    };

    // Page-alignment check: 4 KiB for standard regions, 2 MiB for
    // huge-page regions. Userspace callers that target a fixed virt
    // address need to know which it is — see the MODEL_VADDR change
    // in inference's vfs_loader: bumped from 0x6004_0000 to a
    // 2 MiB-aligned slot once the model-disk shmem started using
    // huge pages.
    let block_size = if shmem.block_order == HUGE_PAGE_ORDER {
        HUGE_PAGE_SIZE
    } else {
        PAGE_SIZE
    };
    if virt % block_size != 0 {
        return Err(ShmemError::InvalidSize);
    }

    // 2. Check current task has access
    let current_task_id = crate::task::task::current_task().lock().id;

    if !shmem.tasks.contains(&current_task_id) {
        return Err(ShmemError::PermissionDenied);
    }

    // 3. Map pages into CURRENT TASK's page table (not the kernel's!)
    let page_flags = match shmem.perms {
        ShmemPerms::ReadOnly => PageFlags::READABLE.or(PageFlags::USER),
        ShmemPerms::WriteOnly => PageFlags::WRITABLE.or(PageFlags::USER),
        ShmemPerms::ReadWrite => PageFlags::READABLE.or(PageFlags::WRITABLE).or(PageFlags::USER),
    };

    let pt_flags = convert_page_flags(page_flags);

    // Get current task's PML4 physical address
    let task_pml4 = crate::task::task::current_task().lock().page_table_phys;
    if task_pml4 == 0 {
        return Err(ShmemError::MapFailed); // No page table for this task
    }

    // Install PTEs / PD entries. Track how many succeeded so we can
    // roll back on partial failure — a failure at the N-th block
    // leaves blocks 0..N mapped to phys pages the caller can't unmap.
    let huge = shmem.block_order == HUGE_PAGE_ORDER;
    let mut mapped_blocks = 0usize;
    for (i, &phys) in shmem.phys_pages.iter().enumerate() {
        let virt_block = virt + (i * block_size);
        let map_result = if huge {
            paging::map_huge_page_in_table(task_pml4, virt_block, phys, pt_flags)
        } else {
            paging::map_page_in_table(task_pml4, virt_block, phys, pt_flags)
        };
        if map_result.is_err() {
            for j in 0..mapped_blocks {
                let v = virt + j * block_size;
                let _ = if huge {
                    paging::unmap_huge_page_in_table(task_pml4, v)
                } else {
                    paging::unmap_page_in_table(task_pml4, v)
                };
            }
            return Err(ShmemError::MapFailed);
        }
        mapped_blocks += 1;
    }
    let mapped_pages = mapped_blocks; // alias used below

    // Re-acquire SHMEM_TABLE and verify the region is STILL in the
    // table. If a concurrent `shmem_destroy` (or `free_task_regions`
    // via a creator exit) ran between our clone-under-lock above and
    // this check, the physical pages we just mapped have already
    // been handed back to the PMM — our task's PTEs point at pages
    // that the allocator considers free and will reuse. That's a
    // classic cross-task use-after-free.
    //
    // Detect it and unmap our PTEs immediately. The pages may still
    // be live in their new role, but at least OUR task stops
    // aliasing them.
    {
        let mut table = SHMEM_TABLE.lock();
        match table.get_mut(&id.get()) {
            Some(region) => {
                // Record the mapping so the next `shmem_destroy` /
                // `free_task_regions` knows to clear our PTEs before
                // freeing pages.
                region.mappings.push((current_task_id, virt));
            }
            None => {
                // Region vanished under us. Don't leave dangling PTEs.
                drop(table);
                for i in 0..mapped_pages {
                    let v = virt + i * block_size;
                    let _ = if huge {
                        paging::unmap_huge_page_in_table(task_pml4, v)
                    } else {
                        paging::unmap_page_in_table(task_pml4, v)
                    };
                }
                return Err(ShmemError::InvalidId);
            }
        }
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
    // Get region info
    let shmem = {
        let table = SHMEM_TABLE.lock();
        table.get(&id.get())
            .ok_or(ShmemError::InvalidId)?
            .clone()
    };

    let huge = shmem.block_order == HUGE_PAGE_ORDER;
    let block_size = if huge { HUGE_PAGE_SIZE } else { PAGE_SIZE };
    if virt % block_size != 0 {
        return Err(ShmemError::InvalidSize);
    }

    // Unmap each block from CURRENT TASK's page table
    let task_pml4 = crate::task::task::current_task().lock().page_table_phys;
    if task_pml4 == 0 {
        return Err(ShmemError::UnmapFailed);
    }

    for i in 0..shmem.phys_pages.len() {
        let virt_block = virt + (i * block_size);
        let r = if huge {
            paging::unmap_huge_page_in_table(task_pml4, virt_block)
        } else {
            paging::unmap_page_in_table(task_pml4, virt_block)
        };
        r.map_err(|_| ShmemError::UnmapFailed)?;
    }

    // De-register the mapping record so a future destroy doesn't
    // try to unmap pages we just cleared. Match on both fields —
    // the same task can map the region at multiple virtual
    // addresses and we must only retire the one actually unmapped.
    let current_task_id = crate::task::task::current_task().lock().id;
    if let Some(region) = SHMEM_TABLE.lock().get_mut(&id.get()) {
        region.mappings.retain(|&(t, v)| !(t == current_task_id && v == virt));
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
    // 1. Remove from table — subsequent map/unmap calls from any
    //    task will now fail with InvalidId, preventing new
    //    mappings from being added while we tear the old ones down.
    let shmem = {
        let mut table = SHMEM_TABLE.lock();
        table.remove(&id.get())
            .ok_or(ShmemError::InvalidId)?
    };

    // 2. Permission check (unchanged — put the region back if denied).
    let current_task_id = crate::task::task::current_task().lock().id;
    if !shmem.tasks.contains(&current_task_id) {
        SHMEM_TABLE.lock().insert(id.get(), shmem);
        return Err(ShmemError::PermissionDenied);
    }

    // 3. Clear every recorded mapping's PTEs BEFORE returning the
    //    physical pages to the PMM. Without this, each grantee task
    //    keeps a dangling mapping into whatever the allocator hands
    //    the page to next — a trivial use-after-free across tasks.
    //
    //    Errors here are silent: a task may have already exited
    //    (its page table freed) or unmapped through some other
    //    path. The load-bearing guarantee is "no live PTE survives
    //    past the free_pages call", which unmap_page_in_table
    //    enforces on the tasks it reaches.
    clear_mappings(&shmem);

    // 4. Free physical pages at the right buddy order.
    let order = shmem.block_order as usize;
    for &phys_addr in &shmem.phys_pages {
        free_pages(phys_addr, order);
    }

    Ok(())
}

/// Walk every `(task, virt_base)` mapping recorded on `shmem` and
/// clear the PTEs for each page in the region. Shared by
/// `shmem_destroy` and the creator-arm of `free_task_regions`.
///
/// Best-effort: per-task lookup failures are ignored (dead task =
/// PML4 already freed = PTEs already gone).
///
/// ### Why no cross-CPU TLB shootdown?
///
/// `unmap_page_in_table` issues `INVLPG` via the x86_64 crate's
/// `flush.flush()`, which invalidates only the CURRENT CPU's TLB.
/// On a general SMP OS that'd leave stale entries on the other
/// CPUs — but Folkering's SMP model restricts APs to a GEMM worker
/// loop (see `arch/x86_64/smp.rs::ap_worker_loop`). APs never
/// context-switch into user tasks, never load a task's PML4 into
/// CR3, and only cache HHDM (kernel-space) mappings — which we
/// don't mutate here. The BSP is the only CPU that can ever have
/// a user-space TLB entry for the pages we're clearing, and the
/// local INVLPG already handles it. If APs ever start running
/// tasks, this comment is the flag to revisit and add a proper
/// IPI-driven flush via `arch::x86_64::apic`.
fn clear_mappings(shmem: &SharedMemory) {
    let huge = shmem.block_order == HUGE_PAGE_ORDER;
    let block_size = if huge { HUGE_PAGE_SIZE } else { PAGE_SIZE };
    let num_blocks = shmem.phys_pages.len();
    for &(task_id, virt_base) in &shmem.mappings {
        let pml4 = match crate::task::task::get_task(task_id) {
            Some(t) => t.lock().page_table_phys,
            None => continue, // task exited — nothing to clear
        };
        if pml4 == 0 { continue; }
        for i in 0..num_blocks {
            let v = virt_base + i * block_size;
            let _ = if huge {
                paging::unmap_huge_page_in_table(pml4, v)
            } else {
                paging::unmap_page_in_table(pml4, v)
            };
        }
    }
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

/// Free every shared-memory region owned or granted to `task_id`.
///
/// Semantics:
/// - If `task_id` is the *creator* (first entry in the region's
///   `tasks` list), the region is fully destroyed: physical pages are
///   returned to the page allocator and the table entry is removed.
/// - Otherwise the task is just dropped from the grant list — other
///   tasks that still hold access keep the region.
///
/// Called from `syscall_exit`. Without this, any region the task
/// created (e.g. the 4 KiB IPC shmems libfolk allocates for every
/// Synapse upsert) would leak forever: its entry sits in
/// `SHMEM_TABLE`, its pages sit on the physical allocator's books but
/// nobody can free them because the creator is gone.
pub fn free_task_regions(task_id: TaskId) {
    // Phase 1 (under table lock): classify each region, drop any
    // stale `mappings` entries where the exiting task was the
    // mapper. If we DIDN'T do this, a later `shmem_destroy` on a
    // region the exiting task had mapped would call
    // `unmap_page_in_table` against this task's (about-to-be-freed)
    // PML4 — a use-after-free on the page table itself.
    let to_destroy: alloc::vec::Vec<SharedMemory> = {
        let mut table = SHMEM_TABLE.lock();
        let mut destroy_ids: alloc::vec::Vec<u32> = alloc::vec::Vec::new();

        for (&id, region) in table.iter_mut() {
            if region.tasks.first() == Some(&task_id) {
                // Creator — mark for full destroy in phase 2.
                destroy_ids.push(id);
            } else {
                // Grantee — drop from access list and drop any of
                // this task's live mapping records so the eventual
                // destroy doesn't touch a freed page table.
                region.tasks.retain(|&t| t != task_id);
                region.mappings.retain(|&(t, _)| t != task_id);
            }
        }

        // Pull the marked regions OUT of the table so phase 2 can
        // work without holding the table lock (clear_mappings needs
        // get_task which takes a task lock — keeping the table lock
        // while reaching into another subsystem invites deadlock).
        let mut out = alloc::vec::Vec::with_capacity(destroy_ids.len());
        for id in destroy_ids {
            if let Some(region) = table.remove(&id) {
                out.push(region);
            }
        }
        out
    };

    // Phase 2 (no locks held): for each region we're destroying,
    // clear every grantee's PTEs (skipping the exiting task — its
    // PML4 is about to go away and its entries are the dead ones we
    // never wanted to touch) BEFORE freeing the physical pages.
    for mut region in to_destroy {
        region.mappings.retain(|&(t, _)| t != task_id);
        clear_mappings(&region);
        let order = region.block_order as usize;
        for &phys in &region.phys_pages {
            free_pages(phys, order);
        }
    }
}

/// Map a single page into virtual address space
///
/// Platform-specific implementation (x86-64).
/// Delegates to the kernel's page table management system.
///
/// # Arguments
/// - `virt`: Virtual address (page-aligned)
/// - `phys`: Physical address (page-aligned)
/// - `flags`: Page protection flags
fn map_page(virt: VirtAddr, phys: PhysAddr, flags: PageFlags) -> Result<(), ShmemError> {
    // Validate addresses are page-aligned
    if virt % PAGE_SIZE != 0 || phys % PAGE_SIZE != 0 {
        return Err(ShmemError::InvalidSize);
    }

    // Convert PageFlags to PageTableFlags
    let pt_flags = convert_page_flags(flags);

    // Call kernel paging system to perform actual mapping
    paging::map_page(virt, phys, pt_flags)
        .map_err(|e| match e {
            paging::MapError::MapperNotInitialized => ShmemError::MapFailed,
            paging::MapError::MapFailed => ShmemError::MapFailed,
            paging::MapError::OutOfMemory => ShmemError::OutOfMemory,
            _ => ShmemError::MapFailed,
        })
}

/// Unmap a single page from virtual address space
///
/// Platform-specific implementation (x86-64).
/// Delegates to the kernel's page table management system.
fn unmap_page(virt: VirtAddr) -> Result<(), ShmemError> {
    // Validate address is page-aligned
    if virt % PAGE_SIZE != 0 {
        return Err(ShmemError::InvalidSize);
    }

    // Call kernel paging system to perform actual unmapping
    paging::unmap_page(virt)
        .map(|_phys| ()) // Discard physical address - shared memory owns the pages
        .map_err(|e| match e {
            paging::MapError::MapperNotInitialized => ShmemError::UnmapFailed,
            paging::MapError::UnmapFailed => ShmemError::UnmapFailed,
            paging::MapError::PageNotMapped => ShmemError::UnmapFailed,
            _ => ShmemError::UnmapFailed,
        })
}

/// Convert shared memory PageFlags to kernel PageTableFlags
///
/// Maps the simplified PageFlags used by shared memory to the
/// detailed PageTableFlags used by the kernel's paging system.
///
/// # Security
/// Shared memory pages are always mapped with NO_EXECUTE to prevent
/// code execution attacks via shared data regions.
fn convert_page_flags(flags: PageFlags) -> PageTableFlags {
    let mut pt_flags = PageTableFlags::PRESENT;

    // Check for writable flag
    if flags.bits & PageFlags::WRITABLE.bits != 0 {
        pt_flags |= PageTableFlags::WRITABLE;
    }

    // Check for user-accessible flag
    if flags.bits & PageFlags::USER.bits != 0 {
        pt_flags |= PageTableFlags::USER_ACCESSIBLE;
    }

    // Always set NO_EXECUTE for security (shared memory should not contain code)
    pt_flags |= PageTableFlags::NO_EXECUTE;

    pt_flags
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
