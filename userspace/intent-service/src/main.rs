//! Intent Service - Semantic Command Router for Folkering OS
//!
//! Routes commands from the compositor (omnibar) to the appropriate handler
//! task. Uses CallerToken-based IPC to support the proxy pattern:
//!
//!   Compositor → Intent Service → Shell → reply chain back
//!
//! The key insight: `recv_async()` returns a CallerToken that survives
//! intermediate `SYS_IPC_SEND` calls. After forwarding to the handler
//! and getting the result, we use `reply_with_token()` to unblock
//! the original caller.

#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

use libfolk::{entry, println};
use libfolk::sys::{yield_cpu, get_pid};

// ── Bump allocator ──────────────────────────────────────────────────
//
// Same rationale as `shell/src/main.rs`: libfolk pulled in alloc via
// `gfx::DisplayListBuilder` in #112, so every linked binary must
// supply a `#[global_allocator]`. Intent-service barely allocates
// (one or two transient `Vec`s during query forwarding), so 32 KiB
// is plenty.
const HEAP_SIZE: usize = 32 * 1024;

struct BumpAllocator {
    heap: UnsafeCell<[u8; HEAP_SIZE]>,
    offset: UnsafeCell<usize>,
}

unsafe impl Sync for BumpAllocator {}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let offset = &mut *self.offset.get();
        let align = layout.align();
        let aligned = (*offset + align - 1) & !(align - 1);
        let new_offset = aligned + layout.size();
        if new_offset > HEAP_SIZE {
            core::ptr::null_mut()
        } else {
            *offset = new_offset;
            (*self.heap.get()).as_mut_ptr().add(aligned)
        }
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    heap: UnsafeCell::new([0; HEAP_SIZE]),
    offset: UnsafeCell::new(0),
};
use libfolk::sys::ipc::{recv_async, reply_with_token, CallerToken, AsyncIpcMessage};
use libfolk::sys::intent::{
    INTENT_OP_REGISTER, INTENT_OP_SUBMIT, INTENT_OP_QUERY,
    INTENT_STATUS_NO_HANDLER, INTENT_STATUS_INVALID,
    CAP_FILE_OPS, CAP_PROCESS_OPS, CAP_SYSTEM_OPS, CAP_EXEC_OPS, CAP_SEARCH_OPS,
};
use libfolk::sys::shell::{
    SHELL_TASK_ID,
    SHELL_OP_LIST_FILES, SHELL_OP_CAT_FILE, SHELL_OP_SEARCH,
    SHELL_OP_PS, SHELL_OP_UPTIME, SHELL_OP_EXEC,
};
use libfolk::syscall::{syscall3, SYS_IPC_SEND};

entry!(main);

// ============================================================================
// Routing Table
// ============================================================================

const MAX_HANDLERS: usize = 8;

#[derive(Clone, Copy)]
struct HandlerEntry {
    task_id: u32,
    capability_flags: u64,
    active: bool,
}

impl HandlerEntry {
    const fn empty() -> Self {
        Self { task_id: 0, capability_flags: 0, active: false }
    }
}

static mut HANDLERS: [HandlerEntry; MAX_HANDLERS] = [HandlerEntry::empty(); MAX_HANDLERS];
static mut HANDLER_COUNT: usize = 0;
static mut ROUTES_TOTAL: u64 = 0;

// ============================================================================
// Opcode → Capability Mapping
// ============================================================================

fn opcode_to_capability(opcode: u64) -> u64 {
    match opcode {
        SHELL_OP_LIST_FILES => CAP_FILE_OPS,
        SHELL_OP_CAT_FILE => CAP_FILE_OPS,
        SHELL_OP_SEARCH => CAP_SEARCH_OPS,
        SHELL_OP_PS => CAP_PROCESS_OPS,
        SHELL_OP_UPTIME => CAP_SYSTEM_OPS,
        SHELL_OP_EXEC => CAP_EXEC_OPS,
        _ => CAP_EXEC_OPS,
    }
}

fn find_handler(capability: u64) -> Option<u32> {
    unsafe {
        for i in 0..HANDLER_COUNT {
            if HANDLERS[i].active && (HANDLERS[i].capability_flags & capability) != 0 {
                return Some(HANDLERS[i].task_id);
            }
        }
    }
    None
}

// ============================================================================
// Request Handlers (all use CallerToken for reply)
// ============================================================================

fn handle_register(msg: AsyncIpcMessage) {
    let capability_flags = msg.payload0 >> 16;
    let task_id = msg.sender;

    unsafe {
        // Check if already registered (update)
        for i in 0..HANDLER_COUNT {
            if HANDLERS[i].task_id == task_id {
                HANDLERS[i].capability_flags = capability_flags;
                HANDLERS[i].active = true;
                println!("[INTENT] Updated task {} caps=0x{:x}", task_id, capability_flags);
                let _ = reply_with_token(msg.token, 0, 0);
                return;
            }
        }

        // New registration
        if HANDLER_COUNT < MAX_HANDLERS {
            HANDLERS[HANDLER_COUNT] = HandlerEntry {
                task_id,
                capability_flags,
                active: true,
            };
            HANDLER_COUNT += 1;
            println!("[INTENT] Registered task {} caps=0x{:x} ({})", task_id, capability_flags, HANDLER_COUNT);
            let _ = reply_with_token(msg.token, 0, 0);
        } else {
            println!("[INTENT] Registry full, rejecting task {}", task_id);
            let _ = reply_with_token(msg.token, INTENT_STATUS_INVALID, 0);
        }
    }
}

/// Handle SUBMIT request (proxy mode with CallerToken)
///
/// 1. Save CallerToken from compositor
/// 2. Forward to handler via SYS_IPC_SEND (blocks until handler replies)
/// 3. Reply to compositor using saved CallerToken
fn handle_submit(msg: AsyncIpcMessage) {
    let shell_opcode = (msg.payload0 >> 8) & 0xFF;
    let arg_data = msg.payload0 >> 16;

    unsafe { ROUTES_TOTAL += 1; }

    // Reconstruct the original shell request
    let original_request = shell_opcode | (arg_data << 8);

    // Find handler based on opcode
    let required_cap = opcode_to_capability(shell_opcode);
    let handler_task_id = match find_handler(required_cap) {
        Some(id) => id,
        None => SHELL_TASK_ID, // fallback
    };

    // Forward to handler (this blocks until handler replies)
    let result = unsafe {
        syscall3(SYS_IPC_SEND, handler_task_id as u64, original_request, 0)
    };

    // Reply to original caller using saved CallerToken
    let _ = reply_with_token(msg.token, result, 0);
}

fn handle_query(msg: AsyncIpcMessage) {
    let handler_task_id = match find_handler(CAP_EXEC_OPS) {
        Some(id) => id,
        None => {
            let _ = reply_with_token(msg.token, INTENT_STATUS_NO_HANDLER, 0);
            return;
        }
    };

    let response = (handler_task_id as u64) | (100u64 << 32);
    let _ = reply_with_token(msg.token, response, 0);
}

// ============================================================================
// Main
// ============================================================================

fn main() -> ! {
    let pid = get_pid();
    println!("[INTENT] Intent Service starting (PID: {})", pid);

    // Auto-register Shell as default handler
    unsafe {
        HANDLERS[0] = HandlerEntry {
            task_id: SHELL_TASK_ID,
            capability_flags: CAP_FILE_OPS | CAP_PROCESS_OPS | CAP_SYSTEM_OPS
                            | CAP_EXEC_OPS | CAP_SEARCH_OPS,
            active: true,
        };
        HANDLER_COUNT = 1;
    }
    println!("[INTENT] Shell (task {}) auto-registered as default handler", SHELL_TASK_ID);
    println!("[INTENT] Entering service loop...\n");

    // Main service loop — uses recv_async for CallerToken support
    loop {
        match recv_async() {
            Ok(msg) => {
                let op = msg.payload0 & 0xFF;
                match op {
                    op if op == INTENT_OP_REGISTER => handle_register(msg),
                    op if op == INTENT_OP_SUBMIT => handle_submit(msg),
                    op if op == INTENT_OP_QUERY => handle_query(msg),
                    _ => {
                        let _ = reply_with_token(msg.token, INTENT_STATUS_INVALID, 0);
                    }
                }
            }
            Err(_) => {
                yield_cpu();
            }
        }
    }
}
