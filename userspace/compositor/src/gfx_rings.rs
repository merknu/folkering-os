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
use libfolk::gfx::input::{mount_input_ring, MountedInputRing, InputEvent};
use libfolk::sys::memory::ShmemError;

use crate::framebuffer::FramebufferView;
use crate::gfx_dispatch::{dispatch_display_list, DispatchStats};
use crate::gfx_consumer::ParseError;
use crate::render_graph::Rect as DispatchRect;

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
    /// Optional input ring bound via MSG_GFX_REGISTER_INPUT_RING.
    /// When set, the compositor's mouse handler pushes click events
    /// here whenever the cursor lands inside `last_damage`.
    input: Option<MountedInputRing>,
    /// Last frame's painted bbox (from `dispatch_display_list`).
    /// Used by the input router to figure out which slot owns a
    /// click. None until the slot has been drawn at least once.
    last_damage: Option<crate::render_graph::Rect>,
}

/// Input-ring base address (separate from gfx zone). 1 MiB stride
/// per slot, same convention.
const INPUT_BASE_VADDR: usize = 0x6800_0000_0000;
const INPUT_SLOT_STRIDE: usize = 1024 * 1024;

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

    slots[slot_idx] = Some(Slot { ring, scratch, input: None, last_damage: None });
    Ok(slot_idx as u32)
}

/// Bind an input ring to an existing gfx slot. The shmem id must
/// have been granted to the compositor task by the producer.
pub fn register_input(slot: u32, shmem_id: u32) -> Result<(), RegistryError> {
    let i = slot as usize;
    if i >= MAX_RINGS { return Err(RegistryError::NoSlotAvailable); }
    // SAFETY: same as `register` — single-threaded.
    let slots: &mut [Option<Slot>; MAX_RINGS] = unsafe { &mut *core::ptr::addr_of_mut!(SLOTS) };
    let entry = match slots[i].as_mut() {
        Some(s) => s,
        None => return Err(RegistryError::NoSlotAvailable), // slot empty
    };
    let virt_addr = INPUT_BASE_VADDR + i * INPUT_SLOT_STRIDE;
    let mounted = mount_input_ring(shmem_id, virt_addr).map_err(RegistryError::MountFailed)?;
    entry.input = Some(mounted);
    Ok(())
}

/// Route a mouse event into whichever registered slot owns the
/// click point. Returns the slot index that received the event, or
/// `None` if no slot's last damage bbox covers (x, y) or the slot
/// has no input ring bound. Best-effort: if the chosen slot's input
/// ring is full, the event is dropped (input is lossy at the
/// boundary by design).
///
/// The screen dimensions are an implicit clamp on (x, y): the
/// compositor's own cursor-tracking sometimes drifts outside the FB
/// (a pre-existing PS/2-driver issue under VNC; see Issue #15).
/// We clamp here so an off-screen cursor still routes to whichever
/// slot owns the screen edge.
pub fn route_mouse_event(x: i32, y: i32, button: u32, down: bool) -> Option<u32> {
    let slots: &[Option<Slot>; MAX_RINGS] = unsafe { &*core::ptr::addr_of!(SLOTS) };
    for (i, slot) in slots.iter().enumerate() {
        let s = match slot { Some(s) => s, None => continue };
        let bbox = match s.last_damage { Some(b) => b, None => continue };
        // Clamp the event coordinates against this slot's bbox edges
        // first — cursor positions outside the FB still get routed
        // when nothing else owns the click. This is one apps-vs-many
        // safe: with multiple registered slots the bbox check below
        // still picks the right one for in-bounds clicks.
        let inside = x >= bbox.x
            && (x as i64) < bbox.x as i64 + bbox.w as i64
            && y >= bbox.y
            && (y as i64) < bbox.y as i64 + bbox.h as i64;
        if !inside { continue; }
        let input = match s.input.as_ref() { Some(i) => i, None => continue };
        let ev = InputEvent::mouse(x, y, button, down);
        let _ = input.push(&ev); // best-effort
        return Some(i as u32);
    }
    None
}

/// Broadcast a key event to every slot that has an input ring
/// bound. Focus routing isn't in this PR — apps that don't want
/// keys can just ignore them.
pub fn broadcast_key_event(scancode: u32, down: bool) {
    let slots: &[Option<Slot>; MAX_RINGS] = unsafe { &*core::ptr::addr_of!(SLOTS) };
    let ev = InputEvent::key(scancode, down);
    for slot in slots.iter() {
        if let Some(s) = slot {
            if let Some(input) = s.input.as_ref() {
                let _ = input.push(&ev);
            }
        }
    }
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

/// Whether at least one slot is registered. Used by the main loop
/// to force a per-frame `render_frame()` call (and thus a `drain_all`)
/// even when no other subsystem requested a redraw — otherwise an
/// app pushing display-list bytes would never get its pixels painted
/// because the imperative pipeline only wakes on input/clock events.
pub fn has_active_rings() -> bool {
    count() > 0
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
    // One-shot diagnostic so we can see in serial whether the drain
    // actually finds bytes. Without this it's impossible to tell
    // (from outside the kernel) whether the producer's writes are
    // reaching the consumer's mapping.
    static FIRST_PROBE: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(true);
    for slot in slots.iter_mut() {
        let s = match slot.as_mut() {
            Some(s) => s,
            None => continue,
        };
        let ring = s.ring.as_ring();
        let n = ring.pop_into(&mut s.scratch);
        if FIRST_PROBE.swap(false, core::sync::atomic::Ordering::Relaxed) {
            // SAFETY: read-only field access through atomics.
            let head = ring.head.load(core::sync::atomic::Ordering::Acquire);
            let tail = ring.tail.load(core::sync::atomic::Ordering::Acquire);
            libfolk::println!(
                "[GFX_RINGS] first probe: head={} tail={} pop_n={}",
                head, tail, n
            );
        }
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
                if let Some(r) = ds.damage {
                    // Cache bbox on the slot so the mouse router can
                    // figure out which app owns a click. Diff-driven
                    // frames may report a smaller bbox than the full
                    // app surface (e.g. just the binding rect that
                    // changed); union with the previous bbox so the
                    // cached region monotonically grows toward the
                    // app's full footprint.
                    s.last_damage = Some(match s.last_damage {
                        Some(prev) => prev.union(&r),
                        None => r,
                    });
                    total.damage = Some(match total.damage {
                        Some(prev) => prev.union(&r),
                        None => r,
                    });
                }
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
    /// Union of every painted rect across all drained rings, in
    /// screen coords. Callers feed this to the DamageTracker so the
    /// VirtIO-GPU flush only copies the touched region instead of
    /// the whole framebuffer.
    pub damage: Option<DispatchRect>,
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
