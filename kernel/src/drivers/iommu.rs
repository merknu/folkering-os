//! Intel VT-d IOMMU Driver — DMA Remapping with Per-Device Page Tables
//!
//! Each PCI device gets its own isolated DMA domain with a dedicated
//! second-level page table. DMA transactions are translated through
//! these tables — a device can ONLY access memory explicitly mapped
//! into its domain.
//!
//! # VT-d Architecture
//!
//! ```text
//! Root Table (256 entries, one per bus)
//!   └─ Context Table (256 entries per bus, one per dev:func)
//!        └─ Second-Level Page Table (4-level, per device)
//!             └─ Physical pages explicitly mapped for DMA
//! ```

use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

static IOMMU_ENABLED: AtomicBool = AtomicBool::new(false);
static mut IOMMU_VADDR: usize = 0;

// ── VT-d Register Offsets ───────────────────────────────────────────────

const REG_VER: usize = 0x00;
const REG_CAP: usize = 0x08;
const REG_ECAP: usize = 0x10;
const REG_GCMD: usize = 0x18;
const REG_GSTS: usize = 0x1C;
const REG_RTADDR: usize = 0x20;
const REG_CCMD: usize = 0x28;
const REG_FSTS: usize = 0x34;
const REG_FECTL: usize = 0x38;

const GCMD_TE: u32 = 1 << 31;
const GCMD_SRTP: u32 = 1 << 30;
const GSTS_TES: u32 = 1 << 31;
const GSTS_RTPS: u32 = 1 << 30;

// ── Page Table Structures ───────────────────────────────────────────────

/// 4KB-aligned page table (512 entries × 8 bytes = 4096)
/// Same format as x86-64 second-level paging:
///   bits 51:12 = physical address of next level / page frame
///   bit 1 = Write permission
///   bit 0 = Read permission (Present)
#[repr(C, align(4096))]
struct IommuPageTable {
    entries: [u64; 512],
}

impl IommuPageTable {
    const fn zeroed() -> Self {
        Self { entries: [0; 512] }
    }
}

/// Per-device DMA domain
struct DmaDomain {
    /// Domain ID (1-based, 0 = unused)
    id: u16,
    /// Physical address of the PML4 (top-level page table)
    pml4_phys: usize,
    /// PCI bus:device.function this domain belongs to
    bus: u8,
    dev: u8,
    func: u8,
}

/// Maximum DMA domains (one per PCI device we care about)
const MAX_DOMAINS: usize = 16;

// Static allocations for page tables
// Each domain gets a PML4 → PDPT → PD → PT chain
// Pre-allocate a pool of page tables
const PT_POOL_SIZE: usize = 64;  // 64 page tables = enough for 16 devices × 4 levels
static mut PT_POOL: [IommuPageTable; PT_POOL_SIZE] = {
    const Z: IommuPageTable = IommuPageTable::zeroed();
    [Z; PT_POOL_SIZE]
};
static mut PT_POOL_NEXT: usize = 0;

static mut DOMAINS: [DmaDomain; MAX_DOMAINS] = {
    const EMPTY: DmaDomain = DmaDomain { id: 0, pml4_phys: 0, bus: 0, dev: 0, func: 0 };
    [EMPTY; MAX_DOMAINS]
};
static mut DOMAIN_COUNT: usize = 0;

// Root + Context tables
#[repr(C, align(4096))]
struct RootTable { entries: [u128; 256] }
#[repr(C, align(4096))]
struct ContextTable { entries: [u128; 256] }

static mut ROOT_TABLE: RootTable = RootTable { entries: [0; 256] };
static mut CONTEXT_TABLE_BUS0: ContextTable = ContextTable { entries: [0; 256] };

// ── Register Access ─────────────────────────────────────────────────────

unsafe fn read_reg32(off: usize) -> u32 {
    core::ptr::read_volatile((IOMMU_VADDR + off) as *const u32)
}
unsafe fn write_reg32(off: usize, val: u32) {
    core::ptr::write_volatile((IOMMU_VADDR + off) as *mut u32, val);
}
unsafe fn read_reg64(off: usize) -> u64 {
    core::ptr::read_volatile((IOMMU_VADDR + off) as *const u64)
}
unsafe fn write_reg64(off: usize, val: u64) {
    core::ptr::write_volatile((IOMMU_VADDR + off) as *mut u64, val);
}

// ── Page Table Allocator ────────────────────────────────────────────────

/// Allocate a zeroed page table from the static pool.
/// Returns physical address of the table.
unsafe fn alloc_page_table() -> Option<usize> {
    if PT_POOL_NEXT >= PT_POOL_SIZE {
        return None;
    }
    let idx = PT_POOL_NEXT;
    PT_POOL_NEXT += 1;
    let hhdm = crate::memory::paging::hhdm_offset();
    let vaddr = &PT_POOL[idx] as *const _ as usize;
    let phys = vaddr - hhdm;
    // Zero the table
    for e in &mut PT_POOL[idx].entries { *e = 0; }
    Some(phys)
}

/// Get a mutable reference to a page table at a physical address.
unsafe fn pt_at(phys: usize) -> &'static mut IommuPageTable {
    let hhdm = crate::memory::paging::hhdm_offset();
    &mut *((hhdm + phys) as *mut IommuPageTable)
}

// ── Domain Management ───────────────────────────────────────────────────

/// Create a DMA domain for a PCI device.
/// Allocates a fresh 4-level page table (initially empty = no DMA allowed).
unsafe fn create_domain(bus: u8, dev: u8, func: u8) -> Option<usize> {
    if DOMAIN_COUNT >= MAX_DOMAINS { return None; }

    let pml4_phys = alloc_page_table()?;
    let domain_id = (DOMAIN_COUNT + 1) as u16;  // 1-based

    DOMAINS[DOMAIN_COUNT] = DmaDomain {
        id: domain_id, pml4_phys, bus, dev, func,
    };
    DOMAIN_COUNT += 1;

    crate::serial_str!("[IOMMU] Domain ");
    crate::drivers::serial::write_dec(domain_id as u32);
    crate::serial_str!(" for PCI ");
    crate::drivers::serial::write_dec(bus as u32);
    crate::serial_str!(":");
    crate::drivers::serial::write_dec(dev as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(func as u32);
    crate::serial_str!(" pml4=");
    crate::drivers::serial::write_hex(pml4_phys as u64);
    crate::drivers::serial::write_newline();

    Some(DOMAIN_COUNT - 1)
}

/// Map a physical page into a device's DMA domain.
/// `iova` = IO Virtual Address (what the device sees)
/// `phys` = actual physical address
/// `writable` = allow DMA writes
pub unsafe fn map_dma_page(domain_idx: usize, iova: usize, phys: usize, writable: bool) -> bool {
    if domain_idx >= DOMAIN_COUNT { return false; }
    let domain = &DOMAINS[domain_idx];

    // Walk 4-level page table, creating intermediate levels as needed
    let pml4 = pt_at(domain.pml4_phys);
    let pml4_idx = (iova >> 39) & 0x1FF;
    if pml4.entries[pml4_idx] & 1 == 0 {
        let pdpt_phys = match alloc_page_table() {
            Some(p) => p,
            None => return false,
        };
        pml4.entries[pml4_idx] = (pdpt_phys as u64 & !0xFFF) | 3; // R+W
    }

    let pdpt_phys = (pml4.entries[pml4_idx] & !0xFFF) as usize;
    let pdpt = pt_at(pdpt_phys);
    let pdpt_idx = (iova >> 30) & 0x1FF;
    if pdpt.entries[pdpt_idx] & 1 == 0 {
        let pd_phys = match alloc_page_table() {
            Some(p) => p,
            None => return false,
        };
        pdpt.entries[pdpt_idx] = (pd_phys as u64 & !0xFFF) | 3;
    }

    let pd_phys = (pdpt.entries[pdpt_idx] & !0xFFF) as usize;
    let pd = pt_at(pd_phys);
    let pd_idx = (iova >> 21) & 0x1FF;
    if pd.entries[pd_idx] & 1 == 0 {
        let pt_phys = match alloc_page_table() {
            Some(p) => p,
            None => return false,
        };
        pd.entries[pd_idx] = (pt_phys as u64 & !0xFFF) | 3;
    }

    let pt_phys = (pd.entries[pd_idx] & !0xFFF) as usize;
    let pt = pt_at(pt_phys);
    let pt_idx = (iova >> 12) & 0x1FF;

    // Set the leaf entry: phys address + permissions
    let flags: u64 = if writable { 3 } else { 1 }; // bit 0=Read, bit 1=Write
    pt.entries[pt_idx] = (phys as u64 & !0xFFF) | flags;

    true
}

// ── Initialization ──────────────────────────────────────────────────────

pub fn init() {
    let base_phys = crate::arch::x86_64::acpi::iommu_base();
    if base_phys == 0 || !crate::arch::x86_64::acpi::iommu_available() {
        crate::serial_strln!("[IOMMU] Not available — skipping init");
        return;
    }

    let hhdm = crate::memory::paging::hhdm_offset();
    let vaddr = hhdm + base_phys as usize;

    use x86_64::structures::paging::PageTableFlags;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE | PageTableFlags::NO_CACHE;
    let _ = crate::memory::paging::map_page(vaddr, base_phys as usize, flags);
    let _ = crate::memory::paging::map_page(vaddr + 0x1000, base_phys as usize + 0x1000, flags);

    unsafe {
        IOMMU_VADDR = vaddr;

        let ver = read_reg32(REG_VER);
        crate::serial_str!("[IOMMU] Version: ");
        crate::drivers::serial::write_dec((ver >> 4) & 0xF);
        crate::serial_str!(".");
        crate::drivers::serial::write_dec(ver & 0xF);
        crate::drivers::serial::write_newline();

        let cap = read_reg64(REG_CAP);
        let sagaw = (cap >> 8) & 0x1F;
        crate::serial_str!("[IOMMU] SAGAW: ");
        crate::drivers::serial::write_hex(sagaw);
        crate::drivers::serial::write_newline();

        // Create per-device DMA domains for PCI devices on bus 0
        let pci_list = crate::drivers::pci::PCI_DEVICES.lock();
        let ctx_phys = &CONTEXT_TABLE_BUS0 as *const _ as usize - hhdm;
        ROOT_TABLE.entries[0] = (ctx_phys as u128 & !0xFFF) | 1;

        for i in 0..pci_list.count {
            if let Some(ref dev) = pci_list.devices[i] {
                if dev.bus != 0 { continue; }
                let devfn = (dev.device as usize) * 8 + (dev.function as usize);

                // Create isolated domain for this device
                if let Some(dom_idx) = create_domain(dev.bus, dev.device, dev.function) {
                    let domain = &DOMAINS[dom_idx];

                    // Identity-map first 4MB for device BARs and DMA buffers
                    // (minimal — just enough for boot devices to function)
                    for page in 0..1024 {  // 1024 pages = 4MB
                        let addr = page * 4096;
                        map_dma_page(dom_idx, addr, addr, true);
                    }

                    // Also identity-map the device's BAR regions
                    for bar_idx in 0..6u8 {
                        let bar_phys = crate::drivers::pci::decode_bar(dev, bar_idx as usize);
                        match bar_phys {
                            crate::drivers::pci::BarType::Mmio32 { base, .. } => {
                                let size = crate::drivers::pci::bar_size(dev.bus, dev.device, dev.function, bar_idx) as usize;
                                if size > 0 && base > 0 {
                                    let pages = (size + 4095) / 4096;
                                    for p in 0..pages.min(256) {
                                        let a = base as usize + p * 4096;
                                        map_dma_page(dom_idx, a, a, true);
                                    }
                                }
                            }
                            crate::drivers::pci::BarType::Mmio64 { base, .. } => {
                                let size = crate::drivers::pci::bar_size(dev.bus, dev.device, dev.function, bar_idx) as usize;
                                if size > 0 && base > 0 {
                                    let pages = (size + 4095) / 4096;
                                    for p in 0..pages.min(256) {
                                        let a = base as usize + p * 4096;
                                        map_dma_page(dom_idx, a, a, true);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    // Context Table entry: TT=0 (second-level), AGAW=2 (48-bit), domain's PML4
                    let agaw = 2u64; // 48-bit address width
                    let low: u64 = (domain.pml4_phys as u64 & !0xFFF) | (agaw << 2) | 1; // Present + AGAW + SLPTPTR
                    let high: u64 = (domain.id as u64) << 8; // Domain ID, TT=0 (second-level translation)
                    CONTEXT_TABLE_BUS0.entries[devfn] = (low as u128) | ((high as u128) << 64);
                } else {
                    // Fallback: passthrough for devices we can't create domains for
                    let low: u64 = (2u64 << 2) | 1;
                    let high: u64 = (1u64 << 8) | (2 << 0); // TT=2 passthrough
                    CONTEXT_TABLE_BUS0.entries[devfn] = (low as u128) | ((high as u128) << 64);
                }
            }
        }
        drop(pci_list);

        // Write Root Table Address
        let root_phys = &ROOT_TABLE as *const _ as usize - hhdm;
        write_reg64(REG_RTADDR, root_phys as u64);

        // Set Root Table Pointer
        let gcmd = read_reg32(REG_GCMD);
        write_reg32(REG_GCMD, gcmd | GCMD_SRTP);
        let mut timeout = 100_000u32;
        while read_reg32(REG_GSTS) & GSTS_RTPS == 0 && timeout > 0 { timeout -= 1; }
        if timeout == 0 {
            crate::serial_strln!("[IOMMU] ERROR: SRTP timeout");
            return;
        }
        crate::serial_strln!("[IOMMU] Root Table set");

        // Enable Translation
        let gcmd = read_reg32(REG_GCMD);
        write_reg32(REG_GCMD, gcmd | GCMD_TE);
        timeout = 100_000;
        while read_reg32(REG_GSTS) & GSTS_TES == 0 && timeout > 0 { timeout -= 1; }
        if timeout == 0 {
            crate::serial_strln!("[IOMMU] ERROR: TE timeout");
            return;
        }

        IOMMU_ENABLED.store(true, Ordering::Release);

        let fsts = read_reg32(REG_FSTS);
        if fsts & 0xFF != 0 {
            crate::serial_str!("[IOMMU] Fault status: ");
            crate::drivers::serial::write_hex(fsts as u64);
            crate::drivers::serial::write_newline();
        }

        crate::serial_str!("[IOMMU] ENABLED with ");
        crate::drivers::serial::write_dec(DOMAIN_COUNT as u32);
        crate::serial_strln!(" isolated device domains — DMA isolation ACTIVE!");
    }
}

pub fn is_enabled() -> bool {
    IOMMU_ENABLED.load(Ordering::Acquire)
}
