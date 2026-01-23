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
    crate::serial_println!("[PAGING] Initializing page table management...");

    // Get active level 4 page table from CR3
    let level_4_table = unsafe { active_level_4_table() };

    // Create mapper with HHDM offset
    let hhdm = HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let phys_mem_offset = VirtAddr::new(hhdm as u64);
    let mapper = unsafe { OffsetPageTable::new(level_4_table, phys_mem_offset) };

    *MAPPER.lock() = Some(mapper);

    crate::serial_println!("[PAGING] Page table management initialized");
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
}
