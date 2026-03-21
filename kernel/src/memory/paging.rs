//! Page Table Management
//!
//! x86-64 4-level paging with recursive mapping for page table access.
//! Manages virtual memory mappings, page table allocation, and TLB invalidation.

use crate::boot::BootInfo;
use crate::memory::physical;
use crate::HHDM_OFFSET;
use core::arch::asm;
use spin::Mutex;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame,
    Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

/// Page size (4 KiB)
const PAGE_SIZE: usize = 4096;

/// Kernel page table mapper
static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);

/// Physical frame allocator wrapper
struct BootFrameAllocator;

unsafe impl FrameAllocator<Size4KiB> for BootFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let phys_addr = physical::alloc_page()?;
        Some(PhysFrame::containing_address(PhysAddr::new(phys_addr as u64)))
    }
}

/// Initialize paging subsystem
///
/// Sets up recursive page table mapping and creates kernel page table mapper.
pub fn init(boot_info: &BootInfo) {
    crate::serial_strln!("[PAGING] Initializing page table management...");

    // Get active level 4 page table from CR3
    let level_4_table = unsafe { active_level_4_table() };

    // Create mapper with HHDM offset
    let hhdm = HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let phys_mem_offset = VirtAddr::new(hhdm as u64);
    let mapper = unsafe { OffsetPageTable::new(level_4_table, phys_mem_offset) };

    *MAPPER.lock() = Some(mapper);

    crate::serial_strln!("[PAGING] Page table management initialized");
}

/// Get active level 4 page table
///
/// # Safety
/// Caller must ensure that HHDM mapping is set up correctly.
unsafe fn active_level_4_table() -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;

    let (level_4_table_frame, _) = Cr3::read();
    let phys = level_4_table_frame.start_address().as_u64() as usize;
    let virt = crate::phys_to_virt(phys);
    &mut *(virt as *mut PageTable)
}

/// Map a virtual page to a physical frame
///
/// # Arguments
/// * `virt_addr` - Virtual address to map
/// * `phys_addr` - Physical address to map to
/// * `flags` - Page table flags (writable, user-accessible, etc.)
///
/// # Returns
/// `Ok(())` on success, `Err` if mapping fails.
///
/// # Safety
/// Caller must ensure that:
/// - Physical address points to valid, allocated memory
/// - Virtual address is not already mapped
/// - Flags are appropriate for the intended use
pub fn map_page(
    virt_addr: usize,
    phys_addr: usize,
    flags: PageTableFlags,
) -> Result<(), MapError> {
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt_addr as u64));
    let frame = PhysFrame::containing_address(PhysAddr::new(phys_addr as u64));

    let mut mapper = MAPPER.lock();
    let mapper = mapper.as_mut().ok_or(MapError::MapperNotInitialized)?;

    let mut frame_allocator = BootFrameAllocator;

    unsafe {
        mapper
            .map_to(page, frame, flags, &mut frame_allocator)
            .map_err(|_| MapError::MapFailed)?
            .flush();
    }

    Ok(())
}

/// Unmap a virtual page
///
/// # Arguments
/// * `virt_addr` - Virtual address to unmap
///
/// # Returns
/// Physical address of the unmapped frame, or error if page wasn't mapped.
pub fn unmap_page(virt_addr: usize) -> Result<usize, MapError> {
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt_addr as u64));

    let mut mapper = MAPPER.lock();
    let mapper = mapper.as_mut().ok_or(MapError::MapperNotInitialized)?;

    let (frame, flush) = mapper.unmap(page).map_err(|_| MapError::UnmapFailed)?;

    flush.flush();

    Ok(frame.start_address().as_u64() as usize)
}

/// Unmap a page from a specific task's page table
pub fn unmap_page_in_table(pml4_phys: u64, virt_addr: usize) -> Result<usize, MapError> {
    let pml4_virt = crate::phys_to_virt(pml4_phys as usize);
    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let phys_mem_offset = VirtAddr::new(hhdm as u64);

    let pml4 = unsafe { &mut *(pml4_virt as *mut PageTable) };
    let mut mapper = unsafe { OffsetPageTable::new(pml4, phys_mem_offset) };

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt_addr as u64));
    let (frame, flush) = mapper.unmap(page).map_err(|_| MapError::UnmapFailed)?;
    flush.flush();

    Ok(frame.start_address().as_u64() as usize)
}

/// Translate virtual address to physical address
///
/// # Arguments
/// * `virt_addr` - Virtual address to translate
///
/// # Returns
/// Physical address, or `None` if not mapped.
pub fn translate(virt_addr: usize) -> Option<usize> {
    let mapper = MAPPER.lock();
    let mapper = mapper.as_ref()?;

    let addr = VirtAddr::new(virt_addr as u64);
    let page = Page::<Size4KiB>::containing_address(addr);

    let frame = mapper.translate_page(page).ok()?;
    let offset = addr.as_u64() % PAGE_SIZE as u64;

    Some(frame.start_address().as_u64() as usize + offset as usize)
}

/// Map a range of pages
///
/// Maps `num_pages` contiguous virtual pages starting at `virt_start` to
/// contiguous physical frames starting at `phys_start`.
///
/// # Arguments
/// * `virt_start` - Starting virtual address (will be page-aligned)
/// * `phys_start` - Starting physical address (will be page-aligned)
/// * `num_pages` - Number of pages to map
/// * `flags` - Page table flags
///
/// # Returns
/// `Ok(())` on success, error if any mapping fails.
///
/// # Safety
/// Same requirements as `map_page()`.
pub fn map_range(
    virt_start: usize,
    phys_start: usize,
    num_pages: usize,
    flags: PageTableFlags,
) -> Result<(), MapError> {
    let virt_aligned = virt_start & !(PAGE_SIZE - 1);
    let phys_aligned = phys_start & !(PAGE_SIZE - 1);

    for i in 0..num_pages {
        let virt = virt_aligned + i * PAGE_SIZE;
        let phys = phys_aligned + i * PAGE_SIZE;

        map_page(virt, phys, flags)?;
    }

    Ok(())
}

/// Unmap a range of pages
///
/// # Arguments
/// * `virt_start` - Starting virtual address
/// * `num_pages` - Number of pages to unmap
///
/// # Returns
/// `Ok(())` on success.
pub fn unmap_range(virt_start: usize, num_pages: usize) -> Result<(), MapError> {
    let virt_aligned = virt_start & !(PAGE_SIZE - 1);

    for i in 0..num_pages {
        let virt = virt_aligned + i * PAGE_SIZE;
        unmap_page(virt)?;
    }

    Ok(())
}

/// Change protection flags for a page
///
/// # Arguments
/// * `virt_addr` - Virtual address
/// * `flags` - New page table flags
///
/// # Returns
/// `Ok(())` on success.
pub fn protect(virt_addr: usize, flags: PageTableFlags) -> Result<(), MapError> {
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt_addr as u64));

    let mut mapper = MAPPER.lock();
    let mapper = mapper.as_mut().ok_or(MapError::MapperNotInitialized)?;

    // Get current mapping
    let frame = mapper
        .translate_page(page)
        .map_err(|_| MapError::PageNotMapped)?;

    // Unmap old mapping
    let (_, flush) = mapper.unmap(page).map_err(|_| MapError::UnmapFailed)?;
    flush.flush();

    // Remap with new flags
    let mut frame_allocator = BootFrameAllocator;
    unsafe {
        mapper
            .map_to(page, frame, flags, &mut frame_allocator)
            .map_err(|_| MapError::MapFailed)?
            .flush();
    }

    Ok(())
}

/// Flush TLB entry for a specific virtual address
///
/// # Arguments
/// * `virt_addr` - Virtual address to flush
///
/// # Safety
/// Must be called after modifying page tables.
#[inline]
pub fn flush_tlb(virt_addr: usize) {
    unsafe {
        asm!("invlpg [{}]", in(reg) virt_addr, options(nostack, preserves_flags));
    }
}

/// Flush entire TLB
///
/// # Safety
/// Must be called with caution - flushes all TLB entries.
#[inline]
pub fn flush_tlb_all() {
    use x86_64::registers::control::Cr3;

    // Reload CR3 to flush TLB
    let (frame, flags) = Cr3::read();
    unsafe {
        Cr3::write(frame, flags);
    }
}

/// Virtual address of the kernel stack guard page (0 = not installed).
///
/// Set by `map_guard_page`. The `#PF` handler can compare CR2 against
/// [STACK_GUARD_BASE, STACK_GUARD_BASE + 4096) to detect stack overflows.
pub static STACK_GUARD_BASE: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Map (or record) a non-present guard page at `virt_addr`.
///
/// Any access to this virtual address will trigger `#PF` immediately, turning
/// silent kernel stack overflow into a clean fault that the panic screen can show.
///
/// If the page is currently mapped (e.g. it is part of the BSS range), it is
/// unmapped to create the guard hole. If it was already unmapped, it is already
/// a guard by virtue of being non-present.
pub fn map_guard_page(virt_addr: VirtAddr) {
    let addr = virt_addr.as_u64() as usize;
    STACK_GUARD_BASE.store(addr, core::sync::atomic::Ordering::Relaxed);
    // Best-effort: unmap the page if it happens to be currently mapped.
    // Ignore errors — if the page was not mapped it is already a guard.
    let _ = unmap_page(addr);
    crate::serial_str!("[GUARD] Stack guard page installed at ");
    crate::drivers::serial::write_hex(addr as u64);
    crate::drivers::serial::write_newline();
}

/// Returns `true` if `cr2` falls inside the installed stack guard page.
#[inline]
pub fn is_stack_guard_fault(cr2: u64) -> bool {
    let guard = STACK_GUARD_BASE.load(core::sync::atomic::Ordering::Relaxed);
    guard != 0 && (cr2 as usize).wrapping_sub(guard) < 4096
}

/// Page mapping errors
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// Mapper not initialized
    MapperNotInitialized,
    /// Mapping operation failed
    MapFailed,
    /// Unmapping operation failed
    UnmapFailed,
    /// Page not currently mapped
    PageNotMapped,
    /// Out of memory for page tables
    OutOfMemory,
}

/// Create a new page table for a task
///
/// Allocates a new PML4 and copies kernel mappings (upper half) from the current page table.
/// This provides isolation while sharing kernel address space.
///
/// # Returns
/// Physical address of the new PML4 on success, or error.
///
/// # Safety
/// The returned page table must be properly freed when the task exits.
pub fn create_task_page_table() -> Result<u64, MapError> {
    use x86_64::registers::control::Cr3;

    crate::drivers::serial::write_str("[PAGING] Creating new task page table...\n");

    // Allocate a page for the new PML4
    let new_pml4_phys = physical::alloc_page().ok_or(MapError::OutOfMemory)?;
    let new_pml4_virt = crate::phys_to_virt(new_pml4_phys);

    // Zero out the new PML4
    unsafe {
        core::ptr::write_bytes(new_pml4_virt as *mut u8, 0, PAGE_SIZE);
    }

    // Get the current (kernel) PML4
    let (current_pml4_frame, _) = Cr3::read();
    let current_pml4_phys = current_pml4_frame.start_address().as_u64() as usize;
    let current_pml4_virt = crate::phys_to_virt(current_pml4_phys);

    // Copy kernel mappings (upper half: entries 256-511)
    // The upper half of the address space (0xFFFF_8000_0000_0000 and above) is kernel space
    unsafe {
        let src = current_pml4_virt as *const u64;
        let dst = new_pml4_virt as *mut u64;

        // Copy entries 256-511 (kernel half)
        for i in 256..512 {
            let entry = *src.add(i);
            *dst.add(i) = entry;
        }
    }

    crate::drivers::serial::write_str("[PAGING] New PML4 at phys ");
    crate::drivers::serial::write_hex(new_pml4_phys as u64);
    crate::drivers::serial::write_newline();

    Ok(new_pml4_phys as u64)
}

/// Map a page in a specific task's page table
///
/// # Arguments
/// * `pml4_phys` - Physical address of the task's PML4
/// * `virt_addr` - Virtual address to map
/// * `phys_addr` - Physical address to map to
/// * `flags` - Page table flags
///
/// # Safety
/// Caller must ensure the page table is valid and the physical address is allocated.
pub fn map_page_in_table(
    pml4_phys: u64,
    virt_addr: usize,
    phys_addr: usize,
    flags: PageTableFlags,
) -> Result<(), MapError> {
    let pml4_virt = crate::phys_to_virt(pml4_phys as usize);

    // Create a temporary mapper for this page table
    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let phys_mem_offset = VirtAddr::new(hhdm as u64);

    let pml4 = unsafe { &mut *(pml4_virt as *mut PageTable) };
    let mut mapper = unsafe { OffsetPageTable::new(pml4, phys_mem_offset) };

    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt_addr as u64));
    let frame = PhysFrame::containing_address(PhysAddr::new(phys_addr as u64));

    let mut frame_allocator = BootFrameAllocator;

    unsafe {
        mapper
            .map_to(page, frame, flags, &mut frame_allocator)
            .map_err(|_| MapError::MapFailed)?
            .flush();
    }

    Ok(())
}

/// Switch to a task's page table
///
/// Loads the specified PML4 into CR3.
///
/// # Arguments
/// * `pml4_phys` - Physical address of the task's PML4
///
/// # Safety
/// The page table must be valid and properly initialized.
#[inline]
pub unsafe fn switch_page_table(pml4_phys: u64) {
    use x86_64::registers::control::{Cr3, Cr3Flags};

    let frame = PhysFrame::containing_address(PhysAddr::new(pml4_phys));
    Cr3::write(frame, Cr3Flags::empty());
}

/// Get the current page table's physical address
#[inline]
pub fn current_page_table_phys() -> u64 {
    use x86_64::registers::control::Cr3;
    Cr3::read().0.start_address().as_u64()
}

/// Free a task's page table
///
/// Deallocates the PML4 and any intermediate page tables allocated for user space.
/// Does NOT free kernel mappings (those are shared).
///
/// # Arguments
/// * `pml4_phys` - Physical address of the task's PML4
///
/// # Safety
/// Must not be called while the page table is active (loaded in CR3).
pub fn free_task_page_table(pml4_phys: u64) -> Result<(), MapError> {
    // For now, just free the PML4 page itself
    // TODO: Walk and free intermediate page tables for user space
    physical::free_page(pml4_phys as usize);

    crate::drivers::serial::write_str("[PAGING] Freed task page table at ");
    crate::drivers::serial::write_hex(pml4_phys);
    crate::drivers::serial::write_newline();

    Ok(())
}

/// Common page table flag combinations
pub mod flags {
    use x86_64::structures::paging::PageTableFlags as PTF;

    /// Kernel code (.text) - present, executable
    pub const KERNEL_CODE: PTF = PTF::PRESENT;

    /// Kernel data (.data, .bss) - present, writable
    pub const KERNEL_DATA: PTF = PTF::PRESENT.union(PTF::WRITABLE);

    /// Kernel stack - present, writable, no-execute
    pub const KERNEL_STACK: PTF = PTF::PRESENT
        .union(PTF::WRITABLE)
        .union(PTF::NO_EXECUTE);

    /// User code - present, user-accessible
    pub const USER_CODE: PTF = PTF::PRESENT.union(PTF::USER_ACCESSIBLE);

    /// User data - present, writable, user-accessible, no-execute
    pub const USER_DATA: PTF = PTF::PRESENT
        .union(PTF::WRITABLE)
        .union(PTF::USER_ACCESSIBLE)
        .union(PTF::NO_EXECUTE);

    /// User stack - present, writable, user-accessible, no-execute
    pub const USER_STACK: PTF = USER_DATA;

    /// Guard page - not present (triggers page fault on access)
    pub const GUARD: PTF = PTF::empty();

    /// Framebuffer mapping - Write-Combining (PAT index 4)
    /// Uses bit 7 (PAT bit for 4K pages) to select PAT index 4
    /// Note: We use from_bits_truncate since BIT_7/PAT isn't directly exposed
    pub const USER_FRAMEBUFFER_WC: PTF = PTF::PRESENT
        .union(PTF::WRITABLE)
        .union(PTF::USER_ACCESSIBLE)
        .union(PTF::NO_EXECUTE)
        .union(PTF::from_bits_truncate(0x80));  // PAT bit (bit 7) for index 4 (Write-Combining)

    /// Uncached MMIO mapping
    pub const USER_MMIO_UNCACHED: PTF = PTF::PRESENT
        .union(PTF::WRITABLE)
        .union(PTF::USER_ACCESSIBLE)
        .union(PTF::NO_EXECUTE)
        .union(PTF::NO_CACHE)
        .union(PTF::WRITE_THROUGH);
}

/// Map a page with Write-Combining caching (PAT index 4)
///
/// This is used for framebuffer mappings where write-combining
/// improves performance by batching sequential writes.
///
/// # Arguments
/// * `virt_addr` - Virtual address to map
/// * `phys_addr` - Physical address to map to
///
/// # Returns
/// `Ok(())` on success, `Err` if mapping fails.
pub fn map_page_wc(virt_addr: usize, phys_addr: usize) -> Result<(), MapError> {
    map_page(virt_addr, phys_addr, flags::USER_FRAMEBUFFER_WC)
}

/// Map a page with Write-Combining in a specific task's page table
///
/// # Arguments
/// * `pml4_phys` - Physical address of the task's PML4
/// * `virt_addr` - Virtual address to map
/// * `phys_addr` - Physical address to map to
pub fn map_page_wc_in_table(
    pml4_phys: u64,
    virt_addr: usize,
    phys_addr: usize,
) -> Result<(), MapError> {
    map_page_in_table(pml4_phys, virt_addr, phys_addr, flags::USER_FRAMEBUFFER_WC)
}

/// Map a range of pages with Write-Combining
///
/// # Arguments
/// * `pml4_phys` - Physical address of the task's PML4
/// * `virt_start` - Starting virtual address
/// * `phys_start` - Starting physical address
/// * `num_pages` - Number of pages to map
pub fn map_range_wc_in_table(
    pml4_phys: u64,
    virt_start: usize,
    phys_start: usize,
    num_pages: usize,
) -> Result<(), MapError> {
    let virt_aligned = virt_start & !(PAGE_SIZE - 1);
    let phys_aligned = phys_start & !(PAGE_SIZE - 1);

    for i in 0..num_pages {
        let virt = virt_aligned + i * PAGE_SIZE;
        let phys = phys_aligned + i * PAGE_SIZE;
        map_page_wc_in_table(pml4_phys, virt, phys)?;
    }

    Ok(())
}
