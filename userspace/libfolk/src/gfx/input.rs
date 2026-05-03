//! Input events: compositor → app via a per-app shmem ring.
//!
//! Apps that want input (clicks, key presses) call:
//!   1. `InputRingHandle::create_at(virt)` — allocates a small shmem
//!      region for events.
//!   2. `handle.grant_to(COMPOSITOR_TASK_ID)` — gives the compositor
//!      permission to push events into it.
//!   3. `register_input_ring(gfx_slot, handle.id)` — tells the
//!      compositor "for the gfx slot you already gave me, also send
//!      input events to this shmem id". `gfx_slot` is whatever the
//!      app got back from `register_gfx_ring`.
//!
//! Each frame, the app polls `handle.pop_event()` until it returns
//! `None`, then handles the events however it likes (hit-test against
//! its own widget tree, mutate state, redraw).
//!
//! The wire format is a simple SPSC byte ring of fixed-size
//! `InputEvent`s. We piggyback on `IpcGraphicsRing` because the
//! producer/consumer discipline is identical — only the schema
//! changes. 4 KiB capacity is plenty: even at 60 events/sec we'd
//! drain on every render frame, so head and tail rarely diverge by
//! more than a couple of records.

extern crate alloc;

use core::mem;

use crate::gfx::ring::IpcGraphicsRing;
use crate::sys::memory::{shmem_create, shmem_destroy, shmem_grant, shmem_map, shmem_unmap, ShmemError};

/// Per-event capacity in bytes. The ring is sized to hold roughly
/// 100 events at the default event size (32 B); apps that need
/// burstier input (drag-and-drop, repeating keys) can grow it later.
pub const INPUT_RING_BYTES: usize = 4 * 1024;

/// Event kind codes. Wire-format integers — adding a new kind means
/// extending this enum and bumping the consumer's match arms.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// Mouse moved/clicked. `button` is 0 for pure motion (no
    /// button transition this tick), 1=left, 2=right, 3=middle.
    /// `down` is 1 on press, 0 on release. Apps that just want
    /// click-detection can ignore motion-only events.
    Mouse  = 1,
    /// Key transition. `key` is the kernel scancode; `down` 1=press,
    /// 0=release. Focus routing isn't in the first cut — every
    /// registered app gets every key event.
    Key    = 2,
}

impl EventKind {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(Self::Mouse),
            2 => Some(Self::Key),
            _ => None,
        }
    }
}

/// On-the-wire event record. `repr(C)` so the producer (compositor,
/// in `compositor::gfx_rings`) and consumer (this crate) agree byte
/// for byte. Total size is exactly 32 bytes — keep it that way; the
/// ring assumes a fixed event size when computing capacity.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InputEvent {
    pub kind: u32,
    /// Mouse: x in screen coords. Key: 0.
    pub x: i32,
    /// Mouse: y. Key: 0.
    pub y: i32,
    /// Mouse: button (0=none, 1=left, ...). Key: 0.
    pub button: u32,
    /// Key: scancode. Mouse: 0.
    pub key: u32,
    /// 1 = pressed, 0 = released.
    pub down: u32,
    /// Pad to 32 bytes for alignment + future fields.
    pub _reserved: u64,
}

const _: () = {
    // Tripwire: ring capacity assumes fixed-size events. Bumping the
    // struct breaks `pop_event` framing.
    if mem::size_of::<InputEvent>() != 32 {
        panic!("InputEvent must be exactly 32 bytes");
    }
};

impl InputEvent {
    pub const fn mouse(x: i32, y: i32, button: u32, down: bool) -> Self {
        Self {
            kind: EventKind::Mouse as u32,
            x, y, button,
            key: 0,
            down: if down { 1 } else { 0 },
            _reserved: 0,
        }
    }

    pub const fn key(scancode: u32, down: bool) -> Self {
        Self {
            kind: EventKind::Key as u32,
            x: 0, y: 0, button: 0,
            key: scancode,
            down: if down { 1 } else { 0 },
            _reserved: 0,
        }
    }

    /// Encode as 32 bytes for the SPSC ring.
    pub fn to_bytes(&self) -> [u8; 32] {
        // SAFETY: `repr(C)`, all fields are `Copy` integers, no padding bits.
        unsafe { core::mem::transmute_copy::<Self, [u8; 32]>(self) }
    }

    /// Decode from 32 bytes. Returns `None` if `bytes.len() != 32`.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 32 { return None; }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(bytes);
        // SAFETY: same bit-pattern reasoning as `to_bytes`.
        Some(unsafe { core::mem::transmute(buf) })
    }
}

/// App-side handle for an input ring. Mirrors `RingHandle` but with
/// a smaller capacity and an event-shaped pop API on top.
pub struct InputRingHandle {
    pub id: u32,
    virt: usize,
}

impl InputRingHandle {
    /// Allocate the shmem region + map it at `virt_addr`.
    pub fn create_at(virt_addr: usize) -> Result<Self, ShmemError> {
        let size = mem::size_of::<IpcGraphicsRing<INPUT_RING_BYTES>>();
        let id = shmem_create(size)?;
        if let Err(e) = shmem_map(id, virt_addr) {
            let _ = shmem_destroy(id);
            return Err(e);
        }
        Ok(Self { id, virt: virt_addr })
    }

    pub fn grant_to(&self, target_task: u32) -> Result<(), ShmemError> {
        shmem_grant(self.id, target_task)
    }

    /// Pop one event off the ring. Returns `None` if empty.
    /// Returns `Some(Err(()))` if the ring contained a partial
    /// record — should never happen because the producer always
    /// pushes 32 bytes at a time, but defensive against a
    /// misbehaving compositor.
    pub fn pop_event(&self) -> Option<InputEvent> {
        let ring = self.as_ring();
        let mut buf = [0u8; 32];
        let n = ring.pop_into(&mut buf);
        if n == 0 { return None; }
        if n != 32 { return None; } // partial record, drop
        InputEvent::from_bytes(&buf)
    }

    /// Borrow the ring directly. Most apps don't need this, but the
    /// compositor's consumer side does.
    pub fn as_ring<'a>(&'a self) -> &'a IpcGraphicsRing<INPUT_RING_BYTES> {
        // SAFETY: same argument as `RingHandle::as_ring` — kernel
        // initialized the region and we own the producer end.
        unsafe { &*(self.virt as *const IpcGraphicsRing<INPUT_RING_BYTES>) }
    }

    /// Tear down the mapping + destroy the region.
    pub fn destroy(self) -> Result<u32, ShmemError> {
        let id = self.id;
        let _ = shmem_unmap(id, self.virt);
        shmem_destroy(id)?;
        Ok(id)
    }
}

/// Compositor-side: mount a granted input shmem id at `virt_addr`.
/// Symmetric to `mount_ring` but typed for input events.
pub fn mount_input_ring(id: u32, virt_addr: usize) -> Result<MountedInputRing, ShmemError> {
    shmem_map(id, virt_addr)?;
    Ok(MountedInputRing { id, virt: virt_addr })
}

pub struct MountedInputRing {
    pub id: u32,
    virt: usize,
}

impl MountedInputRing {
    pub fn as_ring<'a>(&'a self) -> &'a IpcGraphicsRing<INPUT_RING_BYTES> {
        // SAFETY: same as `MountedRing::as_ring`.
        unsafe { &*(self.virt as *const IpcGraphicsRing<INPUT_RING_BYTES>) }
    }

    /// Push an event. Returns `Err(())` if the ring is full —
    /// compositor should drop the event rather than block; input
    /// is fundamentally lossy at the boundary.
    pub fn push(&self, ev: &InputEvent) -> Result<(), ()> {
        let bytes = ev.to_bytes();
        self.as_ring().push(&bytes).map_err(|_| ())
    }

    pub fn unmount(self) -> Result<(), ShmemError> {
        shmem_unmap(self.id, self.virt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_round_trip() {
        let e = InputEvent::mouse(120, 80, 1, true);
        let b = e.to_bytes();
        let d = InputEvent::from_bytes(&b).unwrap();
        let (k, x, y, btn, dwn) = (d.kind, d.x, d.y, d.button, d.down);
        assert_eq!(k, EventKind::Mouse as u32);
        assert_eq!(x, 120);
        assert_eq!(y, 80);
        assert_eq!(btn, 1);
        assert_eq!(dwn, 1);
    }

    #[test]
    fn key_event() {
        let e = InputEvent::key(42, false);
        let d = InputEvent::from_bytes(&e.to_bytes()).unwrap();
        let (k, key, dwn) = (d.kind, d.key, d.down);
        assert_eq!(k, EventKind::Key as u32);
        assert_eq!(key, 42);
        assert_eq!(dwn, 0);
    }
}
