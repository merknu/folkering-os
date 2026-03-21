//! Heap-backed BumpArena shim for host-side testing.
//!
//! Named `BumpArena` so that `use crate::arena::BumpArena` in the included
//! libtensor tokenizer.rs resolves correctly without any source modifications.

use std::cell::UnsafeCell;

const ARENA_ALIGN: usize = 32;

pub struct BumpArena {
    inner: UnsafeCell<ArenaInner>,
}

struct ArenaInner {
    buf: Vec<u8>,
    offset: usize,
}

impl BumpArena {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: UnsafeCell::new(ArenaInner {
                buf: vec![0u8; capacity],
                offset: 0,
            }),
        }
    }

    pub fn alloc(&self, size: usize) -> Option<*mut u8> {
        let inner = unsafe { &mut *self.inner.get() };
        let aligned = (inner.offset + ARENA_ALIGN - 1) & !(ARENA_ALIGN - 1);
        let new_offset = aligned + size;
        if new_offset > inner.buf.len() {
            return None;
        }
        inner.offset = new_offset;
        Some(unsafe { inner.buf.as_mut_ptr().add(aligned) })
    }

    pub fn alloc_slice<T>(&self, count: usize) -> Option<&mut [T]> {
        let size = count * std::mem::size_of::<T>();
        let ptr = self.alloc(size)?;
        Some(unsafe { std::slice::from_raw_parts_mut(ptr as *mut T, count) })
    }

    #[allow(dead_code)]
    pub fn alloc_f32(&self, count: usize) -> Option<&mut [f32]> {
        let slice = self.alloc_slice::<f32>(count)?;
        for v in slice.iter_mut() {
            *v = 0.0;
        }
        Some(slice)
    }

    pub fn used(&self) -> usize {
        let inner = unsafe { &*self.inner.get() };
        inner.offset
    }

    pub fn reset_to(&self, mark: usize) {
        let inner = unsafe { &mut *self.inner.get() };
        inner.offset = mark;
    }

    #[allow(dead_code)]
    pub fn reset(&self) {
        self.reset_to(0);
    }
}
