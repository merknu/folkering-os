//! Folkering OS Compositor Service
//!
//! This is the main entry point for the compositor service that runs
//! as a userspace task. It receives TreeUpdate messages from applications
//! and maintains the WorldTree for AI agent queries.
//!
//! # IPC Protocol
//!
//! Applications communicate with the compositor using the following messages:
//!
//! - `COMPOSITOR_CREATE_WINDOW` (0x01): Create a new window, returns window_id
//! - `COMPOSITOR_UPDATE` (0x02): Send TreeUpdate via shared memory
//! - `COMPOSITOR_CLOSE` (0x03): Close a window
//! - `COMPOSITOR_QUERY_NAME` (0x10): Find node by name (for AI)
//! - `COMPOSITOR_QUERY_FOCUS` (0x11): Get current focus (for AI)

#![no_std]
#![no_main]

extern crate alloc;

use compositor::Compositor;
use libfolk::sys::ipc::{receive, reply, recv_async, reply_with_token, IpcError};
use libfolk::sys::yield_cpu;
use libfolk::{entry, println};

// ============================================================================
// Simple Bump Allocator for userspace
// ============================================================================

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

/// Simple bump allocator for userspace tasks.
/// Allocates from a fixed-size heap, never deallocates (sufficient for Phase 6).
struct BumpAllocator {
    heap: UnsafeCell<[u8; HEAP_SIZE]>,
    next: UnsafeCell<usize>,
}

const HEAP_SIZE: usize = 64 * 1024; // 64KB heap

unsafe impl Sync for BumpAllocator {}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let next = &mut *self.next.get();
        let heap = &mut *self.heap.get();

        // Align up
        let align = layout.align();
        let aligned_next = (*next + align - 1) & !(align - 1);

        let new_next = aligned_next + layout.size();
        if new_next > HEAP_SIZE {
            core::ptr::null_mut() // Out of memory
        } else {
            *next = new_next;
            heap.as_mut_ptr().add(aligned_next)
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator doesn't deallocate
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    heap: UnsafeCell::new([0; HEAP_SIZE]),
    next: UnsafeCell::new(0),
};

// IPC message types
const MSG_CREATE_WINDOW: u64 = 0x01;
const MSG_UPDATE: u64 = 0x02;
const MSG_CLOSE: u64 = 0x03;
const MSG_QUERY_NAME: u64 = 0x10;
const MSG_QUERY_FOCUS: u64 = 0x11;

entry!(main);

fn main() -> ! {
    println!("[COMPOSITOR] Starting Semantic Mirror compositor service...");

    let mut compositor = Compositor::new();

    println!("[COMPOSITOR] Ready. Waiting for IPC messages...");

    loop {
        // Try async receive first (non-blocking for Reply-Later pattern)
        match recv_async() {
            Ok(msg) => {
                let response = handle_message(&mut compositor, msg.payload0, msg.sender);
                // Reply immediately for now (can be deferred for long queries)
                let _ = reply_with_token(msg.token, response, 0);
            }
            Err(IpcError::WouldBlock) => {
                // No messages, try blocking receive
                match receive() {
                    Ok(msg) => {
                        let response = handle_message(&mut compositor, msg.payload0, msg.sender);
                        let _ = reply(response, 0);
                    }
                    Err(IpcError::WouldBlock) => {
                        // Yield CPU when no work to do
                        yield_cpu();
                    }
                    Err(_) => {
                        // Other error, just continue
                    }
                }
            }
            Err(_) => {
                // Other error, continue
            }
        }
    }
}

/// Handle an incoming IPC message.
///
/// Returns the response payload.
fn handle_message(compositor: &mut Compositor, msg_type: u64, _sender: u32) -> u64 {
    match msg_type {
        MSG_CREATE_WINDOW => {
            let window_id = compositor.create_window();
            println!("[COMPOSITOR] Created window {}", window_id);
            window_id
        }

        MSG_UPDATE => {
            // TODO: Payload would contain window_id and shmem_id
            // For now, just acknowledge
            println!("[COMPOSITOR] Received update (TODO: process shmem)");
            0
        }

        MSG_CLOSE => {
            // TODO: Extract window_id from payload
            println!("[COMPOSITOR] Window close request");
            0
        }

        MSG_QUERY_NAME => {
            // TODO: AI query - find node by name
            // This is where Reply-Later shines for expensive searches
            println!("[COMPOSITOR] Name query (TODO: implement)");
            0
        }

        MSG_QUERY_FOCUS => {
            // Return currently focused element
            match compositor.world.get_focus() {
                Some((window_id, node_id, _node)) => {
                    // Pack window_id and node_id
                    ((window_id as u64) << 32) | (node_id & 0xFFFF_FFFF)
                }
                None => u64::MAX, // No focus
            }
        }

        _ => {
            println!("[COMPOSITOR] Unknown message type: {:#x}", msg_type);
            u64::MAX // Error
        }
    }
}
