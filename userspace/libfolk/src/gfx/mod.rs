//! Zero-copy graphics IPC: SPSC ring + display-list opcodes.
//!
//! This is the producer side of the rapport's Del 1 design — apps build a
//! display list of `[CommandHeader | payload]` records and shove it into a
//! shared-memory ring. The compositor is the consumer; it lives in
//! `userspace/compositor/src/gfx_consumer.rs`.
//!
//! The ring is `Single-Producer Single-Consumer`. There's exactly one
//! producer (the app) and one consumer (the compositor) per ring; serializing
//! across producers is *not* supported and would require a different design.
//!
//! Memory ordering is the standard SPSC pattern: producer reads its own
//! `head` Relaxed, reads `tail` Acquire, writes payload bytes, stores `head`
//! Release. The consumer does the mirror image. No locks anywhere.

pub mod ring;
pub mod display_list;
pub mod shmem;

pub use ring::{IpcGraphicsRing, RING_CAPACITY_BYTES, PushError};
pub use display_list::{
    CommandOpCode, CommandHeader,
    DrawRectCmd, DrawTextureCmd, SetClipRectCmd,
    DisplayListBuilder,
};
pub use shmem::{RingHandle, MountedRing, mount_ring};
