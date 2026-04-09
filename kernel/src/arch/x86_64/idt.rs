//! Interrupt Descriptor Table (IDT)

use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};
use core::sync::atomic::{AtomicU64, Ordering};

/// Timer tick counter for debugging
static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.double_fault.set_handler_fn(double_fault_handler);
        idt.general_protection_fault.set_handler_fn(gpf_handler);

        // Timer interrupt (vector 32)
        idt[32].set_handler_fn(timer_interrupt_handler);

        // Keyboard interrupt (vector 33 = IRQ1)
        idt[33].set_handler_fn(keyboard_interrupt_handler);

        // Mouse interrupt (vector 44 = IRQ12)
        idt[44].set_handler_fn(mouse_interrupt_handler);

        // VirtIO block device interrupt (vector 45)
        idt[45].set_handler_fn(virtio_blk_interrupt_handler);

        // WASM driver IRQ vectors (46-63) — dynamically bound to tasks
        idt[46].set_handler_fn(wasm_irq_handler_46);
        idt[47].set_handler_fn(wasm_irq_handler_47);
        idt[48].set_handler_fn(wasm_irq_handler_48);
        idt[49].set_handler_fn(wasm_irq_handler_49);
        idt[50].set_handler_fn(wasm_irq_handler_50);
        idt[51].set_handler_fn(wasm_irq_handler_51);
        idt[52].set_handler_fn(wasm_irq_handler_52);
        idt[53].set_handler_fn(wasm_irq_handler_53);
        idt[54].set_handler_fn(wasm_irq_handler_54);
        idt[55].set_handler_fn(wasm_irq_handler_55);
        idt[56].set_handler_fn(wasm_irq_handler_56);
        idt[57].set_handler_fn(wasm_irq_handler_57);

        idt
    };
}

/// Initialize IDT
pub fn init() {
    IDT.load();
}

extern "x86-interrupt" fn breakpoint_handler(_stack_frame: InterruptStackFrame) {
    crate::drivers::serial::write_str("EXCEPTION: BREAKPOINT\n");
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: x86_64::structures::idt::PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;
    crate::drivers::serial::write_str("[PAGE_FAULT] Address: ");
    // Cr2::read() may return Result in newer x86_64 versions
    if let Ok(addr) = Cr2::read() {
        crate::drivers::serial::write_hex(addr.as_u64());
    } else {
        crate::drivers::serial::write_str("(read failed)");
    }
    crate::drivers::serial::write_str(", RIP: ");
    crate::drivers::serial::write_hex(stack_frame.instruction_pointer.as_u64());
    crate::drivers::serial::write_str("\n");
    loop { x86_64::instructions::hlt(); }
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    crate::drivers::serial::write_str("[DOUBLE_FAULT] RIP: ");
    crate::drivers::serial::write_hex(stack_frame.instruction_pointer.as_u64());
    crate::drivers::serial::write_str("\n");
    loop { x86_64::instructions::hlt(); }
}

extern "x86-interrupt" fn gpf_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    crate::drivers::serial::write_str("[GPF] Error code: ");
    crate::drivers::serial::write_hex(error_code);
    crate::drivers::serial::write_str(", RIP: ");
    crate::drivers::serial::write_hex(stack_frame.instruction_pointer.as_u64());
    crate::drivers::serial::write_str("\n");
    loop { x86_64::instructions::hlt(); }
}

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    // Debug marker to verify handler is called
    crate::serial_str!("T");

    // Update system uptime
    crate::timer::tick();

    // Increment debug counter
    let ticks = TIMER_TICKS.fetch_add(1, Ordering::Relaxed);

    // Print tick every 100 ticks (~1 second at 100Hz)
    if ticks % 100 == 0 {
        crate::drivers::serial::write_str("[TIMER] Tick ");
        crate::drivers::serial::write_dec((ticks / 100) as u32);
        crate::drivers::serial::write_str("s\n");
    }

    // Debug: Print IOAPIC status every 500 ticks (~5 seconds)
    if ticks == 500 {
        super::ioapic::debug_print_status();
    }

    // Send EOI to APIC
    super::apic::send_eoi();

    // Preemptive scheduling: Check if current task should yield
    // Note: We don't force preemption here yet because the timer interrupt
    // arrives while we're in user mode, and we need proper interrupt-context
    // task switching. For now, tasks yield voluntarily via syscall.
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    // Handle keyboard interrupt
    crate::serial_str!("[IDT33]");
    crate::drivers::keyboard::handle_interrupt();
}

extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    // Handle mouse interrupt (IRQ12)
    crate::serial_str!("[M44]");
    crate::drivers::mouse::handle_interrupt();
}

extern "x86-interrupt" fn virtio_blk_interrupt_handler(_stack_frame: InterruptStackFrame) {
    // Handle VirtIO block device interrupt
    crate::drivers::virtio_blk::irq_handler();
}

// ── WASM Driver IRQ Handlers (vectors 46-57) ────────────────────────────
// Each handler: signal the binding table + mask at IOAPIC + send APIC EOI.
// The WASM driver will unmask via folk_ack_irq when it's done processing.

macro_rules! wasm_irq_handler {
    ($name:ident, $vector:expr) => {
        extern "x86-interrupt" fn $name(_stack_frame: InterruptStackFrame) {
            // Signal the IRQ binding table (sets pending flag)
            super::syscall::signal_irq($vector);
            // Mask this IRQ at IOAPIC to prevent storm (driver unmasks via ACK)
            let irq = $vector - 46; // IRQ line = vector - base
            super::ioapic::disable_irq(irq);
            // Send EOI to LAPIC
            super::apic::send_eoi();
        }
    };
}

wasm_irq_handler!(wasm_irq_handler_46, 46);
wasm_irq_handler!(wasm_irq_handler_47, 47);
wasm_irq_handler!(wasm_irq_handler_48, 48);
wasm_irq_handler!(wasm_irq_handler_49, 49);
wasm_irq_handler!(wasm_irq_handler_50, 50);
wasm_irq_handler!(wasm_irq_handler_51, 51);
wasm_irq_handler!(wasm_irq_handler_52, 52);
wasm_irq_handler!(wasm_irq_handler_53, 53);
wasm_irq_handler!(wasm_irq_handler_54, 54);
wasm_irq_handler!(wasm_irq_handler_55, 55);
wasm_irq_handler!(wasm_irq_handler_56, 56);
wasm_irq_handler!(wasm_irq_handler_57, 57);
