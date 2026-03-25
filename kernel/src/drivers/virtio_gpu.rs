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

// ── VirtIO Device Status Bits ────────────────────────────────────────────────

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
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

/// VirtIO Modern MMIO register access
struct MmioTransport {
    common_base: usize,   // Virtual address of common config MMIO region
    notify_base: usize,   // Virtual address of notify MMIO region
    notify_mul: u32,      // Notify offset multiplier
    notify_off: u16,      // Queue 0's notify offset (from Q_NOFF register)
    isr_base: usize,      // Virtual address of ISR region
    device_base: usize,   // Virtual address of device-specific config
}

impl MmioTransport {
    fn read_common32(&self, off: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.common_base + off) as *const u32) }
    }
    fn write_common32(&self, off: usize, val: u32) {
        unsafe { core::ptr::write_volatile((self.common_base + off) as *mut u32, val) }
    }
    fn read_common16(&self, off: usize) -> u16 {
        unsafe { core::ptr::read_volatile((self.common_base + off) as *const u16) }
    }
    fn write_common16(&self, off: usize, val: u16) {
        unsafe { core::ptr::write_volatile((self.common_base + off) as *mut u16, val) }
    }
    fn read_common8(&self, off: usize) -> u8 {
        unsafe { core::ptr::read_volatile((self.common_base + off) as *const u8) }
    }
    fn write_common8(&self, off: usize, val: u8) {
        unsafe { core::ptr::write_volatile((self.common_base + off) as *mut u8, val) }
    }
    fn write_common64(&self, off: usize, val: u64) {
        unsafe { core::ptr::write_volatile((self.common_base + off) as *mut u64, val) }
    }
    fn read_common64(&self, off: usize) -> u64 {
        unsafe { core::ptr::read_volatile((self.common_base + off) as *const u64) }
    }
    fn notify_queue(&self, _queue_idx: u16) {
        // Modern VirtIO: notify address = notify_base + Q_NOFF * notify_off_multiplier
        let off = self.notify_off as usize * self.notify_mul as usize;
        let addr = self.notify_base + off;
        // Log BEFORE write in case MMIO write triggers page fault
        crate::serial_str!("[VIRTIO_GPU] notify_write @ ");
        crate::drivers::serial::write_hex(addr as u64);
        crate::serial_str!(" (base=");
        crate::drivers::serial::write_hex(self.notify_base as u64);
        crate::serial_str!(" off=");
        crate::drivers::serial::write_dec(off as u32);
        crate::serial_str!(")\n");
        // Modern VirtIO: notify register is le32 (NOT le16 like Legacy!)
        unsafe { core::ptr::write_volatile(addr as *mut u32, 0) }
        crate::serial_str!("[VIRTIO_GPU] notify_write done\n");
    }
}

// Modern VirtIO Common Config register offsets
// Modern VirtIO Common Config — OASIS VirtIO v1.0 spec offsets
// Note: device_status (0x14) is u8, config_generation (0x15) is u8
const VIRTIO_PCI_COMMON_DFSELECT: usize = 0x00;  // u32
const VIRTIO_PCI_COMMON_DF: usize = 0x04;        // u32
const VIRTIO_PCI_COMMON_GFSELECT: usize = 0x08;  // u32
const VIRTIO_PCI_COMMON_GF: usize = 0x0C;        // u32
const VIRTIO_PCI_COMMON_MSIX: usize = 0x10;      // u16
const VIRTIO_PCI_COMMON_NUMQ: usize = 0x12;      // u16
const VIRTIO_PCI_COMMON_STATUS: usize = 0x14;     // u8
const VIRTIO_PCI_COMMON_CFGGEN: usize = 0x15;     // u8
const VIRTIO_PCI_COMMON_Q_SELECT: usize = 0x16;   // u16
const VIRTIO_PCI_COMMON_Q_SIZE: usize = 0x18;     // u16
const VIRTIO_PCI_COMMON_Q_MSIX: usize = 0x1A;     // u16
const VIRTIO_PCI_COMMON_Q_ENABLE: usize = 0x1C;   // u16
const VIRTIO_PCI_COMMON_Q_NOFF: usize = 0x1E;     // u16
const VIRTIO_PCI_COMMON_Q_DESCLO: usize = 0x20;   // u32
const VIRTIO_PCI_COMMON_Q_DESCHI: usize = 0x24;   // u32
const VIRTIO_PCI_COMMON_Q_AVAILLO: usize = 0x28;  // u32
const VIRTIO_PCI_COMMON_Q_AVAILHI: usize = 0x2C;  // u32
const VIRTIO_PCI_COMMON_Q_USEDLO: usize = 0x30;   // u32
const VIRTIO_PCI_COMMON_Q_USEDHI: usize = 0x34;   // u32

struct GpuState {
    transport: MmioTransport,
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

    // Enable bus mastering for DMA
    pci::enable_bus_master(pci_dev.bus, pci_dev.device, pci_dev.function);

    // ── Parse PCI Capabilities for Modern VirtIO MMIO transport ─────────
    let mut transport = parse_virtio_capabilities(&pci_dev)?;
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
                    state.transport.notify_queue(0);
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

// ── PCI Capabilities Parsing ────────────────────────────────────────────────

/// Parse VirtIO Modern PCI Capabilities to find MMIO register regions.
fn parse_virtio_capabilities(dev: &PciDevice) -> Result<MmioTransport, &'static str> {
    let hhdm = crate::memory::paging::hhdm_offset();

    // Check if device has capabilities list
    let status = pci::pci_read16(dev.bus, dev.device, dev.function, 0x06);
    if status & (1 << 4) == 0 {
        return Err("no PCI capabilities");
    }

    let mut cap_ptr = pci::pci_read8(dev.bus, dev.device, dev.function, 0x34) as u8;
    cap_ptr &= 0xFC; // Align to 4 bytes

    let mut common_bar: Option<(u8, u32, u32)> = None; // (bar, offset, length)
    let mut notify_bar: Option<(u8, u32, u32, u32)> = None; // (bar, offset, length, multiplier)
    let mut isr_bar: Option<(u8, u32, u32)> = None;
    let mut device_bar: Option<(u8, u32, u32)> = None;

    let mut iterations = 0;
    while cap_ptr != 0 && iterations < 32 {
        iterations += 1;
        let cap_id = pci::pci_read8(dev.bus, dev.device, dev.function, cap_ptr as u8);
        let cap_next = pci::pci_read8(dev.bus, dev.device, dev.function, cap_ptr + 1);

        if cap_id == 0x09 { // VirtIO vendor capability
            let cfg_type = pci::pci_read8(dev.bus, dev.device, dev.function, cap_ptr + 3);
            let bar = pci::pci_read8(dev.bus, dev.device, dev.function, cap_ptr + 4);
            let offset = pci::pci_read32(dev.bus, dev.device, dev.function, cap_ptr + 8);
            let length = pci::pci_read32(dev.bus, dev.device, dev.function, cap_ptr + 12);

            crate::serial_str!("[VIRTIO_GPU] Cap type=");
            crate::drivers::serial::write_dec(cfg_type as u32);
            crate::serial_str!(" bar=");
            crate::drivers::serial::write_dec(bar as u32);
            crate::serial_str!(" off=0x");
            crate::drivers::serial::write_hex(offset as u64);
            crate::serial_str!(" len=");
            crate::drivers::serial::write_dec(length);
            crate::drivers::serial::write_newline();

            match cfg_type {
                1 => common_bar = Some((bar, offset, length)),  // Common config
                2 => {
                    let mul = pci::pci_read32(dev.bus, dev.device, dev.function, cap_ptr + 16);
                    notify_bar = Some((bar, offset, length, mul));
                }
                3 => isr_bar = Some((bar, offset, length)),     // ISR
                4 => device_bar = Some((bar, offset, length)),  // Device config
                _ => {}
            }
        }

        cap_ptr = cap_next & 0xFC;
    }

    let (common_b, common_off, _) = common_bar.ok_or("no common config cap")?;
    let (notify_b, notify_off, _, notify_mul) = notify_bar.ok_or("no notify cap")?;
    let (isr_b, isr_off, _) = isr_bar.ok_or("no ISR cap")?;

    // Resolve BAR physical addresses and map to virtual
    let common_phys = resolve_bar_phys(dev, common_b)? + common_off as usize;
    let notify_phys = resolve_bar_phys(dev, notify_b)? + notify_off as usize;
    let isr_phys = resolve_bar_phys(dev, isr_b)? + isr_off as usize;

    // Map MMIO pages (uncacheable)
    use x86_64::structures::paging::PageTableFlags;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE | PageTableFlags::NO_CACHE;
    // Map ALL pages of the BAR that contains common config (typically 4 pages = 16KB)
    let bar_base = common_phys & !0xFFF;
    let bar_size = 16384usize; // 4 pages covers all cap regions
    let mmio_pages: alloc::vec::Vec<usize> = (0..bar_size).step_by(4096).map(|off| bar_base + off).collect();
    for &phys in &mmio_pages {
        crate::serial_str!("[VIRTIO_GPU] Mapping MMIO phys=");
        crate::drivers::serial::write_hex(phys as u64);
        crate::serial_str!(" -> virt=");
        crate::drivers::serial::write_hex((hhdm + phys) as u64);
        match crate::memory::paging::map_page(hhdm + phys, phys, flags) {
            Ok(()) => crate::serial_str!(" OK\n"),
            Err(_) => crate::serial_str!(" FAILED (already mapped?)\n"),
        }
    }

    let common_base = hhdm + common_phys;
    let notify_base = hhdm + notify_phys;
    let isr_base = hhdm + isr_phys;

    let device_base = if let Some((db, doff, _)) = device_bar {
        let dp = resolve_bar_phys(dev, db)? + doff as usize;
        let _ = crate::memory::paging::map_page(hhdm + (dp & !0xFFF), dp & !0xFFF, flags);
        hhdm + dp
    } else {
        0
    };

    Ok(MmioTransport {
        common_base,
        notify_base,
        notify_mul,
        notify_off: 0, // Will be set after queue setup
        isr_base,
        device_base,
    })
}

/// Resolve a BAR index to its physical base address
fn resolve_bar_phys(dev: &PciDevice, bar_idx: u8) -> Result<usize, &'static str> {
    match pci::decode_bar(dev, bar_idx as usize) {
        BarType::Mmio32 { base, .. } => Ok(base as usize),
        BarType::Mmio64 { base, .. } => Ok(base as usize),
        BarType::Io { .. } => Err("unexpected I/O BAR for MMIO transport"),
        BarType::None => Err("BAR not present"),
    }
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

    crate::serial_str!("[VIRTIO_GPU] submit: d0=");
    crate::drivers::serial::write_dec(d0 as u32);
    crate::serial_str!(" d1=");
    crate::drivers::serial::write_dec(d1 as u32);
    crate::serial_str!(" cmd_phys=");
    crate::drivers::serial::write_hex(cmd_phys as u64);
    crate::serial_str!(" cmd_size=");
    crate::drivers::serial::write_dec(cmd_size as u32);
    crate::serial_str!(" resp_phys=");
    crate::drivers::serial::write_hex(resp_phys as u64);
    crate::drivers::serial::write_newline();

    crate::serial_str!("[VIRTIO_GPU] desc_phys=");
    crate::drivers::serial::write_hex(q.desc_phys());
    crate::serial_str!(" avail_phys=");
    crate::drivers::serial::write_hex(q.avail_phys());
    crate::serial_str!(" used_phys=");
    crate::drivers::serial::write_hex(q.used_phys());
    crate::drivers::serial::write_newline();

    q.submit(d0);

    crate::serial_str!("[VIRTIO_GPU] Notifying queue 0...\n");
    state.transport.notify_queue(0);

    crate::serial_str!("[VIRTIO_GPU] avail_idx=");
    crate::drivers::serial::write_dec(q.next_avail as u32);
    crate::serial_str!(" last_used=");
    crate::drivers::serial::write_dec(q.last_used_idx as u32);
    crate::serial_str!(" dev_used=");
    crate::drivers::serial::write_dec(q.used_idx() as u32);
    crate::serial_str!(" q_size=");
    crate::drivers::serial::write_dec(q.queue_size as u32);
    crate::drivers::serial::write_newline();
    crate::serial_str!("[VIRTIO_GPU] Waiting for used ring (polling)...\n");

    // Wait for completion (init only — blocking OK)
    for i in 0..100_000_000u64 {
        if q.has_used() {
            crate::serial_str!("[VIRTIO_GPU] Got used at iteration ");
            crate::drivers::serial::write_dec(i as u32);
            crate::drivers::serial::write_newline();
            recycle_used(q);
            return Ok(());
        }
        if i % 10_000_000 == 0 && i > 0 {
            crate::serial_str!("[VIRTIO_GPU] Still waiting... ");
            crate::drivers::serial::write_dec((i / 1_000_000) as u32);
            crate::serial_str!("M iters\n");
        }
        core::hint::spin_loop();
    }

    crate::serial_str!("[VIRTIO_GPU] TIMEOUT after 100M iterations\n");
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
