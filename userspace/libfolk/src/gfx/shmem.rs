//! Wire the SPSC display-list ring onto Folkering OS shared memory.
//!
//! The ring itself (`IpcGraphicsRing<RING_CAPACITY_BYTES>`) lives in
//! `gfx::ring` and is purely a memory layout — it doesn't know how the
//! pages got mapped. This module is the bridge: it allocates a shmem
//! region of exactly the right size, maps it at a caller-chosen
//! virtual address, and hands back an `&IpcGraphicsRing` view.
//!
//! Producer (the WASM/userspace app) typically does:
//! ```ignore
//! let h = RingHandle::create_at(0x4000_0000_0000)?;
//! h.grant_to(compositor_task_id)?;
//! // Send the handle's `id` to the compositor over IPC.
//! let r = h.as_ring();
//! r.push(builder.as_slice()).ok();
//! ```
//!
//! Consumer (the compositor) does:
//! ```ignore
//! let r = mount_ring(granted_id, 0x5000_0000_0000)?;
//! r.pop_into(&mut buf);
//! ```
//!
//! Caller-chosen virtual addresses are how `shmem_map` already works —
//! we don't try to abstract them away here. The kernel side will refuse
//! overlap with existing mappings, so callers should pick a region in
//! their reserved range. (Today there's no shared "graphics ring zone"
//! convention; one of the follow-ups is to formalize it so the agent
//! doesn't have to think about addresses.)

extern crate alloc;

use core::mem;

use crate::gfx::ring::{IpcGraphicsRing, RING_CAPACITY_BYTES};
use crate::sys::memory::{shmem_create, shmem_destroy, shmem_grant, shmem_map, shmem_unmap, ShmemError};

/// One byte over the ring's bare capacity to fit the cache-line-padded
/// header. We round up to a page boundary inside `shmem_create`.
const RING_REGION_BYTES: usize = mem::size_of::<IpcGraphicsRing<RING_CAPACITY_BYTES>>();

/// Producer-side handle to a created ring. Carries the kernel shmem id
/// plus the virtual address it was mapped at, so the same struct can
/// be used to grant and to derive `&IpcGraphicsRing`.
pub struct RingHandle {
    pub id: u32,
    virt: usize,
}

impl RingHandle {
    /// Allocate a fresh shmem region sized exactly for the ring layout
    /// and map it at `virt_addr`. The header is *not* explicitly
    /// initialized: the kernel guarantees freshly allocated shmem
    /// pages are zeroed, and `IpcGraphicsRing::new()` is just zero-init
    /// (atomics start at 0, buffer starts at 0). Reading the ring
    /// through `as_ring()` is therefore equivalent to having called
    /// `IpcGraphicsRing::new()` — no `MaybeUninit` dance needed.
    pub fn create_at(virt_addr: usize) -> Result<Self, ShmemError> {
        let id = shmem_create(RING_REGION_BYTES)?;
        if let Err(e) = shmem_map(id, virt_addr) {
            // Best-effort cleanup so a failed map doesn't leak the
            // region — the caller is unlikely to retry, and even if
            // they do, leaks would compound.
            let _ = shmem_destroy(id);
            return Err(e);
        }
        Ok(Self { id, virt: virt_addr })
    }

    /// Grant the consumer task access to the ring. Must be called
    /// before the consumer does `mount_ring`.
    pub fn grant_to(&self, target_task: u32) -> Result<(), ShmemError> {
        shmem_grant(self.id, target_task)
    }

    /// View the mapped region as a ring. Lifetime is tied to `self`,
    /// so the borrow ends when the handle is dropped (and the region
    /// is unmapped) — that's the property we need to keep
    /// producer/consumer cleanly scoped.
    pub fn as_ring<'a>(&'a self) -> &'a IpcGraphicsRing<RING_CAPACITY_BYTES> {
        // SAFETY: `shmem_create` allocated and `shmem_map` mapped at
        // `self.virt` exactly `RING_REGION_BYTES` of contiguous virtual
        // memory. `IpcGraphicsRing` is `repr(C, align(64))` and we
        // requested its `size_of`, so the region holds exactly one
        // valid value. Atomics + `[u8; N]` have no invalid bit
        // patterns, so the cast is sound for shared zero-init memory.
        unsafe { &*(self.virt as *const IpcGraphicsRing<RING_CAPACITY_BYTES>) }
    }

    /// Drop the mapping but leave the region alive (other tasks may
    /// still hold a mapping). Use `destroy()` to actually free.
    pub fn unmap(self) -> Result<(), ShmemError> {
        shmem_unmap(self.id, self.virt)
    }

    /// Tear down both the mapping and the region. Returns the kernel
    /// id back so callers can ignore the result for fire-and-forget
    /// cleanup paths.
    pub fn destroy(self) -> Result<u32, ShmemError> {
        let id = self.id;
        let _ = shmem_unmap(id, self.virt);
        shmem_destroy(id)?;
        Ok(id)
    }
}

/// Consumer-side mount: take a granted shmem id, map it at the given
/// virtual address in our address space, and treat it as the ring.
/// The returned reference borrows from a leaked handle stored inside
/// the wrapping `MountedRing`, so the consumer doesn't have to thread
/// a `RingHandle` through render-loop call sites.
pub fn mount_ring(id: u32, virt_addr: usize) -> Result<MountedRing, ShmemError> {
    shmem_map(id, virt_addr)?;
    Ok(MountedRing { id, virt: virt_addr })
}

/// Long-lived view of a granted ring on the consumer side.
pub struct MountedRing {
    pub id: u32,
    virt: usize,
}

impl MountedRing {
    pub fn as_ring<'a>(&'a self) -> &'a IpcGraphicsRing<RING_CAPACITY_BYTES> {
        // SAFETY: same argument as `RingHandle::as_ring`. The producer
        // initialized the region; we just observe its mutations through
        // the atomic head/tail.
        unsafe { &*(self.virt as *const IpcGraphicsRing<RING_CAPACITY_BYTES>) }
    }

    pub fn unmount(self) -> Result<(), ShmemError> {
        shmem_unmap(self.id, self.virt)
    }
}
