//! Folkering Shell — binary entry point.
//!
//! Phase C4 reduced this file from 2486 lines to ~50 by extracting state,
//! UI builders, IPC dispatch, input handling and individual commands into
//! the `shell` library crate. This binary is now just:
//!
//! 1. `entry!()` declaration
//! 2. Boot banner
//! 3. IPC poll loop (recv_async → handle_ipc_command → reply)

#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

use libfolk::{entry, println};
use libfolk::sys::ipc::{recv_async, reply_with_token, IpcError};
use libfolk::sys::{get_pid, yield_cpu};

use shell::input::print_prompt;
use shell::ipc::handle_ipc_command;

// ── Bump allocator ──────────────────────────────────────────────────
//
// libfolk grew an alloc-using module (`libfolk::gfx::DisplayListBuilder`)
// in #112, which made every binary that links libfolk require a
// `#[global_allocator]` even if the binary itself never touches it
// directly. The other userspace bins (draug-daemon, draug-streamer,
// inference-server, compositor) all use this same bump pattern; we
// match it here. Heap is 32 KiB — shell allocates very little (one or
// two transient `Vec`s during graph commands at most).
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

entry!(main);

fn main() -> ! {
    let pid = get_pid();
    println!("Folkering Shell v0.1.0 (PID: {})", pid);
    println!("Type 'help' for available commands.\n");
    println!("[SHELL] Running (Task {})", pid);
    print_prompt();

    loop {
        // Process all pending async IPC messages before yielding.
        // The compositor sends commands here (ls, ps, uptime, exec, etc.)
        let mut did_work = false;
        loop {
            match recv_async() {
                Ok(msg) => {
                    did_work = true;
                    let response = handle_ipc_command(msg.payload0);
                    let _ = reply_with_token(msg.token, response, 0);
                }
                Err(IpcError::WouldBlock) => break,
                Err(_) => break,
            }
        }

        if !did_work {
            yield_cpu();
        }
    }
}
