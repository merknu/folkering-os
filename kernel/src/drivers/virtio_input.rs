//! VirtIO Input driver — absolute pointer (tablet) support.
//!
//! Replaces the relative-deltas-via-PS/2 path for VNC clients. QEMU's
//! `-device virtio-tablet-pci` exposes a single virtio-input device that
//! delivers absolute coordinates (range 0..0x7FFF on each axis) plus
//! mouse-button keycodes, in the standard Linux input event format
//! (`{type, code, value}` triples terminated by SYN_REPORT).
//!
//! Why we want this: the PS/2 mouse path on VNC requires QEMU to convert
//! VNC absolute coords → PS/2 relative deltas, which are 8-bit signed
//! and clamp at ±127 per packet. Big jumps lose precision and the
//! cursor drifts over time (the calculator-demo session burned an hour
//! working around exactly this). Absolute events go through
//! pixel-perfect.
//!
//! Architecture:
//! - One eventq (queue 0). Pre-fill with N device-write descriptors
//!   pointing at 8-byte landing slots.
//! - `pump_events()` drains the used ring, parses each event, updates
//!   internal state. On SYN_REPORT we snapshot the current
//!   `(x, y, buttons)` triple into a small ring of frames, ready for
//!   the read syscall.
//! - The read syscall (`SYS_READ_MOUSE_ABS = 0x69`) calls
//!   `pump_events()` first, then pops the latest frame. The compositor
//!   consumes that and `set`s its cursor instead of accumulating
//!   relative deltas.
//!
//! Out of scope for this PR:
//! - Status queue (host→guest force-feedback, LED control).
//! - Multi-instance (more than one virtio-input device on the same bus).
//!   We grab the first one we find; tablet/mouse/kbd all share PCI id
//!   0x1052 and we don't disambiguate yet.
//! - Coordinate-range autodetection. We hardcode 0..0x7FFF which is
//!   what `virtio-tablet-pci` exposes by default; if a future device
//!   advertises a different `abs_max` we'll mis-scale until the
//!   `CFG_ABS_INFO` query is wired up.

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use crate::drivers::pci::{self, BarType, PciDevice};
use crate::drivers::virtio::{Virtqueue, VRING_DESC_F_WRITE};
use crate::memory::physical;

// ── VirtIO Modern Common Config Register Offsets ──────────────────────

const VIRTIO_PCI_COMMON_DFSELECT: usize = 0x00;
const VIRTIO_PCI_COMMON_DF: usize = 0x04;
const VIRTIO_PCI_COMMON_GFSELECT: usize = 0x08;
const VIRTIO_PCI_COMMON_GF: usize = 0x0C;
const VIRTIO_PCI_COMMON_STATUS: usize = 0x14;
const VIRTIO_PCI_COMMON_Q_SELECT: usize = 0x16;
const VIRTIO_PCI_COMMON_Q_SIZE: usize = 0x18;
const VIRTIO_PCI_COMMON_Q_ENABLE: usize = 0x1C;
const VIRTIO_PCI_COMMON_Q_NOFF: usize = 0x1E;
const VIRTIO_PCI_COMMON_Q_DESCLO: usize = 0x20;
const VIRTIO_PCI_COMMON_Q_DESCHI: usize = 0x24;
const VIRTIO_PCI_COMMON_Q_AVAILLO: usize = 0x28;
const VIRTIO_PCI_COMMON_Q_AVAILHI: usize = 0x2C;
const VIRTIO_PCI_COMMON_Q_USEDLO: usize = 0x30;
const VIRTIO_PCI_COMMON_Q_USEDHI: usize = 0x34;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 128;

// ── Linux input event constants we care about ─────────────────────────

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
#[allow(dead_code)]
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0x00;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

/// Coordinate range advertised by QEMU's `virtio-tablet-pci`. Both
/// axes report 0..ABS_MAX. We scale to FB pixels in
/// `read_abs_event_scaled`, so the userspace caller never sees raw
/// device coords.
const ABS_MAX: u32 = 0x7FFF;

// ── Wire format ────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct VirtioInputEvent {
    ev_type: u16,
    code: u16,
    value: u32,
}

const EVENT_SIZE: usize = core::mem::size_of::<VirtioInputEvent>(); // 8

// ── Driver state ───────────────────────────────────────────────────────

/// One captured pointer state at SYN_REPORT time. `x`/`y` are absolute
/// device coords (0..ABS_MAX); the read path scales to FB pixels.
#[derive(Clone, Copy, Default, Debug)]
pub struct AbsPointerFrame {
    pub abs_x: u32,
    pub abs_y: u32,
    /// Bitmask: bit 0 = left, bit 1 = right, bit 2 = middle.
    pub buttons: u8,
}

/// Tiny ring of completed frames. The compositor polls at ~60Hz; a
/// burst of host events between polls collapses into the latest few
/// frames — we only need to keep enough so the *most recent* state
/// always lands. Eight is plenty.
const FRAME_RING: usize = 8;

struct InputState {
    transport: MmioTransport,
    eventq: Virtqueue,
    eventq_notify_off: u16,
    /// Phys page used as the contiguous landing region for the eventq's
    /// receive descriptors. Layout: `EVENT_SIZE * queue_size` events
    /// packed into one (or two) 4 KiB pages. We keep it for the lifetime
    /// of the driver — never freed, never resized.
    event_buf_phys: usize,
    /// Physical addresses (in `event_buf_phys`'s page) for each of the
    /// queue's slots, indexed by descriptor id.
    slot_phys: Vec<usize>,
    /// In-progress frame being assembled from individual events. Not
    /// snapshotted into the ring until SYN_REPORT.
    cur_x: u32,
    cur_y: u32,
    cur_buttons: u8,
    /// Last completed frame ring.
    frames: [AbsPointerFrame; FRAME_RING],
    frame_head: usize,
    frame_tail: usize,
}

static STATE: Mutex<Option<InputState>> = Mutex::new(None);
static ACTIVE: AtomicBool = AtomicBool::new(false);

// ── MMIO transport ─────────────────────────────────────────────────────

struct MmioTransport {
    common_base: usize,
    notify_base: usize,
    notify_mul: u32,
}

impl MmioTransport {
    fn read32(&self, off: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.common_base + off) as *const u32) }
    }
    fn write32(&self, off: usize, v: u32) {
        unsafe { core::ptr::write_volatile((self.common_base + off) as *mut u32, v) }
    }
    fn read16(&self, off: usize) -> u16 {
        unsafe { core::ptr::read_volatile((self.common_base + off) as *const u16) }
    }
    fn write16(&self, off: usize, v: u16) {
        unsafe { core::ptr::write_volatile((self.common_base + off) as *mut u16, v) }
    }
    fn read8(&self, off: usize) -> u8 {
        unsafe { core::ptr::read_volatile((self.common_base + off) as *const u8) }
    }
    fn write8(&self, off: usize, v: u8) {
        unsafe { core::ptr::write_volatile((self.common_base + off) as *mut u8, v) }
    }
    fn notify(&self, notify_off: u16) {
        let off = notify_off as usize * self.notify_mul as usize;
        unsafe { core::ptr::write_volatile((self.notify_base + off) as *mut u32, 0) }
    }
}

// ── PCI capability parsing ─────────────────────────────────────────────

fn parse_caps(dev: &PciDevice) -> Result<MmioTransport, &'static str> {
    let hhdm = crate::memory::paging::hhdm_offset();
    let status = pci::pci_read16(dev.bus, dev.device, dev.function, 0x06);
    if status & (1 << 4) == 0 { return Err("no PCI capabilities"); }

    let mut cap = pci::pci_read8(dev.bus, dev.device, dev.function, 0x34) & 0xFC;
    let mut common: Option<(u8, u32)> = None;
    let mut notify: Option<(u8, u32, u32)> = None;

    let mut iter = 0;
    while cap != 0 && iter < 32 {
        iter += 1;
        let id = pci::pci_read8(dev.bus, dev.device, dev.function, cap);
        let next = pci::pci_read8(dev.bus, dev.device, dev.function, cap + 1);
        if id == 0x09 {
            let cfg_type = pci::pci_read8(dev.bus, dev.device, dev.function, cap + 3);
            let bar = pci::pci_read8(dev.bus, dev.device, dev.function, cap + 4);
            let off = pci::pci_read32(dev.bus, dev.device, dev.function, cap + 8);
            match cfg_type {
                1 => common = Some((bar, off)),
                2 => {
                    let mul = pci::pci_read32(dev.bus, dev.device, dev.function, cap + 16);
                    notify = Some((bar, off, mul));
                }
                _ => {}
            }
        }
        cap = next & 0xFC;
    }

    let (cb, coff) = common.ok_or("no common config cap")?;
    let (nb, noff, nmul) = notify.ok_or("no notify cap")?;
    let common_phys = bar_phys(dev, cb)? + coff as usize;
    let notify_phys = bar_phys(dev, nb)? + noff as usize;

    use x86_64::structures::paging::PageTableFlags;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE | PageTableFlags::NO_CACHE;
    // Map up to 4 pages covering all caps in the BAR.
    let base = common_phys & !0xFFF;
    for off in (0..16384usize).step_by(4096) {
        let _ = crate::memory::paging::map_page(hhdm + base + off, base + off, flags);
    }
    Ok(MmioTransport {
        common_base: hhdm + common_phys,
        notify_base: hhdm + notify_phys,
        notify_mul: nmul,
    })
}

fn bar_phys(dev: &PciDevice, idx: u8) -> Result<usize, &'static str> {
    match pci::decode_bar(dev, idx as usize) {
        BarType::Mmio32 { base, .. } => Ok(base as usize),
        BarType::Mmio64 { base, .. } => Ok(base as usize),
        BarType::Io { .. } => Err("unexpected I/O BAR"),
        BarType::None => Err("BAR not present"),
    }
}

// ── Init ───────────────────────────────────────────────────────────────

pub fn init() -> Result<(), &'static str> {
    crate::serial_strln!("[VIRTIO_INPUT] Looking for VirtIO Input device...");
    let dev = pci::find_virtio_input().ok_or("no VirtIO input device")?;
    crate::serial_str!("[VIRTIO_INPUT] Found at PCI ");
    crate::drivers::serial::write_dec(dev.bus as u32);
    crate::serial_str!(":");
    crate::drivers::serial::write_dec(dev.device as u32);
    crate::drivers::serial::write_newline();

    pci::enable_bus_master(dev.bus, dev.device, dev.function);
    let transport = parse_caps(&dev)?;

    // Standard handshake: RESET → ACK → DRIVER → no features → FEATURES_OK → queue setup → DRIVER_OK
    transport.write8(VIRTIO_PCI_COMMON_STATUS, 0);
    transport.write8(VIRTIO_PCI_COMMON_STATUS, STATUS_ACKNOWLEDGE);
    transport.write8(VIRTIO_PCI_COMMON_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    // We don't request any features. Read VERSION_1 (page 1, bit 0) and
    // accept it; that's all virtio-input modern needs.
    transport.write32(VIRTIO_PCI_COMMON_DFSELECT, 0);
    let _features_lo = transport.read32(VIRTIO_PCI_COMMON_DF);
    transport.write32(VIRTIO_PCI_COMMON_DFSELECT, 1);
    let _features_hi = transport.read32(VIRTIO_PCI_COMMON_DF);
    transport.write32(VIRTIO_PCI_COMMON_GFSELECT, 0);
    transport.write32(VIRTIO_PCI_COMMON_GF, 0);
    transport.write32(VIRTIO_PCI_COMMON_GFSELECT, 1);
    transport.write32(VIRTIO_PCI_COMMON_GF, 1); // VERSION_1

    transport.write8(VIRTIO_PCI_COMMON_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
    if transport.read8(VIRTIO_PCI_COMMON_STATUS) & STATUS_FEATURES_OK == 0 {
        return Err("FEATURES_OK rejected");
    }

    // ── Setup eventq (queue 0) ─────────────────────────────────────────
    transport.write16(VIRTIO_PCI_COMMON_Q_SELECT, 0);
    let queue_size = transport.read16(VIRTIO_PCI_COMMON_Q_SIZE);
    if queue_size == 0 { return Err("eventq size is 0"); }
    crate::serial_str!("[VIRTIO_INPUT] eventq size=");
    crate::drivers::serial::write_dec(queue_size as u32);
    crate::drivers::serial::write_newline();

    let mut eventq = Virtqueue::new(queue_size).ok_or("alloc eventq")?;
    let dp = eventq.desc_phys();
    let ap = eventq.avail_phys();
    let up = eventq.used_phys();
    transport.write32(VIRTIO_PCI_COMMON_Q_DESCLO, dp as u32);
    transport.write32(VIRTIO_PCI_COMMON_Q_DESCHI, (dp >> 32) as u32);
    transport.write32(VIRTIO_PCI_COMMON_Q_AVAILLO, ap as u32);
    transport.write32(VIRTIO_PCI_COMMON_Q_AVAILHI, (ap >> 32) as u32);
    transport.write32(VIRTIO_PCI_COMMON_Q_USEDLO, up as u32);
    transport.write32(VIRTIO_PCI_COMMON_Q_USEDHI, (up >> 32) as u32);
    let eventq_notify_off = transport.read16(VIRTIO_PCI_COMMON_Q_NOFF);
    unsafe {
        core::ptr::write_volatile(
            (transport.common_base + VIRTIO_PCI_COMMON_Q_ENABLE) as *mut u16, 1,
        );
    }
    for _ in 0..10_000 { core::hint::spin_loop(); }
    if transport.read16(VIRTIO_PCI_COMMON_Q_ENABLE) != 1 {
        return Err("eventq did not enable");
    }

    // ── Allocate one page for the event landing buffer ─────────────────
    // queue_size events × 8 bytes max = 8 * 64 = 512 bytes. One page
    // covers any reasonable queue size; if the device exposes >512 we
    // truncate at one page worth and the host will just not fill the
    // extras.
    let event_buf_phys = physical::alloc_page().ok_or("alloc event buf")?;
    let max_slots = (4096 / EVENT_SIZE).min(queue_size as usize);
    let mut slot_phys = Vec::with_capacity(max_slots);
    for i in 0..max_slots {
        slot_phys.push(event_buf_phys + i * EVENT_SIZE);
    }
    // Zero the page so half-arrived events don't look like garbage.
    let hhdm = crate::memory::paging::hhdm_offset();
    unsafe { core::ptr::write_bytes((hhdm + event_buf_phys) as *mut u8, 0, 4096); }

    // Pre-fill the eventq with device-write descriptors, one per slot.
    // Each descriptor is its own one-shot chain; the device picks the
    // next free one when an event arrives.
    for i in 0..max_slots {
        let d = eventq.alloc_desc().ok_or("desc exhausted during prefill")?;
        eventq.set_desc(d, slot_phys[i] as u64, EVENT_SIZE as u32, VRING_DESC_F_WRITE, 0);
        eventq.submit(d);
    }

    transport.write8(VIRTIO_PCI_COMMON_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
    if transport.read8(VIRTIO_PCI_COMMON_STATUS) & STATUS_FAILED != 0 {
        return Err("device set FAILED");
    }
    transport.notify(eventq_notify_off);
    crate::serial_strln!("[VIRTIO_INPUT] DRIVER_OK + eventq armed");

    *STATE.lock() = Some(InputState {
        transport,
        eventq,
        eventq_notify_off,
        event_buf_phys,
        slot_phys,
        cur_x: 0,
        cur_y: 0,
        cur_buttons: 0,
        frames: [AbsPointerFrame::default(); FRAME_RING],
        frame_head: 0,
        frame_tail: 0,
    });
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

/// Whether the driver attached to a device. Userspace queries this via
/// the read-abs syscall: `Some(...)` only ever returns when it's true.
pub fn is_active() -> bool { ACTIVE.load(Ordering::Acquire) }

// ── Event pump ─────────────────────────────────────────────────────────

/// Drain any used descriptors, parse the events, advance the in-progress
/// frame, and snapshot it into the frame ring on each SYN_REPORT.
fn pump_events(s: &mut InputState) {
    let hhdm = crate::memory::paging::hhdm_offset();
    while s.eventq.has_used() {
        let (head, len) = match s.eventq.pop_used() {
            Some(x) => x,
            None => break,
        };
        if (len as usize) >= EVENT_SIZE && (head as usize) < s.slot_phys.len() {
            // Read each field individually with read_volatile so the
            // compiler can't merge into one wide load that gets
            // reordered against the descriptor recycle below.
            let base = hhdm + s.slot_phys[head as usize];
            let ev = VirtioInputEvent {
                ev_type: unsafe { core::ptr::read_volatile(base as *const u16) },
                code: unsafe { core::ptr::read_volatile((base + 2) as *const u16) },
                value: unsafe { core::ptr::read_volatile((base + 4) as *const u32) },
            };
            apply_event(s, ev);
        }
        // Re-arm the slot for the next event.
        let phys = s.slot_phys[head as usize];
        s.eventq.set_desc(head, phys as u64, EVENT_SIZE as u32, VRING_DESC_F_WRITE, 0);
        s.eventq.submit(head);
    }
    // One notify per pump batch — cheap and the device only cares
    // about the avail-idx update which `submit` already wrote.
    s.transport.notify(s.eventq_notify_off);
}

fn apply_event(s: &mut InputState, ev: VirtioInputEvent) {
    match ev.ev_type {
        EV_ABS => match ev.code {
            ABS_X => s.cur_x = ev.value,
            ABS_Y => s.cur_y = ev.value,
            _ => {}
        },
        EV_KEY => {
            let bit: u8 = match ev.code {
                BTN_LEFT => 1 << 0,
                BTN_RIGHT => 1 << 1,
                BTN_MIDDLE => 1 << 2,
                _ => 0,
            };
            if bit != 0 {
                if ev.value != 0 {
                    s.cur_buttons |= bit;
                } else {
                    s.cur_buttons &= !bit;
                }
            }
        }
        EV_SYN => {
            if ev.code == SYN_REPORT {
                let frame = AbsPointerFrame {
                    abs_x: s.cur_x,
                    abs_y: s.cur_y,
                    buttons: s.cur_buttons,
                };
                s.frames[s.frame_head] = frame;
                s.frame_head = (s.frame_head + 1) % FRAME_RING;
                if s.frame_head == s.frame_tail {
                    // Ring full: drop the oldest by advancing tail.
                    s.frame_tail = (s.frame_tail + 1) % FRAME_RING;
                }
            }
        }
        _ => {}
    }
}

/// Pop the latest pending frame (in raw 0..0x7FFF coords). Returns
/// `None` when the queue is quiet — caller should not interpret that
/// as "cursor still where it was", but as "no new state since last
/// poll".
pub fn read_frame() -> Option<AbsPointerFrame> {
    let mut guard = STATE.lock();
    let s = guard.as_mut()?;
    pump_events(s);
    if s.frame_head == s.frame_tail { return None; }
    // We coalesce: return only the most recent frame (stop-the-world
    // collapse — losing intermediate states between polls is fine for
    // a pointer device, the user only cares about where it ended up).
    let mut latest = s.frames[(s.frame_head + FRAME_RING - 1) % FRAME_RING];
    s.frame_tail = s.frame_head;
    // Sanity-clamp the device coords against the advertised range so a
    // misbehaving device can't poison downstream scaling math.
    if latest.abs_x > ABS_MAX { latest.abs_x = ABS_MAX; }
    if latest.abs_y > ABS_MAX { latest.abs_y = ABS_MAX; }
    Some(latest)
}

/// Read the latest frame, scaled to a target framebuffer width × height
/// in pixels. Returns `(x, y, buttons)`. Coordinates are clamped so
/// `x ∈ [0, fb_w-1]` and `y ∈ [0, fb_h-1]` even on rounding-up edge
/// cases. Userspace's syscall handler is the only intended caller.
pub fn read_frame_scaled(fb_w: u32, fb_h: u32) -> Option<(u32, u32, u8)> {
    let f = read_frame()?;
    let w = fb_w.max(1);
    let h = fb_h.max(1);
    let x = ((f.abs_x as u64) * (w as u64) / (ABS_MAX as u64 + 1)) as u32;
    let y = ((f.abs_y as u64) * (h as u64) / (ABS_MAX as u64 + 1)) as u32;
    Some((x.min(w - 1), y.min(h - 1), f.buttons))
}

// Suppress unused-warning on the kept-for-the-driver-lifetime field.
// We never free this page; holding the address keeps the static
// `slot_phys` slice valid for the slot indices we hand back to the
// device on each re-arm.
#[allow(dead_code)]
fn _keep_buf_alive(s: &InputState) -> usize { s.event_buf_phys }
