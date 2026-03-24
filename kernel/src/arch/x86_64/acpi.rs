//! ACPI Table Parsing — MADT for SMP CPU enumeration
//!
//! Parses RSDP → RSDT/XSDT → MADT to discover Application Processor APIC IDs.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Number of Application Processors (excludes BSP)
pub static AP_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Maximum supported CPUs
const MAX_CPUS: usize = 16;

/// AP APIC IDs (filled by parse_madt)
static mut AP_APIC_IDS: [u8; MAX_CPUS] = [0; MAX_CPUS];

/// Get AP APIC IDs slice
pub fn ap_apic_ids() -> &'static [u8] {
    let count = AP_COUNT.load(Ordering::Relaxed);
    unsafe { &AP_APIC_IDS[..count] }
}

// --- ACPI table structures ---

#[repr(C, packed)]
struct Rsdp {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt_address: u32,
}

#[repr(C, packed)]
struct Rsdp20 {
    v1: Rsdp,
    length: u32,
    xsdt_address: u64,
    extended_checksum: u8,
    _reserved: [u8; 3],
}

#[repr(C, packed)]
struct SdtHeader {
    signature: [u8; 4],
    length: u32,
    revision: u8,
    checksum: u8,
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    oem_revision: u32,
    creator_id: u32,
    creator_revision: u32,
}

#[repr(C, packed)]
struct MadtHeader {
    header: SdtHeader,
    local_apic_addr: u32,
    flags: u32,
    // Variable-length entries follow
}

const MADT_LOCAL_APIC: u8 = 0;

#[repr(C, packed)]
struct MadtLocalApic {
    entry_type: u8,
    length: u8,
    acpi_processor_id: u8,
    apic_id: u8,
    flags: u32,
}

/// Initialize ACPI — parse MADT for CPU topology
pub fn init(rsdp_addr: usize) {
    if rsdp_addr == 0 {
        crate::serial_str!("[ACPI] No RSDP address — skipping\n");
        return;
    }

    let hhdm = crate::memory::paging::hhdm_offset();

    // Map RSDP page into kernel address space first (RSDP is in ACPI-reclaimable memory
    // which may not be mapped in HHDM). Map 2 pages around the address to be safe.
    let rsdp_phys_page = rsdp_addr & !0xFFF;
    let rsdp_virt_base = hhdm + rsdp_phys_page;
    use x86_64::structures::paging::PageTableFlags;
    let flags = PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE;
    // Map the RSDP page (and next page in case it spans a boundary)
    let _ = crate::memory::paging::map_page(rsdp_virt_base, rsdp_phys_page, flags);
    let _ = crate::memory::paging::map_page(rsdp_virt_base + 0x1000, rsdp_phys_page + 0x1000, flags);

    let rsdp_virt = hhdm + rsdp_addr;

    crate::serial_str!("[ACPI] RSDP at phys=");
    crate::drivers::serial::write_hex(rsdp_addr as u64);
    crate::serial_str!(" virt=");
    crate::drivers::serial::write_hex(rsdp_virt as u64);
    crate::drivers::serial::write_newline();

    // Read RSDP
    let rsdp = unsafe { &*(rsdp_virt as *const Rsdp) };
    let sig = &rsdp.signature;
    if sig != b"RSD PTR " {
        crate::serial_str!("[ACPI] Invalid RSDP signature\n");
        return;
    }

    crate::serial_str!("[ACPI] RSDP found, revision=");
    crate::drivers::serial::write_dec(rsdp.revision as u32);
    crate::drivers::serial::write_newline();

    // Find MADT in RSDT or XSDT
    let madt_virt = if rsdp.revision >= 2 {
        // ACPI 2.0+ — use XSDT (64-bit pointers)
        let rsdp20 = unsafe { &*((hhdm + rsdp_addr) as *const Rsdp20) };
        let xsdt_phys = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(rsdp20.xsdt_address)) };
        find_table_in_xsdt(hhdm, xsdt_phys as usize, b"APIC")
    } else {
        // ACPI 1.0 — use RSDT (32-bit pointers)
        let rsdt_phys = rsdp.rsdt_address as usize;
        find_table_in_rsdt(hhdm, rsdt_phys, b"APIC")
    };

    let madt_virt = match madt_virt {
        Some(v) => v,
        None => {
            crate::serial_str!("[ACPI] MADT not found\n");
            return;
        }
    };

    // Parse MADT entries — ensure full table is mapped
    let madt = unsafe { &*(madt_virt as *const MadtHeader) };
    let madt_length = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(madt.header.length)) } as usize;
    // Map additional pages if MADT spans more than the initial page
    let madt_phys = madt_virt - hhdm;
    ensure_mapped(hhdm, madt_phys, madt_length);
    let bsp_apic_id = super::apic::get_apic_id();

    crate::serial_str!("[ACPI] MADT found, length=");
    crate::drivers::serial::write_dec(madt_length as u32);
    crate::serial_str!(", BSP APIC ID=");
    crate::drivers::serial::write_dec(bsp_apic_id as u32);
    crate::drivers::serial::write_newline();

    // Walk variable-length entries after MadtHeader (44 bytes)
    let entries_start = madt_virt + core::mem::size_of::<MadtHeader>();
    let entries_end = madt_virt + madt_length;
    let mut offset = entries_start;
    let mut ap_count = 0usize;
    let mut total_cpus = 0u32;

    while offset + 2 <= entries_end {
        let entry_type = unsafe { *(offset as *const u8) };
        let entry_len = unsafe { *((offset + 1) as *const u8) } as usize;
        if entry_len < 2 {
            break;
        }

        if entry_type == MADT_LOCAL_APIC && entry_len >= 8 {
            let lapic = unsafe { &*(offset as *const MadtLocalApic) };
            let flags = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(lapic.flags)) };
            let apic_id = lapic.apic_id;

            // bit 0 = enabled, bit 1 = online capable
            if flags & 1 != 0 {
                total_cpus += 1;
                if apic_id != bsp_apic_id && ap_count < MAX_CPUS {
                    unsafe { AP_APIC_IDS[ap_count] = apic_id; }
                    ap_count += 1;
                }
            }
        }

        offset += entry_len;
    }

    AP_COUNT.store(ap_count, Ordering::Relaxed);

    crate::serial_str!("[ACPI] Found ");
    crate::drivers::serial::write_dec(total_cpus);
    crate::serial_str!(" CPUs (");
    crate::drivers::serial::write_dec(ap_count as u32);
    crate::serial_str!(" APs)\n");
}

/// Ensure a physical address range is accessible via HHDM by mapping the page(s)
fn ensure_mapped(hhdm: usize, phys_addr: usize, size: usize) {
    use x86_64::structures::paging::PageTableFlags;
    let flags = PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE;
    let start_page = phys_addr & !0xFFF;
    let end_page = (phys_addr + size + 0xFFF) & !0xFFF;
    let mut page = start_page;
    while page < end_page {
        let _ = crate::memory::paging::map_page(hhdm + page, page, flags);
        page += 0x1000;
    }
}

fn find_table_in_rsdt(hhdm: usize, rsdt_phys: usize, sig: &[u8; 4]) -> Option<usize> {
    ensure_mapped(hhdm, rsdt_phys, 0x1000);
    let rsdt_virt = hhdm + rsdt_phys;
    let header = unsafe { &*(rsdt_virt as *const SdtHeader) };
    let length = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(header.length)) } as usize;
    ensure_mapped(hhdm, rsdt_phys, length);
    let entry_count = (length - core::mem::size_of::<SdtHeader>()) / 4;
    let entries = (rsdt_virt + core::mem::size_of::<SdtHeader>()) as *const u32;

    for i in 0..entry_count {
        let table_phys = unsafe { core::ptr::read_unaligned(entries.add(i)) } as usize;
        ensure_mapped(hhdm, table_phys, 0x1000);
        let table_virt = hhdm + table_phys;
        let table_sig = unsafe { &*(table_virt as *const [u8; 4]) };
        if table_sig == sig {
            return Some(table_virt);
        }
    }
    None
}

fn find_table_in_xsdt(hhdm: usize, xsdt_phys: usize, sig: &[u8; 4]) -> Option<usize> {
    ensure_mapped(hhdm, xsdt_phys, 0x1000);
    let xsdt_virt = hhdm + xsdt_phys;
    let header = unsafe { &*(xsdt_virt as *const SdtHeader) };
    let length = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(header.length)) } as usize;
    ensure_mapped(hhdm, xsdt_phys, length);
    let entry_count = (length - core::mem::size_of::<SdtHeader>()) / 8;
    let entries = (xsdt_virt + core::mem::size_of::<SdtHeader>()) as *const u64;

    for i in 0..entry_count {
        let table_phys = unsafe { core::ptr::read_unaligned(entries.add(i)) } as usize;
        ensure_mapped(hhdm, table_phys, 0x1000);
        let table_virt = hhdm + table_phys;
        let table_sig = unsafe { &*(table_virt as *const [u8; 4]) };
        if table_sig == sig {
            return Some(table_virt);
        }
    }
    None
}
