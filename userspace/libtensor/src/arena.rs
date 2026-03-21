//! Static Bump Arena — zero-heap inference allocation (ULTRA 12)
//!
//! Pre-allocates a contiguous region via SYS_MMAP at startup.
//! All intermediate tensors (activations, attention scores, FFN buffers)
//! are allocated from this arena. `reset()` after each token generation
//! gives O(1) deallocation with zero fragmentation.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Bump arena for zero-heap inference.
///
/// Thread-safety: single-producer (inference server is single-threaded).
/// AtomicUsize used for correctness with potential interrupt preemption.
pub struct BumpArena {
    base: *mut u8,
    size: usize,
    offset: AtomicUsize,
}

// Safety: BumpArena is only used by single-threaded inference server
unsafe impl Send for BumpArena {}
unsafe impl Sync for BumpArena {}

/// Alignment for all arena allocations (AVX2 requires 32-byte alignment)
const ARENA_ALIGN: usize = 32;

impl BumpArena {
    /// Create an uninitialized arena. Must call `init()` before use.
    pub const fn uninit() -> Self {
        Self {
            base: core::ptr::null_mut(),
            size: 0,
            offset: AtomicUsize::new(0),
        }
    }

    /// Initialize the arena with a pre-allocated memory region.
    ///
    /// # Safety
    /// `base` must point to a valid, exclusively-owned region of `size` bytes.
    pub unsafe fn init(&mut self, base: *mut u8, size: usize) {
        self.base = base;
        self.size = size;
        self.offset.store(0, Ordering::Release);
    }

    /// Initialize the arena by allocating memory via SYS_MMAP.
    ///
    /// Returns Ok(()) on success, Err on mmap failure.
    pub fn init_mmap(&mut self, size: usize) -> Result<(), ()> {
        use libfolk::sys::memory::{mmap, PROT_READ, PROT_WRITE};
        let ptr = mmap(size, PROT_READ | PROT_WRITE).map_err(|_| ())?;
        unsafe { self.init(ptr, size); }
        Ok(())
    }

    /// Allocate `size` bytes from the arena, aligned to ARENA_ALIGN.
    ///
    /// Returns None if the arena is exhausted.
    pub fn alloc(&self, size: usize) -> Option<*mut u8> {
        loop {
            let current = self.offset.load(Ordering::Acquire);
            let aligned = (current + ARENA_ALIGN - 1) & !(ARENA_ALIGN - 1);
            let new_offset = aligned + size;
            if new_offset > self.size {
                return None;
            }
            match self.offset.compare_exchange_weak(
                current, new_offset,
                Ordering::AcqRel, Ordering::Acquire,
            ) {
                Ok(_) => return Some(unsafe { self.base.add(aligned) }),
                Err(_) => continue, // retry on contention
            }
        }
    }

    /// Allocate a typed slice of `count` elements.
    pub fn alloc_slice<T>(&self, count: usize) -> Option<&mut [T]> {
        let size = count * core::mem::size_of::<T>();
        let ptr = self.alloc(size)?;
        Some(unsafe { core::slice::from_raw_parts_mut(ptr as *mut T, count) })
    }

    /// Allocate a zero-initialized slice of f32.
    pub fn alloc_f32(&self, count: usize) -> Option<&mut [f32]> {
        let slice = self.alloc_slice::<f32>(count)?;
        for v in slice.iter_mut() {
            *v = 0.0;
        }
        Some(slice)
    }

    /// Reset the arena — O(1), zero fragmentation.
    /// Call after each generated token.
    pub fn reset(&self) {
        self.offset.store(0, Ordering::Release);
    }

    /// Reset to a saved position (partial reset).
    /// Useful for preserving early allocations (e.g., tokenizer tables)
    /// while reclaiming space used by per-token computation.
    pub fn reset_to(&self, mark: usize) {
        self.offset.store(mark, Ordering::Release);
    }

    /// Current bytes used (can be saved as a "mark" for reset_to).
    pub fn used(&self) -> usize {
        self.offset.load(Ordering::Acquire)
    }

    /// Total arena capacity.
    pub fn capacity(&self) -> usize {
        self.size
    }

    /// Remaining bytes available.
    pub fn remaining(&self) -> usize {
        self.size - self.used()
    }
}
