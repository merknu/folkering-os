//! System Call Interface
//!
//! Fast syscall entry using SYSCALL/SYSRET instructions (AMD64).
//!
//! # Module structure
//! - `mod.rs` (this file) — module declarations + public re-exports + `Syscall` enum
//! - `state.rs` — kernel syscall stack, current context pointer, syscall counter
//! - `debug.rs` — DEBUG_* statics + getters + verbose debug print helpers
//! - `entry.rs` — naked syscall_entry / int_syscall_entry asm + their helpers
//! - `init.rs` — init() (configures EFER, MSRs, kernel stack, guard page)
//! - `dispatch.rs` — syscall_handler match-statement (routes by syscall #)
//! - `handlers/` — per-domain handler modules (ipc, memory, task, io, fs,
//!   net, audio, compute, gpu, pci, dma); flattened via `pub use ipc::*` etc.

mod state;
mod debug;
mod entry;
mod init;
mod dispatch;
mod handlers;

// Public API
pub use init::init;
pub use entry::int_syscall_entry;
pub use state::{set_current_context_ptr, get_syscall_count};

// Re-export specific handlers that kernel-side code needs to invoke
// directly (e.g. tcp_shell's `graph-callers` command goes through
// the same proxy-call path as the userspace syscall, not via int 0x80).
// `graph_callers_inner` is the kernel-only path that skips the
// userspace pointer-range validation.
pub use handlers::net::{syscall_graph_callers, graph_callers_inner};
pub use debug::{
    DEBUG_MARKER, DEBUG_CONTEXT_R14, DEBUG_CONTEXT_RSP,
    DEBUG_NEXT_CTX_PTR, DEBUG_NEXT_CTX_CS, DEBUG_NEXT_CTX_RIP,
    SYSCALL_RESULT,
    get_debug_marker, set_debug_marker,
    get_debug_rax, get_debug_context_ptr, get_debug_rip, get_debug_rsp,
    get_debug_rflags, get_debug_return_val, get_debug_context_r14,
    get_debug_context_rsp, get_debug_next_ctx_ptr, get_debug_next_ctx_cs,
    get_debug_next_ctx_rip, get_debug_handler_result, get_debug_rcx,
    verify_task_context, verify_context_canary,
};
pub use handlers::{map_flags, signal_irq};

/// Syscall numbers
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syscall {
    /// Send IPC message
    IpcSend = 0,
    /// Receive IPC message
    IpcReceive = 1,
    /// Reply to IPC message
    IpcReply = 2,
    /// Create shared memory
    ShmemCreate = 3,
    /// Map shared memory
    ShmemMap = 4,
    /// Spawn new task
    Spawn = 5,
    /// Exit current task
    Exit = 6,
    /// Yield CPU
    Yield = 7,
}
