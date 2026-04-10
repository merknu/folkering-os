//! Synapse — the Folkering OS Data Kernel.
//!
//! Synapse runs as Task 2 and provides a unified IPC interface for file
//! access, queries, and (eventually) AI-powered semantic search. It owns
//! the on-disk SQLite database and the directory cache.
//!
//! Phase B2 split this previously-monolithic 2709-line file into focused
//! modules. The two flagship improvements:
//!
//! 1. **`SafeSqliteBuffer`** — wraps the 4 MB database buffer with safe
//!    accessors that bounds-check every read/write and automatically mark
//!    dirty pages. Replaces 80+ scattered `unsafe { SQLITE_STATE.data… }`
//!    blocks.
//! 2. **`ShmemArena`** — RAII handle for shared-memory mappings; calls
//!    `shmem_unmap` automatically on drop. Eliminates the leak/double-map
//!    bugs that plagued the six hardcoded VADDRs.
//!
//! # Module overview
//!
//! - `state` — `SafeSqliteBuffer`, `DirCacheState`, `Backend` enum, consts
//! - `shmem` — `ShmemArena` RAII wrapper
//! - `cache` — refresh / count / update of the directory cache
//! - `mime` — auto-detect MIME from extension + magic bytes
//! - `sqlite_io` — load (ramdisk + VirtIO disk) + flush + BLOB reads
//! - `btree` — B-tree cell insertion (`sqlite_insert_file`, `_intent`,
//!   varint encoding helpers)
//! - `handlers` — IPC dispatcher + per-opcode handlers

#![no_std]

extern crate alloc;

pub mod state;
pub mod shmem;
pub mod cache;
pub mod mime;
pub mod sqlite_io;
pub mod btree;
pub mod handlers;
