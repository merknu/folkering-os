//! Bump allocator used as the global allocator while parsing GGUF metadata.
//! No deallocation, no fragmentation — fine for one-shot init.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

const HEAP_SIZE: usize = 128 * 1024; // 128KB for GGUF metadata + tensor index

pub struct BumpAllocator {
    heap: UnsafeCell<[u8; HEAP_SIZE]>,
    next: UnsafeCell<usize>,
}

unsafe impl Sync for BumpAllocator {}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let next = &mut *self.next.get();
        let heap = &mut *self.heap.get();

        let align = layout.align();
        let aligned = (*next + align - 1) & !(align - 1);
        let new_next = aligned + layout.size();

        if new_next > HEAP_SIZE {
            core::ptr::null_mut()
        } else {
            *next = new_next;
            heap.as_mut_ptr().add(aligned)
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator doesn't deallocate
    }
}

impl BumpAllocator {
    pub const fn new() -> Self {
        Self {
            heap: UnsafeCell::new([0; HEAP_SIZE]),
            next: UnsafeCell::new(0),
        }
    }
}
