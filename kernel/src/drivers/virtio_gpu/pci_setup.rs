//! PCI capability parsing for VirtIO Modern transport.
//!
//! Walks the device's PCI capability list, locates the Common/Notify/ISR
//! and (optionally) Device-config caps, maps the underlying BAR pages
//! uncacheable, and returns a fully-resolved `MmioTransport`.

use crate::drivers::pci::{self, PciDevice, BarType};
use super::io::MmioTransport;

/// Parse VirtIO Modern PCI Capabilities to find MMIO register regions.
pub(super) fn parse_virtio_capabilities(dev: &PciDevice) -> Result<MmioTransport, &'static str> {
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

/// Resolve a BAR index to its physical base address.
fn resolve_bar_phys(dev: &PciDevice, bar_idx: u8) -> Result<usize, &'static str> {
    match pci::decode_bar(dev, bar_idx as usize) {
        BarType::Mmio32 { base, .. } => Ok(base as usize),
        BarType::Mmio64 { base, .. } => Ok(base as usize),
        BarType::Io { .. } => Err("unexpected I/O BAR for MMIO transport"),
        BarType::None => Err("BAR not present"),
    }
}
