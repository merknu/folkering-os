//! Advanced Programmable Interrupt Controller (APIC)
//!
//! Initializes Local APIC and sets up timer for scheduler preemption.

use x86_64::instructions::port::Port;
use x86_64::PhysAddr;
use core::ptr::{read_volatile, write_volatile};

/// Local APIC base address (obtained from MSR or hardcoded)
const LAPIC_BASE: usize = 0xFEE00000;

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
/// Sets up Local APIC and configures timer for 1ms periodic interrupts.
pub fn init() {
    unsafe {
        // 1. Map APIC registers (assume HHDM mapping covers it)
        let apic_virt = crate::phys_to_virt(LAPIC_BASE);

        crate::serial_println!("[APIC] Initializing Local APIC at {:#x}", apic_virt);

        // 2. Enable APIC by setting spurious interrupt vector
        let spurious = read_apic_reg(apic_virt, APIC_SPURIOUS);
        write_apic_reg(apic_virt, APIC_SPURIOUS, spurious | 0x100 | 0xFF);

        // 3. Disable legacy PIC (8259A) by masking all interrupts
        disable_pic();

        // 4. Set up APIC timer
        setup_timer(apic_virt);

        crate::serial_println!("[APIC] Local APIC initialized");
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
/// Configures APIC timer for periodic 1ms interrupts.
unsafe fn setup_timer(apic_virt: usize) {
    // 1. Set timer divide configuration to 16
    write_apic_reg(apic_virt, APIC_TIMER_DIV, 0x3);

    // 2. Set LVT timer entry (periodic mode, vector 32)
    let timer_mode = 0x20000 | (TIMER_VECTOR as u32); // Periodic mode
    write_apic_reg(apic_virt, APIC_LVT_TIMER, timer_mode);

    // 3. Set initial count for 1ms interval
    // Assuming 1GHz TSC: 1ms = 1,000,000 cycles
    // With divide-by-16: 1,000,000 / 16 = 62,500
    // This is approximate - calibration needed for accuracy
    let initial_count = 62500;
    write_apic_reg(apic_virt, APIC_TIMER_INIT_COUNT, initial_count);

    crate::serial_println!("[APIC] Timer configured for 1ms ticks (vector {})", TIMER_VECTOR);
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
    unsafe {
        let apic_virt = crate::phys_to_virt(LAPIC_BASE);
        write_apic_reg(apic_virt, APIC_EOI, 0);
    }
}

/// Get Local APIC ID
pub fn get_apic_id() -> u8 {
    unsafe {
        let apic_virt = crate::phys_to_virt(LAPIC_BASE);
        let id_reg = read_apic_reg(apic_virt, APIC_ID);
        ((id_reg >> 24) & 0xFF) as u8
    }
}
