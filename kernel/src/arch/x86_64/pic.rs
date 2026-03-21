//! 8259A Programmable Interrupt Controller (PIC) Driver
//!
//! This module provides centralized PIC configuration for legacy interrupt handling.
//! The PIC is used for PS/2 keyboard (IRQ1) and mouse (IRQ12) in virtual wire mode
//! alongside the Local APIC.
//!
//! # IRQ to Vector Mapping
//!
//! - PIC1 (Master): IRQ 0-7  → Vectors 32-39
//! - PIC2 (Slave):  IRQ 8-15 → Vectors 40-47
//!
//! ## Relevant IRQs
//!
//! - IRQ1: PS/2 Keyboard → Vector 33
//! - IRQ2: Cascade (connects PIC2)
//! - IRQ12: PS/2 Mouse → Vector 44

use x86_64::instructions::port::Port;
use core::sync::atomic::{AtomicU8, AtomicBool, Ordering};

/// PIC1 (Master) ports
const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;

/// PIC2 (Slave) ports
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// ICW1: Initialize + ICW4 needed
const ICW1_INIT: u8 = 0x11;

/// ICW4: 8086 mode
const ICW4_8086: u8 = 0x01;

/// Vector offset for PIC1 (IRQ0 → Vector 32)
const PIC1_OFFSET: u8 = 32;

/// Vector offset for PIC2 (IRQ8 → Vector 40)
const PIC2_OFFSET: u8 = 40;

/// End of Interrupt command
const EOI: u8 = 0x20;

/// Current PIC1 mask (tracks which IRQs are masked)
static PIC1_MASK: AtomicU8 = AtomicU8::new(0xFF);

/// Current PIC2 mask
static PIC2_MASK: AtomicU8 = AtomicU8::new(0xFF);

/// PIC initialized flag
static PIC_INIT: AtomicBool = AtomicBool::new(false);

/// Initialize both PICs with standard configuration
///
/// This sets up:
/// - PIC1: IRQ0-7 → Vectors 32-39
/// - PIC2: IRQ8-15 → Vectors 40-47
/// - All IRQs masked initially (enable individually with `enable_irq`)
pub fn init() {
    if PIC_INIT.load(Ordering::Relaxed) {
        return; // Already initialized
    }

    unsafe {
        let mut pic1_cmd = Port::<u8>::new(PIC1_CMD);
        let mut pic1_data = Port::<u8>::new(PIC1_DATA);
        let mut pic2_cmd = Port::<u8>::new(PIC2_CMD);
        let mut pic2_data = Port::<u8>::new(PIC2_DATA);

        // Save current masks
        let mask1 = pic1_data.read();
        let mask2 = pic2_data.read();

        // ICW1: Initialize
        pic1_cmd.write(ICW1_INIT);
        io_wait();
        pic2_cmd.write(ICW1_INIT);
        io_wait();

        // ICW2: Vector offsets
        pic1_data.write(PIC1_OFFSET);
        io_wait();
        pic2_data.write(PIC2_OFFSET);
        io_wait();

        // ICW3: Cascade configuration
        pic1_data.write(4); // IRQ2 has slave
        io_wait();
        pic2_data.write(2); // Slave ID 2
        io_wait();

        // ICW4: 8086 mode
        pic1_data.write(ICW4_8086);
        io_wait();
        pic2_data.write(ICW4_8086);
        io_wait();

        // Initially mask all interrupts except cascade (IRQ2)
        // Bit 0 = IRQ0, Bit 1 = IRQ1, etc.
        // ~0x04 = 0xFB = all masked except IRQ2 (cascade)
        let initial_mask1 = 0xFB; // Only IRQ2 (cascade) enabled
        let initial_mask2 = 0xFF; // All masked

        pic1_data.write(initial_mask1);
        io_wait();
        pic2_data.write(initial_mask2);
        io_wait();

        PIC1_MASK.store(initial_mask1, Ordering::Relaxed);
        PIC2_MASK.store(initial_mask2, Ordering::Relaxed);

        crate::serial_strln!("[PIC] 8259A PIC initialized");
        crate::serial_strln!("[PIC] PIC1: IRQ0-7 → Vectors 32-39");
        crate::serial_strln!("[PIC] PIC2: IRQ8-15 → Vectors 40-47");
    }

    PIC_INIT.store(true, Ordering::Relaxed);
}

/// Enable a specific IRQ line
///
/// # Arguments
/// * `irq` - IRQ number (0-15)
///
/// # IRQ Numbers
/// - 1: PS/2 Keyboard
/// - 12: PS/2 Mouse
pub fn enable_irq(irq: u8) {
    if irq >= 16 {
        return;
    }

    unsafe {
        if irq < 8 {
            // PIC1
            let mut pic1_data = Port::<u8>::new(PIC1_DATA);
            let mut mask = PIC1_MASK.load(Ordering::Relaxed);
            mask &= !(1 << irq);
            pic1_data.write(mask);
            PIC1_MASK.store(mask, Ordering::Relaxed);

            crate::serial_str!("[PIC] Enabled IRQ");
            crate::drivers::serial::write_dec(irq as u32);
            crate::serial_str!(" (vector ");
            crate::drivers::serial::write_dec((PIC1_OFFSET + irq) as u32);
            crate::serial_strln!(")");
        } else {
            // PIC2
            let slave_irq = irq - 8;
            let mut pic2_data = Port::<u8>::new(PIC2_DATA);
            let mut mask = PIC2_MASK.load(Ordering::Relaxed);
            mask &= !(1 << slave_irq);
            pic2_data.write(mask);
            PIC2_MASK.store(mask, Ordering::Relaxed);

            crate::serial_str!("[PIC] Enabled IRQ");
            crate::drivers::serial::write_dec(irq as u32);
            crate::serial_str!(" (vector ");
            crate::drivers::serial::write_dec((PIC2_OFFSET + slave_irq) as u32);
            crate::serial_strln!(")");
        }
    }
}

/// Disable a specific IRQ line
pub fn disable_irq(irq: u8) {
    if irq >= 16 {
        return;
    }

    unsafe {
        if irq < 8 {
            let mut pic1_data = Port::<u8>::new(PIC1_DATA);
            let mut mask = PIC1_MASK.load(Ordering::Relaxed);
            mask |= 1 << irq;
            pic1_data.write(mask);
            PIC1_MASK.store(mask, Ordering::Relaxed);
        } else {
            let slave_irq = irq - 8;
            let mut pic2_data = Port::<u8>::new(PIC2_DATA);
            let mut mask = PIC2_MASK.load(Ordering::Relaxed);
            mask |= 1 << slave_irq;
            pic2_data.write(mask);
            PIC2_MASK.store(mask, Ordering::Relaxed);
        }
    }
}

/// Send End-of-Interrupt for an IRQ
///
/// Must be called at the end of interrupt handlers.
/// For IRQs 8-15 (PIC2), sends EOI to both PICs.
pub fn send_eoi(irq: u8) {
    unsafe {
        if irq >= 8 {
            // PIC2 interrupt - send EOI to slave first
            let mut pic2_cmd = Port::<u8>::new(PIC2_CMD);
            pic2_cmd.write(EOI);
        }
        // Always send EOI to master
        let mut pic1_cmd = Port::<u8>::new(PIC1_CMD);
        pic1_cmd.write(EOI);
    }
}

/// Check if PIC has been initialized
pub fn is_initialized() -> bool {
    PIC_INIT.load(Ordering::Relaxed)
}

/// Get current mask for debugging
pub fn get_masks() -> (u8, u8) {
    (PIC1_MASK.load(Ordering::Relaxed), PIC2_MASK.load(Ordering::Relaxed))
}

/// Small I/O wait for PIC operations
#[inline]
unsafe fn io_wait() {
    // Write to unused port 0x80 for small delay
    let mut port = Port::<u8>::new(0x80);
    port.write(0);
}
