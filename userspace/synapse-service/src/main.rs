//! Synapse — The Data Kernel for Folkering OS (binary entry point).
//!
//! Phase B2 reduced this file from 2709 lines to ~150 by extracting the
//! state, B-tree, I/O, and handler logic into the `synapse_service` library
//! crate. This binary is now just:
//!
//! 1. Allocator + global state declarations
//! 2. Boot sequence (load DB, populate cache)
//! 3. IPC loop (recv_async → dispatch)

#![no_std]
#![no_main]

extern crate alloc;

// ── Heap allocator ─────────────────────────────────────────────────────

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

const HEAP_SIZE: usize = 64 * 1024;

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

// ── Imports ────────────────────────────────────────────────────────────

use libfolk::{entry, println};
use libfolk::sys::{yield_cpu, get_pid};
use libfolk::sys::ipc::{recv_async, CallerToken};
use libfolk::sys::synapse::SYNAPSE_VERSION;

use synapse_service::cache::{refresh_fpk_cache, refresh_sqlite_cache};
use synapse_service::handlers::{handle_request, TokenSource};
use synapse_service::sqlite_io::{try_load_sqlite, try_load_sqlite_from_disk};
use synapse_service::state::{Backend, DirCacheState, SafeSqliteBuffer};

entry!(main);

// ── Global state ───────────────────────────────────────────────────────
//
// `SafeSqliteBuffer` is 4 MB and lives in BSS — too large for stack/heap.
// Other modules access it via `&mut SafeSqliteBuffer` parameters from
// `handle_request`, never as a global. The global is only touched here in
// `main()` to set up the borrow chain.

const DB_FILENAME: &str = "files.db";

#[repr(C, align(4096))]
struct SqliteCell(SafeSqliteBuffer);
unsafe impl Sync for SqliteCell {}

static mut SQLITE_BUFFER: SqliteCell = SqliteCell(SafeSqliteBuffer::new());
static mut DIR_CACHE: DirCacheState = DirCacheState::new();
static mut BACKEND: Backend = Backend::Fpk;
static mut CURRENT_TOKEN: Option<CallerToken> = None;

/// `TokenSource` impl for the global `CURRENT_TOKEN` static.
struct GlobalToken;
impl TokenSource for GlobalToken {
    fn take_token(&mut self) -> Option<CallerToken> {
        unsafe { CURRENT_TOKEN.take() }
    }
}

fn main() -> ! {
    let pid = get_pid();
    println!("[SYNAPSE] Data Kernel starting (PID: {})", pid);
    println!("[SYNAPSE] Protocol version: {}.{}",
             (SYNAPSE_VERSION >> 16) as u16,
             (SYNAPSE_VERSION & 0xFFFF) as u16);

    // Phase 1: Load DB (VirtIO disk preferred, then ramdisk, then FPK fallback)
    let (sqlite, cache, backend) = unsafe {
        let sqlite = &mut SQLITE_BUFFER.0;
        let cache = &mut DIR_CACHE;

        let chosen_backend = if try_load_sqlite_from_disk(sqlite) {
            println!("[SYNAPSE] SQLite loaded from VirtIO disk (persistent!)");
            refresh_sqlite_cache(sqlite, cache);
            println!("[SYNAPSE] Ready - {} ({} files, VirtIO)", DB_FILENAME, cache.count);
            Backend::Sqlite
        } else if try_load_sqlite(sqlite) {
            println!("[SYNAPSE] SQLite loaded from ramdisk (volatile)");
            refresh_sqlite_cache(sqlite, cache);
            println!("[SYNAPSE] Ready - {} ({} files, ramdisk)", DB_FILENAME, cache.count);
            Backend::Sqlite
        } else {
            println!("[SYNAPSE] SQLite not found, using FPK backend");
            refresh_fpk_cache(cache);
            println!("[SYNAPSE] Ready - {} files indexed (FPK)", cache.count);
            Backend::Fpk
        };

        BACKEND = chosen_backend;
        (sqlite, cache, chosen_backend)
    };

    println!("[SYNAPSE] Entering service loop...\n");

    // Phase 2: IPC service loop
    let mut tok = GlobalToken;
    loop {
        match recv_async() {
            Ok(msg) => {
                unsafe { CURRENT_TOKEN = Some(msg.token); }
                handle_request(msg, &mut tok, sqlite, cache, backend);
            }
            Err(_) => {
                yield_cpu();
            }
        }
    }
}
