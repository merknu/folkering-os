//! `ShmemArena` — RAII wrapper around shared-memory mapping.
//!
//! The original synapse-service had six different hardcoded virtual addresses
//! (0x10000000 .. 0x15000000), each manually `shmem_map`/`shmem_unmap`'d in
//! every handler. Forgetting to unmap on an early-out leaked the slot;
//! double-mapping crashed.
//!
//! `ShmemArena` is a thin wrapper that calls `shmem_unmap` automatically when
//! it goes out of scope. Use it instead of bare `shmem_map`/`shmem_unmap`
//! pairs whenever the mapping is short-lived (one handler invocation).

use libfolk::sys::{shmem_map, shmem_unmap};

/// Shared-memory virtual addresses used by Synapse.
/// Each handler family uses a dedicated VADDR to avoid collisions.
pub mod vaddr {
    /// READ_FILE_SHMEM, LIST_FILES output buffer
    pub const SHMEM_BUFFER: usize = 0x10000000;
    /// VECTOR_SEARCH query embedding (input)
    pub const VECTOR_QUERY: usize = 0x11000000;
    /// VECTOR_SEARCH results (output)
    pub const VECTOR_RESULTS: usize = 0x12000000;
    /// WRITE_FILE input parsing
    pub const WRITE_SHMEM: usize = 0x13000000;
    /// WRITE_INTENT / READ_INTENT
    pub const INTENT_SHMEM: usize = 0x14000000;
    /// QUERY_INTENT input string
    pub const QUERY_INTENT: usize = 0x15000000;
}

/// RAII handle to a mapped shared memory region.
///
/// Created by `ShmemArena::map`, automatically calls `shmem_unmap` on drop.
/// The handle itself is **not** destroyed — caller is responsible for
/// `shmem_destroy` if they own it.
pub struct ShmemArena {
    handle: u32,
    vaddr: usize,
    /// Set to false to suppress unmapping (e.g. when transferring ownership).
    active: bool,
}

impl ShmemArena {
    /// Map a shared-memory handle at `vaddr`. Returns an arena that will
    /// automatically unmap when dropped.
    ///
    /// Returns `Err(())` if the underlying `shmem_map` syscall fails.
    pub fn map(handle: u32, vaddr: usize) -> Result<Self, ()> {
        shmem_map(handle, vaddr).map_err(|_| ())?;
        Ok(Self { handle, vaddr, active: true })
    }

    /// Get the mapped virtual address.
    pub fn vaddr(&self) -> usize {
        self.vaddr
    }

    /// Get the underlying shmem handle.
    pub fn handle(&self) -> u32 {
        self.handle
    }

    /// Borrow the mapped region as a read-only slice of `len` bytes.
    ///
    /// # Safety
    /// Caller must ensure `len` is within the actual shmem allocation size.
    pub unsafe fn as_slice(&self, len: usize) -> &[u8] {
        core::slice::from_raw_parts(self.vaddr as *const u8, len)
    }

    /// Borrow the mapped region as a writable slice of `len` bytes.
    ///
    /// # Safety
    /// Caller must ensure `len` is within the actual shmem allocation size,
    /// and that no other reference to this region exists.
    pub unsafe fn as_mut_slice(&mut self, len: usize) -> &mut [u8] {
        core::slice::from_raw_parts_mut(self.vaddr as *mut u8, len)
    }

    /// Suppress automatic unmapping. Useful when transferring the mapping
    /// to another owner. Returns the (handle, vaddr) so the caller can
    /// unmap manually later.
    pub fn forget(mut self) -> (u32, usize) {
        self.active = false;
        (self.handle, self.vaddr)
    }
}

impl Drop for ShmemArena {
    fn drop(&mut self) {
        if self.active {
            let _ = shmem_unmap(self.handle, self.vaddr);
        }
    }
}
