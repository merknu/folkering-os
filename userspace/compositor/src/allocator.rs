//! Heap allocator for the compositor binary.
//! Free-list allocator with block splitting and coalescing.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

const HEAP_SIZE: usize = 16 * 1024 * 1024; // 16MB heap (wasmi engine ~1MB + WASM 4MB surface + DrawCmd + previous app remnants)

/// Minimum block size (header + usable). Must fit a FreeNode.
const MIN_BLOCK: usize = 32;

/// Header stored before every allocated block
#[repr(C)]
struct BlockHeader {
    size: usize,  // Total size including header, aligned
    _pad: usize,  // Alignment padding to 16 bytes
}

const HEADER_SIZE: usize = core::mem::size_of::<BlockHeader>();

/// Node in the free list (stored inside free blocks)
#[repr(C)]
struct FreeNode {
    size: usize,
    next: *mut FreeNode,
}

struct FreeListAllocator {
    heap: UnsafeCell<[u8; HEAP_SIZE]>,
    free_head: UnsafeCell<*mut FreeNode>,
    initialized: UnsafeCell<bool>,
}

unsafe impl Sync for FreeListAllocator {}

impl FreeListAllocator {
    /// Initialize the free list with the entire heap as one free block
    unsafe fn init(&self) {
        let heap_ptr = (*self.heap.get()).as_mut_ptr();
        let node = heap_ptr as *mut FreeNode;
        (*node).size = HEAP_SIZE;
        (*node).next = core::ptr::null_mut();
        *self.free_head.get() = node;
        *self.initialized.get() = true;
    }
}

unsafe impl GlobalAlloc for FreeListAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !*self.initialized.get() {
            self.init();
        }

        // Required size: header + payload, aligned up
        let align = layout.align().max(16); // Minimum 16-byte alignment
        let payload_size = layout.size();
        let total_size = ((HEADER_SIZE + payload_size + align - 1) & !(align - 1)).max(MIN_BLOCK);

        // First-fit search through free list
        let mut prev: *mut FreeNode = core::ptr::null_mut();
        let mut current = *self.free_head.get();

        while !current.is_null() {
            let block_size = (*current).size;

            if block_size >= total_size {
                // Found a suitable block
                let remaining = block_size - total_size;

                if remaining >= MIN_BLOCK {
                    // Split: create new free node after our allocation
                    let new_free = (current as *mut u8).add(total_size) as *mut FreeNode;
                    (*new_free).size = remaining;
                    (*new_free).next = (*current).next;

                    // Update links
                    if prev.is_null() {
                        *self.free_head.get() = new_free;
                    } else {
                        (*prev).next = new_free;
                    }
                } else {
                    // Use entire block (no split, avoid tiny fragments)
                    let actual_size = block_size; // Use full block
                    if prev.is_null() {
                        *self.free_head.get() = (*current).next;
                    } else {
                        (*prev).next = (*current).next;
                    }
                    // Store actual block size in header
                    let header = current as *mut BlockHeader;
                    (*header).size = actual_size;
                    return (header as *mut u8).add(HEADER_SIZE);
                }

                // Store header
                let header = current as *mut BlockHeader;
                (*header).size = total_size;
                return (header as *mut u8).add(HEADER_SIZE);
            }

            prev = current;
            current = (*current).next;
        }

        // Out of memory
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() { return; }

        // Recover header
        let header = ptr.sub(HEADER_SIZE) as *mut BlockHeader;
        let block_start = header as *mut u8;
        let block_size = (*header).size;

        // Insert into free list (sorted by address for coalescing)
        let new_node = block_start as *mut FreeNode;
        (*new_node).size = block_size;

        let mut prev: *mut FreeNode = core::ptr::null_mut();
        let mut current = *self.free_head.get();

        // Find insertion point (sorted by address)
        while !current.is_null() && (current as *mut u8) < block_start {
            prev = current;
            current = (*current).next;
        }

        (*new_node).next = current;

        if prev.is_null() {
            *self.free_head.get() = new_node;
        } else {
            (*prev).next = new_node;
        }

        // Coalesce with next block if adjacent
        if !current.is_null() {
            let new_end = (new_node as *mut u8).add((*new_node).size);
            if new_end == current as *mut u8 {
                (*new_node).size += (*current).size;
                (*new_node).next = (*current).next;
            }
        }

        // Coalesce with previous block if adjacent
        if !prev.is_null() {
            let prev_end = (prev as *mut u8).add((*prev).size);
            if prev_end == new_node as *mut u8 {
                (*prev).size += (*new_node).size;
                (*prev).next = (*new_node).next;
            }
        }
    }
}

#[global_allocator]
static ALLOCATOR: FreeListAllocator = FreeListAllocator {
    heap: UnsafeCell::new([0; HEAP_SIZE]),
    free_head: UnsafeCell::new(core::ptr::null_mut()),
    initialized: UnsafeCell::new(false),
};
