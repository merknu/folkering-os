//! Model disk: secondary VirtIO block device for paging in `.fbin`
//! tensor files (D.3.7.virtio).
//!
//! The primary VirtIO block device (`drivers::virtio_blk`) hosts the
//! FOLKDISK persistence partition that Synapse uses for its SQLite
//! store + journaling. A *second* VirtIO block device, identified
//! by a 4 KiB FMDL header in sector 0, carries a single named
//! `.fbin` payload. This driver picks it up at boot, parses the
//! header, and exposes raw sector reads so the inference task can
//! stream the file's bytes on demand without buffering 232 MiB into
//! initrd RAM.
//!
//! Differences from `virtio_blk`:
//! - Polling-only completion (no IRQ / MSI-X registration). One
//!   read at a time is fine for our access pattern; eliminates a
//!   whole class of vector / EOI subtleties for the secondary
//!   device.
//! - No journal, no FOLKDISK header, no self-test write — read-only
//!   by design. Swapping models is a host-side `dd` operation.
//! - Single virtqueue, single in-flight request — the inference task
//!   serializes reads.
//!
//! On-disk layout (matches `tools/fbin-gen/build_model_disk.py`):
//!   sector 0..7   FMDL header (4 KiB)
//!   sector 8..    raw .fbin bytes, sector-padded
//!
//! Header struct (little-endian):
//!   +0x000  magic       u32   = b"FMDL"
//!   +0x004  version     u16   = 1
//!   +0x006  reserved    u16
//!   +0x008  filename    [u8; 256]  NUL-padded UTF-8
//!   +0x108  data_offset u64   = 4096
//!   +0x110  data_len    u64   = .fbin payload size
//!   +0x118  reserved    [u8; ...]

use core::sync::atomic::{Ordering, fence};
use spin::Mutex;
use x86_64::instructions::port::Port;

use super::pci::{self, PciDevice, BarType};
use super::virtio::{Virtqueue, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE};

// ── VirtIO Legacy PCI Register Offsets ─────────────────────────────

const VIRTIO_PCI_DEVICE_FEATURES: u16 = 0x00;
const VIRTIO_PCI_DRIVER_FEATURES: u16 = 0x04;
const VIRTIO_PCI_QUEUE_PFN: u16 = 0x08;
const VIRTIO_PCI_QUEUE_SIZE: u16 = 0x0C;
const VIRTIO_PCI_QUEUE_SEL: u16 = 0x0E;
const VIRTIO_PCI_QUEUE_NOTIFY: u16 = 0x10;
const VIRTIO_PCI_DEVICE_STATUS: u16 = 0x12;
const VIRTIO_PCI_ISR_STATUS: u16 = 0x13;
const VIRTIO_PCI_CONFIG: u16 = 0x14;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FAILED: u8 = 128;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_S_OK: u8 = 0;

const SECTOR_SIZE: usize = 512;

// ── FMDL Header ────────────────────────────────────────────────────

pub const FMDL_MAGIC: [u8; 4] = *b"FMDL";
pub const FMDL_VERSION: u16 = 1;
pub const FMDL_HEADER_BYTES: usize = 4096;
pub const FMDL_FILENAME_BYTES: usize = 256;

#[derive(Clone, Copy)]
pub struct FmdlHeader {
    pub filename: [u8; FMDL_FILENAME_BYTES],
    pub filename_len: usize,
    pub data_offset: u64,
    pub data_len: u64,
}

impl FmdlHeader {
    /// True if `name`'s bytes match the header filename (NUL-trimmed).
    pub fn name_matches(&self, name: &str) -> bool {
        let nb = name.as_bytes();
        nb.len() == self.filename_len && nb == &self.filename[..self.filename_len]
    }
}

// ── Errors ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ModelDiskError {
    NotInitialized,
    DeviceNotFound,
    NotPresent,        // Bus only has one virtio_blk; no model disk attached.
    QueueSetupFailed,
    DeviceFailed,
    IoError,
    Timeout,
    InvalidSector,
    BadMagic,
    BadVersion(u16),
    NotEnoughCapacity, // FMDL header asks for more sectors than the disk has.
}

// ── Per-device state ───────────────────────────────────────────────

#[repr(C)]
struct VirtioBlkReqHeader {
    req_type: u32,
    reserved: u32,
    sector: u64,
}

struct ModelDisk {
    io_base: u16,
    queue: Virtqueue,
    /// Single 4 KiB request page: [header(16)] [data(SECTOR_SIZE)] [status(1)]
    req_buf_phys: usize,
    req_buf_virt: usize,
    capacity: u64,
}

static MODEL_DISK: Mutex<Option<ModelDisk>> = Mutex::new(None);
static MODEL_DISK_HEADER: Mutex<Option<FmdlHeader>> = Mutex::new(None);

// ── I/O Helpers ────────────────────────────────────────────────────

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

// ── Init ───────────────────────────────────────────────────────────

/// Look for a secondary virtio_blk device on the bus and initialise
/// it as the model disk. Returns `NotPresent` (cleanly) when only
/// the primary device is attached, so the boot path can call this
/// unconditionally.
pub fn init() -> Result<(), ModelDiskError> {
    // Skip the primary (index 0); the secondary is index 1.
    let pci_dev = match pci::find_virtio_block_nth(1) {
        Some(d) => d,
        None => return Err(ModelDiskError::NotPresent),
    };

    crate::serial_str!("[MODEL_DISK] Found secondary virtio_blk at PCI ");
    crate::drivers::serial::write_dec(pci_dev.bus as u32);
    crate::serial_str!(":");
    crate::drivers::serial::write_dec(pci_dev.device as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(pci_dev.function as u32);
    crate::drivers::serial::write_newline();

    let io_base = match pci::decode_bar(&pci_dev, 0) {
        BarType::Io { base } => base,
        _ => {
            crate::serial_strln!("[MODEL_DISK] BAR0 not I/O space — abort");
            return Err(ModelDiskError::DeviceNotFound);
        }
    };

    pci::enable_bus_master(pci_dev.bus, pci_dev.device, pci_dev.function);

    // ── VirtIO handshake ───────────────────────────────────────────
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, 0);
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE);
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    let device_features = read_io32(io_base, VIRTIO_PCI_DEVICE_FEATURES);
    let _ = device_features;
    // Accept no special features — basic block read is all we need.
    write_io32(io_base, VIRTIO_PCI_DRIVER_FEATURES, 0);

    // ── Setup virtqueue 0 ─────────────────────────────────────────
    write_io16(io_base, VIRTIO_PCI_QUEUE_SEL, 0);
    let queue_size = read_io16(io_base, VIRTIO_PCI_QUEUE_SIZE);
    if queue_size == 0 {
        write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_FAILED);
        return Err(ModelDiskError::QueueSetupFailed);
    }

    let queue = Virtqueue::new(queue_size).ok_or(ModelDiskError::QueueSetupFailed)?;
    let queue_pfn = (queue.queue_phys / 4096) as u32;
    write_io32(io_base, VIRTIO_PCI_QUEUE_PFN, queue_pfn);

    // DRIVER_OK
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS,
              STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
    let status = read_io8(io_base, VIRTIO_PCI_DEVICE_STATUS);
    if status & STATUS_FAILED != 0 {
        return Err(ModelDiskError::DeviceFailed);
    }

    // ── Read capacity ─────────────────────────────────────────────
    // Polling path → MSI-X is OFF, so device-config sits at 0x14.
    let cap_lo = read_io32(io_base, VIRTIO_PCI_CONFIG) as u64;
    let cap_hi = read_io32(io_base, VIRTIO_PCI_CONFIG + 4) as u64;
    let capacity = cap_lo | (cap_hi << 32);

    crate::serial_str!("[MODEL_DISK] Capacity: ");
    crate::drivers::serial::write_dec(capacity as u32);
    crate::serial_str!(" sectors (");
    crate::drivers::serial::write_dec((capacity * 512 / 1024) as u32);
    crate::serial_strln!(" KB)");

    // ── Allocate single request buffer page ───────────────────────
    // Layout: [header(16)] [data(512)] [status(1)] = 529 bytes ⊂ 4 KiB.
    let req_buf_phys = crate::memory::physical::alloc_page()
        .ok_or(ModelDiskError::QueueSetupFailed)?;
    let req_buf_virt = crate::phys_to_virt(req_buf_phys);
    unsafe { core::ptr::write_bytes(req_buf_virt as *mut u8, 0, 4096); }

    *MODEL_DISK.lock() = Some(ModelDisk {
        io_base,
        queue,
        req_buf_phys,
        req_buf_virt,
        capacity,
    });

    crate::serial_strln!("[MODEL_DISK] device initialised (polling I/O)");
    Ok(())
}

// ── Single-sector read (polling) ───────────────────────────────────

/// Read one sector (512 bytes) from the model disk into `buf`. Uses
/// busy-poll on ISR + used-ring index — no interrupts.
pub fn read_sector(sector: u64, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), ModelDiskError> {
    let mut dev = MODEL_DISK.lock();
    let dsk = dev.as_mut().ok_or(ModelDiskError::NotInitialized)?;

    if sector >= dsk.capacity {
        return Err(ModelDiskError::InvalidSector);
    }

    let header_phys = dsk.req_buf_phys;
    let data_phys = dsk.req_buf_phys + 16;
    let status_phys = dsk.req_buf_phys + 16 + SECTOR_SIZE;

    let header_virt = dsk.req_buf_virt;
    let data_virt = dsk.req_buf_virt + 16;
    let status_virt = dsk.req_buf_virt + 16 + SECTOR_SIZE;

    // Build header
    unsafe {
        let h = header_virt as *mut VirtioBlkReqHeader;
        (*h).req_type = VIRTIO_BLK_T_IN;
        (*h).reserved = 0;
        (*h).sector = sector;
    }
    unsafe { core::ptr::write_volatile(status_virt as *mut u8, 0xFF); }
    fence(Ordering::SeqCst);

    // Three-descriptor chain: header → data → status
    let d0 = dsk.queue.alloc_desc().ok_or(ModelDiskError::IoError)?;
    let d1 = dsk.queue.alloc_desc().ok_or_else(|| {
        dsk.queue.free_desc(d0);
        ModelDiskError::IoError
    })?;
    let d2 = dsk.queue.alloc_desc().ok_or_else(|| {
        dsk.queue.free_desc(d0);
        dsk.queue.free_desc(d1);
        ModelDiskError::IoError
    })?;

    unsafe {
        let desc = &mut *dsk.queue.desc(d0);
        desc.addr = header_phys as u64;
        desc.len = 16;
        desc.flags = VRING_DESC_F_NEXT;
        desc.next = d1;
    }
    unsafe {
        let desc = &mut *dsk.queue.desc(d1);
        desc.addr = data_phys as u64;
        desc.len = SECTOR_SIZE as u32;
        desc.flags = VRING_DESC_F_NEXT | VRING_DESC_F_WRITE;
        desc.next = d2;
    }
    unsafe {
        let desc = &mut *dsk.queue.desc(d2);
        desc.addr = status_phys as u64;
        desc.len = 1;
        desc.flags = VRING_DESC_F_WRITE;
        desc.next = 0;
    }

    dsk.queue.submit(d0);
    write_io16(dsk.io_base, VIRTIO_PCI_QUEUE_NOTIFY, 0);

    let io_base = dsk.io_base;
    drop(dev);

    // Polling completion: peek the used ring directly. ISR alone
    // is unreliable on legacy VirtIO with no IRQ wired — the
    // device sets the bit but if we read+clear it racing with the
    // memory update, the next request sees a stale ISR and
    // pop_used returns None. Authoritative source: the used ring
    // index in memory. When it advances past last_used_idx, the
    // request is done.
    let mut timeout = 5_000_000u32;
    let mut completed = false;
    while !completed {
        {
            let mut peek = MODEL_DISK.lock();
            if let Some(d) = peek.as_mut() {
                if d.queue.pop_used().is_some() {
                    completed = true;
                }
            }
        }
        if completed { break; }
        // Also drain ISR so the device doesn't keep re-asserting
        // (read clears it on legacy transport). We don't TRUST
        // ISR for completion; this is just hygiene.
        let _ = read_io8(io_base, VIRTIO_PCI_ISR_STATUS);
        core::hint::spin_loop();
        timeout -= 1;
        if timeout == 0 {
            return Err(ModelDiskError::Timeout);
        }
    }

    let mut dev = MODEL_DISK.lock();
    let dsk = dev.as_mut().ok_or(ModelDiskError::NotInitialized)?;
    // pop_used already advanced last_used_idx for this request.
    // Free the descriptor chain so subsequent reads can reuse them.
    dsk.queue.free_chain(d0);

    fence(Ordering::SeqCst);
    let status = unsafe { core::ptr::read_volatile(status_virt as *const u8) };
    if status != VIRTIO_BLK_S_OK {
        return Err(ModelDiskError::IoError);
    }

    // Copy data out of the DMA buffer.
    unsafe {
        core::ptr::copy_nonoverlapping(
            data_virt as *const u8,
            buf.as_mut_ptr(),
            SECTOR_SIZE,
        );
    }

    Ok(())
}

// ── FMDL Header Parsing ────────────────────────────────────────────

/// Read sector 0, parse the FMDL header, store it in
/// `MODEL_DISK_HEADER`. Logs the filename and payload size.
pub fn read_fmdl_header() -> Result<FmdlHeader, ModelDiskError> {
    let mut sector = [0u8; SECTOR_SIZE];
    read_sector(0, &mut sector)?;

    if sector[0..4] != FMDL_MAGIC {
        crate::serial_strln!("[MODEL_DISK] sector 0 lacks FMDL magic — wrong disk?");
        return Err(ModelDiskError::BadMagic);
    }
    let version = u16::from_le_bytes([sector[4], sector[5]]);
    if version != FMDL_VERSION {
        return Err(ModelDiskError::BadVersion(version));
    }

    // Filename: 256 bytes from offset 8, NUL-padded.
    let mut filename = [0u8; FMDL_FILENAME_BYTES];
    filename.copy_from_slice(&sector[8..8 + FMDL_FILENAME_BYTES]);
    let filename_len = filename.iter().position(|&b| b == 0).unwrap_or(FMDL_FILENAME_BYTES);

    let data_offset = u64::from_le_bytes([
        sector[0x108], sector[0x109], sector[0x10A], sector[0x10B],
        sector[0x10C], sector[0x10D], sector[0x10E], sector[0x10F],
    ]);
    let data_len = u64::from_le_bytes([
        sector[0x110], sector[0x111], sector[0x112], sector[0x113],
        sector[0x114], sector[0x115], sector[0x116], sector[0x117],
    ]);

    let header = FmdlHeader { filename, filename_len, data_offset, data_len };

    // Sanity-check: payload must fit on the disk.
    let need_sectors = (data_offset + data_len + 511) / 512;
    let dev = MODEL_DISK.lock();
    let cap = dev.as_ref().map(|d| d.capacity).unwrap_or(0);
    drop(dev);
    if need_sectors > cap {
        crate::serial_str!("[MODEL_DISK] FMDL data ");
        crate::drivers::serial::write_dec(need_sectors as u32);
        crate::serial_str!(" sectors > capacity ");
        crate::drivers::serial::write_dec(cap as u32);
        crate::serial_strln!("");
        return Err(ModelDiskError::NotEnoughCapacity);
    }

    crate::serial_str!("[MODEL_DISK] FMDL v");
    crate::drivers::serial::write_dec(version as u32);
    crate::serial_str!(" file=\"");
    for i in 0..filename_len {
        let b = filename[i];
        if (0x20..=0x7E).contains(&b) {
            unsafe {
                let mut tmp = [0u8; 1];
                tmp[0] = b;
                let s = core::str::from_utf8_unchecked(&tmp);
                crate::serial_str!(s);
            }
        }
    }
    crate::serial_str!("\" data_offset=");
    crate::drivers::serial::write_dec(data_offset as u32);
    crate::serial_str!(" data_len=");
    crate::drivers::serial::write_dec(data_len as u32);
    crate::serial_str!(" (");
    crate::drivers::serial::write_dec((data_len / (1024 * 1024)) as u32);
    crate::serial_strln!(" MB)");

    *MODEL_DISK_HEADER.lock() = Some(header);
    Ok(header)
}

/// Returns the cached FMDL header, if `read_fmdl_header` succeeded
/// at boot.
#[allow(dead_code)]
pub fn header() -> Option<FmdlHeader> {
    *MODEL_DISK_HEADER.lock()
}

/// True iff the model disk was successfully initialised AND its
/// FMDL header parsed cleanly.
#[allow(dead_code)]
pub fn is_ready() -> bool {
    MODEL_DISK_HEADER.lock().is_some()
}

/// Read multiple consecutive sectors into the caller's buffer.
/// Loops single-sector reads — fine for boot-time verification and
/// for the upcoming `read_model_file_shmem` syscall (which streams
/// 4 KiB at a time per shmem page). At ~100 µs per sector on KVM, a
/// 232 MiB payload is ~45 s; multi-sector DMA will land alongside
/// the syscall so it's a single ~few-hundred-ms allocation.
///
/// `buf.len()` MUST be a multiple of 512.
#[allow(dead_code)]
pub fn read_sectors(sector: u64, buf: &mut [u8]) -> Result<(), ModelDiskError> {
    if buf.len() % SECTOR_SIZE != 0 {
        return Err(ModelDiskError::InvalidSector);
    }
    let n = buf.len() / SECTOR_SIZE;
    for i in 0..n {
        let mut tmp = [0u8; SECTOR_SIZE];
        read_sector(sector + i as u64, &mut tmp)?;
        let off = i * SECTOR_SIZE;
        buf[off..off + SECTOR_SIZE].copy_from_slice(&tmp);
    }
    Ok(())
}

/// FNV-1a 32-bit hash. Same algorithm `libfolk::sys::synapse::hash_name`
/// uses, so userspace can hand us a pre-computed hash in a syscall arg
/// instead of a string pointer + length pair.
fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Hash of the FMDL filename (NUL-trimmed). Cached at boot via
/// `read_fmdl_header()`; use this to compare against a userspace
/// hash without holding the header lock during the comparison.
pub fn filename_hash() -> Option<u32> {
    let h = *MODEL_DISK_HEADER.lock();
    h.map(|h| fnv1a_32(&h.filename[..h.filename_len]))
}

/// Stream the entire model file payload into a fresh shmem region.
/// Verifies the requested name hash matches the FMDL filename,
/// allocates a shmem of `data_len` bytes, then loops the model
/// disk's pages into the shmem's physical pages 4 KiB at a time
/// (8 sectors per page).
///
/// On success returns `(shmem_id, data_len)`. Userspace maps the
/// shmem at any free vaddr and reads the .fbin bytes directly out
/// of the mapping — no kernel-side copy, no Synapse round-trip.
///
/// The shmem's owner is the calling task (per `shmem_create`'s
/// `current_task()` capture). The caller is responsible for
/// `shmem_destroy` when done if it cares about reclaiming the
/// pages; for the inference task's path the shmem persists for
/// the lifetime of the process and the kernel's task teardown
/// reclaims it.
pub fn read_into_shmem(name_hash: u32) -> Result<(u32, u64), ModelDiskError> {
    use crate::ipc::shared_memory::{shmem_create, ShmemPerms, SHMEM_TABLE};

    let header = *MODEL_DISK_HEADER.lock();
    let header = header.ok_or(ModelDiskError::NotInitialized)?;

    let actual_hash = fnv1a_32(&header.filename[..header.filename_len]);
    if actual_hash != name_hash {
        return Err(ModelDiskError::BadMagic);
    }
    if header.data_offset % SECTOR_SIZE as u64 != 0 {
        return Err(ModelDiskError::InvalidSector);
    }

    let data_len = header.data_len as usize;
    let payload_start_sector = header.data_offset / SECTOR_SIZE as u64;

    let shmem_id = shmem_create(data_len, ShmemPerms::ReadWrite)
        .map_err(|_| ModelDiskError::IoError)?;

    // Snapshot the shmem's physical page list. We don't hold the
    // SHMEM_TABLE lock while reading from disk — that would pin a
    // global mutex across ~5 s of polling I/O. Cloning the page
    // list is cheap (one Vec<usize> of ~57k entries for 232 MiB).
    let pages = {
        let table = SHMEM_TABLE.lock();
        let shmem = match table.get(&shmem_id.get()) {
            Some(s) => s,
            None => return Err(ModelDiskError::IoError),
        };
        shmem.phys_pages.clone()
    };

    crate::serial_str!("[MODEL_DISK] streaming ");
    crate::drivers::serial::write_dec((data_len / (1024 * 1024)) as u32);
    crate::serial_str!(" MiB into shmem ");
    crate::drivers::serial::write_dec(shmem_id.get());
    crate::serial_str!(" (");
    crate::drivers::serial::write_dec(pages.len() as u32);
    crate::serial_strln!(" pages)...");

    // Walk the shmem's pages, filling each with up to 4 KiB from
    // the model disk. Last page may be partial.
    for (page_idx, &phys) in pages.iter().enumerate() {
        let page_off = page_idx * 4096;
        let bytes_left = data_len.saturating_sub(page_off);
        if bytes_left == 0 { break; }
        let bytes_this_page = bytes_left.min(4096);
        // Round up to sector boundary; trailing bytes past
        // `bytes_this_page` get whatever the disk has there
        // (sector-padded zeros from build_model_disk.py).
        let sectors_this_page = (bytes_this_page + SECTOR_SIZE - 1) / SECTOR_SIZE;
        let sector = payload_start_sector + (page_idx * 8) as u64;
        let virt = crate::phys_to_virt(phys);
        let buf = unsafe {
            core::slice::from_raw_parts_mut(
                virt as *mut u8,
                sectors_this_page * SECTOR_SIZE,
            )
        };
        read_sectors(sector, buf)?;
        // Yield periodically so the rest of the kernel keeps
        // breathing during the multi-second stream.
        if page_idx % 1024 == 0 && page_idx != 0 {
            crate::serial_str!("[MODEL_DISK]   ");
            crate::drivers::serial::write_dec(page_idx as u32);
            crate::serial_str!(" / ");
            crate::drivers::serial::write_dec(pages.len() as u32);
            crate::serial_strln!(" pages streamed");
        }
    }

    crate::serial_str!("[MODEL_DISK] streaming done — shmem_id=");
    crate::drivers::serial::write_dec(shmem_id.get());
    crate::serial_str!(" size=");
    crate::drivers::serial::write_dec(data_len as u32);
    crate::serial_strln!("");

    Ok((shmem_id.get(), header.data_len))
}

/// Boot-time spot check: re-read the first 8 payload sectors (4 KiB
/// after the FMDL header) and verify the .fbin magic `FBN1` lives
/// at offset 0. Confirms the multi-sector read path works end-to-
/// end on real hardware before the userspace path tries to lean
/// on it for the full 232 MiB stream.
pub fn verify_payload_magic() -> Result<(), ModelDiskError> {
    let header = *MODEL_DISK_HEADER.lock();
    let header = header.ok_or(ModelDiskError::NotInitialized)?;

    if header.data_offset % SECTOR_SIZE as u64 != 0 {
        return Err(ModelDiskError::InvalidSector);
    }
    let payload_sector = header.data_offset / SECTOR_SIZE as u64;

    let mut buf = [0u8; SECTOR_SIZE * 8]; // 4 KiB
    read_sectors(payload_sector, &mut buf)?;

    if buf[0..4] != *b"FBN1" {
        crate::serial_str!("[MODEL_DISK] payload magic mismatch — got 0x");
        crate::drivers::serial::write_hex(u32::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3],
        ]) as u64);
        crate::serial_strln!(" — wrong .fbin written?");
        return Err(ModelDiskError::BadMagic);
    }

    let version = u16::from_le_bytes([buf[4], buf[5]]);
    let n_tensors = u16::from_le_bytes([buf[6], buf[7]]);
    crate::serial_str!("[MODEL_DISK] payload OK: FBN1 v");
    crate::drivers::serial::write_dec(version as u32);
    crate::serial_str!(" with ");
    crate::drivers::serial::write_dec(n_tensors as u32);
    crate::serial_strln!(" tensors");
    Ok(())
}
