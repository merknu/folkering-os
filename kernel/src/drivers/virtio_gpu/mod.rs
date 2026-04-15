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
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) fb_phys_pages: Vec<usize>, // Physical page addresses for backing
    pub(super) active: bool,
    pub(super) has_virgl: bool,
    pub(super) has_edid: bool,
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
    crate::serial_str!(" V1=");
    crate::drivers::serial::write_dec(if features_hi & 1 != 0 { 1 } else { 0 });
    crate::drivers::serial::write_newline();

    // Accept features for Modern transport
    // Page 0: accept VIRTIO_F_EVENT_IDX (bit 29) + VIRTIO_F_INDIRECT_DESC (bit 28)
    transport.write_common32(VIRTIO_PCI_COMMON_GFSELECT, 0);
    transport.write_common32(VIRTIO_PCI_COMMON_GF, (1 << 29) | (1 << 28));
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

    // Setup control queue (queue 0)
    transport.write_common16(VIRTIO_PCI_COMMON_Q_SELECT, 0);
    let queue_size = transport.read_common16(VIRTIO_PCI_COMMON_Q_SIZE);
    if queue_size == 0 {
        return Err("controlq size is 0");
    }

    crate::serial_str!("[VIRTIO_GPU] Controlq size: ");
    crate::drivers::serial::write_dec(queue_size as u32);
    crate::drivers::serial::write_newline();

    let controlq = Virtqueue::new(queue_size).ok_or("failed to alloc controlq")?;

    // Modern: write descriptor/available/used ring addresses directly
    let dp = controlq.desc_phys();
    let ap = controlq.avail_phys();
    let up = controlq.used_phys();

    crate::serial_str!("[VIRTIO_GPU] Ring addrs: desc=");
    crate::drivers::serial::write_hex(dp);
    crate::serial_str!(" avail=");
    crate::drivers::serial::write_hex(ap);
    crate::serial_str!(" used=");
    crate::drivers::serial::write_hex(up);
    crate::serial_str!(" (align: desc%16=");
    crate::drivers::serial::write_dec((dp % 16) as u32);
    crate::serial_str!(" avail%2=");
    crate::drivers::serial::write_dec((ap % 2) as u32);
    crate::serial_str!(" used%4=");
    crate::drivers::serial::write_dec((up % 4) as u32);
    crate::serial_str!(")\n");

    // Write ring addresses (standard 32-bit lo/hi per OASIS spec)
    transport.write_common32(VIRTIO_PCI_COMMON_Q_DESCLO, dp as u32);
    transport.write_common32(VIRTIO_PCI_COMMON_Q_DESCHI, (dp >> 32) as u32);
    transport.write_common32(VIRTIO_PCI_COMMON_Q_AVAILLO, ap as u32);
    transport.write_common32(VIRTIO_PCI_COMMON_Q_AVAILHI, (ap >> 32) as u32);
    transport.write_common32(VIRTIO_PCI_COMMON_Q_USEDLO, up as u32);
    transport.write_common32(VIRTIO_PCI_COMMON_Q_USEDHI, (up >> 32) as u32);

    // Read queue notify offset (needed for correct notify address)
    let q_notify_off = transport.read_common16(VIRTIO_PCI_COMMON_Q_NOFF);
    transport.notify_off = q_notify_off;
    crate::serial_str!("[VIRTIO_GPU] Queue notify offset: ");
    crate::drivers::serial::write_dec(q_notify_off as u32);
    crate::serial_str!(", notify_mul=");
    crate::drivers::serial::write_dec(transport.notify_mul);
    crate::drivers::serial::write_newline();

    // Verify USED readback
    let rb_used = transport.read_common32(VIRTIO_PCI_COMMON_Q_USEDLO);
    crate::serial_str!("[VIRTIO_GPU] USED readback=");
    crate::drivers::serial::write_hex(rb_used as u64);
    crate::drivers::serial::write_newline();
    // Try writing as both u16 and u32 to handle potential alignment issues
    unsafe {
        core::ptr::write_volatile(
            (transport.common_base + VIRTIO_PCI_COMMON_Q_ENABLE) as *mut u16, 1
        );
    }
    // Force a small delay for device to process
    for _ in 0..10000 { core::hint::spin_loop(); }
    let q_enabled = transport.read_common16(VIRTIO_PCI_COMMON_Q_ENABLE);
    crate::serial_str!("[VIRTIO_GPU] Q_ENABLE readback: ");
    crate::drivers::serial::write_dec(q_enabled as u32);
    crate::drivers::serial::write_newline();

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
        width: 0,
        height: 0,
        fb_phys_pages: Vec::new(),
        active: false,
        has_virgl,
        has_edid,
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
