//! JIT Memory Block — W^X (Write XOR Execute) memory management.
//!
//! Lifecycle:
//!   1. `JitMemoryBlock::alloc(size)` → mmap RW pages
//!   2. `block.as_mut_slice()` → write JIT machine code
//!   3. `block.make_executable()` → mprotect RW→RX (code can run)
//!   4. `block.call_void(offset)` → execute as native function
//!   5. `block.make_writable()` → mprotect RX→RW (for updates)
//!   6. `drop(block)` → munmap
//!
//! W^X enforcement: the kernel rejects PROT_WRITE|PROT_EXEC.
//! Memory is NEVER simultaneously writable and executable.

use libfolk::sys::memory::{mmap, munmap, mprotect, PROT_READ, PROT_WRITE, PROT_EXEC, MmapError};

/// A block of memory that can be flipped between writable and executable.
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
}

impl JitMemoryBlock {
    /// Allocate a W^X memory block. Starts as RW (writable, not executable).
    pub fn alloc(size: usize) -> Result<Self, JitMemError> {
        // Round up to page size
        let page_size = 4096;
        let aligned_size = (size + page_size - 1) & !(page_size - 1);

        let ptr = mmap(aligned_size, PROT_READ | PROT_WRITE)
            .map_err(|_| JitMemError::AllocFailed)?;

        Ok(Self {
            ptr,
            size: aligned_size,
            executable: false,
        })
    }

    /// Get a mutable slice for writing machine code.
    /// Only valid when block is in writable state.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        assert!(!self.executable, "cannot write to executable memory (W^X)");
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.size) }
    }

    /// Flip from RW → RX. After this, code can be executed but not written.
    pub fn make_executable(&mut self) -> Result<(), JitMemError> {
        mprotect(self.ptr, self.size, PROT_READ | PROT_EXEC)
            .map_err(|_| JitMemError::ProtectFailed)?;
        self.executable = true;
        Ok(())
    }

    /// Flip from RX → RW. After this, code can be written but not executed.
    pub fn make_writable(&mut self) -> Result<(), JitMemError> {
        mprotect(self.ptr, self.size, PROT_READ | PROT_WRITE)
            .map_err(|_| JitMemError::ProtectFailed)?;
        self.executable = false;
        Ok(())
    }

    /// Execute a void→void function at the given byte offset.
    ///
    /// # Safety
    /// The caller must ensure the code at `offset` is valid x86_64
    /// machine code that ends with a `ret` instruction.
    pub unsafe fn call_void(&self, offset: usize) -> Result<(), JitMemError> {
        if !self.executable {
            return Err(JitMemError::NotExecutable);
        }
        if offset >= self.size {
            return Err(JitMemError::ProtectFailed);
        }

        let fn_ptr: unsafe extern "C" fn() = core::mem::transmute(
            self.ptr.add(offset)
        );
        fn_ptr();
        Ok(())
    }

    /// Base pointer (for calculating offsets into emitted code).
    pub fn base_ptr(&self) -> *const u8 {
        self.ptr as *const u8
    }

    /// Allocated size in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Whether the block is currently executable.
    pub fn is_executable(&self) -> bool {
        self.executable
    }
}

impl Drop for JitMemoryBlock {
    fn drop(&mut self) {
        // If executable, flip back to writable first (some OS require this)
        if self.executable {
            let _ = mprotect(self.ptr, self.size, PROT_READ | PROT_WRITE);
        }
        let _ = munmap(self.ptr, self.size);
    }
}
