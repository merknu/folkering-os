//! VirtIO GPU Driver (Legacy PCI Transport, 2D Mode)
//!
//! Provides hardware-accelerated 2D scanout via VirtIO-GPU protocol.
//! Uses async command submission with lazy descriptor recycling.
//!
//! # Architecture
//! - VirtIO legacy PCI: BAR0 I/O ports
//! - Control queue (queue 0): GPU commands
//! - Cursor queue (queue 1): unused for now
//! - Resource ID 1: primary framebuffer (created once at init)
//! - Hot path: TRANSFER_TO_HOST_2D + RESOURCE_FLUSH (fire-and-forget)
//!
//! # Hardening
//! - Feature bit logging (VIRGL, EDID) for future 3D/Vulkan capability detection
//! - Scatter-gather lists for ATTACH_BACKING (non-contiguous physical pages)
//! - Async controlq with lazy descriptor recycling (no BSP blocking)
//! - Limine framebuffer fallback if GPU init fails

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use spin::Mutex;
use x86_64::instructions::port::Port;

use super::pci::{self, PciDevice, BarType};
use super::virtio::{self, Virtqueue, VirtqDesc, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE};
use crate::memory::physical;

// ── VirtIO Legacy PCI Register Offsets ──────────────────────────────────────

const VIRTIO_PCI_DEVICE_FEATURES: u16 = 0x00;
const VIRTIO_PCI_DRIVER_FEATURES: u16 = 0x04;
const VIRTIO_PCI_QUEUE_PFN: u16 = 0x08;
const VIRTIO_PCI_QUEUE_SIZE: u16 = 0x0C;
const VIRTIO_PCI_QUEUE_SEL: u16 = 0x0E;
const VIRTIO_PCI_QUEUE_NOTIFY: u16 = 0x10;
const VIRTIO_PCI_DEVICE_STATUS: u16 = 0x12;
const VIRTIO_PCI_ISR_STATUS: u16 = 0x13;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FAILED: u8 = 128;

// ── VirtIO GPU Feature Bits ─────────────────────────────────────────────────

const VIRTIO_GPU_F_VIRGL: u32 = 1 << 0;  // 3D/VirGL support
const VIRTIO_GPU_F_EDID: u32 = 1 << 1;   // EDID display info

// ── VirtIO GPU Command Types ────────────────────────────────────────────────

const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0102;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0104;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0106;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;

const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;

// ── VirtIO GPU Formats ──────────────────────────────────────────────────────

const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2; // BGRX 32-bit

// ── VirtIO GPU Command Structures ───────────────────────────────────────────

#[repr(C)]
struct GpuCtrlHdr {
    cmd_type: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GpuRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
struct GpuGetDisplayInfo {
    hdr: GpuCtrlHdr,
}

#[repr(C)]
struct GpuRespDisplayInfo {
    hdr: GpuCtrlHdr,
    pmodes: [GpuDisplayOne; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GpuDisplayOne {
    r: GpuRect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
struct GpuResourceCreate2D {
    hdr: GpuCtrlHdr,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
struct GpuResourceAttachBacking {
    hdr: GpuCtrlHdr,
    resource_id: u32,
    nr_entries: u32,
    // Followed by nr_entries × GpuMemEntry
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GpuMemEntry {
    addr: u64,
    length: u32,
    padding: u32,
}

#[repr(C)]
struct GpuSetScanout {
    hdr: GpuCtrlHdr,
    r: GpuRect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
struct GpuTransferToHost2D {
    hdr: GpuCtrlHdr,
    r: GpuRect,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
struct GpuResourceFlush {
    hdr: GpuCtrlHdr,
    r: GpuRect,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
struct GpuRespHdr {
    cmd_type: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    _padding: u32,
}

// ── Driver State ────────────────────────────────────────────────────────────

struct GpuState {
    io_base: u16,
    controlq: Virtqueue,
    width: u32,
    height: u32,
    fb_phys_pages: alloc::vec::Vec<usize>, // Physical page addresses for backing
    active: bool,
    has_virgl: bool,
    has_edid: bool,
}

static GPU_STATE: Mutex<Option<GpuState>> = Mutex::new(None);
pub static GPU_ACTIVE: AtomicBool = AtomicBool::new(false);

extern crate alloc;
use alloc::vec::Vec;

// ── Public API ──────────────────────────────────────────────────────────────

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

    // VirtIO-VGA: BAR0 = VGA framebuffer (MMIO), BAR2 = Legacy I/O transport
    // Try BAR2 first (VirtIO legacy I/O), then BAR0 as fallback
    let io_base = if let BarType::Io { base } = pci::decode_bar(&pci_dev, 2) {
        base
    } else if let BarType::Io { base } = pci::decode_bar(&pci_dev, 0) {
        base
    } else if let BarType::Io { base } = pci::decode_bar(&pci_dev, 4) {
        base
    } else {
        // Log all BARs for debugging
        for bar_idx in 0usize..6 {
            crate::serial_str!("[VIRTIO_GPU] BAR");
            crate::drivers::serial::write_dec(bar_idx as u32);
            crate::serial_str!(": ");
            match pci::decode_bar(&pci_dev, bar_idx) {
                BarType::Io { base } => {
                    crate::serial_str!("I/O 0x");
                    crate::drivers::serial::write_hex(base as u64);
                }
                BarType::Mmio32 { base, .. } => {
                    crate::serial_str!("MMIO32 0x");
                    crate::drivers::serial::write_hex(base as u64);
                }
                BarType::Mmio64 { base, .. } => {
                    crate::serial_str!("MMIO64 0x");
                    crate::drivers::serial::write_hex(base);
                }
                BarType::None => crate::serial_str!("None"),
            }
            crate::drivers::serial::write_newline();
        }
        return Err("no I/O BAR found for VirtIO-VGA legacy transport");
    };

    crate::serial_str!("[VIRTIO_GPU] BAR0 I/O base: 0x");
    crate::drivers::serial::write_hex(io_base as u64);
    crate::drivers::serial::write_newline();

    // Enable bus mastering for DMA
    pci::enable_bus_master(pci_dev.bus, pci_dev.device, pci_dev.function);

    // ── VirtIO Handshake ────────────────────────────────────────────────

    // Reset
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, 0);
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE);
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    // Read and log feature bits (critical for future 3D/Vulkan detection)
    let features = read_io32(io_base, VIRTIO_PCI_DEVICE_FEATURES);
    let has_virgl = features & VIRTIO_GPU_F_VIRGL != 0;
    let has_edid = features & VIRTIO_GPU_F_EDID != 0;

    crate::serial_str!("[VIRTIO_GPU] Features: 0x");
    crate::drivers::serial::write_hex(features as u64);
    crate::serial_str!(" VIRGL=");
    crate::drivers::serial::write_dec(if has_virgl { 1 } else { 0 });
    crate::serial_str!(" EDID=");
    crate::drivers::serial::write_dec(if has_edid { 1 } else { 0 });
    crate::drivers::serial::write_newline();

    // Accept no features (2D only)
    write_io32(io_base, VIRTIO_PCI_DRIVER_FEATURES, 0);

    // Setup control queue (queue 0)
    write_io16(io_base, VIRTIO_PCI_QUEUE_SEL, 0);
    let queue_size = read_io16(io_base, VIRTIO_PCI_QUEUE_SIZE);
    if queue_size == 0 {
        return Err("controlq size is 0");
    }

    crate::serial_str!("[VIRTIO_GPU] Controlq size: ");
    crate::drivers::serial::write_dec(queue_size as u32);
    crate::drivers::serial::write_newline();

    let controlq = Virtqueue::new(queue_size).ok_or("failed to alloc controlq")?;
    let pfn = (controlq.queue_phys / 4096) as u32;
    write_io32(io_base, VIRTIO_PCI_QUEUE_PFN, pfn);

    // DRIVER_OK
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);

    let status = read_io8(io_base, VIRTIO_PCI_DEVICE_STATUS);
    if status & STATUS_FAILED != 0 {
        return Err("device set FAILED");
    }

    crate::serial_strln!("[VIRTIO_GPU] Device DRIVER_OK");

    // Store state
    let mut state = GpuState {
        io_base,
        controlq,
        width: 0,
        height: 0,
        fb_phys_pages: Vec::new(),
        active: false,
        has_virgl,
        has_edid,
    };

    // ── Get Display Info ────────────────────────────────────────────────

    let (w, h) = get_display_info(&mut state)?;
    state.width = w;
    state.height = h;

    crate::serial_str!("[VIRTIO_GPU] Display: ");
    crate::drivers::serial::write_dec(w);
    crate::serial_str!("x");
    crate::drivers::serial::write_dec(h);
    crate::drivers::serial::write_newline();

    // ── Create Resource + Attach Backing + Set Scanout ──────────────────

    create_framebuffer_resource(&mut state)?;
    crate::serial_strln!("[VIRTIO_GPU] Framebuffer resource created");

    attach_framebuffer_backing(&mut state)?;
    crate::serial_strln!("[VIRTIO_GPU] Backing attached (scatter-gather)");

    set_scanout(&mut state)?;
    crate::serial_strln!("[VIRTIO_GPU] Scanout active!");

    state.active = true;
    GPU_ACTIVE.store(true, Ordering::Release);
    *GPU_STATE.lock() = Some(state);

    Ok(())
}

/// Flush a dirty rectangle to the display (hot path).
/// Non-blocking: submits TRANSFER_TO_HOST_2D + RESOURCE_FLUSH, returns immediately.
/// Called from SYS_GPU_FLUSH syscall.
pub fn flush_rect(x: u32, y: u32, w: u32, h: u32) {
    let mut guard = GPU_STATE.lock();
    let state = match guard.as_mut() {
        Some(s) if s.active => s,
        _ => return,
    };

    // Recycle used descriptors before submitting new ones
    recycle_used(&mut state.controlq);

    // Allocate a command page for both commands + response
    let cmd_phys = match physical::alloc_page() {
        Some(p) => p,
        None => return,
    };
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    // Layout: [TransferToHost2D @ 0] [ResourceFlush @ 64] [RespHdr @ 128] [RespHdr @ 160]
    unsafe {
        // TRANSFER_TO_HOST_2D
        let transfer = cmd_virt as *mut GpuTransferToHost2D;
        (*transfer).hdr = make_hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
        (*transfer).r = GpuRect { x, y, width: w, height: h };
        (*transfer).offset = 0;
        (*transfer).resource_id = 1;
        (*transfer).padding = 0;

        // RESOURCE_FLUSH
        let flush = cmd_virt.add(64) as *mut GpuResourceFlush;
        (*flush).hdr = make_hdr(VIRTIO_GPU_CMD_RESOURCE_FLUSH);
        (*flush).r = GpuRect { x, y, width: w, height: h };
        (*flush).resource_id = 1;
        (*flush).padding = 0;

        // Response buffers (zeroed)
        core::ptr::write_bytes(cmd_virt.add(128), 0, 64);
    }

    // Submit: 4 chained descriptors (transfer cmd, resp1, flush cmd, resp2)
    let q = &mut state.controlq;
    if let Some(d0) = q.alloc_desc() {
        if let Some(d1) = q.alloc_desc() {
            if let Some(d2) = q.alloc_desc() {
                if let Some(d3) = q.alloc_desc() {
                    // Transfer command
                    q.set_desc(d0, cmd_phys as u64,
                        core::mem::size_of::<GpuTransferToHost2D>() as u32,
                        VRING_DESC_F_NEXT, d1);
                    // Transfer response
                    q.set_desc(d1, (cmd_phys + 128) as u64,
                        core::mem::size_of::<GpuRespHdr>() as u32,
                        VRING_DESC_F_WRITE, 0);
                    // Flush command
                    q.set_desc(d2, (cmd_phys + 64) as u64,
                        core::mem::size_of::<GpuResourceFlush>() as u32,
                        VRING_DESC_F_NEXT, d3);
                    // Flush response
                    q.set_desc(d3, (cmd_phys + 160) as u64,
                        core::mem::size_of::<GpuRespHdr>() as u32,
                        VRING_DESC_F_WRITE, 0);

                    // Submit both as separate available ring entries
                    q.submit(d0);
                    q.submit(d2);

                    // Ring doorbell (async — don't wait)
                    write_io16(state.io_base, VIRTIO_PCI_QUEUE_NOTIFY, 0);
                } else { q.free_desc(d2); q.free_desc(d1); q.free_desc(d0); }
            } else { q.free_desc(d1); q.free_desc(d0); }
        } else { q.free_desc(d0); }
    }
}

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

// ── Internal: GPU Commands ──────────────────────────────────────────────────

fn make_hdr(cmd_type: u32) -> GpuCtrlHdr {
    GpuCtrlHdr {
        cmd_type,
        flags: 0,
        fence_id: 0,
        ctx_id: 0,
        _padding: 0,
    }
}

fn get_display_info(state: &mut GpuState) -> Result<(u32, u32), &'static str> {
    let cmd_phys = physical::alloc_page().ok_or("alloc failed")?;
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    unsafe {
        // Command
        let cmd = cmd_virt as *mut GpuGetDisplayInfo;
        (*cmd).hdr = make_hdr(VIRTIO_GPU_CMD_GET_DISPLAY_INFO);

        // Response (after command)
        let resp_offset = core::mem::size_of::<GpuGetDisplayInfo>();
        core::ptr::write_bytes(cmd_virt.add(resp_offset), 0,
            core::mem::size_of::<GpuRespDisplayInfo>());
    }

    // Submit synchronously (init only — blocking is OK)
    submit_and_wait(state, cmd_phys,
        core::mem::size_of::<GpuGetDisplayInfo>(),
        cmd_phys + core::mem::size_of::<GpuGetDisplayInfo>(),
        core::mem::size_of::<GpuRespDisplayInfo>())?;

    // Read response
    let resp = unsafe {
        &*((hhdm + cmd_phys + core::mem::size_of::<GpuGetDisplayInfo>())
            as *const GpuRespDisplayInfo)
    };

    if resp.hdr.cmd_type != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
        return Err("GET_DISPLAY_INFO failed");
    }

    // Find first enabled display
    for pmode in &resp.pmodes {
        if pmode.enabled != 0 && pmode.r.width > 0 && pmode.r.height > 0 {
            return Ok((pmode.r.width, pmode.r.height));
        }
    }

    // Default fallback
    Ok((1024, 768))
}

fn create_framebuffer_resource(state: &mut GpuState) -> Result<(), &'static str> {
    let cmd_phys = physical::alloc_page().ok_or("alloc")?;
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    unsafe {
        let cmd = cmd_virt as *mut GpuResourceCreate2D;
        (*cmd).hdr = make_hdr(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D);
        (*cmd).resource_id = 1;
        (*cmd).format = VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM;
        (*cmd).width = state.width;
        (*cmd).height = state.height;

        let resp_off = core::mem::size_of::<GpuResourceCreate2D>();
        core::ptr::write_bytes(cmd_virt.add(resp_off), 0, 24);
    }

    submit_and_wait(state, cmd_phys,
        core::mem::size_of::<GpuResourceCreate2D>(),
        cmd_phys + core::mem::size_of::<GpuResourceCreate2D>(), 24)
}

fn attach_framebuffer_backing(state: &mut GpuState) -> Result<(), &'static str> {
    let fb_size = (state.width * state.height * 4) as usize;
    let num_pages = (fb_size + 4095) / 4096;

    // Allocate physical pages (scatter-gather — NOT contiguous)
    let mut pages = Vec::with_capacity(num_pages);
    for _ in 0..num_pages {
        let page = physical::alloc_page().ok_or("FB page alloc failed")?;
        // Zero the page
        let hhdm = crate::memory::paging::hhdm_offset();
        unsafe { core::ptr::write_bytes((hhdm + page) as *mut u8, 0, 4096); }
        pages.push(page);
    }

    crate::serial_str!("[VIRTIO_GPU] Allocated ");
    crate::drivers::serial::write_dec(num_pages as u32);
    crate::serial_str!(" pages for ");
    crate::drivers::serial::write_dec((fb_size / 1024) as u32);
    crate::serial_str!("KB framebuffer\n");

    // Build ATTACH_BACKING command with scatter-gather list
    // Need: header (24 bytes) + nr_entries × GpuMemEntry (16 bytes each)
    let entries_size = num_pages * core::mem::size_of::<GpuMemEntry>();
    let cmd_size = core::mem::size_of::<GpuResourceAttachBacking>() + entries_size;
    let total_pages_needed = (cmd_size + 24 + 4095) / 4096; // +24 for response

    let cmd_phys = physical::alloc_page().ok_or("cmd alloc")?;
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    unsafe {
        let cmd = cmd_virt as *mut GpuResourceAttachBacking;
        (*cmd).hdr = make_hdr(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING);
        (*cmd).resource_id = 1;
        (*cmd).nr_entries = num_pages as u32;

        // Write scatter-gather entries after the header
        let entries_ptr = cmd_virt.add(core::mem::size_of::<GpuResourceAttachBacking>())
            as *mut GpuMemEntry;
        for (i, &page_phys) in pages.iter().enumerate() {
            let remaining = fb_size.saturating_sub(i * 4096);
            (*entries_ptr.add(i)) = GpuMemEntry {
                addr: page_phys as u64,
                length: remaining.min(4096) as u32,
                padding: 0,
            };
        }

        // Response after command + entries
        let resp_off = core::mem::size_of::<GpuResourceAttachBacking>() + entries_size;
        core::ptr::write_bytes(cmd_virt.add(resp_off), 0, 24);
    }

    let resp_off = core::mem::size_of::<GpuResourceAttachBacking>() + entries_size;
    submit_and_wait(state, cmd_phys, resp_off, cmd_phys + resp_off, 24)?;

    state.fb_phys_pages = pages;
    Ok(())
}

fn set_scanout(state: &mut GpuState) -> Result<(), &'static str> {
    let cmd_phys = physical::alloc_page().ok_or("alloc")?;
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    unsafe {
        let cmd = cmd_virt as *mut GpuSetScanout;
        (*cmd).hdr = make_hdr(VIRTIO_GPU_CMD_SET_SCANOUT);
        (*cmd).r = GpuRect { x: 0, y: 0, width: state.width, height: state.height };
        (*cmd).scanout_id = 0;
        (*cmd).resource_id = 1;

        let resp_off = core::mem::size_of::<GpuSetScanout>();
        core::ptr::write_bytes(cmd_virt.add(resp_off), 0, 24);
    }

    submit_and_wait(state, cmd_phys,
        core::mem::size_of::<GpuSetScanout>(),
        cmd_phys + core::mem::size_of::<GpuSetScanout>(), 24)
}

// ── VirtQueue Helpers ───────────────────────────────────────────────────────

/// Submit a command synchronously (for init only). Blocks until completion.
fn submit_and_wait(
    state: &mut GpuState,
    cmd_phys: usize,
    cmd_size: usize,
    resp_phys: usize,
    resp_size: usize,
) -> Result<(), &'static str> {
    let q = &mut state.controlq;
    recycle_used(q);

    let d0 = q.alloc_desc().ok_or("no descriptors")?;
    let d1 = q.alloc_desc().ok_or("no descriptors")?;

    q.set_desc(d0, cmd_phys as u64, cmd_size as u32, VRING_DESC_F_NEXT, d1);
    q.set_desc(d1, resp_phys as u64, resp_size as u32, VRING_DESC_F_WRITE, 0);

    q.submit(d0);
    write_io16(state.io_base, VIRTIO_PCI_QUEUE_NOTIFY, 0);

    // Wait for completion (init only — blocking OK)
    for _ in 0..10_000_000u64 {
        if q.has_used() {
            recycle_used(q);
            return Ok(());
        }
        core::hint::spin_loop();
    }

    Err("GPU command timeout")
}

/// Recycle all used descriptors back to free pool.
fn recycle_used(q: &mut Virtqueue) {
    while q.has_used() {
        if let Some((head_idx, _len)) = q.pop_used() {
            // Free the descriptor chain
            let mut idx = head_idx;
            loop {
                let next = q.desc_next(idx);
                let has_next = q.desc_has_next(idx);
                q.free_desc(idx);
                if !has_next { break; }
                idx = next;
            }
        }
    }
}

// ── Port I/O Helpers ────────────────────────────────────────────────────────

fn read_io32(base: u16, offset: u16) -> u32 {
    unsafe { Port::<u32>::new(base + offset).read() }
}

fn write_io32(base: u16, offset: u16, val: u32) {
    unsafe { Port::<u32>::new(base + offset).write(val) }
}

fn read_io16(base: u16, offset: u16) -> u16 {
    unsafe { Port::<u16>::new(base + offset).read() }
}

fn write_io16(base: u16, offset: u16, val: u16) {
    unsafe { Port::<u16>::new(base + offset).write(val) }
}

fn read_io8(base: u16, offset: u16) -> u8 {
    unsafe { Port::<u8>::new(base + offset).read() }
}

fn write_io8(base: u16, offset: u16, val: u8) {
    unsafe { Port::<u8>::new(base + offset).write(val) }
}
