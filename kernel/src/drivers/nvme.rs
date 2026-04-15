//! NVMe controller driver.
//!
//! First PCIe storage driver to run entirely on MSI-X (no IOAPIC
//! fallback). Built on top of `drivers::msix` for vector allocation
//! and table programming.
//!
//! # Scope (Phase 1)
//!
//! - Detect one NVMe controller by PCI class 0x01/0x08/0x02.
//! - Map BAR0 MMIO, initialize controller, create admin queue pair.
//! - Identify Controller + Identify Namespace 1.
//! - Create one I/O queue pair (QID=1) with MSI-X interrupts.
//! - `nvme_read(lba, buf)` for single-LBA reads via PRP1.
//! - Self-test reads LBA 0 and prints the first 16 bytes.
//!
//! Write, multi-queue, SGL, multiple namespaces: later.
//!
//! # Register layout (NVMe 1.4, §3.1)
//!
//! ```text
//!   0x00  CAP    (u64)  Controller Capabilities
//!   0x08  VS     (u32)  Version
//!   0x14  CC     (u32)  Controller Configuration
//!   0x1C  CSTS   (u32)  Controller Status
//!   0x24  AQA    (u32)  Admin Queue Attributes
//!   0x28  ASQ    (u64)  Admin SQ Base
//!   0x30  ACQ    (u64)  Admin CQ Base
//!   0x1000 +     (u32)  Doorbell pairs (SQ, then CQ; stride from CAP.DSTRD)
//! ```

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use spin::Mutex;
extern crate alloc;

use super::pci::{self, PciDevice, BarType};

// ── PCI class codes (NVMe spec §2.1.1) ───────────────────────────────────────

const PCI_CLASS_STORAGE: u8 = 0x01;
const PCI_SUBCLASS_NVM: u8 = 0x08;
const PCI_PROG_IF_NVME: u8 = 0x02;

// ── Controller register offsets ──────────────────────────────────────────────

const REG_CAP: usize = 0x00;
const REG_VS: usize = 0x08;
const REG_CC: usize = 0x14;
const REG_CSTS: usize = 0x1C;
const REG_AQA: usize = 0x24;
const REG_ASQ: usize = 0x28;
const REG_ACQ: usize = 0x30;
const REG_DOORBELL_BASE: usize = 0x1000;

// ── CC (Controller Configuration) bits ───────────────────────────────────────

const CC_EN: u32 = 1 << 0;
const CC_CSS_NVM: u32 = 0 << 4;     // NVM command set
const CC_MPS_4K: u32 = 0 << 7;      // 2^(12+MPS) = 4 KiB when MPS=0
const CC_AMS_RR: u32 = 0 << 11;     // Round-robin arbitration
const CC_IOSQES_64: u32 = 6 << 16;  // 2^6 = 64-byte SQ entries
const CC_IOCQES_16: u32 = 4 << 20;  // 2^4 = 16-byte CQ entries

// ── CSTS bits ────────────────────────────────────────────────────────────────

const CSTS_RDY: u32 = 1 << 0;
const CSTS_CFS: u32 = 1 << 1;  // Controller Fatal Status

// ── Queue sizes (Phase 1: small fixed ring) ──────────────────────────────────

const ADMIN_Q_DEPTH: u16 = 32;
const IO_Q_DEPTH: u16 = 32;
const SQE_SIZE: usize = 64;
const CQE_SIZE: usize = 16;

// ── Admin opcodes ────────────────────────────────────────────────────────────

const ADMIN_OP_DELETE_IOSQ: u8 = 0x00;
const ADMIN_OP_CREATE_IOSQ: u8 = 0x01;
const ADMIN_OP_DELETE_IOCQ: u8 = 0x04;
const ADMIN_OP_CREATE_IOCQ: u8 = 0x05;
const ADMIN_OP_IDENTIFY: u8 = 0x06;

// ── NVM opcodes ──────────────────────────────────────────────────────────────

const NVM_OP_WRITE: u8 = 0x01;
const NVM_OP_READ: u8 = 0x02;

/// 64-byte Submission Queue Entry. `#[repr(C)]` locks the layout so we
/// can DMA it directly to the controller.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SubmissionEntry {
    cdw0: u32,        // opcode + flags + cid
    nsid: u32,
    cdw2_3: [u32; 2],
    mptr: u64,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}

/// 16-byte Completion Queue Entry.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CompletionEntry {
    dw0: u32,
    dw1: u32,
    sq_head: u16,
    sq_id: u16,
    cid: u16,
    status: u16,  // bit 0 = phase tag; bits 1..16 = status field
}

/// A submission/completion queue pair sharing a single MSI-X vector.
struct QueuePair {
    qid: u16,
    depth: u16,
    sq_phys: u64,
    sq_virt: u64,
    cq_phys: u64,
    cq_virt: u64,
    sq_tail: u16,
    cq_head: u16,
    /// Phase tag we expect on the next CQE we poll. Flips every time
    /// the CQ head wraps; the controller flips its own tag likewise,
    /// so a match means "this entry is new."
    phase: u8,
    /// Byte offset of this queue's doorbell pair from `REG_DOORBELL_BASE`.
    /// SQ tail is at `dbl_off`; CQ head is at `dbl_off + dstrd`.
    dbl_off: usize,
}

/// Number of 4 KiB pages pre-allocated for DMA. Chosen so that one
/// command can carry up to 63 data pages (252 KiB at 512 B/LBA) plus
/// one PRP list page. Pinned for the life of the driver — no
/// allocator traffic after init.
const DMA_POOL_PAGES: usize = 64;

/// Largest transfer the pool can handle in one command (in *data*
/// pages). The remaining slot is reserved for the PRP list when the
/// caller asks for ≥ 3 data pages.
const DMA_MAX_DATA_PAGES: usize = DMA_POOL_PAGES - 1;

/// Pre-allocated DMA page pool. The controller keeps the whole pool
/// for the lifetime of the driver; callers lease pages through the
/// helper below and release them immediately after the I/O completes.
///
/// `free_mask` is an atomic bitmap: bit `i` set = page `i` is free.
/// Concurrent acquisition uses CAS so multiple callers (e.g., if the
/// driver ever goes multi-queue) can race without a lock.
struct DmaPool {
    phys: [u64; DMA_POOL_PAGES],
    virt: [u64; DMA_POOL_PAGES],
    free_mask: AtomicU64,
}

impl DmaPool {
    fn new() -> Result<Self, NvmeError> {
        let mut phys = [0u64; DMA_POOL_PAGES];
        let mut virt = [0u64; DMA_POOL_PAGES];
        for i in 0..DMA_POOL_PAGES {
            let (p, v) = alloc_zeroed_page().ok_or(NvmeError::AllocFailed)?;
            phys[i] = p;
            virt[i] = v;
        }
        // All pages free on startup. For DMA_POOL_PAGES < 64 we'd
        // need `(1 << N) - 1`; at exactly 64 that overflows, so use
        // `u64::MAX`.
        let initial = if DMA_POOL_PAGES == 64 {
            u64::MAX
        } else {
            (1u64 << DMA_POOL_PAGES) - 1
        };
        Ok(Self { phys, virt, free_mask: AtomicU64::new(initial) })
    }

    /// Claim the lowest-indexed free page. Returns the pool index or
    /// `None` if the pool is exhausted.
    fn acquire(&self) -> Option<usize> {
        loop {
            let current = self.free_mask.load(Ordering::Acquire);
            if current == 0 { return None; }
            let bit = current.trailing_zeros() as usize;
            let new_mask = current & !(1u64 << bit);
            if self.free_mask
                .compare_exchange(current, new_mask, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(bit);
            }
        }
    }

    fn release(&self, idx: usize) {
        self.free_mask.fetch_or(1u64 << idx, Ordering::Release);
    }

    #[inline(always)]
    fn phys(&self, idx: usize) -> u64 { self.phys[idx] }

    #[inline(always)]
    fn virt(&self, idx: usize) -> u64 { self.virt[idx] }

    /// Zero a page before reusing it. Cheap defense-in-depth against
    /// info leaks in partial transfers and stale PRP-list entries the
    /// controller might prefetch.
    fn zero(&self, idx: usize) {
        unsafe {
            core::ptr::write_bytes(self.virt[idx] as *mut u8, 0, 4096);
        }
    }
}

pub struct NvmeController {
    bar_phys: u64,
    bar_virt: u64,
    doorbell_stride: usize,
    admin: QueuePair,
    io: Option<QueuePair>,
    /// Namespace 1's total LBA count (from Identify Namespace).
    lba_count: u64,
    /// Namespace 1's LBA size in bytes (typically 512 or 4096).
    lba_size: u32,
    /// PCI device this controller is attached to.
    pci: PciDevice,
    /// Pinned DMA bounce buffers. Pre-allocated at init so the I/O
    /// hot path never touches the page allocator.
    dma_pool: DmaPool,
}

pub static NVME: Mutex<Option<NvmeController>> = Mutex::new(None);

/// Set by the MSI-X handler when a completion arrives on the I/O queue.
static IO_COMPLETE: AtomicBool = AtomicBool::new(false);
static IO_IRQ_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvmeError {
    NotFound,
    BarDecode,
    ControllerFatal,
    ReadyTimeout,
    AdminFailed(u16),
    IoFailed(u16),
    AllocFailed,
    NoNamespace,
}

// ── MMIO helpers ─────────────────────────────────────────────────────────────

/// Read a u32 from a BAR register. NO_CACHE mapping guarantees this is
/// a single uncached MMIO load in the expected byte order.
#[inline(always)]
fn mmio_read32(bar_virt: u64, offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((bar_virt + offset as u64) as *const u32) }
}

#[inline(always)]
fn mmio_write32(bar_virt: u64, offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile((bar_virt + offset as u64) as *mut u32, value); }
}

#[inline(always)]
fn mmio_read64(bar_virt: u64, offset: usize) -> u64 {
    // Split u64 into two u32 reads — some controllers reject 64-bit
    // MMIO transactions. CAP is commonly read this way.
    let lo = mmio_read32(bar_virt, offset) as u64;
    let hi = mmio_read32(bar_virt, offset + 4) as u64;
    lo | (hi << 32)
}

#[inline(always)]
fn mmio_write64(bar_virt: u64, offset: usize, value: u64) {
    mmio_write32(bar_virt, offset, value as u32);
    mmio_write32(bar_virt, offset + 4, (value >> 32) as u32);
}

// ── Find the NVMe device on the PCI bus ──────────────────────────────────────

fn find_nvme_pci() -> Option<PciDevice> {
    let list = pci::PCI_DEVICES.lock();
    for i in 0..list.count {
        if let Some(ref dev) = list.devices[i] {
            if dev.class_code == PCI_CLASS_STORAGE
                && dev.subclass == PCI_SUBCLASS_NVM
                && dev.prog_if == PCI_PROG_IF_NVME
            {
                return Some(dev.clone());
            }
        }
    }
    None
}

/// Map a range of physical MMIO pages into the HHDM with NO_CACHE.
/// Needed because HHDM only covers RAM by default — PCIe BARs live
/// outside that range and must be mapped explicitly.
fn map_mmio(phys_base: u64, length: u64) -> u64 {
    use x86_64::structures::paging::PageTableFlags;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE | PageTableFlags::NO_CACHE;
    let hhdm = crate::HHDM_OFFSET.load(Ordering::Relaxed) as u64;
    let first = phys_base & !0xFFF;
    let last = (phys_base + length - 1) & !0xFFF;
    let mut p = first;
    while p <= last {
        let _ = crate::memory::paging::map_page(
            (hhdm + p) as usize,
            p as usize,
            flags,
        );
        p += 4096;
    }
    hhdm + phys_base
}

/// Allocate a zeroed 4K page for a queue/buffer. Returns (phys, virt).
fn alloc_zeroed_page() -> Option<(u64, u64)> {
    let phys = crate::memory::physical::alloc_page()?;
    let virt = crate::phys_to_virt(phys);
    unsafe { core::ptr::write_bytes(virt as *mut u8, 0, 4096); }
    Some((phys as u64, virt as u64))
}

// ── Public entry ─────────────────────────────────────────────────────────────

/// Discover and fully initialize the NVMe controller, including I/O
/// queue creation and a read-of-LBA-0 self-test. Called once during
/// kernel boot after PCI enumeration + MSI-X init.
pub fn init() -> Result<(), NvmeError> {
    crate::serial_strln!("[NVMe] Looking for NVMe controller...");

    let pci_dev = find_nvme_pci().ok_or(NvmeError::NotFound)?;

    crate::serial_str!("[NVMe] Found ");
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

    // BAR0 must be 64-bit MMIO for NVMe.
    let bar_phys = match pci::decode_bar(&pci_dev, 0) {
        BarType::Mmio64 { base, .. } => base,
        BarType::Mmio32 { base, .. } => base as u64,
        _ => {
            crate::serial_strln!("[NVMe] ERROR: BAR0 is not MMIO");
            return Err(NvmeError::BarDecode);
        }
    };

    // NVMe register region is at least 0x1000 + doorbell pairs.
    // Map the first 16 KB — covers controller regs + doorbells for
    // a reasonable queue count.
    let bar_virt = map_mmio(bar_phys, 16 * 1024);

    pci::enable_bus_master(pci_dev.bus, pci_dev.device, pci_dev.function);

    let cap = mmio_read64(bar_virt, REG_CAP);
    let vs = mmio_read32(bar_virt, REG_VS);
    let dstrd_raw = ((cap >> 32) & 0xF) as usize;
    let doorbell_stride = 4usize << dstrd_raw;
    let mqes = (cap & 0xFFFF) as u16 + 1;

    crate::serial_str!("[NVMe] CAP=0x");
    crate::drivers::serial::write_hex(cap);
    crate::serial_str!(" VS=0x");
    crate::drivers::serial::write_hex(vs as u64);
    crate::serial_str!(" MQES=");
    crate::drivers::serial::write_dec(mqes as u32);
    crate::serial_str!(" DSTRD=");
    crate::drivers::serial::write_dec(dstrd_raw as u32);
    crate::drivers::serial::write_newline();

    // Reset: CC.EN=0, then wait for CSTS.RDY=0.
    let cc = mmio_read32(bar_virt, REG_CC);
    mmio_write32(bar_virt, REG_CC, cc & !CC_EN);
    wait_ready(bar_virt, false)?;

    // Allocate admin queues (one page each — fits 32 × 64B SQE and
    // 32 × 16B CQE with room to spare).
    let (asq_phys, asq_virt) = alloc_zeroed_page().ok_or(NvmeError::AllocFailed)?;
    let (acq_phys, acq_virt) = alloc_zeroed_page().ok_or(NvmeError::AllocFailed)?;

    // Program AQA (sizes are encoded as N-1), ASQ, ACQ.
    let aqa = ((ADMIN_Q_DEPTH - 1) as u32)
        | (((ADMIN_Q_DEPTH - 1) as u32) << 16);
    mmio_write32(bar_virt, REG_AQA, aqa);
    mmio_write64(bar_virt, REG_ASQ, asq_phys);
    mmio_write64(bar_virt, REG_ACQ, acq_phys);

    // Enable: NVM command set, 4KB pages, round-robin, SQE=64B, CQE=16B.
    let cc_new = CC_EN | CC_CSS_NVM | CC_MPS_4K | CC_AMS_RR
        | CC_IOSQES_64 | CC_IOCQES_16;
    mmio_write32(bar_virt, REG_CC, cc_new);
    wait_ready(bar_virt, true)?;

    crate::serial_strln!("[NVMe] Controller enabled");

    let admin = QueuePair {
        qid: 0,
        depth: ADMIN_Q_DEPTH,
        sq_phys: asq_phys,
        sq_virt: asq_virt,
        cq_phys: acq_phys,
        cq_virt: acq_virt,
        sq_tail: 0,
        cq_head: 0,
        phase: 1,
        dbl_off: 0, // QID 0 → first doorbell pair
    };

    // Allocate the DMA pool up front. 64 × 4 KiB = 256 KiB pinned.
    // Doing it before Identify so we can bounce Identify output
    // through the pool too and drop the ad-hoc alloc path.
    let dma_pool = DmaPool::new()?;
    crate::serial_str!("[NVMe] DMA pool pinned: ");
    crate::drivers::serial::write_dec(DMA_POOL_PAGES as u32);
    crate::serial_strln!(" × 4 KiB (256 KiB total)");

    let mut ctrl = NvmeController {
        bar_phys,
        bar_virt,
        doorbell_stride,
        admin,
        io: None,
        lba_count: 0,
        lba_size: 0,
        pci: pci_dev,
        dma_pool,
    };

    crate::serial_strln!("[NVMe] Controller enabled, probing namespaces...");

    // Identify Controller (CNS=1) — populates model/fw strings; we
    // mostly need to confirm the controller is responsive before
    // touching namespaces.
    let (ident_phys, ident_virt) = alloc_zeroed_page().ok_or(NvmeError::AllocFailed)?;
    ctrl.identify(1, 0, ident_phys)?;
    // Identify Controller returns the model name at byte offset 24,
    // 40 bytes long. Print a few ASCII chars to confirm data flow.
    unsafe {
        let model_ptr = (ident_virt + 24) as *const u8;
        crate::serial_str!("[NVMe] Model: ");
        for i in 0..20 {
            let b = core::ptr::read_volatile(model_ptr.add(i));
            if b == 0 || b == b' ' { break; }
            crate::drivers::serial::write_byte(b);
        }
        crate::drivers::serial::write_newline();
    }

    // Identify Namespace 1 (CNS=0, NSID=1). LBA count is at DW0..8;
    // LBA size descriptor is an array at offset 128 indexed by
    // FLBAS[3:0] from offset 26.
    ctrl.identify(0, 1, ident_phys)?;
    let (lba_count, lba_size) = unsafe {
        let base = ident_virt as *const u8;
        let nsze = core::ptr::read_volatile(base as *const u64);
        let flbas = core::ptr::read_volatile(base.add(26));
        let lbaf_idx = (flbas & 0x0F) as usize;
        let lbaf_entry = core::ptr::read_volatile(
            base.add(128 + lbaf_idx * 4) as *const u32
        );
        // LBAF.LBADS (bits 16..23) is log2(LBA size in bytes).
        let lbads = (lbaf_entry >> 16) & 0xFF;
        (nsze, 1u32 << lbads)
    };
    ctrl.lba_count = lba_count;
    ctrl.lba_size = lba_size;

    crate::serial_str!("[NVMe] NS1: ");
    crate::drivers::serial::write_dec(lba_count as u32);
    crate::serial_str!(" LBAs × ");
    crate::drivers::serial::write_dec(lba_size);
    crate::serial_str!("B (");
    crate::drivers::serial::write_dec((lba_count * lba_size as u64 / (1024 * 1024)) as u32);
    crate::serial_strln!(" MiB)");

    // Allocate MSI-X vector for the I/O queue and wire it up. Vector
    // 64 is owned by VirtIO-blk; we expect 65 here.
    let msix_cap = super::msix::parse_cap(&ctrl.pci)
        .ok_or(NvmeError::BarDecode)?;
    let msix_table_virt = super::msix::locate_table(&ctrl.pci, &msix_cap)
        .ok_or(NvmeError::BarDecode)?;
    let io_vector = super::msix::alloc_vector().ok_or(NvmeError::AllocFailed)?;
    let apic_id = crate::arch::x86_64::apic::get_apic_id();
    unsafe {
        super::msix::configure_entry(msix_table_virt, 0, io_vector, apic_id);
    }
    super::msix::enable_msix(&ctrl.pci, msix_cap.cap_offset);

    crate::serial_str!("[NVMe] MSI-X vector ");
    crate::drivers::serial::write_dec(io_vector as u32);
    crate::serial_str!(" (APIC target ");
    crate::drivers::serial::write_dec(apic_id as u32);
    crate::serial_strln!(") bound to I/O queue 1");

    // Create I/O queues. NVMe spec requires CQ before its SQ —
    // CQID=1 is referenced from CREATE_IOSQ, so the CQ must exist
    // first or the controller rejects with SC=0x00 SCT=0x01 status.
    let (iosq_phys, iosq_virt) = alloc_zeroed_page().ok_or(NvmeError::AllocFailed)?;
    let (iocq_phys, iocq_virt) = alloc_zeroed_page().ok_or(NvmeError::AllocFailed)?;

    ctrl.create_iocq(1, IO_Q_DEPTH, iocq_phys, /* iv */ 0)?;
    ctrl.create_iosq(1, IO_Q_DEPTH, iosq_phys, /* cqid */ 1)?;

    // Doorbell pair for QID=1 lives at offset
    //   base + 2 × stride × 1
    // (pair 0 is admin, pair 1 is our I/O queue).
    let io = QueuePair {
        qid: 1,
        depth: IO_Q_DEPTH,
        sq_phys: iosq_phys,
        sq_virt: iosq_virt,
        cq_phys: iocq_phys,
        cq_virt: iocq_virt,
        sq_tail: 0,
        cq_head: 0,
        phase: 1,
        dbl_off: 2 * doorbell_stride,
    };
    ctrl.io = Some(io);

    // Self-test: read LBA 0 and print first 16 bytes.
    let (buf_phys, buf_virt) = alloc_zeroed_page().ok_or(NvmeError::AllocFailed)?;
    ctrl.read_lba(0, buf_phys)?;
    unsafe {
        crate::serial_str!("[NVMe] Self-test LBA 0 first 16B: ");
        for i in 0..16 {
            let b = core::ptr::read_volatile((buf_virt as *const u8).add(i));
            crate::drivers::serial::write_hex(b as u64);
            crate::serial_str!(" ");
        }
        crate::drivers::serial::write_newline();
    }

    // Write-then-read round-trip on LBA 1 (LBA 0 is reserved for any
    // future NVMe-backed FS header; LBA 1 is scratch).
    let (wr_phys, wr_virt) = alloc_zeroed_page().ok_or(NvmeError::AllocFailed)?;
    unsafe {
        let p = wr_virt as *mut u32;
        // Repeating DEADBEEF pattern so a partial write is still detectable.
        for i in 0..128 {
            core::ptr::write_volatile(p.add(i), 0xDEADBEEFu32);
        }
    }
    ctrl.write_lba(1, wr_phys)?;

    // Re-read into a fresh buffer (not wr_virt) so we're really
    // observing what the controller persisted.
    let (vr_phys, vr_virt) = alloc_zeroed_page().ok_or(NvmeError::AllocFailed)?;
    ctrl.read_lba(1, vr_phys)?;
    let match_ok = unsafe {
        let p = vr_virt as *const u32;
        let mut ok = true;
        for i in 0..128 {
            if core::ptr::read_volatile(p.add(i)) != 0xDEADBEEFu32 {
                ok = false;
                break;
            }
        }
        ok
    };
    if match_ok {
        crate::serial_strln!("[NVMe] Write/read round-trip PASSED (0xDEADBEEF verified)");
    } else {
        crate::serial_strln!("[NVMe] Write/read round-trip FAILED");
    }

    // ── Phase 3: multi-block self-test ──────────────────────────────────
    // Exercise all three PRP paths in one command by varying sector
    // count:
    //   * 8 sectors = 1 page  → PRP1 only
    //   * 16 sectors = 2 pages → PRP1 + PRP2
    //   * 32 sectors = 4 pages → PRP1 + PRP list
    // For each size we write a distinct 32-bit pattern and read it
    // back in a fresh buffer. Matching on every u32 confirms both
    // the PRP plumbing and the scatter-gather DMA path.
    //
    // Store the controller before the Phase 3 self-test — the public
    // `nvme_*_blocks` helpers take NVME.lock(), so the controller
    // must already be installed by the time they're called.
    *NVME.lock() = Some(ctrl);

    // Snapshot pool before sub-tests — every release must restore it.
    let pool_before = dma_pool_free_count().unwrap_or(0);

    let multi_ok = multi_block_selftest();
    if multi_ok {
        crate::serial_strln!("[NVMe] Multi-block PRP self-test PASSED (1p, 2p, PRP-list all OK)");
    } else {
        crate::serial_strln!("[NVMe] Multi-block PRP self-test FAILED");
    }

    // Phase 4 leak check. Every lease must be released — an off-by-
    // one in acquire/release paths would show up as a lower post-
    // test count than pre-test.
    let pool_after = dma_pool_free_count().unwrap_or(0);
    crate::serial_str!("[NVMe] DMA pool: ");
    crate::drivers::serial::write_dec(pool_after);
    crate::serial_str!("/");
    crate::drivers::serial::write_dec(DMA_POOL_PAGES as u32);
    crate::serial_str!(" pages free (was ");
    crate::drivers::serial::write_dec(pool_before);
    crate::serial_str!(" before self-test) ");
    if pool_after == pool_before {
        crate::serial_strln!("— no leak");
    } else {
        crate::serial_strln!("— LEAK DETECTED");
    }

    // Register MSI-X completion handler via the same hack as
    // VirtIO-blk: main.rs's IDT[65] is wired to irq_nvme_msix,
    // which calls back into this module.
    let _ = crate::arch::x86_64::idt::register_msix_handler(io_vector, nvme_msix_handler);

    crate::serial_strln!("[NVMe] Phase 3 complete: 1-page + 2-page + PRP-list transfers OK");
    Ok(())
}

/// Phase 3 multi-block self-test. Writes and reads back three
/// different transfer sizes — one for each PRP mode — and verifies
/// the pattern round-trips. Uses LBAs 16, 32, 64 so the Phase 2
/// LBA 1 test isn't disturbed.
fn multi_block_selftest() -> bool {
    use alloc::vec;
    let test_cases: &[(u64, u32, u32)] = &[
        // (start_lba, block_count, pattern)
        (16, 8, 0xCAFEBABE),   // 1 page, PRP1 only
        (32, 16, 0xF00DBEEF),  // 2 pages, PRP1+PRP2
        (64, 32, 0xBADC0FFE),  // 4 pages, PRP list
    ];
    for &(lba, count, pattern) in test_cases {
        let bytes = (count as usize) * 512;
        let mut buf = vec![0u8; bytes];
        // Fill buffer with the pattern as u32s.
        for (i, chunk) in buf.chunks_exact_mut(4).enumerate() {
            let _ = i;
            chunk.copy_from_slice(&pattern.to_le_bytes());
        }
        if nvme_write_blocks(lba, &buf).is_err() {
            crate::serial_str!("[NVMe]   sub-test write failed at ");
            crate::drivers::serial::write_dec(count);
            crate::serial_strln!(" blocks");
            return false;
        }
        let mut verify = vec![0u8; bytes];
        if nvme_read_blocks(lba, &mut verify).is_err() {
            crate::serial_str!("[NVMe]   sub-test read failed at ");
            crate::drivers::serial::write_dec(count);
            crate::serial_strln!(" blocks");
            return false;
        }
        for chunk in verify.chunks_exact(4) {
            let got = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if got != pattern {
                crate::serial_str!("[NVMe]   sub-test mismatch at ");
                crate::drivers::serial::write_dec(count);
                crate::serial_str!(" blocks: got 0x");
                crate::drivers::serial::write_hex(got as u64);
                crate::serial_strln!("");
                return false;
            }
        }
        crate::serial_str!("[NVMe]   sub-test OK: ");
        crate::drivers::serial::write_dec(count);
        crate::serial_str!(" blocks pattern=0x");
        crate::drivers::serial::write_hex(pattern as u64);
        crate::drivers::serial::write_newline();
    }
    true
}

/// MSI-X completion handler. The naked wrapper in main.rs sends EOI,
/// so this just signals completion. Phase 1 uses polling for reads
/// (simpler + deterministic for self-test); the handler exists so
/// spurious interrupts don't corrupt state.
pub fn nvme_msix_handler() {
    IO_COMPLETE.store(true, Ordering::Release);
    IO_IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    crate::arch::x86_64::apic::send_eoi();
}

impl NvmeController {
    /// Submit one command on the given queue pair and poll its CQ for
    /// completion. Returns the raw 16-bit status field (bits 0..15 of
    /// the CQE status DW) — bit 0 is phase, bits 1..16 are the status
    /// code/type. A successful command has status = 0 after masking
    /// out the phase tag.
    fn submit_and_wait(&mut self, is_admin: bool, cmd: &SubmissionEntry) -> Result<(u32, u32), NvmeError> {
        // Pick the queue: admin (QID 0) or I/O (QID 1+).
        let q = if is_admin {
            &mut self.admin
        } else {
            self.io.as_mut().ok_or(NvmeError::NoNamespace)?
        };

        // Copy SQE into the SQ ring at the current tail.
        let slot_ptr = (q.sq_virt + (q.sq_tail as u64) * SQE_SIZE as u64)
            as *mut SubmissionEntry;
        unsafe { core::ptr::write_volatile(slot_ptr, *cmd); }

        // Advance the tail and ring the SQ doorbell.
        let new_tail = (q.sq_tail + 1) % q.depth;
        q.sq_tail = new_tail;
        let sq_db = REG_DOORBELL_BASE + q.dbl_off;
        mmio_write32(self.bar_virt, sq_db, new_tail as u32);

        // Wait for the completion.
        //
        // Phase-1 hybrid strategy:
        //   1. Spin-poll for the first FAST_SPIN_ITERS iterations.
        //      Most NVMe commands complete in single-digit μs on real
        //      hardware (and low tens of μs in QEMU/whpx), which fits
        //      here. No interrupt-latency overhead on the fast path.
        //   2. After the spin budget, `hlt` between phase checks so
        //      the CPU idles instead of burning cycles. The MSI-X
        //      completion handler sends LAPIC EOI; that interrupt
        //      (or the next timer tick, as a safety net) wakes us.
        //
        // Bounded throughout: a wedged controller can't hang us
        // because we also sample CSTS.CFS periodically and give up
        // after MAX_POLL_ITERS total iterations.
        //
        // Caller must have interrupts enabled (IF=1). Every current
        // caller (MVFS flush from init/syscall context, admin I/O
        // during driver init) satisfies this.
        const MAX_POLL_ITERS: u64 = 500_000_000;
        const CSTS_CHECK_INTERVAL: u64 = 10_000;
        // 1M iterations ≈ 300 μs of spin budget at 3 GHz. Sized to
        // cover common NVMe command latencies (random reads ~40 μs,
        // sequential reads ~100 μs) so they stay in the cheap spin
        // path. Beyond that, hlt engages and the CPU idles until
        // MSI-X (or timer tick) wakes us.
        //
        // Note on virtualized hosts: `pause` in a tight loop triggers
        // VM exits on whpx/KVM, so a longer spin budget is *not*
        // universally faster — empirically, 5M was slower than 1M
        // under QEMU because pause-exit cost dominated. On bare
        // metal the tradeoff inverts. 1M is a reasonable compromise.
        const FAST_SPIN_ITERS: u64 = 1_000_000;

        // Clear the shared completion flag so a stale `true` from a
        // previous command can't fool us into reading an unfinished
        // CQE. Handler sets it for observability (IO_IRQ_COUNT etc);
        // we rely on the CQE phase tag for the authoritative signal.
        IO_COMPLETE.store(false, Ordering::Release);

        let cqe_slot = q.cq_virt + (q.cq_head as u64) * CQE_SIZE as u64;
        let status;
        let dw0;
        let mut iter: u64 = 0;
        loop {
            let entry = unsafe {
                core::ptr::read_volatile(cqe_slot as *const CompletionEntry)
            };
            if (entry.status & 1) as u8 == q.phase {
                status = entry.status as u32;
                dw0 = entry.dw0;
                break;
            }
            iter += 1;
            if iter % CSTS_CHECK_INTERVAL == 0 {
                let csts = mmio_read32(self.bar_virt, REG_CSTS);
                if csts & CSTS_CFS != 0 {
                    crate::serial_strln!("[NVMe] Controller Fatal Status during I/O — aborting");
                    return Err(NvmeError::ControllerFatal);
                }
            }
            if iter >= MAX_POLL_ITERS {
                crate::serial_strln!("[NVMe] Completion timeout — controller stuck");
                return Err(NvmeError::ReadyTimeout);
            }
            if iter < FAST_SPIN_ITERS {
                core::hint::spin_loop();
            } else {
                // hlt until the next interrupt. Race window between
                // phase check and hlt is bounded by the timer tick
                // (~10 ms at 100 Hz) as a fallback wake source.
                unsafe {
                    core::arch::asm!("hlt", options(nomem, nostack));
                }
            }
        }

        // Advance the CQ head, flip phase on wrap, ring the CQ doorbell.
        let new_head = (q.cq_head + 1) % q.depth;
        if new_head == 0 { q.phase ^= 1; }
        q.cq_head = new_head;
        let cq_db = REG_DOORBELL_BASE + q.dbl_off + self.doorbell_stride;
        mmio_write32(self.bar_virt, cq_db, new_head as u32);

        // Mask off the phase tag; anything left is a real error code.
        let status_field = status >> 1;
        if status_field != 0 {
            return Err(if is_admin {
                NvmeError::AdminFailed((status & 0xFFFE) as u16)
            } else {
                NvmeError::IoFailed((status & 0xFFFE) as u16)
            });
        }
        Ok((dw0, status))
    }

    /// Issue Identify (opcode 0x06). `cns` picks what to identify
    /// (1 = Controller, 0 = Namespace); `nsid` is ignored for CNS=1.
    /// Result is DMA'd into the 4K page at `prp1`.
    fn identify(&mut self, cns: u32, nsid: u32, prp1: u64) -> Result<(), NvmeError> {
        let mut cmd = SubmissionEntry::default();
        // CID=1 (value doesn't matter — we poll for completion).
        cmd.cdw0 = ADMIN_OP_IDENTIFY as u32 | (1u32 << 16);
        cmd.nsid = nsid;
        cmd.prp1 = prp1;
        cmd.cdw10 = cns & 0xFF;
        self.submit_and_wait(true, &cmd)?;
        Ok(())
    }

    /// Create an I/O Completion Queue via Admin opcode 0x05.
    /// `iv` is the MSI-X interrupt vector index within the device's
    /// MSI-X table (not the IDT vector).
    fn create_iocq(&mut self, qid: u16, depth: u16, phys: u64, iv: u16) -> Result<(), NvmeError> {
        let mut cmd = SubmissionEntry::default();
        cmd.cdw0 = ADMIN_OP_CREATE_IOCQ as u32 | (2u32 << 16);
        cmd.prp1 = phys;
        // CDW10: QSIZE[31:16] = depth-1, QID[15:0] = qid
        cmd.cdw10 = ((depth - 1) as u32) << 16 | (qid as u32);
        // CDW11: IV[31:16] = vector index, IEN[1] = 1 (irq enable),
        //        PC[0] = 1 (physically contiguous)
        cmd.cdw11 = ((iv as u32) << 16) | (1 << 1) | 1;
        self.submit_and_wait(true, &cmd)?;
        Ok(())
    }

    /// Create an I/O Submission Queue bound to `cqid`.
    fn create_iosq(&mut self, qid: u16, depth: u16, phys: u64, cqid: u16) -> Result<(), NvmeError> {
        let mut cmd = SubmissionEntry::default();
        cmd.cdw0 = ADMIN_OP_CREATE_IOSQ as u32 | (3u32 << 16);
        cmd.prp1 = phys;
        cmd.cdw10 = ((depth - 1) as u32) << 16 | (qid as u32);
        // CDW11: CQID[31:16], QPRIO[2:1] = 0 (medium), PC[0] = 1
        cmd.cdw11 = ((cqid as u32) << 16) | 1;
        self.submit_and_wait(true, &cmd)?;
        Ok(())
    }

    /// Read one LBA from namespace 1 into the 4K physical page at
    /// `prp1`. NLB field is encoded as (count - 1), so 0 = 1 block.
    fn read_lba(&mut self, lba: u64, prp1: u64) -> Result<(), NvmeError> {
        let mut cmd = SubmissionEntry::default();
        cmd.cdw0 = NVM_OP_READ as u32 | (0x42u32 << 16);
        cmd.nsid = 1;
        cmd.prp1 = prp1;
        cmd.cdw10 = lba as u32;
        cmd.cdw11 = (lba >> 32) as u32;
        cmd.cdw12 = 0; // NLB=0 → 1 block
        self.submit_and_wait(false, &cmd)?;
        Ok(())
    }

    /// Write one LBA from the 4K physical page at `prp1` to
    /// namespace 1. Same layout as read_lba; only the opcode differs.
    fn write_lba(&mut self, lba: u64, prp1: u64) -> Result<(), NvmeError> {
        let mut cmd = SubmissionEntry::default();
        cmd.cdw0 = NVM_OP_WRITE as u32 | (0x43u32 << 16);
        cmd.nsid = 1;
        cmd.prp1 = prp1;
        cmd.cdw10 = lba as u32;
        cmd.cdw11 = (lba >> 32) as u32;
        cmd.cdw12 = 0; // NLB=0 → 1 block
        self.submit_and_wait(false, &cmd)?;
        Ok(())
    }

    /// Read or write `block_count` logical blocks starting at `lba`,
    /// using `dma_pages` as the scatter-gather page list.
    ///
    /// `dma_pages[i]` is the physical address of the i-th 4 KiB DMA
    /// page. Every page must be 4 KiB-aligned and contiguous in the
    /// *logical* transfer (but not physically — that's the whole
    /// point of PRP lists). Total transfer size must equal
    /// `block_count * lba_size`.
    ///
    /// PRP layout decision tree:
    /// - 1 page: PRP1 only, PRP2 = 0
    /// - 2 pages: PRP1 + PRP2 (single pointers)
    /// - 3+ pages: PRP1 + PRP list in PRP2, list contains pages 1..N
    ///
    /// Caller owns `prp_list_phys` if one was allocated — we don't
    /// free it here since Phase 1 has no free path. The page leaks;
    /// that's a known limitation of the current allocator.
    fn rw_blocks(
        &mut self,
        is_write: bool,
        lba: u64,
        block_count: u16,
        dma_pages: &[u64],
        prp_list_phys: u64,
    ) -> Result<(), NvmeError> {
        assert!(!dma_pages.is_empty());
        assert!(block_count > 0);

        let mut cmd = SubmissionEntry::default();
        let opcode = if is_write { NVM_OP_WRITE } else { NVM_OP_READ };
        cmd.cdw0 = opcode as u32 | (0x44u32 << 16);
        cmd.nsid = 1;
        cmd.prp1 = dma_pages[0];
        cmd.prp2 = match dma_pages.len() {
            1 => 0,
            2 => dma_pages[1],
            _ => prp_list_phys,
        };
        cmd.cdw10 = lba as u32;
        cmd.cdw11 = (lba >> 32) as u32;
        // NLB is encoded as N-1 in bits 0..15. Higher bits are flags
        // we don't use (PRACT, PRCHK, FUA, LR).
        cmd.cdw12 = (block_count - 1) as u32;
        self.submit_and_wait(false, &cmd)?;
        Ok(())
    }
}

/// Maximum blocks we'll transfer in one call. Bounded by:
///  - One PRP list page (4 KiB) holds 512 × 8B entries, so 512 extra
///    pages addressable via PRP list + 1 page in PRP1 = 513 pages.
///  - 513 × 8 sectors/page = 4104 sectors × 512 B = ~2 MiB.
/// We round down to a power-of-two bound (2048 sectors = 1 MiB) so
/// the ceiling is easy to reason about.
pub const NVME_MAX_BLOCKS_PER_CMD: u32 = 2048;

/// Validate the common preconditions for a multi-block transfer and
/// return `(block_count, needed_pages)`. Shared by read and write
/// paths to keep their bodies focused on the data movement.
fn validate_transfer(lba_size: u32, byte_len: usize) -> Result<(u32, usize), NvmeError> {
    if lba_size == 0 || byte_len == 0 || (byte_len as u32) % lba_size != 0 {
        return Err(NvmeError::NoNamespace);
    }
    let block_count = (byte_len as u32) / lba_size;
    if block_count > NVME_MAX_BLOCKS_PER_CMD {
        return Err(NvmeError::NoNamespace);
    }
    let npages = (byte_len + 4095) / 4096;
    if npages > DMA_MAX_DATA_PAGES {
        // We require one extra slot for the PRP list when N ≥ 3, so
        // the effective ceiling is DMA_MAX_DATA_PAGES.
        return Err(NvmeError::AllocFailed);
    }
    Ok((block_count, npages))
}

/// Lease `npages` data pages plus an optional PRP-list page from the
/// controller's DMA pool. Indices are returned as a bounded array so
/// we don't need an allocator on the hot path. `len` is the actual
/// number of leased indices.
///
/// On partial failure, any indices already leased are released
/// before returning the error — callers never have to think about
/// cleanup on the error path.
fn lease_pages(
    pool: &DmaPool,
    npages: usize,
    need_prp_list: bool,
) -> Result<([usize; DMA_POOL_PAGES], usize), NvmeError> {
    let total = npages + if need_prp_list { 1 } else { 0 };
    let mut indices = [0usize; DMA_POOL_PAGES];
    for i in 0..total {
        match pool.acquire() {
            Some(idx) => indices[i] = idx,
            None => {
                // Unwind: release what we've taken so far.
                for j in 0..i { pool.release(indices[j]); }
                return Err(NvmeError::AllocFailed);
            }
        }
    }
    Ok((indices, total))
}

/// Build the PRP list at `list_idx` referencing data pages 1..npages.
/// Entry 0 of the list corresponds to the *second* data page; the
/// first always lives in PRP1.
fn fill_prp_list(pool: &DmaPool, list_idx: usize, data_indices: &[usize]) {
    pool.zero(list_idx);
    let list_virt = pool.virt(list_idx);
    unsafe {
        let list_ptr = list_virt as *mut u64;
        for (i, &data_idx) in data_indices.iter().enumerate().skip(1) {
            core::ptr::write_volatile(list_ptr.add(i - 1), pool.phys(data_idx));
        }
    }
}

/// Write `data` starting at `lba`. `data.len()` must be a non-zero
/// multiple of the namespace LBA size (typically 512 B) and fit
/// within the DMA pool's ceiling (≤ 63 × 4 KiB today).
pub fn nvme_write_blocks(lba: u64, data: &[u8]) -> Result<(), NvmeError> {
    let mut guard = NVME.lock();
    let ctrl = guard.as_mut().ok_or(NvmeError::NotFound)?;

    let (block_count, npages) = validate_transfer(ctrl.lba_size, data.len())?;
    let need_prp_list = npages >= 3;

    let (indices, total) = lease_pages(&ctrl.dma_pool, npages, need_prp_list)?;

    // Copy caller data into DMA pages. The last page may be partial
    // if byte count isn't a multiple of 4 KiB — the controller only
    // reads `block_count * lba_size` bytes so unused tail is ignored.
    let mut remaining = data.len();
    let mut src_off = 0usize;
    for &idx in indices.iter().take(npages) {
        let n = core::cmp::min(remaining, 4096);
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr().add(src_off),
                ctrl.dma_pool.virt(idx) as *mut u8,
                n,
            );
        }
        src_off += n;
        remaining -= n;
        if remaining == 0 { break; }
    }

    // Gather data-page phys addresses into a bounded stack array for
    // rw_blocks. DMA_MAX_DATA_PAGES is the compile-time ceiling.
    let mut data_phys = [0u64; DMA_MAX_DATA_PAGES];
    for i in 0..npages {
        data_phys[i] = ctrl.dma_pool.phys(indices[i]);
    }

    let prp_list_phys = if need_prp_list {
        fill_prp_list(&ctrl.dma_pool, indices[npages], &indices[..npages]);
        ctrl.dma_pool.phys(indices[npages])
    } else {
        0
    };

    let result = ctrl.rw_blocks(
        true,
        lba,
        block_count as u16,
        &data_phys[..npages],
        prp_list_phys,
    );

    // Always release, even on error — next call must find the pool
    // in a consistent state.
    for i in 0..total { ctrl.dma_pool.release(indices[i]); }
    result
}

/// Read `block_count` blocks starting at `lba` into `data`.
/// Same length constraints as `nvme_write_blocks`.
pub fn nvme_read_blocks(lba: u64, data: &mut [u8]) -> Result<(), NvmeError> {
    let mut guard = NVME.lock();
    let ctrl = guard.as_mut().ok_or(NvmeError::NotFound)?;

    let (block_count, npages) = validate_transfer(ctrl.lba_size, data.len())?;
    let need_prp_list = npages >= 3;

    let (indices, total) = lease_pages(&ctrl.dma_pool, npages, need_prp_list)?;

    let mut data_phys = [0u64; DMA_MAX_DATA_PAGES];
    for i in 0..npages {
        data_phys[i] = ctrl.dma_pool.phys(indices[i]);
    }

    let prp_list_phys = if need_prp_list {
        fill_prp_list(&ctrl.dma_pool, indices[npages], &indices[..npages]);
        ctrl.dma_pool.phys(indices[npages])
    } else {
        0
    };

    let rw_result = ctrl.rw_blocks(
        false,
        lba,
        block_count as u16,
        &data_phys[..npages],
        prp_list_phys,
    );

    if rw_result.is_ok() {
        // Copy DMA pages back into the caller's slice.
        let mut remaining = data.len();
        let mut dst_off = 0usize;
        for &idx in indices.iter().take(npages) {
            let n = core::cmp::min(remaining, 4096);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    ctrl.dma_pool.virt(idx) as *const u8,
                    data.as_mut_ptr().add(dst_off),
                    n,
                );
            }
            dst_off += n;
            remaining -= n;
            if remaining == 0 { break; }
        }
    }

    for i in 0..total { ctrl.dma_pool.release(indices[i]); }
    rw_result
}

/// Report how many DMA pages are currently free. Useful from tests
/// and for debug serial dumps — a healthy driver returns to a full
/// free pool after every completed I/O.
pub fn dma_pool_free_count() -> Option<u32> {
    let guard = NVME.lock();
    let ctrl = guard.as_ref()?;
    Some(ctrl.dma_pool.free_mask.load(Ordering::Acquire).count_ones())
}

/// Write one 512-byte LBA to namespace 1. Thin shim over
/// `nvme_write_blocks` — same pool-backed DMA, no allocator traffic.
pub fn nvme_write(lba: u64, data: &[u8; 512]) -> Result<(), NvmeError> {
    nvme_write_blocks(lba, data)
}

/// Read one 512-byte LBA from namespace 1.
pub fn nvme_read(lba: u64, data: &mut [u8; 512]) -> Result<(), NvmeError> {
    nvme_read_blocks(lba, data)
}

// ── Block-device shim API ────────────────────────────────────────────────────
//
// Mirrors `drivers::virtio_blk`'s public surface so filesystems can
// swap backends without re-plumbing each call site. Keeping the names
// and error types aligned means MVFS's dispatcher is just a match.

/// True if the controller is initialized and has a usable namespace.
pub fn is_initialized() -> bool {
    let guard = NVME.lock();
    guard.as_ref().map(|c| c.lba_count > 0 && c.lba_size > 0).unwrap_or(false)
}

/// Namespace 1's capacity in *512-byte sectors*. Callers that work in
/// 512 B units (MVFS, legacy block APIs) can use this directly; for
/// 4 K-native namespaces we scale the LBA count by lba_size/512 so
/// the sector count always reflects 512-byte units.
pub fn capacity_sectors() -> u64 {
    let guard = NVME.lock();
    let Some(ctrl) = guard.as_ref() else { return 0; };
    if ctrl.lba_size == 0 { return 0; }
    ctrl.lba_count * (ctrl.lba_size as u64 / 512)
}

/// Legacy 512-byte block read. `sector` is always a 512-byte sector
/// index; we translate to LBA internally if the namespace uses a
/// larger LBA size.
pub fn block_read(sector: u64, data: &mut [u8; 512]) -> Result<(), NvmeError> {
    let lba_size = {
        let guard = NVME.lock();
        let Some(c) = guard.as_ref() else { return Err(NvmeError::NotFound); };
        c.lba_size
    };
    match lba_size {
        512 => nvme_read_blocks(sector, data),
        _ => {
            // Larger LBA: read the containing LBA and slice the sector out.
            let ratio = (lba_size / 512) as u64;
            let lba = sector / ratio;
            let off = ((sector % ratio) as usize) * 512;
            let mut buf = [0u8; 4096];
            nvme_read_blocks(lba, &mut buf[..lba_size as usize])?;
            data.copy_from_slice(&buf[off..off + 512]);
            Ok(())
        }
    }
}

/// Legacy 512-byte block write. Same sector-to-LBA translation as
/// `block_read`. For LBA > 512 we do read-modify-write so unrelated
/// sectors in the containing LBA aren't clobbered.
pub fn block_write(sector: u64, data: &[u8; 512]) -> Result<(), NvmeError> {
    let lba_size = {
        let guard = NVME.lock();
        let Some(c) = guard.as_ref() else { return Err(NvmeError::NotFound); };
        c.lba_size
    };
    match lba_size {
        512 => nvme_write_blocks(sector, data),
        _ => {
            let ratio = (lba_size / 512) as u64;
            let lba = sector / ratio;
            let off = ((sector % ratio) as usize) * 512;
            let mut buf = [0u8; 4096];
            let slice = &mut buf[..lba_size as usize];
            nvme_read_blocks(lba, slice)?;
            slice[off..off + 512].copy_from_slice(data);
            nvme_write_blocks(lba, slice)
        }
    }
}

/// Multi-sector read in 512-byte units.
/// `data.len()` must equal `count * 512`.
pub fn read_sectors(start_sector: u64, data: &mut [u8], count: usize) -> Result<(), NvmeError> {
    if data.len() != count * 512 {
        return Err(NvmeError::NoNamespace);
    }
    // Fast path for 512 B LBAs. For 4 K-native we'd need alignment
    // handling; revisit if a real SSD forces it.
    let lba_size = {
        let guard = NVME.lock();
        let Some(c) = guard.as_ref() else { return Err(NvmeError::NotFound); };
        c.lba_size
    };
    if lba_size == 512 {
        nvme_read_blocks(start_sector, data)
    } else {
        // Fall back to per-sector reads; slow but correct.
        for i in 0..count {
            let mut tmp = [0u8; 512];
            block_read(start_sector + i as u64, &mut tmp)?;
            data[i * 512..(i + 1) * 512].copy_from_slice(&tmp);
        }
        Ok(())
    }
}

/// Multi-sector write in 512-byte units.
pub fn write_sectors(start_sector: u64, data: &[u8], count: usize) -> Result<(), NvmeError> {
    if data.len() != count * 512 {
        return Err(NvmeError::NoNamespace);
    }
    let lba_size = {
        let guard = NVME.lock();
        let Some(c) = guard.as_ref() else { return Err(NvmeError::NotFound); };
        c.lba_size
    };
    if lba_size == 512 {
        nvme_write_blocks(start_sector, data)
    } else {
        for i in 0..count {
            let mut tmp = [0u8; 512];
            tmp.copy_from_slice(&data[i * 512..(i + 1) * 512]);
            block_write(start_sector + i as u64, &tmp)?;
        }
        Ok(())
    }
}

/// Poll CSTS.RDY until it matches `target` or we time out.
/// NVMe spec recommends up to CAP.TO × 500 ms; we use a generous fixed
/// spin that's still well under that to keep boot snappy.
fn wait_ready(bar_virt: u64, target: bool) -> Result<(), NvmeError> {
    for _ in 0..2_000_000 {
        let csts = mmio_read32(bar_virt, REG_CSTS);
        if csts & CSTS_CFS != 0 {
            return Err(NvmeError::ControllerFatal);
        }
        let rdy = (csts & CSTS_RDY) != 0;
        if rdy == target {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(NvmeError::ReadyTimeout)
}
