//! Kernel Heap Allocator
//!
//! Provides dynamic memory allocation for the kernel.
//! Protected by guard page to detect overflow.
//!
//! Wraps `linked_list_allocator::LockedHeap` in a thin tracking
//! shell so we can answer "what's actually growing under flood" via
//! the `heap_walk` syscall (0x85). The tracking adds two atomic
//! increments per alloc + one per dealloc — no measurable hot-path
//! cost, gives us X-ray vision into heap growth.

use linked_list_allocator::LockedHeap;
use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicU64, Ordering};

/// Tracking wrapper around `LockedHeap`. Counters live here so we
/// can read them without locking the heap (the inner mutex is held
/// during the actual alloc — we avoid re-locking on the read side).
pub struct TrackedHeap {
    inner: LockedHeap,
    alloc_count: AtomicU64,
    dealloc_count: AtomicU64,
    /// Sum of `layout.size()` for every successful alloc, minus the
    /// same for every dealloc. NOT exact heap usage (alignment
    /// padding + free-list bookkeeping skew this) — it's the
    /// requested-bytes high-water signal, which is what tells us
    /// "user code is asking for more than it returns" (= leak shape).
    requested_bytes: AtomicU64,
    /// Peak `requested_bytes` seen since boot. Crucial for #54-style
    /// "did the heap ever grow this big" analysis after the system
    /// has GC'd back down.
    high_water: AtomicU64,
}

impl TrackedHeap {
    pub const fn new() -> Self {
        Self {
            inner: LockedHeap::empty(),
            alloc_count: AtomicU64::new(0),
            dealloc_count: AtomicU64::new(0),
            requested_bytes: AtomicU64::new(0),
            high_water: AtomicU64::new(0),
        }
    }

    /// Initialize the underlying heap. Same signature as
    /// `LockedHeap::init`. Called once at boot.
    pub unsafe fn init(&self, start: *mut u8, size: usize) {
        self.inner.lock().init(start, size);
    }

    /// Snapshot the underlying heap's `(size, used, free)`. The inner
    /// `Heap` reports actual byte accounting (with alignment padding),
    /// our atomics track requested bytes — they will not always agree
    /// to the byte. Both views are useful: inner = "what the
    /// allocator sees", atomics = "what code asked for".
    pub fn inner_snapshot(&self) -> (usize, usize, usize) {
        let h = self.inner.lock();
        (h.size(), h.used(), h.free())
    }

    pub fn alloc_count(&self) -> u64 { self.alloc_count.load(Ordering::Relaxed) }
    pub fn dealloc_count(&self) -> u64 { self.dealloc_count.load(Ordering::Relaxed) }
    pub fn requested_bytes(&self) -> u64 { self.requested_bytes.load(Ordering::Relaxed) }
    pub fn high_water(&self) -> u64 { self.high_water.load(Ordering::Relaxed) }
}

unsafe impl GlobalAlloc for TrackedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = self.inner.alloc(layout);
        if !ptr.is_null() {
            self.alloc_count.fetch_add(1, Ordering::Relaxed);
            // Update requested_bytes; capture new value so we can
            // race-free update high_water without re-locking.
            let new_total = self
                .requested_bytes
                .fetch_add(layout.size() as u64, Ordering::Relaxed)
                + layout.size() as u64;

            // High-water update: standard CAS-loop pattern. Only
            // proceeds while the new value strictly exceeds what
            // someone else already wrote.
            let mut current = self.high_water.load(Ordering::Relaxed);
            while new_total > current {
                match self.high_water.compare_exchange_weak(
                    current,
                    new_total,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(c) => current = c,
                }
            }
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.inner.dealloc(ptr, layout);
        self.dealloc_count.fetch_add(1, Ordering::Relaxed);
        self.requested_bytes
            .fetch_sub(layout.size() as u64, Ordering::Relaxed);
    }
}

#[global_allocator]
pub static ALLOCATOR: TrackedHeap = TrackedHeap::new();

const HEAP_START: usize = 0xFFFF_FFFF_8100_0000;
const HEAP_SIZE: usize = 32 * 1024 * 1024; // 32MB (doubled for 24h+ Draug runs)

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
        ALLOCATOR.init(HEAP_START as *mut u8, HEAP_SIZE);
    }

    crate::serial_str!("[HEAP] Kernel heap initialized at ");
    crate::drivers::serial::write_hex(HEAP_START as u64);
    crate::drivers::serial::write_newline();
}

/// Return kernel heap stats: (total_bytes, used_bytes, free_bytes).
/// Same signature pre-tracking — kept for in-tree call sites.
pub fn heap_stats() -> (usize, usize, usize) {
    ALLOCATOR.inner_snapshot()
}

/// Allocation error handler
#[alloc_error_handler]
fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    panic!("Kernel heap exhausted: {} bytes requested", layout.size());
}
