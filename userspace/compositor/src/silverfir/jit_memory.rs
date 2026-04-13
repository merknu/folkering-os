//! JIT Memory — W^X blocks + pre-allocated pool.
//!
//! Two allocation strategies:
//!   1. `JitMemoryBlock` — per-module mmap/munmap (original, for one-shot)
//!   2. `JitPool` — 1MB pre-allocated region with bitmap, zero mmap churn
//!
//! The pool eliminates kernel page allocator fragmentation from repeated
//! JIT compilations over weeks of continuous operation.

use libfolk::sys::memory::{mmap, munmap, mprotect, PROT_READ, PROT_WRITE, PROT_EXEC};

// ── JitMemoryBlock (per-module, original) ───────────────────────────

pub struct JitMemoryBlock {
    ptr: *mut u8,
    size: usize,
    executable: bool,
}

#[derive(Debug)]
pub enum JitMemError {
    AllocFailed,
    ProtectFailed,
    NotExecutable,
    PoolFull,
}

impl JitMemoryBlock {
    pub fn alloc(size: usize) -> Result<Self, JitMemError> {
        let page_size = 4096;
        let aligned_size = (size + page_size - 1) & !(page_size - 1);
        let ptr = mmap(aligned_size, PROT_READ | PROT_WRITE)
            .map_err(|_| JitMemError::AllocFailed)?;
        Ok(Self { ptr, size: aligned_size, executable: false })
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        assert!(!self.executable, "W^X: cannot write to executable memory");
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.size) }
    }

    pub fn make_executable(&mut self) -> Result<(), JitMemError> {
        mprotect(self.ptr, self.size, PROT_READ | PROT_EXEC)
            .map_err(|_| JitMemError::ProtectFailed)?;
        self.executable = true;
        Ok(())
    }

    pub fn make_writable(&mut self) -> Result<(), JitMemError> {
        mprotect(self.ptr, self.size, PROT_READ | PROT_WRITE)
            .map_err(|_| JitMemError::ProtectFailed)?;
        self.executable = false;
        Ok(())
    }

    pub unsafe fn call_void(&self, offset: usize) -> Result<(), JitMemError> {
        if !self.executable { return Err(JitMemError::NotExecutable); }
        if offset >= self.size { return Err(JitMemError::ProtectFailed); }
        let fn_ptr: unsafe extern "C" fn() = core::mem::transmute(self.ptr.add(offset));
        fn_ptr();
        Ok(())
    }

    pub unsafe fn call_u32(&self, offset: usize) -> Result<u32, JitMemError> {
        if !self.executable { return Err(JitMemError::NotExecutable); }
        if offset >= self.size { return Err(JitMemError::ProtectFailed); }
        let fn_ptr: unsafe extern "C" fn() -> u32 = core::mem::transmute(self.ptr.add(offset));
        Ok(fn_ptr())
    }

    pub fn base_ptr(&self) -> *const u8 { self.ptr as *const u8 }
    pub fn size(&self) -> usize { self.size }
    pub fn is_executable(&self) -> bool { self.executable }
}

impl Drop for JitMemoryBlock {
    fn drop(&mut self) {
        if self.executable {
            let _ = mprotect(self.ptr, self.size, PROT_READ | PROT_WRITE);
        }
        let _ = munmap(self.ptr, self.size);
    }
}

// ── JitPool — pre-allocated 1MB region with bitmap ──────────────────
//
// Eliminates mmap/munmap churn. One mmap at init, bitmap tracks which
// 4KB pages are in use. Modules allocate/deallocate within the pool.
// W^X is per-page: entire pool flips RW↔RX together.

/// Pool size: 1MB = 256 pages of 4KB
const POOL_SIZE: usize = 1024 * 1024;
const PAGE_SIZE: usize = 4096;
const POOL_PAGES: usize = POOL_SIZE / PAGE_SIZE; // 256

/// Pre-allocated JIT memory pool.
pub struct JitPool {
    /// Base pointer to the mmap'd region.
    base: *mut u8,
    /// Bitmap: 1 = page in use, 0 = free. 256 bits = 4 × u64.
    bitmap: [u64; 4],
    /// Whether the pool is currently executable.
    executable: bool,
    /// Whether the pool has been initialized.
    initialized: bool,
}

/// Handle to an allocation within the pool.
pub struct JitPoolAlloc {
    /// Offset from pool base in bytes.
    pub offset: usize,
    /// Size in bytes (page-aligned).
    pub size: usize,
    /// Number of pages.
    pages: usize,
    /// First page index in the bitmap.
    first_page: usize,
}

impl JitPool {
    /// Create an uninitialized pool. Call `init()` to mmap.
    pub const fn new() -> Self {
        Self {
            base: core::ptr::null_mut(),
            bitmap: [0u64; 4],
            executable: false,
            initialized: false,
        }
    }

    /// Allocate the 1MB pool region via mmap. Call once at startup.
    pub fn init(&mut self) -> Result<(), JitMemError> {
        if self.initialized { return Ok(()); }
        let ptr = mmap(POOL_SIZE, PROT_READ | PROT_WRITE)
            .map_err(|_| JitMemError::AllocFailed)?;
        self.base = ptr;
        self.bitmap = [0u64; 4];
        self.executable = false;
        self.initialized = true;
        libfolk::sys::io::write_str("[JitPool] Allocated 1MB pool\n");
        Ok(())
    }

    /// Allocate `size` bytes from the pool (page-aligned).
    /// Returns offset + handle for deallocation.
    pub fn alloc(&mut self, size: usize) -> Result<JitPoolAlloc, JitMemError> {
        if !self.initialized { return Err(JitMemError::AllocFailed); }

        let pages_needed = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        if pages_needed == 0 || pages_needed > POOL_PAGES {
            return Err(JitMemError::AllocFailed);
        }

        // Find contiguous free pages (first-fit)
        let first = self.find_free_run(pages_needed)
            .ok_or(JitMemError::PoolFull)?;

        // Mark pages as used
        for p in first..first + pages_needed {
            let word = p / 64;
            let bit = p % 64;
            self.bitmap[word] |= 1u64 << bit;
        }

        Ok(JitPoolAlloc {
            offset: first * PAGE_SIZE,
            size: pages_needed * PAGE_SIZE,
            pages: pages_needed,
            first_page: first,
        })
    }

    /// Free a pool allocation. Pages are zeroed for safety.
    pub fn dealloc(&mut self, alloc: &JitPoolAlloc) {
        // Zero the memory (prevent stale code execution)
        if self.initialized && !self.base.is_null() {
            unsafe {
                let dst = self.base.add(alloc.offset);
                core::ptr::write_bytes(dst, 0xCC, alloc.size); // INT3 fill
            }
        }

        // Clear bitmap
        for p in alloc.first_page..alloc.first_page + alloc.pages {
            let word = p / 64;
            let bit = p % 64;
            self.bitmap[word] &= !(1u64 << bit);
        }
    }

    /// Get a mutable slice into the pool at the given allocation.
    /// Pool must be in writable state.
    pub fn write_slice(&mut self, alloc: &JitPoolAlloc) -> &mut [u8] {
        assert!(!self.executable, "W^X: pool is executable, cannot write");
        unsafe {
            core::slice::from_raw_parts_mut(
                self.base.add(alloc.offset),
                alloc.size,
            )
        }
    }

    /// Flip entire pool RW → RX.
    pub fn make_executable(&mut self) -> Result<(), JitMemError> {
        if !self.initialized { return Err(JitMemError::AllocFailed); }
        mprotect(self.base, POOL_SIZE, PROT_READ | PROT_EXEC)
            .map_err(|_| JitMemError::ProtectFailed)?;
        self.executable = true;
        Ok(())
    }

    /// Flip entire pool RX → RW.
    pub fn make_writable(&mut self) -> Result<(), JitMemError> {
        if !self.initialized { return Err(JitMemError::AllocFailed); }
        mprotect(self.base, POOL_SIZE, PROT_READ | PROT_WRITE)
            .map_err(|_| JitMemError::ProtectFailed)?;
        self.executable = false;
        Ok(())
    }

    /// Execute code at an absolute offset within the pool.
    pub unsafe fn call_u32(&self, offset: usize) -> Result<u32, JitMemError> {
        if !self.executable { return Err(JitMemError::NotExecutable); }
        if offset >= POOL_SIZE { return Err(JitMemError::ProtectFailed); }
        let fn_ptr: unsafe extern "C" fn() -> u32 = core::mem::transmute(
            self.base.add(offset)
        );
        Ok(fn_ptr())
    }

    /// Number of free pages remaining.
    pub fn free_pages(&self) -> usize {
        POOL_PAGES - self.used_pages()
    }

    /// Number of pages in use.
    pub fn used_pages(&self) -> usize {
        self.bitmap.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Find a contiguous run of `n` free pages. Returns first page index.
    fn find_free_run(&self, n: usize) -> Option<usize> {
        let mut run_start = 0;
        let mut run_len = 0;

        for page in 0..POOL_PAGES {
            let word = page / 64;
            let bit = page % 64;
            if self.bitmap[word] & (1u64 << bit) == 0 {
                // Free page
                if run_len == 0 { run_start = page; }
                run_len += 1;
                if run_len >= n { return Some(run_start); }
            } else {
                run_len = 0;
            }
        }
        None
    }
}

impl Drop for JitPool {
    fn drop(&mut self) {
        if self.initialized && !self.base.is_null() {
            if self.executable {
                let _ = mprotect(self.base, POOL_SIZE, PROT_READ | PROT_WRITE);
            }
            let _ = munmap(self.base, POOL_SIZE);
            self.initialized = false;
        }
    }
}
