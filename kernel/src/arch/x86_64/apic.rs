//! Advanced Programmable Interrupt Controller (APIC)
//!
//! Initializes Local APIC and sets up timer for scheduler preemption.

use x86_64::instructions::port::Port;
use x86_64::structures::paging::PageTableFlags;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Local APIC base physical address
const LAPIC_BASE_PHYS: usize = 0xFEE00000;

/// Virtual address where APIC is mapped (fixed kernel address above HHDM)
const LAPIC_BASE_VIRT: usize = 0xFFFF_FFFF_FEE0_0000;

/// Cached APIC virtual address for fast access
static APIC_VIRT_ADDR: AtomicUsize = AtomicUsize::new(0);

/// APIC register offsets
const APIC_ID: usize = 0x20;
const APIC_VERSION: usize = 0x30;
const APIC_TPR: usize = 0x80;
const APIC_EOI: usize = 0xB0;
const APIC_SPURIOUS: usize = 0xF0;
const APIC_LVT_TIMER: usize = 0x320;
const APIC_TIMER_INIT_COUNT: usize = 0x380;
const APIC_TIMER_CURRENT_COUNT: usize = 0x390;
const APIC_TIMER_DIV: usize = 0x3E0;

/// Timer IRQ vector
const TIMER_VECTOR: u8 = 32;

/// Initialize Local APIC
///
/// Sets up Local APIC and configures timer for 10ms periodic interrupts.
pub fn init() {
    unsafe {
        // 1. Map APIC MMIO registers to a fixed virtual address
        // The APIC is at physical 0xFEE00000 which is outside HHDM range
        crate::drivers::serial::write_str("[APIC] Mapping APIC registers at phys ");
        crate::drivers::serial::write_hex(LAPIC_BASE_PHYS as u64);
        crate::drivers::serial::write_str(" to virt ");
        crate::drivers::serial::write_hex(LAPIC_BASE_VIRT as u64);
        crate::drivers::serial::write_newline();

        // Map APIC page with write-through and cache-disable for MMIO
        let flags = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::NO_EXECUTE
            | PageTableFlags::WRITE_THROUGH
            | PageTableFlags::NO_CACHE;

        if let Err(_e) = crate::memory::paging::map_page(LAPIC_BASE_VIRT, LAPIC_BASE_PHYS, flags) {
            crate::drivers::serial::write_str("[APIC] ERROR: Failed to map APIC registers!\n");
            return;
        }

        // Store mapped address for later use
        APIC_VIRT_ADDR.store(LAPIC_BASE_VIRT, Ordering::Relaxed);
        let apic_virt = LAPIC_BASE_VIRT;

        crate::drivers::serial::write_str("[APIC] APIC registers mapped successfully\n");

        // 2. Enable APIC by setting spurious interrupt vector
        let spurious = read_apic_reg(apic_virt, APIC_SPURIOUS);
        write_apic_reg(apic_virt, APIC_SPURIOUS, spurious | 0x100 | 0xFF);

        // 3. Disable legacy PIC (8259A) by masking all interrupts
        disable_pic();

        // 4. Set up APIC timer
        setup_timer(apic_virt);

        crate::drivers::serial::write_str("[APIC] Local APIC initialized\n");
    }
}

/// Disable legacy PIC (8259A)
///
/// Masks all interrupts on both PIC chips to prevent spurious interrupts.
unsafe fn disable_pic() {
    let mut pic1_data = Port::<u8>::new(0x21);
    let mut pic2_data = Port::<u8>::new(0xA1);

    // Mask all interrupts on both PICs
    pic1_data.write(0xFF);
    pic2_data.write(0xFF);
}

/// Setup APIC timer
///
/// Configures APIC timer for periodic 10ms interrupts.
unsafe fn setup_timer(apic_virt: usize) {
    // 1. Set timer divide configuration to 16
    write_apic_reg(apic_virt, APIC_TIMER_DIV, 0x3);

    // 2. Set LVT timer entry (periodic mode, vector 32)
    // NOTE: Timer disabled for now (masked) until interrupt handling is stable
    // To enable: use 0x20000 | TIMER_VECTOR instead of 0x10000 | 0x20000 | TIMER_VECTOR
    let timer_mode = 0x10000 | 0x20000 | (TIMER_VECTOR as u32); // Masked (disabled) + Periodic mode
    write_apic_reg(apic_virt, APIC_LVT_TIMER, timer_mode);

    // 3. Set initial count for 10ms interval (100 Hz)
    // Assuming ~1GHz bus frequency: 10ms = 10,000,000 cycles
    // With divide-by-16: 10,000,000 / 16 = 625,000
    // This is approximate - calibration needed for accuracy
    // Using a larger interval (10ms) to reduce interrupt overhead
    let initial_count = 625000;
    write_apic_reg(apic_virt, APIC_TIMER_INIT_COUNT, initial_count);

    crate::drivers::serial::write_str("[APIC] Timer configured but MASKED (vector ");
    crate::drivers::serial::write_dec(TIMER_VECTOR as u32);
    crate::drivers::serial::write_str(")\n");
    crate::drivers::serial::write_str("[APIC] Timer will be enabled after interrupt handling is stable\n");
}

/// Read APIC register
#[inline]
unsafe fn read_apic_reg(base: usize, offset: usize) -> u32 {
    read_volatile((base + offset) as *const u32)
}

/// Write APIC register
#[inline]
unsafe fn write_apic_reg(base: usize, offset: usize, value: u32) {
    write_volatile((base + offset) as *mut u32, value);
}

/// Send End-Of-Interrupt signal
///
/// Must be called at the end of interrupt handlers to acknowledge interrupt.
#[inline]
pub fn send_eoi() {
    let apic_virt = APIC_VIRT_ADDR.load(Ordering::Relaxed);
    if apic_virt == 0 {
        return; // APIC not initialized yet
    }
    unsafe {
        write_apic_reg(apic_virt, APIC_EOI, 0);
    }
}

/// Get Local APIC ID
pub fn get_apic_id() -> u8 {
    let apic_virt = APIC_VIRT_ADDR.load(Ordering::Relaxed);
    if apic_virt == 0 {
        return 0; // APIC not initialized yet
    }
    unsafe {
        let id_reg = read_apic_reg(apic_virt, APIC_ID);
        ((id_reg >> 24) & 0xFF) as u8
    }
}
