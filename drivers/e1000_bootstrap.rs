//! Folkering OS — Intel E1000 Bootstrap Driver (v1)
//!
//! Handwritten baseline for the E1000 Gigabit Ethernet Controller (8086:100E).
//! This driver:
//! - Resets the device
//! - Sets link up
//! - Clears and enables interrupts
//! - Sits in IRQ wait loop (no packet handling yet)
//!
//! AutoDream can improve this into a full network driver over time.

#![no_std]
#![no_main]
#![allow(unused)]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

extern "C" {
    fn folk_device_vendor_id() -> i32;
    fn folk_device_id() -> i32;
    fn folk_bar_size(bar: i32) -> i32;
    fn folk_mmio_read_u32(bar: i32, offset: i32) -> i32;
    fn folk_mmio_write_u32(bar: i32, offset: i32, value: i32);
    fn folk_wait_irq();
    fn folk_ack_irq();
    fn folk_log(ptr: i32, len: i32);
}

// E1000 Register offsets (BAR0 MMIO)
const CTRL: i32    = 0x0000;  // Device Control
const STATUS: i32  = 0x0008;  // Device Status
const EECD: i32    = 0x0010;  // EEPROM/Flash Control
const ICR: i32     = 0x00C0;  // Interrupt Cause Read (clears on read)
const IMS: i32     = 0x00D0;  // Interrupt Mask Set
const IMC: i32     = 0x00D8;  // Interrupt Mask Clear
const RCTL: i32    = 0x0100;  // Receive Control
const TCTL: i32    = 0x0400;  // Transmit Control

// CTRL register bits
const CTRL_SLU: i32  = 1 << 6;   // Set Link Up
const CTRL_RST: i32  = 1 << 26;  // Device Reset
const CTRL_ASDE: i32 = 1 << 5;   // Auto-Speed Detection Enable

// Interrupt bits
const ICR_TXDW: i32  = 1 << 0;   // TX Descriptor Written Back
const ICR_TXQE: i32  = 1 << 1;   // TX Queue Empty
const ICR_LSC: i32   = 1 << 2;   // Link Status Change
const ICR_RXT0: i32  = 1 << 7;   // RX Timer Interrupt

static mut IRQ_COUNT: u32 = 0;
static mut LINK_UP: bool = false;

fn log(msg: &[u8]) {
    unsafe { folk_log(msg.as_ptr() as i32, msg.len() as i32); }
}

#[no_mangle]
pub extern "C" fn driver_main() {
    unsafe {
        log(b"[E1000] Bootstrap v1 starting");

        // Verify device identity
        let vid = folk_device_vendor_id();
        let did = folk_device_id();
        if vid != 0x8086 || did != 0x100E {
            log(b"[E1000] Wrong device!");
            return;
        }

        let bar0_size = folk_bar_size(0);
        log(b"[E1000] Device verified, resetting...");

        // Step 1: Reset device
        let ctrl = folk_mmio_read_u32(0, CTRL);
        folk_mmio_write_u32(0, CTRL, ctrl | CTRL_RST);

        // Spin-wait for reset to complete (RST bit self-clears)
        let mut wait = 0i32;
        loop {
            wait += 1;
            if wait > 10000 { break; }
            let c = folk_mmio_read_u32(0, CTRL);
            if (c & CTRL_RST) == 0 { break; }
        }

        log(b"[E1000] Reset complete");

        // Step 2: Set Link Up + Auto-Speed Detection
        let ctrl = folk_mmio_read_u32(0, CTRL);
        folk_mmio_write_u32(0, CTRL, ctrl | CTRL_SLU | CTRL_ASDE);

        // Step 3: Clear all pending interrupts
        folk_mmio_write_u32(0, IMC, 0x7FFF_FFFF); // mask all
        let _ = folk_mmio_read_u32(0, ICR);        // read to clear

        // Step 4: Check initial link status
        let status = folk_mmio_read_u32(0, STATUS);
        LINK_UP = (status & (1 << 1)) != 0; // STATUS.LU bit
        if LINK_UP {
            log(b"[E1000] Link UP");
        } else {
            log(b"[E1000] Link DOWN (waiting for cable)");
        }

        // Step 5: Enable interrupts we care about
        // LSC (link change) + RXT0 (receive) + TXDW (transmit done)
        folk_mmio_write_u32(0, IMS, ICR_LSC | ICR_RXT0 | ICR_TXDW);

        log(b"[E1000] IRQ enabled, entering main loop");

        // Step 6: Main IRQ wait loop
        loop {
            folk_wait_irq();

            // Read and clear interrupt cause
            let icr = folk_mmio_read_u32(0, ICR);
            IRQ_COUNT = IRQ_COUNT.saturating_add(1);

            // Handle Link Status Change
            if (icr & ICR_LSC) != 0 {
                let status = folk_mmio_read_u32(0, STATUS);
                LINK_UP = (status & (1 << 1)) != 0;
                if LINK_UP {
                    log(b"[E1000] Link UP");
                } else {
                    log(b"[E1000] Link DOWN");
                }
            }

            // Handle RX (placeholder — no buffer ring yet)
            if (icr & ICR_RXT0) != 0 {
                // Future: read from RX descriptor ring
                log(b"[E1000] RX interrupt (no handler yet)");
            }

            // Handle TX complete
            if (icr & ICR_TXDW) != 0 {
                // Future: reclaim TX descriptors
            }

            folk_ack_irq();
        }
    }
}
