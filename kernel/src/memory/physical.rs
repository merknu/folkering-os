//! Physical Memory Manager (Buddy Allocator)
//!
//! O(log n) buddy allocator for physical page allocation.
//! Manages physical memory in power-of-2 sized blocks (orders 0-10).

use crate::boot::BootInfo;
use core::ptr::NonNull;
use spin::Mutex;

const PAGE_SIZE: usize = 4096;
const MAX_ORDER: usize = 10; // 2^10 pages = 4MB max allocation

/// Free block in linked list
#[repr(C)]
struct FreeBlock {
    next: Option<NonNull<FreeBlock>>,
}

/// Buddy allocator state
struct BuddyAllocator {
    /// Free lists for each order (0-10)
    /// Order N contains blocks of 2^N pages
    free_lists: [Option<NonNull<FreeBlock>>; MAX_ORDER + 1],

    /// Base physical address
    base_addr: usize,

    /// Total pages managed
    total_pages: usize,

    /// Free pages available
    free_pages: usize,
}

// Safety: BuddyAllocator is protected by a Mutex, ensuring exclusive access.
// The NonNull pointers are only used within the allocator's methods.
unsafe impl Send for BuddyAllocator {}

impl BuddyAllocator {
    /// Create new empty allocator
    const fn new() -> Self {
        Self {
            free_lists: [None; MAX_ORDER + 1],
            base_addr: 0,
            total_pages: 0,
            free_pages: 0,
        }
    }

    /// Initialize allocator with memory map from bootloader
    fn init(&mut self, boot_info: &BootInfo) {
        use limine::memory_map::EntryType;

        crate::serial_strln!("[PMM] Scanning memory map...");

        // TEMPORARY WORKAROUND: Just count memory, don't try to add regions yet
        // The intrusive list approach (writing FreeBlock to memory) doesn't work
        // before heap initialization. Need to refactor to external tracking later.

        let mut total_mem = 0;
        let mut usable_mem = 0;
        let mut region_count = 0u32;

        for entry in boot_info.memory_map {
            region_count += 1;
            crate::serial_str!("[");
            crate::drivers::serial::write_dec(region_count);
            crate::serial_str!("]");

            if entry.entry_type == EntryType::USABLE {
                crate::serial_print!("U");
                let base = entry.base as usize;
                let size = entry.length as usize;

                // Align to page boundary
                let aligned_base = (base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
                let aligned_end = (base + size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

                if aligned_end > aligned_base {
                    let aligned_size = aligned_end - aligned_base;
                    let num_pages = aligned_size / PAGE_SIZE;

                    total_mem += num_pages;
                    if aligned_base >= 0x100000 {
                        usable_mem += num_pages;
                    }
                    crate::serial_println!(" OK");
                } else {
                    crate::serial_println!(" SKIP");
                }
            } else {
                crate::serial_println!(" Not usable");
            }
        }

        crate::serial_println!();

        // Store counts but don't actually initialize free lists yet
        self.total_pages = total_mem;
        self.free_pages = usable_mem;

        // Set global RAM counters for status bar
        TOTAL_RAM_PAGES.store(total_mem, core::sync::atomic::Ordering::Relaxed);

        crate::serial_str!("[PMM] Total: ");
        crate::drivers::serial::write_dec((total_mem * PAGE_SIZE / (1024 * 1024)) as u32);
        crate::serial_str!(" MB, Usable: ");
        crate::drivers::serial::write_dec((usable_mem * PAGE_SIZE / (1024 * 1024)) as u32);
        crate::serial_strln!(" MB");

        // Set up bootstrap allocator with first usable region > 1MB
        // We'll use this for heap initialization
        for entry in boot_info.memory_map {
            if entry.entry_type == EntryType::USABLE {
                let base = entry.base as usize;
                let size = entry.length as usize;

                let aligned_base = (base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
                let aligned_end = (base + size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

                if aligned_end > aligned_base && aligned_base >= 0x100000 {
                    let aligned_size = aligned_end - aligned_base;
                    let num_pages = aligned_size / PAGE_SIZE;

                    crate::serial_println!("[PMM] Bootstrap allocator ready");

                    *BOOTSTRAP_ALLOCATOR.lock() = Some(BootstrapAllocator::new(aligned_base, num_pages));
                    break;
                }
            }
        }
    }

    /// Add a physical memory region to the allocator
    ///
    /// Splits region into power-of-2 sized blocks and adds to free lists.
    fn add_region(&mut self, mut base: usize, mut num_pages: usize) {
        while num_pages > 0 {
            // Find largest power-of-2 block that fits
            let order = num_pages.trailing_zeros().min(MAX_ORDER as u32) as usize;
            let _block_pages = 1 << order;

            // Check alignment constraint
            let aligned_order = (base / PAGE_SIZE).trailing_zeros().min(order as u32) as usize;
            let actual_order = aligned_order.min(order);
            let actual_pages = 1 << actual_order;

            // Add block to free list
            unsafe {
                self.free_block_unchecked(base, actual_order);
            }

            base += actual_pages * PAGE_SIZE;
            num_pages -= actual_pages;
            self.total_pages += actual_pages;
            self.free_pages += actual_pages;
        }
    }

    /// Allocate 2^order contiguous pages
    ///
    /// Returns physical address of allocated block, or None if allocation fails.
    ///
    /// # Performance
    /// O(log n) in the worst case (requires splitting larger blocks).
    fn alloc_pages(&mut self, order: usize) -> Option<usize> {
        if order > MAX_ORDER {
            return None;
        }

        // Try to allocate from this order's free list
        if let Some(block_ptr) = self.free_lists[order] {
            // Remove from free list
            let block = unsafe { block_ptr.as_ref() };
            self.free_lists[order] = block.next;
            self.free_pages -= 1 << order;
            // Convert virtual address back to physical
            let virt_addr = block_ptr.as_ptr() as usize;
            return crate::virt_to_phys(virt_addr);
        }

        // No blocks at this order - try splitting larger block
        if order < MAX_ORDER {
            if let Some(larger_block_addr) = self.alloc_pages(order + 1) {
                // Split block in half
                let buddy_addr = larger_block_addr + ((1 << order) * PAGE_SIZE);

                // Add second half (buddy) to free list
                unsafe {
                    self.free_block_unchecked(buddy_addr, order);
                }
                self.free_pages += 1 << order; // Buddy is free

                return Some(larger_block_addr);
            }
        }

        None
    }

    /// Free 2^order contiguous pages starting at addr
    ///
    /// Attempts to coalesce with buddy block if it's also free.
    ///
    /// # Performance
    /// O(log n) due to potential coalescing up the tree.
    ///
    /// # Safety
    /// Caller must ensure:
    /// - addr was previously allocated with same order
    /// - addr is not already freed (double-free)
    fn free_pages(&mut self, addr: usize, order: usize) {
        debug_assert!(order <= MAX_ORDER, "Order {} exceeds MAX_ORDER", order);
        debug_assert!(addr % (PAGE_SIZE * (1 << order)) == 0, "Misaligned free");

        // CRITICAL: Check for double-free
        if self.is_block_free(addr, order) {
            panic!(
                "Double-free detected: block 0x{:x} (order {}) is already free!",
                addr, order
            );
        }

        // Try to coalesce with buddy
        if order < MAX_ORDER {
            let buddy_addr = self.buddy_address(addr, order);

            // Check if buddy is free
            if self.is_block_free(buddy_addr, order) {
                // Remove buddy from free list
                self.remove_from_free_list(buddy_addr, order);
                self.free_pages -= 1 << order; // Buddy no longer free separately

                // Merge and free at higher order
                let merged_addr = addr.min(buddy_addr);
                return self.free_pages(merged_addr, order + 1);
            }
        }

        // No coalescing possible - add to free list
        unsafe {
            self.free_block_unchecked(addr, order);
        }
        self.free_pages += 1 << order;
    }

    /// Calculate buddy address for a block
    #[inline]
    fn buddy_address(&self, addr: usize, order: usize) -> usize {
        let block_size = PAGE_SIZE * (1 << order);
        addr ^ block_size
    }

    /// Check if a block is in the free list
    ///
    /// Capped at 1M iterations to defend against a corrupted free
    /// list (double-free creating a cycle, driver-induced memory
    /// stomp, etc). On a 4 GB machine the longest possible order-0
    /// freelist is ~1M entries — the cap is "if you exceeded this,
    /// the list is corrupt, fail closed instead of looping forever."
    fn is_block_free(&self, addr: usize, order: usize) -> bool {
        let virt_addr = crate::phys_to_virt(addr);
        let mut current = self.free_lists[order];
        let mut hops = 0u32;

        while let Some(block_ptr) = current {
            if hops >= 1_000_000 {
                crate::serial_strln!("[PMM] is_block_free: freelist walk exceeded 1M — list corrupt");
                return false;
            }
            if block_ptr.as_ptr() as usize == virt_addr {
                return true;
            }
            let block = unsafe { block_ptr.as_ref() };
            current = block.next;
            hops += 1;
        }

        false
    }

    /// Remove a specific block from free list
    fn remove_from_free_list(&mut self, addr: usize, order: usize) {
        let target_ptr = crate::phys_to_virt(addr) as *mut FreeBlock;
        let mut prev: Option<NonNull<FreeBlock>> = None;
        let mut current = self.free_lists[order];
        let mut hops = 0u32;

        while let Some(block_ptr) = current {
            if hops >= 1_000_000 {
                crate::serial_strln!("[PMM] remove_from_free_list: walk exceeded 1M — list corrupt");
                return;
            }
            hops += 1;
            if block_ptr.as_ptr() == target_ptr {
                // Found it - remove from list
                let block = unsafe { block_ptr.as_ref() };
                let next = block.next;

                if let Some(prev_ptr) = prev {
                    // Update previous block's next pointer
                    unsafe {
                        prev_ptr.as_ptr().write(FreeBlock { next });
                    }
                } else {
                    // Update list head
                    self.free_lists[order] = next;
                }
                return;
            }

            prev = current;
            let block = unsafe { block_ptr.as_ref() };
            current = block.next;
        }

        panic!("Tried to remove non-existent block 0x{:x} from order {}", addr, order);
    }

    /// Add block to free list (unsafe - doesn't update free_pages counter)
    ///
    /// # Safety
    /// Caller must ensure:
    /// - addr points to valid physical memory
    /// - block is not already in any free list
    /// - block is properly aligned for the order
    unsafe fn free_block_unchecked(&mut self, addr: usize, order: usize) {
        let virt_addr = crate::phys_to_virt(addr);

        // Quick sanity check - virtual address should be in higher half
        if virt_addr < 0xFFFF_8000_0000_0000 {
            crate::serial_println!("[PMM-ERROR] Invalid virtual address {:#x} from physical {:#x}", virt_addr, addr);
            return;
        }

        let block_ptr = virt_addr as *mut FreeBlock;
        let next = self.free_lists[order];

        block_ptr.write(FreeBlock { next });
        self.free_lists[order] = Some(NonNull::new_unchecked(block_ptr));
    }

    /// Get current memory statistics
    fn stats(&self) -> MemoryStats {
        MemoryStats {
            total_bytes: self.total_pages * PAGE_SIZE,
            free_bytes: self.free_pages * PAGE_SIZE,
            used_bytes: (self.total_pages - self.free_pages) * PAGE_SIZE,
        }
    }
}

/// Memory statistics
#[derive(Debug, Clone, Copy)]
pub struct MemoryStats {
    pub total_bytes: usize,
    pub free_bytes: usize,
    pub used_bytes: usize,
}

/// Global buddy allocator
static ALLOCATOR: Mutex<BuddyAllocator> = Mutex::new(BuddyAllocator::new());

/// Simple bump allocator for bootstrap (before buddy allocator is ready)
static BOOTSTRAP_ALLOCATOR: Mutex<Option<BootstrapAllocator>> = Mutex::new(None);

/// Simple bump allocator that doesn't need free list metadata
struct BootstrapAllocator {
    next_page: usize,
    end_page: usize,
}

impl BootstrapAllocator {
    fn new(start: usize, num_pages: usize) -> Self {
        Self {
            next_page: start,
            end_page: start + num_pages * PAGE_SIZE,
        }
    }

    fn alloc_page(&mut self) -> Option<usize> {
        if self.next_page < self.end_page {
            let page = self.next_page;
            self.next_page += PAGE_SIZE;
            ALLOCATED_PAGES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            Some(page)
        } else {
            None
        }
    }

    /// Allocate `1 << order` contiguous, naturally aligned pages from
    /// the bump arena. Used by `alloc_pages(order)` when the buddy
    /// free-list is empty for that order — the buddy lists only get
    /// populated as pages get freed back, so at boot they're empty
    /// and a huge-page request would otherwise fail despite plenty
    /// of contiguous physical RAM sitting in the bump arena.
    ///
    /// Skips bytes if needed to land the allocation on a `(PAGE_SIZE
    /// << order)`-aligned boundary; the skipped tail isn't returned
    /// to anyone (acceptable for boot-time allocation, the alternative
    /// is a freelist of small fragments which complicates teardown).
    fn alloc_block(&mut self, order: usize) -> Option<usize> {
        let block_size = PAGE_SIZE << order;
        let alignment = block_size; // natural alignment
        let aligned_start = (self.next_page + alignment - 1) & !(alignment - 1);
        let aligned_end = aligned_start.checked_add(block_size)?;
        if aligned_end > self.end_page {
            return None;
        }
        self.next_page = aligned_end;
        let pages = 1usize << order;
        ALLOCATED_PAGES.fetch_add(pages, core::sync::atomic::Ordering::Relaxed);
        Some(aligned_start)
    }
}

/// Initialize physical memory manager
pub fn init(boot_info: &BootInfo) {
    ALLOCATOR.lock().init(boot_info);
}

/// Allocate 2^order contiguous pages
///
/// Returns physical address of allocated block, or None if allocation fails.
///
/// # Examples
/// ```
/// // Allocate 1 page (4KB)
/// let addr = alloc_pages(0).expect("Out of memory");
///
/// // Allocate 4 pages (16KB)
/// let addr = alloc_pages(2).expect("Out of memory");
/// ```
pub fn alloc_pages(order: usize) -> Option<usize> {
    // Try buddy first (recycled freed allocations).
    if let Some(addr) = ALLOCATOR.lock().alloc_pages(order) {
        return Some(addr);
    }
    // Buddy is empty for this order — most likely it's never been
    // populated yet (free lists only fill on free_pages). Fall back
    // to the bump arena for naturally-aligned multi-page blocks. This
    // is what makes the 2 MiB huge-page shmem path actually work at
    // boot — alloc_pages(9) was always failing because the buddy
    // never had a 2 MiB block to hand out.
    if order > 0 {
        if let Some(ref mut bootstrap) = *BOOTSTRAP_ALLOCATOR.lock() {
            if let Some(addr) = bootstrap.alloc_block(order) {
                return Some(addr);
            }
        }
    }
    None
}

/// Free 2^order contiguous pages starting at addr
///
/// # Safety
/// Caller must ensure:
/// - addr was previously allocated with same order
/// - addr is not already freed (double-free)
///
/// # Examples
/// ```
/// let addr = alloc_pages(2).unwrap();
/// // ... use memory ...
/// unsafe { free_pages(addr, 2); }
/// ```
pub fn free_pages(addr: usize, order: usize) {
    ALLOCATOR.lock().free_pages(addr, order);
}

/// Get current memory statistics
pub fn stats() -> MemoryStats {
    ALLOCATOR.lock().stats()
}

/// Total physical RAM detected at boot (set during PMM init)
static TOTAL_RAM_PAGES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static ALLOCATED_PAGES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Get memory statistics: (total_pages, free_pages)
pub fn memory_stats() -> (usize, usize) {
    let total = TOTAL_RAM_PAGES.load(core::sync::atomic::Ordering::Relaxed);
    let allocated = ALLOCATED_PAGES.load(core::sync::atomic::Ordering::Relaxed);
    let free = total.saturating_sub(allocated);
    (total, free)
}

/// Allocate contiguous physical pages for DMA.
/// Rounds up to nearest power-of-2 order.
pub fn alloc_contiguous(num_pages: usize) -> Option<usize> {
    if num_pages == 0 { return None; }
    // Find smallest order that satisfies: 2^order >= num_pages
    let mut order = 0;
    while (1 << order) < num_pages && order < 10 {
        order += 1;
    }
    alloc_pages(order)
}

/// Allocate a single page (convenience wrapper).
///
/// Checks buddy allocator first (for recycled freed pages), then
/// falls back to bootstrap bump allocator. This ordering is critical:
/// without it, freed pages go into buddy but are never reused because
/// bootstrap always had pages available.
#[inline]
pub fn alloc_page() -> Option<usize> {
    // Try buddy allocator first — this is where free_page() puts pages
    if let Some(page) = alloc_pages(0) {
        return Some(page);
    }

    // Fall back to bootstrap allocator (bump, used before heap is ready
    // and when buddy has no free blocks)
    if let Some(ref mut bootstrap) = *BOOTSTRAP_ALLOCATOR.lock() {
        if let Some(page) = bootstrap.alloc_page() {
            return Some(page);
        }
    }

    None
}

/// Free a single page (convenience wrapper)
///
/// # Safety
/// Same requirements as `free_pages`.
#[inline]
pub fn free_page(addr: usize) {
    free_pages(addr, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buddy_address() {
        let allocator = BuddyAllocator::new();

        // Order 0 (4KB blocks)
        assert_eq!(allocator.buddy_address(0x0000, 0), 0x1000);
        assert_eq!(allocator.buddy_address(0x1000, 0), 0x0000);

        // Order 1 (8KB blocks)
        assert_eq!(allocator.buddy_address(0x0000, 1), 0x2000);
        assert_eq!(allocator.buddy_address(0x2000, 1), 0x0000);
    }

    #[test]
    fn test_alloc_free_cycle() {
        // This would require mock BootInfo - omitted for brevity
    }
}
