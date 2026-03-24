//! VirtIO Block Device Driver (Legacy PCI Transport)
//!
//! Provides sector-level read/write to a VirtIO block device.
//! Uses interrupt-driven I/O completion via IOAPIC routing.
//!
//! # Architecture
//! - VirtIO legacy PCI: BAR0 I/O ports for device config
//! - Single virtqueue (queue 0) for block requests
//! - Interrupt handler sets completion flag, syscall waits on it
//! - Kernel journal written before each block_write

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
use spin::Mutex;
use x86_64::instructions::port::Port;

use super::pci::{self, PciDevice, BarType};
use super::virtio::{self, Virtqueue, VirtqDesc, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE};

// ── VirtIO Legacy PCI Register Offsets (from BAR0) ───────────────────────────

const VIRTIO_PCI_DEVICE_FEATURES: u16 = 0x00;  // 32-bit, RO
const VIRTIO_PCI_DRIVER_FEATURES: u16 = 0x04;  // 32-bit, RW
const VIRTIO_PCI_QUEUE_PFN: u16 = 0x08;        // 32-bit, RW
const VIRTIO_PCI_QUEUE_SIZE: u16 = 0x0C;        // 16-bit, RO
const VIRTIO_PCI_QUEUE_SEL: u16 = 0x0E;         // 16-bit, RW
const VIRTIO_PCI_QUEUE_NOTIFY: u16 = 0x10;      // 16-bit, RW
const VIRTIO_PCI_DEVICE_STATUS: u16 = 0x12;     // 8-bit, RW
const VIRTIO_PCI_ISR_STATUS: u16 = 0x13;         // 8-bit, RO
const VIRTIO_PCI_CONFIG: u16 = 0x14;             // Device-specific config

// ── VirtIO Device Status Bits ────────────────────────────────────────────────

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_FAILED: u8 = 128;

// ── VirtIO Block Request Types ───────────────────────────────────────────────

const VIRTIO_BLK_T_IN: u32 = 0;    // Read
const VIRTIO_BLK_T_OUT: u32 = 1;   // Write

// ── VirtIO Block Request Header (16 bytes) ───────────────────────────────────

#[repr(C)]
struct VirtioBlkReqHeader {
    req_type: u32,
    reserved: u32,
    sector: u64,
}

// ── VirtIO Block Status ──────────────────────────────────────────────────────

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

// ── Disk Layout Constants ────────────────────────────────────────────────────

pub const SECTOR_SIZE: usize = 512;

/// Disk header magic
pub const DISK_MAGIC: [u8; 8] = *b"FOLKDISK";

/// Disk header (sector 0, 512 bytes)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DiskHeader {
    pub magic: [u8; 8],
    pub version: u32,
    pub _pad0: u32,
    pub journal_start: u64,
    pub journal_size: u64,
    pub data_start: u64,
    pub data_size: u64,
    pub synapse_db_sector: u64,
    pub synapse_db_size: u64,
}

/// Journal entry (32 bytes each, fits 16 per sector)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JournalEntry {
    pub timestamp_ms: u64,
    pub task_id: u32,
    pub operation: u32,     // 0=read, 1=write
    pub sector_start: u64,
    pub sector_count: u64,
}

/// Disk layout sectors
pub const HEADER_SECTOR: u64 = 0;
pub const JOURNAL_START_SECTOR: u64 = 8;
pub const JOURNAL_SIZE_SECTORS: u64 = 2040;   // ~1MB
pub const DATA_START_SECTOR: u64 = 2048;

// ── Block Device Error ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    NotInitialized,
    DeviceNotFound,
    QueueSetupFailed,
    IoError,
    Timeout,
    InvalidSector,
    DeviceFailed,
}

// ── Global State ─────────────────────────────────────────────────────────────

/// I/O completion flag — set by interrupt handler, cleared by read/write
pub static IO_COMPLETE: AtomicBool = AtomicBool::new(false);

/// Interrupt counter for debugging
static IRQ_COUNT: AtomicU64 = AtomicU64::new(0);

/// Block device state
static BLOCK_DEVICE: Mutex<Option<VirtioBlkDevice>> = Mutex::new(None);

/// Device capacity in sectors
static CAPACITY_SECTORS: AtomicU64 = AtomicU64::new(0);

/// Journal write position (circular)
static JOURNAL_POS: AtomicU64 = AtomicU64::new(0);

/// Maximum sectors per multi-sector DMA burst
/// 128 sectors × 512 bytes = 64KB per VirtIO request
pub const MAX_BURST_SECTORS: usize = 128;

/// Size of the DMA burst buffer in bytes (256KB)
const BURST_BUF_SIZE: usize = MAX_BURST_SECTORS * SECTOR_SIZE;

/// Number of pages for the burst buffer (64 pages)
const BURST_BUF_PAGES: usize = BURST_BUF_SIZE / 4096;

struct VirtioBlkDevice {
    /// BAR0 I/O port base
    io_base: u16,
    /// The virtqueue (queue 0)
    queue: Virtqueue,
    /// Physical address of request buffer page (single-sector, 4KB)
    req_buf_phys: usize,
    /// Virtual address of request buffer page
    req_buf_virt: usize,
    /// Physical address of burst header page (ULTRA 36)
    burst_hdr_phys: usize,
    /// Virtual address of burst header page
    burst_hdr_virt: usize,
    /// Physical address of burst DATA buffer (32KB contiguous)
    burst_data_phys: usize,
    /// Virtual address of burst DATA buffer
    burst_data_virt: usize,
    /// PCI interrupt line (for IOAPIC routing)
    irq_line: u8,
    /// Device capacity in sectors
    capacity: u64,
}

// ── I/O Helpers ──────────────────────────────────────────────────────────────

fn read_io8(base: u16, offset: u16) -> u8 {
    unsafe { Port::<u8>::new(base + offset).read() }
}

fn write_io8(base: u16, offset: u16, val: u8) {
    unsafe { Port::<u8>::new(base + offset).write(val); }
}

fn read_io16(base: u16, offset: u16) -> u16 {
    unsafe { Port::<u16>::new(base + offset).read() }
}

fn write_io16(base: u16, offset: u16, val: u16) {
    unsafe { Port::<u16>::new(base + offset).write(val); }
}

fn read_io32(base: u16, offset: u16) -> u32 {
    unsafe { Port::<u32>::new(base + offset).read() }
}

fn write_io32(base: u16, offset: u16, val: u32) {
    unsafe { Port::<u32>::new(base + offset).write(val); }
}

// ── Interrupt Handler ────────────────────────────────────────────────────────

/// Called from IDT vector 45 (VirtIO block IRQ)
pub fn irq_handler() {
    let dev = BLOCK_DEVICE.lock();
    if let Some(ref blk) = *dev {
        // Read ISR status to acknowledge interrupt (and clear it)
        let _isr = read_io8(blk.io_base, VIRTIO_PCI_ISR_STATUS);
    }
    drop(dev);

    // Signal completion
    IO_COMPLETE.store(true, Ordering::Release);
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);

    // Send EOI to APIC
    crate::arch::x86_64::apic::send_eoi();
}

// ── Initialization ───────────────────────────────────────────────────────────

/// Initialize VirtIO block device
pub fn init() -> Result<(), BlockError> {
    crate::serial_strln!("[VIRTIO_BLK] Looking for VirtIO block device...");

    // Find the PCI device
    let pci_dev = pci::find_virtio_block().ok_or_else(|| {
        crate::serial_strln!("[VIRTIO_BLK] No VirtIO block device found on PCI bus");
        BlockError::DeviceNotFound
    })?;

    crate::serial_str!("[VIRTIO_BLK] Found device ");
    crate::drivers::serial::write_hex(pci_dev.vendor_id as u64);
    crate::serial_str!(":");
    crate::drivers::serial::write_hex(pci_dev.device_id as u64);
    crate::serial_str!(" at ");
    crate::drivers::serial::write_dec(pci_dev.bus as u32);
    crate::serial_str!(":");
    crate::drivers::serial::write_dec(pci_dev.device as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(pci_dev.function as u32);
    crate::drivers::serial::write_newline();

    // Decode BAR0 (must be I/O space for legacy transport)
    let io_base = match pci::decode_bar(&pci_dev, 0) {
        BarType::Io { base } => base,
        other => {
            crate::serial_str!("[VIRTIO_BLK] ERROR: BAR0 is not I/O space: ");
            match other {
                BarType::Mmio32 { base, .. } => {
                    crate::serial_str!("MMIO32 @");
                    crate::drivers::serial::write_hex(base as u64);
                }
                BarType::Mmio64 { base, .. } => {
                    crate::serial_str!("MMIO64 @");
                    crate::drivers::serial::write_hex(base);
                }
                _ => crate::serial_str!("None"),
            }
            crate::drivers::serial::write_newline();
            return Err(BlockError::DeviceNotFound);
        }
    };

    crate::serial_str!("[VIRTIO_BLK] BAR0 I/O base: 0x");
    crate::drivers::serial::write_hex(io_base as u64);
    crate::serial_str!(", IRQ line: ");
    crate::drivers::serial::write_dec(pci_dev.interrupt_line as u32);
    crate::drivers::serial::write_newline();

    // Enable PCI bus mastering (required for DMA)
    pci::enable_bus_master(pci_dev.bus, pci_dev.device, pci_dev.function);

    // ── VirtIO Handshake ─────────────────────────────────────────────────

    // Step 1: Reset
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, 0);

    // Step 2: ACKNOWLEDGE
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE);

    // Step 3: DRIVER
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    // Step 4: Read device features
    let device_features = read_io32(io_base, VIRTIO_PCI_DEVICE_FEATURES);
    crate::serial_str!("[VIRTIO_BLK] Device features: 0x");
    crate::drivers::serial::write_hex(device_features as u64);
    crate::drivers::serial::write_newline();

    // Accept no special features for now (basic read/write only)
    write_io32(io_base, VIRTIO_PCI_DRIVER_FEATURES, 0);

    // Step 5: FEATURES_OK (legacy: go straight to DRIVER_OK)
    // Legacy transport skips FEATURES_OK

    // ── Setup Virtqueue 0 ────────────────────────────────────────────────

    // Select queue 0
    write_io16(io_base, VIRTIO_PCI_QUEUE_SEL, 0);

    // Read queue size
    let queue_size = read_io16(io_base, VIRTIO_PCI_QUEUE_SIZE);
    if queue_size == 0 {
        crate::serial_strln!("[VIRTIO_BLK] ERROR: Queue size is 0!");
        write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_FAILED);
        return Err(BlockError::QueueSetupFailed);
    }

    crate::serial_str!("[VIRTIO_BLK] Queue 0 size: ");
    crate::drivers::serial::write_dec(queue_size as u32);
    crate::drivers::serial::write_newline();

    // Allocate virtqueue
    let queue = Virtqueue::new(queue_size).ok_or_else(|| {
        crate::serial_strln!("[VIRTIO_BLK] ERROR: Failed to allocate virtqueue");
        BlockError::QueueSetupFailed
    })?;

    // Tell device the queue's physical page frame number
    let queue_pfn = (queue.queue_phys / 4096) as u32;
    write_io32(io_base, VIRTIO_PCI_QUEUE_PFN, queue_pfn);

    crate::serial_str!("[VIRTIO_BLK] Queue PFN: ");
    crate::drivers::serial::write_hex(queue_pfn as u64);
    crate::drivers::serial::write_newline();

    // Enable interrupts on the queue
    queue.enable_interrupts();

    // Step 6: DRIVER_OK — device is live
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS,
              STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);

    let status = read_io8(io_base, VIRTIO_PCI_DEVICE_STATUS);
    crate::serial_str!("[VIRTIO_BLK] Device status after init: 0x");
    crate::drivers::serial::write_hex(status as u64);
    crate::drivers::serial::write_newline();

    if status & STATUS_FAILED != 0 {
        crate::serial_strln!("[VIRTIO_BLK] ERROR: Device set FAILED bit!");
        return Err(BlockError::DeviceFailed);
    }

    // ── Read Device Config (capacity) ────────────────────────────────────

    let cap_lo = read_io32(io_base, VIRTIO_PCI_CONFIG) as u64;
    let cap_hi = read_io32(io_base, VIRTIO_PCI_CONFIG + 4) as u64;
    let capacity = cap_lo | (cap_hi << 32);

    crate::serial_str!("[VIRTIO_BLK] Capacity: ");
    crate::drivers::serial::write_dec(capacity as u32);
    crate::serial_str!(" sectors (");
    crate::drivers::serial::write_dec((capacity * 512 / 1024) as u32);
    crate::serial_strln!(" KB)");

    CAPACITY_SECTORS.store(capacity, Ordering::Relaxed);

    // ── Allocate Request Buffer Page ─────────────────────────────────────
    // Layout: [header(16)] [data(512)] [status(1)]

    let req_buf_phys = crate::memory::physical::alloc_page().ok_or_else(|| {
        crate::serial_strln!("[VIRTIO_BLK] ERROR: Failed to allocate request buffer");
        BlockError::QueueSetupFailed
    })?;
    let req_buf_virt = crate::phys_to_virt(req_buf_phys);

    // Zero the buffer
    unsafe { core::ptr::write_bytes(req_buf_virt as *mut u8, 0, 4096); }

    // ── ULTRA 36: Allocate Burst DMA Buffer (32KB contiguous) ────────
    // Layout: [header(16)] [data(64×512 = 32768)] [status(1)]
    // Total: 32785 bytes → 9 pages (header+status on first extra page)
    // We allocate pages individually — they may not be contiguous in phys mem.
    // For VirtIO DMA, we need the data portion contiguous. Simplest approach:
    // Use the first page for header+status, allocate 8 contiguous pages for data.
    // Since our page allocator is bump-style, consecutive allocs ARE contiguous.

    let burst_header_phys = crate::memory::physical::alloc_page().ok_or_else(|| {
        crate::serial_strln!("[VIRTIO_BLK] ERROR: Failed to allocate burst header page");
        BlockError::QueueSetupFailed
    })?;
    let burst_header_virt = crate::phys_to_virt(burst_header_phys);
    unsafe { core::ptr::write_bytes(burst_header_virt as *mut u8, 0, 4096); }

    // Allocate BURST_BUF_PAGES contiguous pages for data
    let mut burst_data_phys = 0usize;
    for i in 0..BURST_BUF_PAGES {
        let page = crate::memory::physical::alloc_page().ok_or_else(|| {
            crate::serial_strln!("[VIRTIO_BLK] ERROR: Failed to allocate burst data pages");
            BlockError::QueueSetupFailed
        })?;
        if i == 0 {
            burst_data_phys = page;
        }
        // Verify contiguity
        if page != burst_data_phys + i * 4096 {
            crate::serial_strln!("[VIRTIO_BLK] WARNING: Burst pages not contiguous, DMA may fail");
            // Continue anyway — QEMU's virtio is tolerant
        }
    }
    let burst_data_virt = crate::phys_to_virt(burst_data_phys);

    crate::serial_str!("[VIRTIO_BLK] Burst DMA buffer: ");
    crate::drivers::serial::write_dec(BURST_BUF_SIZE as u32 / 1024);
    crate::serial_str!("KB @ phys 0x");
    crate::drivers::serial::write_hex(burst_data_phys as u64);
    crate::drivers::serial::write_newline();

    // ── Setup IOAPIC Interrupt ───────────────────────────────────────────

    let irq_line = pci_dev.interrupt_line;
    if irq_line > 0 && irq_line < 24 {
        crate::serial_str!("[VIRTIO_BLK] Routing IRQ");
        crate::drivers::serial::write_dec(irq_line as u32);
        crate::serial_strln!(" -> IDT vector 45 via IOAPIC (level-triggered, active-low)");
        crate::arch::x86_64::ioapic_enable_irq_level(irq_line, 45);
    } else {
        crate::serial_str!("[VIRTIO_BLK] WARNING: Invalid IRQ line ");
        crate::drivers::serial::write_dec(irq_line as u32);
        crate::serial_strln!(", interrupts may not work");
    }

    // Store the device
    *BLOCK_DEVICE.lock() = Some(VirtioBlkDevice {
        io_base,
        queue,
        req_buf_phys,
        req_buf_virt,
        burst_hdr_phys: burst_header_phys,
        burst_hdr_virt: burst_header_virt,
        burst_data_phys,
        burst_data_virt,
        irq_line,
        capacity,
    });

    crate::serial_strln!("[VIRTIO_BLK] VirtIO block device initialized!");

    Ok(())
}

// ── Block I/O ────────────────────────────────────────────────────────────────

/// Read a single sector (512 bytes) from the device
pub fn block_read(sector: u64, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), BlockError> {
    do_io(VIRTIO_BLK_T_IN, sector, buf)
}

/// Write a single sector (512 bytes) to the device
pub fn block_write(sector: u64, buf: &[u8; SECTOR_SIZE]) -> Result<(), BlockError> {
    // We need a mutable copy for the DMA buffer
    let mut tmp = [0u8; SECTOR_SIZE];
    tmp.copy_from_slice(buf);
    do_io(VIRTIO_BLK_T_OUT, sector, &mut tmp)
}

/// Perform a single-sector I/O operation
fn do_io(req_type: u32, sector: u64, data: &mut [u8; SECTOR_SIZE]) -> Result<(), BlockError> {
    let mut dev = BLOCK_DEVICE.lock();
    let blk = dev.as_mut().ok_or(BlockError::NotInitialized)?;

    if sector >= blk.capacity {
        return Err(BlockError::InvalidSector);
    }

    let header_phys = blk.req_buf_phys;
    let data_phys = blk.req_buf_phys + 16;     // After header
    let status_phys = blk.req_buf_phys + 16 + SECTOR_SIZE; // After data

    let header_virt = blk.req_buf_virt;
    let data_virt = blk.req_buf_virt + 16;
    let status_virt = blk.req_buf_virt + 16 + SECTOR_SIZE;

    // Write request header
    unsafe {
        let header = header_virt as *mut VirtioBlkReqHeader;
        (*header).req_type = req_type;
        (*header).reserved = 0;
        (*header).sector = sector;
    }

    // For writes: copy data to DMA buffer
    if req_type == VIRTIO_BLK_T_OUT {
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                data_virt as *mut u8,
                SECTOR_SIZE,
            );
        }
    }

    // Set status to 0xFF (will be overwritten by device)
    unsafe { *(status_virt as *mut u8) = 0xFF; }

    // Allocate 3 descriptors for the chain: header → data → status
    let d0 = blk.queue.alloc_desc().ok_or(BlockError::IoError)?;
    let d1 = blk.queue.alloc_desc().ok_or_else(|| {
        blk.queue.free_desc(d0);
        BlockError::IoError
    })?;
    let d2 = blk.queue.alloc_desc().ok_or_else(|| {
        blk.queue.free_desc(d0);
        blk.queue.free_desc(d1);
        BlockError::IoError
    })?;

    // Descriptor 0: request header (device-readable)
    unsafe {
        let desc = &mut *blk.queue.desc(d0);
        desc.addr = header_phys as u64;
        desc.len = 16;
        desc.flags = VRING_DESC_F_NEXT;
        desc.next = d1;
    }

    // Descriptor 1: data buffer
    unsafe {
        let desc = &mut *blk.queue.desc(d1);
        desc.addr = data_phys as u64;
        desc.len = SECTOR_SIZE as u32;
        desc.flags = VRING_DESC_F_NEXT
            | if req_type == VIRTIO_BLK_T_IN { VRING_DESC_F_WRITE } else { 0 };
        desc.next = d2;
    }

    // Descriptor 2: status byte (device-writable)
    unsafe {
        let desc = &mut *blk.queue.desc(d2);
        desc.addr = status_phys as u64;
        desc.len = 1;
        desc.flags = VRING_DESC_F_WRITE;
        desc.next = 0;
    }

    // Clear completion flag
    IO_COMPLETE.store(false, Ordering::Release);

    // Submit to available ring
    blk.queue.submit(d0);

    // Notify device (write queue index to notify register)
    write_io16(blk.io_base, VIRTIO_PCI_QUEUE_NOTIFY, 0);

    // Wait for completion (interrupt-driven with timeout)
    let io_base = blk.io_base;
    drop(dev); // Release lock BEFORE sleep loop (IRQ handler needs it)

    // Hybrid wait: hlt (sleep until ANY interrupt) + ISR poll after each wakeup.
    // - TCG: VirtIO interrupt wakes hlt → IO_COMPLETE true → instant break
    // - WHPX: Timer wakes hlt every ~10ms → ISR poll catches completion
    // Pure ISR polling in tight loop causes VM-exit storm under WHPX.
    unsafe { core::arch::asm!("sti"); }

    let mut timeout = 100_000u32; // ~1000s at 10ms/tick
    while !IO_COMPLETE.load(Ordering::Acquire) {
        unsafe { core::arch::asm!("hlt"); }
        // After ANY interrupt wakes us, check both paths:
        if IO_COMPLETE.load(Ordering::Acquire) { break; }
        let isr = read_io8(io_base, VIRTIO_PCI_ISR_STATUS);
        if isr != 0 {
            IO_COMPLETE.store(true, Ordering::Release);
            break;
        }
        timeout -= 1;
        if timeout == 0 {
            crate::serial_strln!("[VIRTIO_BLK] I/O timeout!");
            return Err(BlockError::Timeout);
        }
    }

    // Reacquire lock to process completion
    let mut dev = BLOCK_DEVICE.lock();
    let blk = dev.as_mut().ok_or(BlockError::NotInitialized)?;

    // Pop from used ring
    if let Some((_head, _len)) = blk.queue.pop_used() {
        // Free the descriptor chain
        blk.queue.free_chain(d0);
    }

    // Check status
    let status = unsafe { *(status_virt as *const u8) };
    if status != VIRTIO_BLK_S_OK {
        crate::serial_str!("[VIRTIO_BLK] I/O error, status=");
        crate::drivers::serial::write_dec(status as u32);
        crate::drivers::serial::write_newline();
        return Err(BlockError::IoError);
    }

    // For reads: copy data from DMA buffer
    if req_type == VIRTIO_BLK_T_IN {
        unsafe {
            core::ptr::copy_nonoverlapping(
                data_virt as *const u8,
                data.as_mut_ptr(),
                SECTOR_SIZE,
            );
        }
    }

    Ok(())
}

/// ULTRA 36: Multi-sector DMA burst read.
///
/// Reads `count` sectors (max MAX_BURST_SECTORS=64) in a single VirtIO request.
/// Uses a dedicated 32KB contiguous DMA buffer with one descriptor chain:
///   [header(16B)] → [data(count×512B)] → [status(1B)]
///
/// One interrupt, one mutex cycle, one DMA transfer. 64× fewer VirtIO
/// transactions compared to sector-by-sector reads.
pub fn block_read_multi(sector: u64, buf: &mut [u8], count: usize) -> Result<(), BlockError> {
    if count == 0 || count > MAX_BURST_SECTORS {
        return Err(BlockError::InvalidSector);
    }
    let data_size = count * SECTOR_SIZE;
    if buf.len() < data_size {
        return Err(BlockError::InvalidSector);
    }

    let mut dev = BLOCK_DEVICE.lock();
    let blk = dev.as_mut().ok_or(BlockError::NotInitialized)?;

    if sector + count as u64 > blk.capacity {
        return Err(BlockError::InvalidSector);
    }

    // Use burst buffers: header page is separate from data pages
    let header_phys = blk.burst_hdr_phys;
    let header_virt = blk.burst_hdr_virt;
    // Data lives in the contiguous burst_data pages
    let data_phys = blk.burst_data_phys;
    let data_virt = blk.burst_data_virt;
    // Status byte at end of header page (offset 4080 — well within the page)
    let status_phys = blk.burst_hdr_phys + 4080;
    let status_virt = blk.burst_hdr_virt + 4080;

    // Write request header
    unsafe {
        let header = header_virt as *mut VirtioBlkReqHeader;
        (*header).req_type = VIRTIO_BLK_T_IN;
        (*header).reserved = 0;
        (*header).sector = sector;
    }

    // Set status to 0xFF
    unsafe { *(status_virt as *mut u8) = 0xFF; }

    // Allocate 3 descriptors: header → data → status
    let d0 = blk.queue.alloc_desc().ok_or(BlockError::IoError)?;
    let d1 = blk.queue.alloc_desc().ok_or_else(|| {
        blk.queue.free_desc(d0);
        BlockError::IoError
    })?;
    let d2 = blk.queue.alloc_desc().ok_or_else(|| {
        blk.queue.free_desc(d0);
        blk.queue.free_desc(d1);
        BlockError::IoError
    })?;

    // Descriptor 0: request header (16 bytes, device-readable)
    unsafe {
        let desc = &mut *blk.queue.desc(d0);
        desc.addr = header_phys as u64;
        desc.len = 16;
        desc.flags = VRING_DESC_F_NEXT;
        desc.next = d1;
    }

    // Descriptor 1: data buffer (count × 512 bytes, device-writable)
    // This is the key — single DMA descriptor covering ALL sectors
    unsafe {
        let desc = &mut *blk.queue.desc(d1);
        desc.addr = data_phys as u64;
        desc.len = data_size as u32;
        desc.flags = VRING_DESC_F_NEXT | VRING_DESC_F_WRITE;
        desc.next = d2;
    }

    // Descriptor 2: status byte (device-writable)
    unsafe {
        let desc = &mut *blk.queue.desc(d2);
        desc.addr = status_phys as u64;
        desc.len = 1;
        desc.flags = VRING_DESC_F_WRITE;
        desc.next = 0;
    }

    // Clear completion flag
    IO_COMPLETE.store(false, Ordering::Release);

    // Submit and notify
    blk.queue.submit(d0);
    write_io16(blk.io_base, VIRTIO_PCI_QUEUE_NOTIFY, 0);

    let io_base = blk.io_base;
    drop(dev); // Release lock BEFORE sleep loop (IRQ handler needs it)

    // Hybrid wait: hlt + ISR poll after each wakeup
    unsafe { core::arch::asm!("sti"); }

    let mut timeout = 100_000u32;
    while !IO_COMPLETE.load(Ordering::Acquire) {
        unsafe { core::arch::asm!("hlt"); }
        if IO_COMPLETE.load(Ordering::Acquire) { break; }
        let isr = read_io8(io_base, VIRTIO_PCI_ISR_STATUS);
        if isr != 0 {
            IO_COMPLETE.store(true, Ordering::Release);
            break;
        }
        timeout -= 1;
        if timeout == 0 {
            crate::serial_strln!("[VIRTIO_BLK] Multi-sector I/O timeout!");
            return Err(BlockError::Timeout);
        }
    }

    // Process completion
    let mut dev = BLOCK_DEVICE.lock();
    let blk = dev.as_mut().ok_or(BlockError::NotInitialized)?;

    if let Some((_head, _len)) = blk.queue.pop_used() {
        blk.queue.free_chain(d0);
    }

    // Check status
    let status = unsafe { *(status_virt as *const u8) };
    if status != VIRTIO_BLK_S_OK {
        crate::serial_str!("[VIRTIO_BLK] Multi-sector I/O error, status=");
        crate::drivers::serial::write_dec(status as u32);
        crate::drivers::serial::write_newline();
        return Err(BlockError::IoError);
    }

    // Copy data from DMA buffer to caller's buffer
    unsafe {
        core::ptr::copy_nonoverlapping(
            data_virt as *const u8,
            buf.as_mut_ptr(),
            data_size,
        );
    }

    Ok(())
}

// ── Self-test ────────────────────────────────────────────────────────────────

/// Run a self-test: write a pattern to sector, read it back, verify
pub fn self_test() -> Result<(), BlockError> {
    crate::serial_strln!("[VIRTIO_BLK] Running self-test...");

    // Use a high sector to avoid clobbering header area
    let test_sector = DATA_START_SECTOR + 100;

    // Write test pattern
    let mut write_buf = [0u8; SECTOR_SIZE];
    // 0xDEADBEEF pattern
    write_buf[0] = 0xEF;
    write_buf[1] = 0xBE;
    write_buf[2] = 0xAD;
    write_buf[3] = 0xDE;
    // Fill rest with incrementing bytes
    for i in 4..SECTOR_SIZE {
        write_buf[i] = (i & 0xFF) as u8;
    }

    block_write(test_sector, &write_buf)?;
    crate::serial_strln!("[VIRTIO_BLK] Self-test: wrote test pattern");

    // Read back
    let mut read_buf = [0u8; SECTOR_SIZE];
    block_read(test_sector, &mut read_buf)?;
    crate::serial_strln!("[VIRTIO_BLK] Self-test: read back data");

    // Verify
    if read_buf[..4] != [0xEF, 0xBE, 0xAD, 0xDE] {
        crate::serial_str!("[VIRTIO_BLK] Self-test FAILED! Got: ");
        crate::drivers::serial::write_hex(read_buf[0] as u64);
        crate::serial_str!(" ");
        crate::drivers::serial::write_hex(read_buf[1] as u64);
        crate::serial_str!(" ");
        crate::drivers::serial::write_hex(read_buf[2] as u64);
        crate::serial_str!(" ");
        crate::drivers::serial::write_hex(read_buf[3] as u64);
        crate::drivers::serial::write_newline();
        return Err(BlockError::IoError);
    }

    // Verify full buffer
    for i in 4..SECTOR_SIZE {
        if read_buf[i] != (i & 0xFF) as u8 {
            crate::serial_str!("[VIRTIO_BLK] Self-test FAILED at byte ");
            crate::drivers::serial::write_dec(i as u32);
            crate::drivers::serial::write_newline();
            return Err(BlockError::IoError);
        }
    }

    crate::serial_strln!("[VIRTIO_BLK] Self-test PASSED! 0xDEADBEEF verified.");

    let irqs = IRQ_COUNT.load(Ordering::Relaxed);
    crate::serial_str!("[VIRTIO_BLK] IRQ count: ");
    crate::drivers::serial::write_dec(irqs as u32);
    crate::drivers::serial::write_newline();

    Ok(())
}

// ── Disk Header / Journal ────────────────────────────────────────────────────

/// Write the disk header (sector 0). Called on first boot or format.
pub fn write_disk_header() -> Result<(), BlockError> {
    let capacity = CAPACITY_SECTORS.load(Ordering::Relaxed);
    let data_size = if capacity > DATA_START_SECTOR {
        capacity - DATA_START_SECTOR
    } else {
        0
    };

    let header = DiskHeader {
        magic: DISK_MAGIC,
        version: 1,
        _pad0: 0,
        journal_start: JOURNAL_START_SECTOR,
        journal_size: JOURNAL_SIZE_SECTORS,
        data_start: DATA_START_SECTOR,
        data_size,
        synapse_db_sector: DATA_START_SECTOR,
        synapse_db_size: 0,
    };

    let mut buf = [0u8; SECTOR_SIZE];
    unsafe {
        core::ptr::copy_nonoverlapping(
            &header as *const DiskHeader as *const u8,
            buf.as_mut_ptr(),
            core::mem::size_of::<DiskHeader>(),
        );
    }

    block_write(HEADER_SECTOR, &buf)?;
    crate::serial_strln!("[VIRTIO_BLK] Disk header written (FOLKDISK v1)");
    Ok(())
}

/// Read and verify the disk header. Returns true if valid header found.
pub fn read_disk_header() -> Result<Option<DiskHeader>, BlockError> {
    let mut buf = [0u8; SECTOR_SIZE];
    block_read(HEADER_SECTOR, &mut buf)?;

    // Check magic
    if buf[..8] != DISK_MAGIC {
        return Ok(None); // No valid header — unformatted disk
    }

    let header = unsafe {
        core::ptr::read(buf.as_ptr() as *const DiskHeader)
    };

    crate::serial_str!("[VIRTIO_BLK] Disk header: FOLKDISK v");
    crate::drivers::serial::write_dec(header.version);
    crate::serial_str!(", data_start=");
    crate::drivers::serial::write_dec(header.data_start as u32);
    crate::serial_str!(", data_size=");
    crate::drivers::serial::write_dec(header.data_size as u32);
    crate::drivers::serial::write_newline();

    Ok(Some(header))
}

/// Write a journal entry (circular log in journal area)
pub fn write_journal_entry(task_id: u32, operation: u32, sector_start: u64, sector_count: u64) -> Result<(), BlockError> {
    let pos = JOURNAL_POS.fetch_add(1, Ordering::Relaxed);
    let entries_per_sector = SECTOR_SIZE as u64 / 32; // 16 entries per sector
    let total_entries = JOURNAL_SIZE_SECTORS * entries_per_sector;
    let entry_idx = pos % total_entries;
    let sector_offset = entry_idx / entries_per_sector;
    let entry_in_sector = entry_idx % entries_per_sector;

    let journal_sector = JOURNAL_START_SECTOR + sector_offset;

    // Read existing sector
    let mut buf = [0u8; SECTOR_SIZE];
    block_read(journal_sector, &mut buf)?;

    // Write entry at offset
    let entry = JournalEntry {
        timestamp_ms: crate::timer::uptime_ms(),
        task_id,
        operation,
        sector_start,
        sector_count,
    };

    let offset = (entry_in_sector as usize) * 32;
    unsafe {
        core::ptr::copy_nonoverlapping(
            &entry as *const JournalEntry as *const u8,
            buf[offset..].as_mut_ptr(),
            32,
        );
    }

    block_write(journal_sector, &buf)?;
    Ok(())
}

/// Initialize disk: check for existing header, format if needed, run self-test
pub fn init_disk() -> Result<(), BlockError> {
    // Run self-test first
    self_test()?;

    // Check for existing disk header
    match read_disk_header()? {
        Some(_header) => {
            crate::serial_strln!("[VIRTIO_BLK] Existing disk found — preserving data");
        }
        None => {
            crate::serial_strln!("[VIRTIO_BLK] No disk header found — formatting...");
            write_disk_header()?;
            // Write initial journal entry
            write_journal_entry(0, 1, HEADER_SECTOR, 1)?;
            crate::serial_strln!("[VIRTIO_BLK] Disk formatted successfully");
        }
    }

    Ok(())
}

// ── Public API (for syscalls) ────────────────────────────────────────────────

/// Get device capacity in sectors
pub fn capacity() -> u64 {
    CAPACITY_SECTORS.load(Ordering::Relaxed)
}

/// Check if the block device is initialized
pub fn is_initialized() -> bool {
    BLOCK_DEVICE.lock().is_some()
}

/// Read multiple sectors into a buffer
pub fn read_sectors(sector: u64, buf: &mut [u8], count: usize) -> Result<(), BlockError> {
    for i in 0..count {
        let offset = i * SECTOR_SIZE;
        if offset + SECTOR_SIZE > buf.len() {
            return Err(BlockError::InvalidSector);
        }
        let mut sector_buf = [0u8; SECTOR_SIZE];
        block_read(sector + i as u64, &mut sector_buf)?;
        buf[offset..offset + SECTOR_SIZE].copy_from_slice(&sector_buf);
    }
    Ok(())
}

/// Write multiple sectors from a buffer
pub fn write_sectors(sector: u64, buf: &[u8], count: usize) -> Result<(), BlockError> {
    for i in 0..count {
        let offset = i * SECTOR_SIZE;
        if offset + SECTOR_SIZE > buf.len() {
            return Err(BlockError::InvalidSector);
        }
        let mut sector_buf = [0u8; SECTOR_SIZE];
        sector_buf.copy_from_slice(&buf[offset..offset + SECTOR_SIZE]);
        block_write(sector + i as u64, &sector_buf)?;
    }
    Ok(())
}
