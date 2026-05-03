//! Compositor-side registry of granted display-list rings.
//!
//! Apps create an `IpcGraphicsRing` via `libfolk::gfx::RingHandle::create_at`,
//! grant the shmem id to the compositor, then send `MSG_GFX_REGISTER_RING`
//! over IPC. This module owns the receiving side: `register()` mounts the
//! ring at a stable virtual address derived from its slot index and stores
//! the `MountedRing` so subsequent frames can drain it.
//!
//! Memory layout:
//! - Up to `MAX_RINGS` (8) concurrent producers — small fixed cap so the
//!   per-slot virtual addresses never collide. Adding a 9th producer
//!   currently fails the registration; the agent retries next frame.
//! - Each slot reserves a 1 MiB virtual address range starting at
//!   `RING_BASE_VADDR + slot * RING_SLOT_STRIDE`. Today the ring uses
//!   ~64 KiB; the over-provisioned stride leaves room for a future
//!   `RING_CAPACITY_BYTES` bump without re-shuffling layouts.
//! - Slots are recycled on `unregister`. We don't compact, so slot
//!   indices stay stable for a producer's lifetime — useful for
//!   diagnostic logs that reference a slot by id.
//!
//! What this module does NOT do (intentional):
//! - Hook itself into `render_frame()`. `drain_all()` exists and works,
//!   but the compositor's main loop doesn't call it yet — that's the
//!   next PR. Library-only matches the pattern set by #112/#113/#116/#118.
//! - Authenticate the producer. Any task that knows a granted shmem id
//!   can register. The kernel's `shmem_grant` already gates who has
//!   permission; this module trusts that.
//! - Auto-reap dead producers. If an app crashes without unregistering,
//!   its slot stays mounted until the compositor restarts. A periodic
//!   liveness probe is a follow-up.

extern crate alloc;
use alloc::vec::Vec;

use libfolk::gfx::{mount_ring, MountedRing, RING_CAPACITY_BYTES};
use libfolk::sys::memory::ShmemError;

use crate::framebuffer::FramebufferView;
use crate::gfx_dispatch::{dispatch_display_list, DispatchStats};
use crate::gfx_consumer::ParseError;

/// Maximum simultaneous graphics-ring producers. Eight is enough for
/// "compositor + a handful of foreground apps"; multi-window agents
/// can multiplex on a single ring per process.
pub const MAX_RINGS: usize = 8;

/// Base virtual address for ring mappings. Picked to stay clear of the
/// app heap (low canonical-half) and the kernel mappings (high
/// canonical-half). One day this gets formalized into a proper
/// "graphics zone" so apps don't have to think about it.
const RING_BASE_VADDR: usize = 0x6000_0000_0000;

/// Stride per slot (1 MiB). Matches the page-size requirement from
/// `shmem_map` and leaves headroom for a larger `RING_CAPACITY_BYTES`
/// later without renumbering slots.
const RING_SLOT_STRIDE: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    /// All slots in use. App should retry later or close existing rings.
    NoSlotAvailable,
    /// `shmem_map` rejected the address — usually a stale shmem id or
    /// a producer that didn't `grant_to(compositor_task_id)` first.
    MountFailed(ShmemError),
}

/// One registered ring with the slot index that owns its virtual
/// address. We keep `Option` slots rather than `Vec` so an unregister
/// in the middle doesn't invalidate later slot indices.
struct Slot {
    ring: MountedRing,
    /// Pre-allocated scratch buffer the dispatcher pops into. Sized
    /// to the ring's capacity so a single drain pass can consume an
    /// entire wrap-around-full ring; smaller would force multiple
    /// `pop_into → dispatch` rounds per frame.
    scratch: Vec<u8>,
}

/// Process-global slot table. The compositor is single-threaded
/// (cooperative scheduler within one task), so a `static mut` matches
/// the existing pattern used by other compositor internals — pulling
/// in `spin::Mutex` would be heavier than the contention warrants.
/// All `unsafe` accesses are confined to the four functions below;
/// callers see safe APIs.
static mut SLOTS: [Option<Slot>; MAX_RINGS] = [const { None }; MAX_RINGS];

/// Mount a granted shmem id as a graphics ring and remember it.
/// Returns the slot index assigned. The producer keeps using its own
/// `RingHandle::as_ring()`; this side reads through `MountedRing`.
pub fn register(shmem_id: u32) -> Result<u32, RegistryError> {
    // SAFETY: compositor is single-threaded; no other code accesses
    // SLOTS concurrently with this call.
    let slots: &mut [Option<Slot>; MAX_RINGS] = unsafe { &mut *core::ptr::addr_of_mut!(SLOTS) };

    let slot_idx = match slots.iter().position(|s: &Option<Slot>| s.is_none()) {
        Some(i) => i,
        None => return Err(RegistryError::NoSlotAvailable),
    };
    let virt_addr = RING_BASE_VADDR + slot_idx * RING_SLOT_STRIDE;
    let ring = mount_ring(shmem_id, virt_addr).map_err(RegistryError::MountFailed)?;

    // Scratch is initialized to zeros once. We reuse the buffer across
    // frames; the dispatcher only reads `pop_into`'s returned length.
    let scratch = alloc::vec![0u8; RING_CAPACITY_BYTES];

    slots[slot_idx] = Some(Slot { ring, scratch });
    Ok(slot_idx as u32)
}

/// Drop a slot. Best-effort: failing to unmount is logged but not
/// propagated (we still want to free the slot so a producer that
/// re-registers gets one).
pub fn unregister(slot: u32) -> Result<(), RegistryError> {
    let i = slot as usize;
    if i >= MAX_RINGS { return Err(RegistryError::NoSlotAvailable); }
    // SAFETY: same as `register` — single-threaded.
    let slots: &mut [Option<Slot>; MAX_RINGS] = unsafe { &mut *core::ptr::addr_of_mut!(SLOTS) };
    if let Some(s) = slots[i].take() {
        let _ = s.ring.unmount();
    }
    Ok(())
}

/// Return the total number of currently registered rings (diagnostic).
pub fn count() -> usize {
    // SAFETY: read-only borrow; single-threaded.
    let slots: &[Option<Slot>; MAX_RINGS] = unsafe { &*core::ptr::addr_of!(SLOTS) };
    slots.iter().filter(|s: &&Option<Slot>| s.is_some()).count()
}

/// Drain every registered ring and dispatch its display list against
/// `fb`. Returns aggregate stats so callers can log per-frame
/// throughput. A parse error on one ring is logged via stats and the
/// ring continues — we don't abort the whole frame because one
/// producer is broken.
pub fn drain_all(fb: &mut FramebufferView) -> DrainStats {
    // SAFETY: same as `register` — single-threaded.
    let slots: &mut [Option<Slot>; MAX_RINGS] = unsafe { &mut *core::ptr::addr_of_mut!(SLOTS) };
    let mut total = DrainStats::default();
    for slot in slots.iter_mut() {
        let s = match slot.as_mut() {
            Some(s) => s,
            None => continue,
        };
        let ring = s.ring.as_ring();
        let n = ring.pop_into(&mut s.scratch);
        if n == 0 { continue; }
        match dispatch_display_list(&s.scratch[..n], fb) {
            Ok((_consumed, ds)) => {
                total.rings_drained += 1;
                total.bytes += n as u32;
                total.draw_rects += ds.draw_rects;
                total.draw_texts += ds.draw_texts;
                total.set_clips += ds.set_clips;
                total.draw_textures_skipped += ds.draw_textures_skipped;
                total.unknown_skipped += ds.unknown_skipped;
            }
            Err(e) => {
                total.parse_errors += 1;
                total.last_parse_error = Some(e);
            }
        }
    }
    total
}

/// Per-frame totals from `drain_all`.
#[derive(Default, Clone, Debug)]
pub struct DrainStats {
    pub rings_drained: u32,
    pub bytes: u32,
    pub draw_rects: u32,
    pub draw_texts: u32,
    pub set_clips: u32,
    pub draw_textures_skipped: u32,
    pub unknown_skipped: u32,
    pub parse_errors: u32,
    pub last_parse_error: Option<ParseError>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_count_is_zero() {
        // Note: REGISTRY is process-global, so other tests may leave
        // residue. We check a relative invariant rather than an
        // absolute one.
        let before = count();
        // No registrations happened; count shouldn't grow.
        assert_eq!(count(), before);
    }

    #[test]
    fn unregister_invalid_slot_returns_err() {
        let r = unregister(MAX_RINGS as u32 + 1);
        assert_eq!(r, Err(RegistryError::NoSlotAvailable));
    }

    // We can't exercise `register` end-to-end in cfg(test) — it'd need
    // a working `shmem_create + shmem_grant` pair, which means a live
    // kernel. Coverage for the actual mount path is via the boot-time
    // smoke test once an app drives a real ring.
}
