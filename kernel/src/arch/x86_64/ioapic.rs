//! I/O APIC (IOAPIC) Driver
//!
//! The IOAPIC routes device interrupts (IRQs) to Local APIC(s).
//! This replaces the legacy PIC for interrupt delivery when APIC is enabled.
//!
//! # Default IOAPIC Configuration
//! - Base address: 0xFEC00000 (standard location)
//! - IRQ 0-23 can be routed to any Local APIC
//!
//! # Relevant IRQs
//! - IRQ1: PS/2 Keyboard → Vector 33
//! - IRQ12: PS/2 Mouse → Vector 44

use x86_64::structures::paging::PageTableFlags;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// Removed unused Port import

/// Default IOAPIC physical base address
const IOAPIC_BASE_PHYS: usize = 0xFEC00000;

/// Virtual address to map IOAPIC (in high kernel space, near APIC mapping)
const IOAPIC_BASE_VIRT: usize = 0xFFFF_FFFF_FEC0_0000;

/// IOAPIC register select (index)
const IOREGSEL: usize = 0x00;

/// IOAPIC register data window
const IOWIN: usize = 0x10;

/// IOAPIC ID register
const IOAPICID: u32 = 0x00;

/// IOAPIC Version register
const IOAPICVER: u32 = 0x01;

/// IOAPIC Redirection Table base (entries 0-23)
const IOREDTBL_BASE: u32 = 0x10;

/// Virtual address for IOAPIC MMIO (set during init)
static IOAPIC_VIRT: AtomicUsize = AtomicUsize::new(0);

/// IOAPIC initialized flag
static IOAPIC_INIT: AtomicBool = AtomicBool::new(false);

/// Read IOAPIC register
unsafe fn read_ioapic(reg: u32) -> u32 {
    let base = IOAPIC_VIRT.load(Ordering::Relaxed);
    let sel = base as *mut u32;
    let win = (base + IOWIN) as *mut u32;

    core::ptr::write_volatile(sel, reg);
    core::ptr::read_volatile(win)
}

/// Write IOAPIC register
unsafe fn write_ioapic(reg: u32, value: u32) {
    let base = IOAPIC_VIRT.load(Ordering::Relaxed);
    let sel = base as *mut u32;
    let win = (base + IOWIN) as *mut u32;

    core::ptr::write_volatile(sel, reg);
    core::ptr::write_volatile(win, value);
}

/// IOAPIC redirection entry (64-bit, split into low and high 32-bit registers)
///
/// Low 32 bits:
/// - Bits 0-7: Vector
/// - Bits 8-10: Delivery Mode (000=Fixed, 001=Lowest Priority, 010=SMI, 100=NMI, 101=INIT, 111=ExtINT)
/// - Bit 11: Destination Mode (0=Physical, 1=Logical)
/// - Bit 12: Delivery Status (read-only)
/// - Bit 13: Pin Polarity (0=Active High, 1=Active Low)
/// - Bit 14: Remote IRR (read-only)
/// - Bit 15: Trigger Mode (0=Edge, 1=Level)
/// - Bit 16: Mask (0=Enabled, 1=Masked)
///
/// High 32 bits:
/// - Bits 24-31: Destination (APIC ID in physical mode)
fn make_redirection_entry(vector: u8, dest_apic: u8, masked: bool, level_triggered: bool) -> u64 {
    let low: u32 = (vector as u32)          // Vector
        | (0 << 8)                          // Delivery Mode: Fixed
        | (0 << 11)                         // Destination Mode: Physical
        | (if level_triggered { 1 << 13 } else { 0 })  // Pin Polarity: Active Low for level
        | (if level_triggered { 1 << 15 } else { 0 })  // Trigger Mode: Level for PCI
        | (if masked { 1 << 16 } else { 0 }); // Mask

    let high: u32 = (dest_apic as u32) << 24;

    ((high as u64) << 32) | (low as u64)
}

/// Set IOAPIC redirection entry for an IRQ (edge-triggered, for ISA devices like keyboard/mouse)
unsafe fn set_irq_route(irq: u8, vector: u8, dest_apic: u8, enabled: bool) {
    let entry = make_redirection_entry(vector, dest_apic, !enabled, false);
    let reg_low = IOREDTBL_BASE + (irq as u32) * 2;
    let reg_high = reg_low + 1;

    write_ioapic(reg_low, entry as u32);
    write_ioapic(reg_high, (entry >> 32) as u32);
}

/// Set IOAPIC redirection entry with level-triggered, active-low (for PCI devices)
unsafe fn set_irq_route_level(irq: u8, vector: u8, dest_apic: u8, enabled: bool) {
    let entry = make_redirection_entry(vector, dest_apic, !enabled, true);
    let reg_low = IOREDTBL_BASE + (irq as u32) * 2;
    let reg_high = reg_low + 1;

    write_ioapic(reg_low, entry as u32);
    write_ioapic(reg_high, (entry >> 32) as u32);
}

/// Initialize IOAPIC
pub fn init() {
    if IOAPIC_INIT.load(Ordering::Relaxed) {
        return;
    }

    unsafe {
        crate::serial_strln!("[IOAPIC] Initializing I/O APIC...");

        // Map IOAPIC MMIO registers to virtual address
        crate::serial_str!("[IOAPIC] Mapping IOAPIC at phys ");
        crate::drivers::serial::write_hex(IOAPIC_BASE_PHYS as u64);
        crate::serial_str!(" to virt ");
        crate::drivers::serial::write_hex(IOAPIC_BASE_VIRT as u64);
        crate::drivers::serial::write_newline();

        // Map with write-through and cache-disable for MMIO
        let flags = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::NO_EXECUTE
            | PageTableFlags::WRITE_THROUGH
            | PageTableFlags::NO_CACHE;

        if let Err(_e) = crate::memory::paging::map_page(IOAPIC_BASE_VIRT, IOAPIC_BASE_PHYS, flags) {
            crate::serial_strln!("[IOAPIC] ERROR: Failed to map IOAPIC registers!");
            return;
        }

        // Store mapped address
        IOAPIC_VIRT.store(IOAPIC_BASE_VIRT, Ordering::Relaxed);
        crate::serial_strln!("[IOAPIC] IOAPIC registers mapped successfully");

        // Set IMCR (Interrupt Mode Control Register) to APIC mode
        // This disconnects the 8259A PIC from the CPU and routes external
        // interrupts through the I/O APIC instead.
        // IMCR is accessed via ports 0x22 (index) and 0x23 (data)
        use x86_64::instructions::port::Port;
        let mut imcr_addr = Port::<u8>::new(0x22);
        let mut imcr_data = Port::<u8>::new(0x23);
        imcr_addr.write(0x70);  // Select IMCR register
        imcr_data.write(0x01);  // Set to APIC mode (disconnect PIC)
        crate::serial_strln!("[IOAPIC] IMCR set to APIC mode");

        // Read IOAPIC ID and version
        let id = read_ioapic(IOAPICID);
        let ver = read_ioapic(IOAPICVER);
        let max_entries = ((ver >> 16) & 0xFF) + 1;

        crate::serial_str!("[IOAPIC] ID=");
        crate::drivers::serial::write_hex(((id >> 24) & 0xF) as u64);
        crate::serial_str!(", Version=");
        crate::drivers::serial::write_hex((ver & 0xFF) as u64);
        crate::serial_str!(", Max entries=");
        crate::drivers::serial::write_dec(max_entries);
        crate::drivers::serial::write_newline();

        // Mask all IRQs initially
        for irq in 0..max_entries {
            set_irq_route(irq as u8, 0, 0, false);
        }

        crate::serial_strln!("[IOAPIC] All IRQs masked");
    }

    IOAPIC_INIT.store(true, Ordering::Relaxed);
    crate::serial_strln!("[IOAPIC] I/O APIC initialized");
}

/// Enable an IRQ on the IOAPIC
///
/// Routes the IRQ to the BSP (APIC ID 0) with the specified vector.
pub fn enable_irq(irq: u8, vector: u8) {
    if !IOAPIC_INIT.load(Ordering::Relaxed) {
        crate::serial_strln!("[IOAPIC] ERROR: IOAPIC not initialized!");
        return;
    }

    unsafe {
        // Route to BSP (APIC ID 0)
        set_irq_route(irq, vector, 0, true);

        // Read back and verify the entry
        let reg_low = IOREDTBL_BASE + (irq as u32) * 2;
        let reg_high = reg_low + 1;
        let entry_low = read_ioapic(reg_low);
        let entry_high = read_ioapic(reg_high);

        crate::serial_str!("[IOAPIC] Enabled IRQ");
        crate::drivers::serial::write_dec(irq as u32);
        crate::serial_str!(" -> Vector ");
        crate::drivers::serial::write_dec(vector as u32);
        crate::serial_str!(" (low=0x");
        crate::drivers::serial::write_hex(entry_low as u64);
        crate::serial_str!(", high=0x");
        crate::drivers::serial::write_hex(entry_high as u64);
        crate::serial_str!(")\n");
    }
}

/// Enable a PCI IRQ on the IOAPIC (level-triggered, active-low)
///
/// PCI interrupts are level-triggered and active-low per the PCI specification.
/// This is different from ISA interrupts (edge-triggered, active-high).
pub fn enable_irq_level(irq: u8, vector: u8) {
    if !IOAPIC_INIT.load(Ordering::Relaxed) {
        crate::serial_strln!("[IOAPIC] ERROR: IOAPIC not initialized!");
        return;
    }

    unsafe {
        set_irq_route_level(irq, vector, 0, true);

        let reg_low = IOREDTBL_BASE + (irq as u32) * 2;
        let reg_high = reg_low + 1;
        let entry_low = read_ioapic(reg_low);
        let entry_high = read_ioapic(reg_high);

        crate::serial_str!("[IOAPIC] Enabled IRQ");
        crate::drivers::serial::write_dec(irq as u32);
        crate::serial_str!(" -> Vector ");
        crate::drivers::serial::write_dec(vector as u32);
        crate::serial_str!(" [level,active-low] (low=0x");
        crate::drivers::serial::write_hex(entry_low as u64);
        crate::serial_str!(", high=0x");
        crate::drivers::serial::write_hex(entry_high as u64);
        crate::serial_str!(")\n");
    }
}

/// Disable an IRQ on the IOAPIC
pub fn disable_irq(irq: u8) {
    if !IOAPIC_INIT.load(Ordering::Relaxed) {
        return;
    }

    unsafe {
        set_irq_route(irq, 0, 0, false);
    }
}

/// Debug: Print status of keyboard and mouse IOAPIC entries
pub fn debug_print_status() {
    if !IOAPIC_INIT.load(Ordering::Relaxed) {
        return;
    }

    unsafe {
        // Read IRQ1 (keyboard) redirection entry
        let irq1_low = read_ioapic(IOREDTBL_BASE + 1 * 2);
        let irq1_high = read_ioapic(IOREDTBL_BASE + 1 * 2 + 1);

        // Read IRQ12 (mouse) redirection entry
        let irq12_low = read_ioapic(IOREDTBL_BASE + 12 * 2);
        let irq12_high = read_ioapic(IOREDTBL_BASE + 12 * 2 + 1);

        crate::serial_str!("[IOAPIC-DBG] IRQ1: low=");
        crate::drivers::serial::write_hex(irq1_low as u64);
        crate::serial_str!(" high=");
        crate::drivers::serial::write_hex(irq1_high as u64);
        crate::serial_str!(" IRQ12: low=");
        crate::drivers::serial::write_hex(irq12_low as u64);
        crate::serial_str!(" high=");
        crate::drivers::serial::write_hex(irq12_high as u64);
        crate::serial_str!("\n");
    }
}
