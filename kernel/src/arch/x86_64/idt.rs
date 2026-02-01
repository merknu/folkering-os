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
