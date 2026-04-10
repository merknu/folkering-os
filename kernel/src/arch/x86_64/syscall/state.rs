//! Per-CPU syscall state: kernel stack, current context pointer, syscall counter.

use core::sync::atomic::AtomicUsize;

/// Per-CPU context pointer (single-core for now)
/// This is set during context switch to avoid mutex locking in syscall path
pub(super) static CURRENT_CONTEXT_PTR: AtomicUsize = AtomicUsize::new(0);

/// Kernel syscall stack (16KB) - Must be 16-byte aligned for x86-64 ABI!
/// Used for handling syscalls - SYSCALL doesn't switch stacks automatically
#[repr(C, align(16))]
pub(super) struct AlignedStack(pub(super) [u8; 16384]);

#[no_mangle]
#[link_section = ".bss"]
pub(super) static mut SYSCALL_STACK: AlignedStack = AlignedStack([0; 16384]);

/// Syscall counter for debugging
#[no_mangle]
pub(super) static SYSCALL_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Set the current context pointer (called during task switch)
pub fn set_current_context_ptr(ptr: *mut crate::task::task::Context) {
    CURRENT_CONTEXT_PTR.store(ptr as usize, core::sync::atomic::Ordering::Release);
}

/// Get the current syscall count
pub fn get_syscall_count() -> u64 {
    SYSCALL_COUNT.load(core::sync::atomic::Ordering::Relaxed)
}
