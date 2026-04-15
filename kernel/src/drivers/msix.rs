//! MSI-X (Message Signaled Interrupts, Extended) support for PCIe drivers.
//!
//! MSI-X replaces legacy INTx / IOAPIC interrupt routing with
//! device-originated memory writes that land directly on a target
//! LAPIC. Per-queue vectors, no shared IRQ lines, no IOAPIC mask-
//! storm mitigation.
//!
//! # Message format
//!
//! A memory write to `0xFEE0_0000 | (dest_apic_id << 12)` with data
//! `vector as u32` is delivered as a fixed-mode, edge-triggered
//! interrupt at IDT `vector` on the LAPIC identified by
//! `dest_apic_id`. Bits 4..8 of the data word encode delivery mode
//! (0 = fixed), bit 14 = level (we use 0 = edge), bit 15 = trigger
//! mode (ignored for edge).
//!
//! # Table layout (per the PCIe spec)
//!
//! Each MSI-X vector table entry is 16 bytes:
//!
//! ```text
//!   bytes  0..4   message_address_lo
//!   bytes  4..8   message_address_hi (0 on x86-64, we target LAPIC MMIO)
//!   bytes  8..12  message_data
//!   bytes 12..16  vector_control (bit 0 = mask)
//! ```
//!
//! The table lives at `BAR[table_bar] + table_offset`. BIR and offset
//! are encoded in the MSI-X Capability's Table Offset/BIR register
//! at `cap_offset + 4`: lower 3 bits = BAR index, upper 29 bits =
//! byte offset (table is always 8-byte aligned).
//!
//! # Vector allocation
//!
//! Vectors 64..=95 are reserved for MSI-X — outside the static
//! allocations (33 keyboard, 44 mouse, 45 VirtIO-blk legacy fallback,
//! 46-57 WASM drivers, 58-63 headroom). A simple atomic bitmap
//! tracks which are free.

use core::sync::atomic::{AtomicU32, Ordering};

use super::pci::{self, PciDevice, BarType};

/// PCI capability ID for MSI-X (from PCI spec).
pub const PCI_CAP_ID_MSIX: u8 = 0x11;

/// MSI-X Enable bit in the Message Control register (at cap_offset + 2).
const MSIX_CTRL_ENABLE: u16 = 1 << 15;

/// Mask-All bit in the Message Control register.
const MSIX_CTRL_MASK_ALL: u16 = 1 << 14;

/// Vector Control mask bit (per-entry, at entry + 12).
const MSIX_VECTOR_CONTROL_MASK: u32 = 1 << 0;

/// LAPIC MMIO base. MSI writes to `LAPIC_BASE | (dest << 12)` land
/// on the LAPIC with APIC ID = `dest`.
const LAPIC_MSI_BASE: u64 = 0xFEE0_0000;

/// Lowest IDT vector reserved for MSI-X.
pub const MSIX_VECTOR_MIN: u8 = 64;
/// Highest IDT vector (inclusive) reserved for MSI-X.
pub const MSIX_VECTOR_MAX: u8 = 95;

/// Bitmap of free MSI-X vectors. Bit `i` set == vector `MSIX_VECTOR_MIN + i` is free.
/// 32 bits covers the full 64..=95 range.
static FREE_VECTORS: AtomicU32 = AtomicU32::new(0xFFFF_FFFF);

/// Parsed MSI-X capability information. Returned by `parse_cap`.
#[derive(Debug, Clone, Copy)]
pub struct MsixCapInfo {
    /// Offset of the MSI-X cap in PCI config space (used for writes
    /// to Message Control and for enable).
    pub cap_offset: u8,
    /// Number of vectors the device supports (1..=2048).
    pub table_size: u16,
    /// BAR index (0..=5) holding the vector table.
    pub table_bar: u8,
    /// Byte offset within that BAR where the table starts.
    pub table_offset: u32,
}

/// Walk the device's PCI capability list, looking for MSI-X.
/// Returns `None` if the device doesn't support MSI-X.
pub fn parse_cap(dev: &PciDevice) -> Option<MsixCapInfo> {
    let mut ptr = dev.capabilities_ptr;
    // Safety bound: 48 capabilities is the maximum a spec-compliant
    // device can legally chain. If we see more we assume corrupt
    // config space and bail.
    for _ in 0..48 {
        if ptr == 0 { return None; }
        let cap_id = pci::pci_read8(dev.bus, dev.device, dev.function, ptr);
        if cap_id == PCI_CAP_ID_MSIX {
            let msg_ctrl = pci::pci_read16(dev.bus, dev.device, dev.function, ptr + 2);
            // Bits 0..10 = Table Size - 1 (the encoded value is N-1
            // where N is the number of vectors). Max legal = 2047.
            let table_size = (msg_ctrl & 0x07FF) + 1;

            // Table Offset / BIR register at cap+4.
            //   bits 0..3   = BIR (BAR index)
            //   bits 3..32  = byte offset (always 8-aligned per spec,
            //                 so bits 0..3 being BIR is unambiguous)
            let table_reg = pci::pci_read32(dev.bus, dev.device, dev.function, ptr + 4);
            let table_bar = (table_reg & 0x07) as u8;
            let table_offset = table_reg & !0x07;

            return Some(MsixCapInfo {
                cap_offset: ptr,
                table_size,
                table_bar,
                table_offset,
            });
        }
        let next = pci::pci_read8(dev.bus, dev.device, dev.function, ptr + 1);
        ptr = next;
    }
    None
}

/// Locate the MSI-X table's virtual address. Combines the cap's
/// (BAR index, offset) with the device's decoded BAR, maps the MMIO
/// page(s) covering the table as NO_CACHE, and returns an HHDM-
/// mapped pointer to the first entry.
///
/// HHDM covers only RAM regions by default — MMIO BARs live in device
/// address space and must be mapped explicitly with the correct
/// caching attributes. We map enough pages to cover the full table
/// (`table_size * 16` bytes starting from `table_offset`).
///
/// Returns `None` if the BAR isn't MMIO (MSI-X tables must be in
/// MMIO BARs, not I/O BARs) or if the BAR index is out of range.
pub fn locate_table(dev: &PciDevice, cap: &MsixCapInfo) -> Option<u64> {
    if cap.table_bar >= 6 { return None; }
    let phys_base = match pci::decode_bar(dev, cap.table_bar as usize) {
        BarType::Mmio32 { base, .. } => base as u64,
        BarType::Mmio64 { base, .. } => base,
        _ => return None,
    };
    let phys_addr = phys_base + cap.table_offset as u64;
    let hhdm = crate::HHDM_OFFSET.load(Ordering::Relaxed) as u64;

    // Map every 4K page the table spans. NO_CACHE is critical — MMIO
    // writes must not be coalesced or reordered by the cache.
    use x86_64::structures::paging::PageTableFlags;
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE
        | PageTableFlags::NO_EXECUTE | PageTableFlags::NO_CACHE;
    let table_bytes = (cap.table_size as u64) * 16;
    let first_page = phys_addr & !0xFFF;
    let last_page = (phys_addr + table_bytes - 1) & !0xFFF;
    let mut page = first_page;
    while page <= last_page {
        // Ignore "already mapped" — another driver (or a prior call on
        // the same device) may own this page. That's fine; we just need
        // the page to exist with the right flags.
        let _ = crate::memory::paging::map_page(
            (hhdm + page) as usize,
            page as usize,
            flags,
        );
        page += 4096;
    }

    Some(hhdm + phys_addr)
}

/// Claim a free IDT vector in the MSI-X range (64..=95). Returns
/// `None` if all 32 vectors are already allocated — the pool is
/// intentionally small for now; scale up when NVMe arrives.
pub fn alloc_vector() -> Option<u8> {
    loop {
        let current = FREE_VECTORS.load(Ordering::Acquire);
        if current == 0 { return None; }
        // Pick the lowest-order free bit.
        let bit = current.trailing_zeros() as u8;
        let mask = 1u32 << bit;
        let new = current & !mask;
        if FREE_VECTORS
            .compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(MSIX_VECTOR_MIN + bit);
        }
        // CAS failed — someone else raced us, try again with fresh
        // state. Loop terminates because each CAS either succeeds or
        // observes a reduction in the free count.
    }
}

/// Return a vector to the free pool. Callers must ensure no further
/// interrupts can arrive on this vector before freeing (typically
/// by masking the device first).
pub fn free_vector(vector: u8) {
    if !(MSIX_VECTOR_MIN..=MSIX_VECTOR_MAX).contains(&vector) { return; }
    let bit = vector - MSIX_VECTOR_MIN;
    FREE_VECTORS.fetch_or(1u32 << bit, Ordering::Release);
}

/// Program one entry in the MSI-X table.
///
/// After this call the device can post the given IDT vector to the
/// target LAPIC by DMA-writing to the MSI address. Caller is
/// responsible for pointing the device's per-queue vector register
/// at `entry_idx` — MSI-X itself only sets up the *mapping* from
/// entry → (address, vector); the device decides which entry to
/// use for which interrupt source.
///
/// # Safety
///
/// `table_virt` must point to a mapped MSI-X table of at least
/// `(entry_idx + 1) * 16` bytes. Caller must have exclusive access.
pub unsafe fn configure_entry(
    table_virt: u64,
    entry_idx: u16,
    vector: u8,
    dest_apic_id: u8,
) {
    let entry_ptr = (table_virt + (entry_idx as u64) * 16) as *mut u32;
    // Message Address Low: LAPIC MMIO base with destination bits.
    //   bits 20..32 = 0xFEE
    //   bits 12..20 = destination APIC ID (Physical mode, no RH/DM)
    //   bits  0..12 = 0
    let addr_lo = (LAPIC_MSI_BASE as u32) | ((dest_apic_id as u32) << 12);
    // Message Address High = 0 on x86-64 (LAPIC is always below 4 GiB).
    let addr_hi: u32 = 0;
    // Message Data: bits 0..8 = IDT vector. Delivery mode = 0 (fixed),
    // trigger = 0 (edge), level = 0. Those bits are 0 in our
    // encoding, which is what we want.
    let data = vector as u32;
    // Vector Control = 0 (unmasked).
    let vctrl: u32 = 0;
    // Write order: mask first (it's already 0 if freshly claimed, but
    // be defensive), then addr/data, then unmask. The spec requires
    // the table entry be masked while being programmed; entries
    // start life as 0x0000_0001 (masked) per PCIe reset state.
    core::ptr::write_volatile(entry_ptr.add(3), MSIX_VECTOR_CONTROL_MASK);
    core::ptr::write_volatile(entry_ptr.add(0), addr_lo);
    core::ptr::write_volatile(entry_ptr.add(1), addr_hi);
    core::ptr::write_volatile(entry_ptr.add(2), data);
    core::ptr::write_volatile(entry_ptr.add(3), vctrl);
}

/// Enable MSI-X on the device by setting the Enable bit in the
/// Message Control register. Also clears the Mask-All bit in case
/// firmware left it set. Must be called *after* `configure_entry`
/// so the device doesn't post interrupts against unprogrammed
/// entries.
pub fn enable_msix(dev: &PciDevice, cap_offset: u8) {
    let ctrl_offset = cap_offset + 2;
    let mut msg_ctrl = pci::pci_read16(dev.bus, dev.device, dev.function, ctrl_offset);
    msg_ctrl &= !MSIX_CTRL_MASK_ALL; // unmask all
    msg_ctrl |= MSIX_CTRL_ENABLE;
    pci::pci_write16(dev.bus, dev.device, dev.function, ctrl_offset, msg_ctrl);
}
