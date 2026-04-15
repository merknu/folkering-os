//! PCI Bus Enumeration (Mechanism 1)
//!
//! Scans PCI bus 0 using I/O ports 0xCF8 (CONFIG_ADDRESS) and 0xCFC (CONFIG_DATA).
//! Discovers devices, decodes BARs, and traverses PCI capabilities lists.

use x86_64::instructions::port::Port;
use spin::Mutex;

/// PCI config address port
const CONFIG_ADDRESS: u16 = 0xCF8;
/// PCI config data port
const CONFIG_DATA: u16 = 0xCFC;

/// Maximum devices per bus
const MAX_DEVICES: usize = 32;
/// Maximum functions per device
const MAX_FUNCTIONS: usize = 8;

/// VirtIO vendor ID
pub const VIRTIO_VENDOR_ID: u16 = 0x1AF4;
/// VirtIO block device (transitional)
pub const VIRTIO_BLK_DEVICE_TRANSITIONAL: u16 = 0x1001;
/// VirtIO block device (modern)
pub const VIRTIO_BLK_DEVICE_MODERN: u16 = 0x1042;
/// VirtIO network device (transitional)
pub const VIRTIO_NET_DEVICE_TRANSITIONAL: u16 = 0x1000;
/// VirtIO network device (modern)
pub const VIRTIO_NET_DEVICE_MODERN: u16 = 0x1041;

/// PCI device information
#[derive(Clone, Debug)]
pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub header_type: u8,
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
    pub bars: [u32; 6],
    pub subsystem_vendor_id: u16,
    pub subsystem_device_id: u16,
    pub capabilities_ptr: u8,
}

/// BAR type
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BarType {
    /// I/O port space
    Io { base: u16 },
    /// Memory-mapped I/O (32-bit)
    Mmio32 { base: u32, prefetchable: bool },
    /// Memory-mapped I/O (64-bit)
    Mmio64 { base: u64, prefetchable: bool },
    /// BAR not present
    None,
}

/// Discovered PCI devices
pub static PCI_DEVICES: Mutex<PciDeviceList> = Mutex::new(PciDeviceList::new());

pub struct PciDeviceList {
    pub devices: [Option<PciDevice>; 64],
    pub count: usize,
}

impl PciDeviceList {
    const fn new() -> Self {
        const NONE: Option<PciDevice> = None;
        Self {
            devices: [NONE; 64],
            count: 0,
        }
    }

    fn push(&mut self, dev: PciDevice) {
        if self.count < 64 {
            self.devices[self.count] = Some(dev);
            self.count += 1;
        }
    }
}

/// Build PCI config address for mechanism 1
fn pci_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC)
}

/// Read 32-bit value from PCI config space
pub fn pci_read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    unsafe {
        let mut addr_port = Port::<u32>::new(CONFIG_ADDRESS);
        let mut data_port = Port::<u32>::new(CONFIG_DATA);
        addr_port.write(pci_address(bus, device, function, offset));
        data_port.read()
    }
}

/// Write 32-bit value to PCI config space
pub fn pci_write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    unsafe {
        let mut addr_port = Port::<u32>::new(CONFIG_ADDRESS);
        let mut data_port = Port::<u32>::new(CONFIG_DATA);
        addr_port.write(pci_address(bus, device, function, offset));
        data_port.write(value);
    }
}

/// Read 16-bit value from PCI config space
pub fn pci_read16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let val32 = pci_read32(bus, device, function, offset & 0xFC);
    ((val32 >> ((offset & 2) * 8)) & 0xFFFF) as u16
}

/// Write 16-bit value to PCI config space. Implemented as a
/// read-modify-write on the enclosing dword so we preserve the
/// adjacent 16 bits. Needed by MSI-X enable (bit 15 of Message
/// Control at cap+2).
pub fn pci_write16(bus: u8, device: u8, function: u8, offset: u8, value: u16) {
    let aligned = offset & 0xFC;
    let shift = (offset & 2) * 8;
    let mut val32 = pci_read32(bus, device, function, aligned);
    val32 &= !(0xFFFFu32 << shift);
    val32 |= (value as u32) << shift;
    pci_write32(bus, device, function, aligned, val32);
}

/// Read 8-bit value from PCI config space
pub fn pci_read8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let val32 = pci_read32(bus, device, function, offset & 0xFC);
    ((val32 >> ((offset & 3) * 8)) & 0xFF) as u8
}

/// Read a PCI device's information
fn read_device(bus: u8, device: u8, function: u8) -> Option<PciDevice> {
    let vendor_device = pci_read32(bus, device, function, 0x00);
    let vendor_id = (vendor_device & 0xFFFF) as u16;
    let device_id = ((vendor_device >> 16) & 0xFFFF) as u16;

    if vendor_id == 0xFFFF {
        return None; // No device
    }

    let class_rev = pci_read32(bus, device, function, 0x08);
    let header_info = pci_read32(bus, device, function, 0x0C);
    let subsystem = pci_read32(bus, device, function, 0x2C);
    let cap_ptr = pci_read8(bus, device, function, 0x34);
    let int_info = pci_read32(bus, device, function, 0x3C);

    let mut bars = [0u32; 6];
    for i in 0..6 {
        bars[i] = pci_read32(bus, device, function, 0x10 + (i as u8) * 4);
    }

    Some(PciDevice {
        bus,
        device,
        function,
        vendor_id,
        device_id,
        revision: (class_rev & 0xFF) as u8,
        prog_if: ((class_rev >> 8) & 0xFF) as u8,
        subclass: ((class_rev >> 16) & 0xFF) as u8,
        class_code: ((class_rev >> 24) & 0xFF) as u8,
        header_type: ((header_info >> 16) & 0xFF) as u8,
        interrupt_line: (int_info & 0xFF) as u8,
        interrupt_pin: ((int_info >> 8) & 0xFF) as u8,
        bars,
        subsystem_vendor_id: (subsystem & 0xFFFF) as u16,
        subsystem_device_id: ((subsystem >> 16) & 0xFFFF) as u16,
        capabilities_ptr: cap_ptr,
    })
}

/// Decode a BAR value
pub fn decode_bar(dev: &PciDevice, bar_index: usize) -> BarType {
    if bar_index >= 6 {
        return BarType::None;
    }
    let bar = dev.bars[bar_index];
    if bar == 0 {
        return BarType::None;
    }

    if bar & 1 != 0 {
        // I/O space
        BarType::Io {
            base: (bar & 0xFFFC) as u16,
        }
    } else {
        let prefetchable = (bar & 0x08) != 0;
        let bar_type = (bar >> 1) & 0x03;
        match bar_type {
            0x00 => BarType::Mmio32 {
                base: bar & 0xFFFF_FFF0,
                prefetchable,
            },
            0x02 => {
                // 64-bit MMIO - uses next BAR too
                if bar_index >= 5 {
                    return BarType::None;
                }
                let high = dev.bars[bar_index + 1] as u64;
                let low = (bar & 0xFFFF_FFF0) as u64;
                BarType::Mmio64 {
                    base: (high << 32) | low,
                    prefetchable,
                }
            }
            _ => BarType::None,
        }
    }
}

/// Get BAR size by writing all 1s and reading back
pub fn bar_size(bus: u8, device: u8, function: u8, bar_index: u8) -> u32 {
    let offset = 0x10 + bar_index * 4;
    let original = pci_read32(bus, device, function, offset);

    // Write all 1s
    pci_write32(bus, device, function, offset, 0xFFFF_FFFF);
    let size_mask = pci_read32(bus, device, function, offset);

    // Restore original
    pci_write32(bus, device, function, offset, original);

    if size_mask == 0 || size_mask == 0xFFFF_FFFF {
        return 0;
    }

    if original & 1 != 0 {
        // I/O BAR
        let mask = size_mask & 0xFFFC;
        (!mask).wrapping_add(1) & 0xFFFF
    } else {
        // MMIO BAR
        let mask = size_mask & 0xFFFF_FFF0;
        (!mask).wrapping_add(1)
    }
}

/// Enable PCI bus mastering for a device (required for DMA)
pub fn enable_bus_master(bus: u8, device: u8, function: u8) {
    let cmd = pci_read16(bus, device, function, 0x04);
    // Set bit 2 (Bus Master Enable)
    let new_cmd = cmd | 0x04;
    let full = pci_read32(bus, device, function, 0x04);
    pci_write32(bus, device, function, 0x04, (full & 0xFFFF_0000) | (new_cmd as u32));
}

/// Scan PCI bus 0 and discover all devices
pub fn init() {
    crate::serial_strln!("[PCI] Scanning PCI bus 0...");

    let mut list = PCI_DEVICES.lock();

    for dev_num in 0..MAX_DEVICES as u8 {
        // Check function 0 first
        if let Some(dev) = read_device(0, dev_num, 0) {
            crate::serial_str!("[PCI] ");
            crate::drivers::serial::write_dec(dev_num as u32);
            crate::serial_str!(".0: ");
            crate::drivers::serial::write_hex(dev.vendor_id as u64);
            crate::serial_str!(":");
            crate::drivers::serial::write_hex(dev.device_id as u64);
            crate::serial_str!(" class=");
            crate::drivers::serial::write_hex(dev.class_code as u64);
            crate::serial_str!(".");
            crate::drivers::serial::write_hex(dev.subclass as u64);

            if dev.vendor_id == VIRTIO_VENDOR_ID {
                crate::serial_str!(" [VirtIO]");
            }
            crate::drivers::serial::write_newline();

            let is_multifunction = dev.header_type & 0x80 != 0;
            list.push(dev);

            // Check additional functions if multi-function device
            if is_multifunction {
                for func in 1..MAX_FUNCTIONS as u8 {
                    if let Some(fdev) = read_device(0, dev_num, func) {
                        crate::serial_str!("[PCI] ");
                        crate::drivers::serial::write_dec(dev_num as u32);
                        crate::serial_str!(".");
                        crate::drivers::serial::write_dec(func as u32);
                        crate::serial_str!(": ");
                        crate::drivers::serial::write_hex(fdev.vendor_id as u64);
                        crate::serial_str!(":");
                        crate::drivers::serial::write_hex(fdev.device_id as u64);
                        crate::drivers::serial::write_newline();
                        list.push(fdev);
                    }
                }
            }
        }
    }

    crate::serial_str!("[PCI] Found ");
    crate::drivers::serial::write_dec(list.count as u32);
    crate::serial_strln!(" devices");
}

/// Find a PCI device by vendor and device ID
pub fn find_device(vendor_id: u16, device_id: u16) -> Option<PciDevice> {
    let list = PCI_DEVICES.lock();
    for i in 0..list.count {
        if let Some(ref dev) = list.devices[i] {
            if dev.vendor_id == vendor_id && dev.device_id == device_id {
                return Some(dev.clone());
            }
        }
    }
    None
}

/// Find a PCI device by vendor ID only (first match)
pub fn find_by_vendor(vendor_id: u16) -> Option<PciDevice> {
    let list = PCI_DEVICES.lock();
    for i in 0..list.count {
        if let Some(ref dev) = list.devices[i] {
            if dev.vendor_id == vendor_id {
                return Some(dev.clone());
            }
        }
    }
    None
}

/// Find VirtIO block device (checks both transitional and modern IDs)
pub fn find_virtio_block() -> Option<PciDevice> {
    // Try modern first
    if let Some(dev) = find_device(VIRTIO_VENDOR_ID, VIRTIO_BLK_DEVICE_MODERN) {
        return Some(dev);
    }
    // Fall back to transitional
    find_device(VIRTIO_VENDOR_ID, VIRTIO_BLK_DEVICE_TRANSITIONAL)
}

/// Find VirtIO network device (checks both transitional and modern IDs)
pub fn find_virtio_net() -> Option<PciDevice> {
    if let Some(dev) = find_device(VIRTIO_VENDOR_ID, VIRTIO_NET_DEVICE_MODERN) {
        return Some(dev);
    }
    find_device(VIRTIO_VENDOR_ID, VIRTIO_NET_DEVICE_TRANSITIONAL)
}

/// VirtIO GPU device IDs
const VIRTIO_GPU_DEVICE_TRANSITIONAL: u16 = 0x1050;
const VIRTIO_GPU_DEVICE_MODERN: u16 = 0x1040 + 16; // 0x1050

/// Find VirtIO GPU device
pub fn find_virtio_gpu() -> Option<PciDevice> {
    // VirtIO GPU transitional ID
    if let Some(dev) = find_device(VIRTIO_VENDOR_ID, VIRTIO_GPU_DEVICE_TRANSITIONAL) {
        return Some(dev);
    }
    // Also check display class (0x03) with VirtIO vendor
    let list = PCI_DEVICES.lock();
    for i in 0..list.count {
        if let Some(ref dev) = list.devices[i] {
            if dev.vendor_id == VIRTIO_VENDOR_ID && dev.class_code == 0x03 {
                return Some(dev.clone());
            }
        }
    }
    None
}
