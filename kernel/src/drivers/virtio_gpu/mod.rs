//! VirtIO GPU Driver (Modern MMIO Transport, 2D Mode)
//!
//! Provides hardware-accelerated 2D scanout via VirtIO-GPU protocol.
//! Uses async command submission with lazy descriptor recycling.
//!
//! # Architecture
//! - VirtIO Modern MMIO transport (BAR-mapped Common/Notify/ISR regions)
//! - Control queue (queue 0): GPU commands
//! - Resource ID 1: primary framebuffer (created once at init)
//! - Hot path: TRANSFER_TO_HOST_2D + RESOURCE_FLUSH (fire-and-forget)
//!
//! # Hardening
//! - Feature bit logging (VIRGL, EDID) for future 3D/Vulkan capability detection
//! - Scatter-gather lists for ATTACH_BACKING (non-contiguous physical pages)
//! - Async controlq with lazy descriptor recycling (no BSP blocking)
//! - Limine framebuffer fallback if GPU init fails
//!
//! # Module structure
//! - `mod.rs` (this file) — GpuState, init() orchestration, public API, hot-path flush
//! - `io.rs` — MmioTransport + register offsets + status/feature bits
//! - `commands.rs` — Wire format structs, submit_and_wait, recycle_used
//! - `resources.rs` — display info, framebuffer create/attach/scanout
//! - `pci_setup.rs` — PCI capability parsing → MmioTransport

mod io;
mod commands;
mod pci_setup;
mod resources;
mod flush;

pub use flush::{flush_rect, flush_rects_batched, flush_and_vsync};

extern crate alloc;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use crate::drivers::pci;
use crate::drivers::virtio::Virtqueue;
use crate::memory::physical;

use io::*;
use commands::*;

// ── Driver State ───────────────────────────────────────────────────────

pub(super) struct GpuState {
    pub(super) transport: MmioTransport,
    pub(super) controlq: Virtqueue,
    /// Cursor queue (queue 1) — dedicated path for UPDATE_CURSOR/MOVE_CURSOR
    /// so cursor stays responsive even when controlq is backed up. Optional
    /// because not every transport exposes it (rare; the spec mandates it).
    pub(super) cursorq: Option<Virtqueue>,
    /// Notify offset for cursorq, kept separate because each queue has its
    /// own slot in the notify capability region.
    pub(super) cursorq_notify_off: u16,
    /// Static page used to stage cursor commands (avoids alloc_page per send).
    pub(super) cursor_cmd_page: Option<usize>,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) fb_phys_pages: Vec<usize>, // Physical page addresses for backing
    pub(super) active: bool,
    pub(super) has_virgl: bool,
    pub(super) has_edid: bool,
    /// Set if VIRTIO_GPU_F_RESOURCE_BLOB was advertised AND accepted. Today
    /// this is informational — the framebuffer still uses ATTACH_BACKING. Once
    /// we wire up `create_blob` it gates whether to take the zero-copy path.
    pub(super) has_resource_blob: bool,
    /// Set if VIRTIO_GPU_F_CONTEXT_INIT was negotiated.
    pub(super) has_context_init: bool,
}

pub(super) static GPU_STATE: Mutex<Option<GpuState>> = Mutex::new(None);
pub static GPU_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Static command page for flush operations (avoids alloc_page per flush)
pub(super) static FLUSH_CMD_PAGE: Mutex<Option<usize>> = Mutex::new(None);

/// VSync fence support
pub(super) static FENCE_COUNTER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
pub(super) static FENCE_COMPLETE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// ── Public API: Initialization ─────────────────────────────────────────

/// Initialize VirtIO GPU. Returns Ok if device found and scanout configured.
/// Falls back silently on failure — Limine framebuffer remains active.
pub fn init() -> Result<(), &'static str> {
    crate::serial_strln!("[VIRTIO_GPU] Looking for VirtIO GPU device...");

    // Find VirtIO GPU on PCI bus
    let pci_dev = pci::find_virtio_gpu().ok_or("no VirtIO GPU")?;

    crate::serial_str!("[VIRTIO_GPU] Found at PCI ");
    crate::drivers::serial::write_dec(pci_dev.bus as u32);
    crate::serial_str!(":");
    crate::drivers::serial::write_dec(pci_dev.device as u32);
    crate::drivers::serial::write_newline();

    // Enable bus mastering for DMA
    pci::enable_bus_master(pci_dev.bus, pci_dev.device, pci_dev.function);

    // ── Parse PCI Capabilities for Modern VirtIO MMIO transport ─────────
    let mut transport = pci_setup::parse_virtio_capabilities(&pci_dev)?;
    crate::serial_strln!("[VIRTIO_GPU] Modern MMIO transport initialized");

    // ── VirtIO Modern Handshake ─────────────────────────────────────────

    // Reset
    transport.write_common8(VIRTIO_PCI_COMMON_STATUS, 0);
    transport.write_common8(VIRTIO_PCI_COMMON_STATUS, STATUS_ACKNOWLEDGE);
    transport.write_common8(VIRTIO_PCI_COMMON_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    // Read feature bits page 0 (bits 0-31)
    transport.write_common32(VIRTIO_PCI_COMMON_DFSELECT, 0);
    let features_lo = transport.read_common32(VIRTIO_PCI_COMMON_DF);
    let has_virgl = features_lo & VIRTIO_GPU_F_VIRGL != 0;
    let has_edid = features_lo & VIRTIO_GPU_F_EDID != 0;
    let host_offers_blob = features_lo & VIRTIO_GPU_F_RESOURCE_BLOB != 0;
    let host_offers_ctxinit = features_lo & VIRTIO_GPU_F_CONTEXT_INIT != 0;

    // Read feature bits page 1 (bits 32-63) — includes VIRTIO_F_VERSION_1
    transport.write_common32(VIRTIO_PCI_COMMON_DFSELECT, 1);
    let features_hi = transport.read_common32(VIRTIO_PCI_COMMON_DF);

    crate::serial_str!("[VIRTIO_GPU] Features lo=0x");
    crate::drivers::serial::write_hex(features_lo as u64);
    crate::serial_str!(" hi=0x");
    crate::drivers::serial::write_hex(features_hi as u64);
    crate::serial_str!(" VIRGL=");
    crate::drivers::serial::write_dec(if has_virgl { 1 } else { 0 });
    crate::serial_str!(" EDID=");
    crate::drivers::serial::write_dec(if has_edid { 1 } else { 0 });
    crate::serial_str!(" BLOB=");
    crate::drivers::serial::write_dec(if host_offers_blob { 1 } else { 0 });
    crate::serial_str!(" CTXINIT=");
    crate::drivers::serial::write_dec(if host_offers_ctxinit { 1 } else { 0 });
    crate::serial_str!(" V1=");
    crate::drivers::serial::write_dec(if features_hi & 1 != 0 { 1 } else { 0 });
    crate::drivers::serial::write_newline();

    // Accept features for Modern transport
    // Page 0: VIRTIO_F_EVENT_IDX (bit 29) + VIRTIO_F_INDIRECT_DESC (bit 28),
    // plus RESOURCE_BLOB and CONTEXT_INIT when the host advertises them. The
    // accept-only-what-host-offers pattern keeps boot working on QEMU builds
    // that don't expose the newer feature bits.
    let mut gf_lo: u32 = (1 << 29) | (1 << 28);
    if host_offers_blob    { gf_lo |= VIRTIO_GPU_F_RESOURCE_BLOB; }
    if host_offers_ctxinit { gf_lo |= VIRTIO_GPU_F_CONTEXT_INIT; }
    transport.write_common32(VIRTIO_PCI_COMMON_GFSELECT, 0);
    transport.write_common32(VIRTIO_PCI_COMMON_GF, gf_lo);
    // Page 1: VIRTIO_F_VERSION_1 (bit 0)
    transport.write_common32(VIRTIO_PCI_COMMON_GFSELECT, 1);
    transport.write_common32(VIRTIO_PCI_COMMON_GF, 1);

    // FEATURES_OK
    transport.write_common8(VIRTIO_PCI_COMMON_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);

    // Verify FEATURES_OK was accepted
    let status_check = transport.read_common8(VIRTIO_PCI_COMMON_STATUS);
    if status_check & STATUS_FEATURES_OK == 0 {
        crate::serial_str!("[VIRTIO_GPU] FEATURES_OK not set by device!\n");
        return Err("features not accepted");
    }
    crate::serial_str!("[VIRTIO_GPU] FEATURES_OK accepted\n");

    // Setup control queue (queue 0) — primary command channel.
    let (controlq, controlq_notify_off) = setup_queue(&mut transport, 0, "controlq")?;
    transport.notify_off = controlq_notify_off;

    // Setup cursor queue (queue 1) — separate fast-path for UPDATE_CURSOR /
    // MOVE_CURSOR. Failure here is non-fatal: we still get scanout via
    // controlq, just without the dedicated cursor channel.
    let (cursorq, cursorq_notify_off) = match setup_queue(&mut transport, 1, "cursorq") {
        Ok(pair) => (Some(pair.0), pair.1),
        Err(e) => {
            crate::serial_str!("[VIRTIO_GPU] cursorq setup failed (");
            crate::serial_str!(e);
            crate::serial_str!(") — falling back to controlq cursor\n");
            (None, 0)
        }
    };

    // DRIVER_OK
    transport.write_common8(VIRTIO_PCI_COMMON_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);

    let status = transport.read_common8(VIRTIO_PCI_COMMON_STATUS);
    if status & STATUS_FAILED != 0 {
        return Err("device set FAILED");
    }

    crate::serial_strln!("[VIRTIO_GPU] Device DRIVER_OK");

    // Store state
    let mut state = GpuState {
        transport,
        controlq,
        cursorq,
        cursorq_notify_off,
        cursor_cmd_page: None,
        width: 0,
        height: 0,
        fb_phys_pages: Vec::new(),
        active: false,
        has_virgl,
        has_edid,
        has_resource_blob: host_offers_blob,
        has_context_init: host_offers_ctxinit,
    };

    // ── Get Display Info ────────────────────────────────────────────────

    let (w, h) = resources::get_display_info(&mut state)?;
    state.width = w;
    state.height = h;

    crate::serial_str!("[VIRTIO_GPU] Display: ");
    crate::drivers::serial::write_dec(w);
    crate::serial_str!("x");
    crate::drivers::serial::write_dec(h);
    crate::drivers::serial::write_newline();

    // ── Create Resource + Attach Backing + Set Scanout ──────────────────

    resources::create_framebuffer_resource(&mut state)?;
    crate::serial_strln!("[VIRTIO_GPU] Framebuffer resource created");

    resources::attach_framebuffer_backing(&mut state)?;
    crate::serial_strln!("[VIRTIO_GPU] Backing attached (scatter-gather)");

    resources::set_scanout(&mut state)?;
    crate::serial_strln!("[VIRTIO_GPU] Scanout active!");

    // TEST: Fill backing buffer with bright red pixels before declaring active
    let hhdm_off = crate::memory::paging::hhdm_offset();
    for &page_phys in &state.fb_phys_pages {
        let page_virt = (hhdm_off + page_phys) as *mut u32;
        for i in 0..1024 { // 4096 bytes / 4 bytes per pixel = 1024 pixels
            unsafe { page_virt.add(i).write_volatile(0x00FF0000); } // Red in BGRX
        }
    }
    crate::serial_str!("[VIRTIO_GPU] Test pattern: filled backing with RED\n");

    // Do a sync transfer+flush to verify display works
    {
        let cmd_phys = physical::alloc_page().unwrap();
        let cmd_virt = (hhdm_off + cmd_phys) as *mut u8;
        unsafe {
            let transfer = cmd_virt as *mut GpuTransferToHost2D;
            (*transfer).hdr = make_hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
            (*transfer).r = GpuRect { x: 0, y: 0, width: state.width, height: state.height };
            (*transfer).offset = 0;
            (*transfer).resource_id = 1;
            (*transfer).padding = 0;
        }
        submit_and_wait(&mut state, cmd_phys,
            core::mem::size_of::<GpuTransferToHost2D>(),
            cmd_phys + core::mem::size_of::<GpuTransferToHost2D>(), 24)?;
        crate::serial_str!("[VIRTIO_GPU] TRANSFER_TO_HOST_2D OK\n");

        let cmd_phys2 = physical::alloc_page().unwrap();
        let cmd_virt2 = (hhdm_off + cmd_phys2) as *mut u8;
        unsafe {
            let flush_cmd = cmd_virt2 as *mut GpuResourceFlush;
            (*flush_cmd).hdr = make_hdr(VIRTIO_GPU_CMD_RESOURCE_FLUSH);
            (*flush_cmd).r = GpuRect { x: 0, y: 0, width: state.width, height: state.height };
            (*flush_cmd).resource_id = 1;
            (*flush_cmd).padding = 0;
            core::ptr::write_bytes(cmd_virt2.add(core::mem::size_of::<GpuResourceFlush>()), 0, 24);
        }
        submit_and_wait(&mut state, cmd_phys2,
            core::mem::size_of::<GpuResourceFlush>(),
            cmd_phys2 + core::mem::size_of::<GpuResourceFlush>(), 24)?;
        crate::serial_str!("[VIRTIO_GPU] RESOURCE_FLUSH OK — screen should be RED!\n");
    }

    state.active = true;
    GPU_ACTIVE.store(true, Ordering::Release);
    *GPU_STATE.lock() = Some(state);

    Ok(())
}

// ── Public API: Queries ────────────────────────────────────────────────

/// Get the framebuffer physical address for the first page (for compositor mapping).
pub fn framebuffer_phys() -> Option<usize> {
    let guard = GPU_STATE.lock();
    guard.as_ref().and_then(|s| s.fb_phys_pages.first().copied())
}

/// Get display dimensions.
pub fn display_size() -> Option<(u32, u32)> {
    let guard = GPU_STATE.lock();
    guard.as_ref().map(|s| (s.width, s.height))
}

/// Get all framebuffer physical pages (for userspace mapping).
pub fn framebuffer_pages() -> Option<Vec<usize>> {
    let guard = GPU_STATE.lock();
    guard.as_ref().map(|s| s.fb_phys_pages.clone())
}

/// Whether VIRTIO_GPU_F_RESOURCE_BLOB was negotiated. Userspace can query
/// this to decide whether a zero-copy framebuffer path is available.
pub fn has_resource_blob() -> bool {
    GPU_STATE.lock().as_ref().map(|s| s.has_resource_blob).unwrap_or(false)
}

/// Whether the device exposed a working cursorq. Mainly diagnostic — callers
/// of `move_cursor` don't need to check this; the driver falls back silently.
pub fn has_cursor_queue() -> bool {
    GPU_STATE.lock().as_ref().map(|s| s.cursorq.is_some()).unwrap_or(false)
}

// ── Public API: Cursor (queue 1, fire-and-forget) ──────────────────────

/// Move the hardware cursor without touching its sprite. Fire-and-forget on
/// the cursor queue so this doesn't contend with the heavier control-queue
/// flushes. No-op when cursorq isn't available.
pub fn move_cursor(scanout_id: u32, x: u32, y: u32) {
    let mut guard = GPU_STATE.lock();
    let Some(state) = guard.as_mut() else { return };
    if state.cursorq.is_none() { return; }
    submit_cursor_cmd(state, VIRTIO_GPU_CMD_MOVE_CURSOR, scanout_id, x, y, 0, 0, 0);
}

/// Bind a sprite resource to the hardware cursor at `(x, y)` with the given
/// hotspot. `resource_id == 0` hides the cursor (per spec).
pub fn update_cursor(
    scanout_id: u32,
    x: u32, y: u32,
    resource_id: u32, hot_x: u32, hot_y: u32,
) {
    let mut guard = GPU_STATE.lock();
    let Some(state) = guard.as_mut() else { return };
    if state.cursorq.is_none() { return; }
    submit_cursor_cmd(state, VIRTIO_GPU_CMD_UPDATE_CURSOR,
        scanout_id, x, y, resource_id, hot_x, hot_y);
}

// ── Internals ──────────────────────────────────────────────────────────

/// Configure one virtqueue (descriptor/avail/used rings, notify offset,
/// enable bit) and return the queue and its per-queue notify offset. Used
/// for both controlq and cursorq.
fn setup_queue(
    transport: &mut MmioTransport,
    queue_idx: u16,
    label: &'static str,
) -> Result<(Virtqueue, u16), &'static str> {
    transport.write_common16(VIRTIO_PCI_COMMON_Q_SELECT, queue_idx);
    let queue_size = transport.read_common16(VIRTIO_PCI_COMMON_Q_SIZE);
    if queue_size == 0 {
        return Err("queue size is 0");
    }

    crate::serial_str!("[VIRTIO_GPU] ");
    crate::serial_str!(label);
    crate::serial_str!(" size=");
    crate::drivers::serial::write_dec(queue_size as u32);
    crate::drivers::serial::write_newline();

    let q = Virtqueue::new(queue_size).ok_or("failed to alloc queue")?;
    let dp = q.desc_phys();
    let ap = q.avail_phys();
    let up = q.used_phys();

    transport.write_common32(VIRTIO_PCI_COMMON_Q_DESCLO, dp as u32);
    transport.write_common32(VIRTIO_PCI_COMMON_Q_DESCHI, (dp >> 32) as u32);
    transport.write_common32(VIRTIO_PCI_COMMON_Q_AVAILLO, ap as u32);
    transport.write_common32(VIRTIO_PCI_COMMON_Q_AVAILHI, (ap >> 32) as u32);
    transport.write_common32(VIRTIO_PCI_COMMON_Q_USEDLO, up as u32);
    transport.write_common32(VIRTIO_PCI_COMMON_Q_USEDHI, (up >> 32) as u32);

    let notify_off = transport.read_common16(VIRTIO_PCI_COMMON_Q_NOFF);
    crate::serial_str!("[VIRTIO_GPU] ");
    crate::serial_str!(label);
    crate::serial_str!(" notify_off=");
    crate::drivers::serial::write_dec(notify_off as u32);
    crate::drivers::serial::write_newline();

    unsafe {
        core::ptr::write_volatile(
            (transport.common_base + VIRTIO_PCI_COMMON_Q_ENABLE) as *mut u16, 1,
        );
    }
    for _ in 0..10_000 { core::hint::spin_loop(); }

    let enabled = transport.read_common16(VIRTIO_PCI_COMMON_Q_ENABLE);
    if enabled != 1 {
        return Err("queue did not enable");
    }

    Ok((q, notify_off))
}

/// Stage and submit one cursor-queue command. Fire-and-forget: we recycle
/// any used descriptors at the start of the next call rather than blocking.
fn submit_cursor_cmd(
    state: &mut GpuState,
    cmd_type: u32,
    scanout_id: u32, x: u32, y: u32,
    resource_id: u32, hot_x: u32, hot_y: u32,
) {
    use commands::{GpuUpdateCursor, GpuCursorPos, make_hdr, recycle_used};

    let cursorq = match state.cursorq.as_mut() {
        Some(q) => q,
        None => return,
    };

    // Reuse the staging page across calls — cursor traffic is tiny (~32B per
    // command) and we never need to overlap two in flight: by spec the cursor
    // queue serializes naturally, and we only ever post one command before
    // returning.
    let cmd_phys = match state.cursor_cmd_page {
        Some(p) => p,
        None => match physical::alloc_page() {
            Some(p) => { state.cursor_cmd_page = Some(p); p }
            None => return,
        }
    };

    recycle_used(cursorq);

    let hhdm_off = crate::memory::paging::hhdm_offset();
    unsafe {
        let cmd = (hhdm_off + cmd_phys) as *mut GpuUpdateCursor;
        (*cmd).hdr = make_hdr(cmd_type);
        (*cmd).pos = GpuCursorPos { scanout_id, x, y, padding: 0 };
        (*cmd).resource_id = resource_id;
        (*cmd).hot_x = hot_x;
        (*cmd).hot_y = hot_y;
        (*cmd).padding = 0;
    }

    // Single descriptor, device-read only — cursor commands have no response.
    let Some(d0) = cursorq.alloc_desc() else { return };
    cursorq.set_desc(
        d0,
        cmd_phys as u64,
        core::mem::size_of::<GpuUpdateCursor>() as u32,
        0,
        0,
    );
    cursorq.submit(d0);

    // Ring the cursorq doorbell using its own per-queue notify offset.
    let off = state.cursorq_notify_off as usize * state.transport.notify_mul as usize;
    let addr = state.transport.notify_base + off;
    unsafe { core::ptr::write_volatile(addr as *mut u32, 1) };
}
