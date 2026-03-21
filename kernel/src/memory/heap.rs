//! Kernel Heap Allocator
//!
//! Provides dynamic memory allocation for the kernel.
//! Protected by guard page to detect overflow.

use linked_list_allocator::LockedHeap;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

const HEAP_START: usize = 0xFFFF_FFFF_8100_0000;
const HEAP_SIZE: usize = 16 * 1024 * 1024; // 16MB

/// Initialize kernel heap
pub fn init() {
    use crate::memory::paging::{map_page, MapError};
    use crate::memory::physical;
    use x86_64::structures::paging::PageTableFlags;

    crate::serial_str!("[HEAP] Initializing kernel heap (");
    crate::drivers::serial::write_dec((HEAP_SIZE / (1024 * 1024)) as u32);
    crate::serial_strln!(" MB)...");

    // Calculate number of pages needed
    let num_pages = (HEAP_SIZE + 4095) / 4096;

    // Allocate and map heap pages directly (writable, kernel-only, NX)
    // NOTE: We do NOT use Vec here as the heap isn't initialized yet
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;

    for i in 0..num_pages {
        let phys_addr = physical::alloc_page()
            .unwrap_or_else(|| panic!("Failed to allocate physical page {} for heap", i));
        let virt_addr = HEAP_START + i * 4096;

        map_page(virt_addr, phys_addr, flags)
            .unwrap_or_else(|e| panic!("Failed to map heap page {}: {:?}", i, e));
    }

    // Setup guard page at end (unmapped - causes page fault on overflow)
    // Guard page is NOT mapped, so accessing it triggers a page fault
    crate::serial_str!("[HEAP] Guard page at ");
    crate::drivers::serial::write_hex((HEAP_START + HEAP_SIZE) as u64);
    crate::serial_strln!(" (unmapped)");

    // Initialize allocator
    unsafe {
        ALLOCATOR.lock().init(HEAP_START as *mut u8, HEAP_SIZE);
    }

    crate::serial_str!("[HEAP] Kernel heap initialized at ");
    crate::drivers::serial::write_hex(HEAP_START as u64);
    crate::drivers::serial::write_newline();
}

/// Allocation error handler
#[alloc_error_handler]
fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    panic!("Kernel heap exhausted: {} bytes requested", layout.size());
}
