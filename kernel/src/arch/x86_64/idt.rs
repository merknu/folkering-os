//! Interrupt Descriptor Table (IDT)

use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Timer tick counter for debugging
static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

/// MSI-X handler dispatch table for vectors 64..=95.
/// Slot i corresponds to IDT vector `MSIX_BASE + i`. A slot of 0 means
/// unregistered; otherwise it's an `fn()` pointer cast to usize.
///
/// We use AtomicUsize rather than AtomicPtr<fn()> because `AtomicPtr` over
/// function pointers isn't stable; the usize cast is the canonical
/// no_std-friendly way to pass `fn()` atomically.
const MSIX_BASE: u8 = 64;
const MSIX_COUNT: usize = 32;
static MSIX_HANDLERS: [AtomicUsize; MSIX_COUNT] = {
    const ZERO: AtomicUsize = AtomicUsize::new(0);
    [ZERO; MSIX_COUNT]
};

/// Register an MSI-X handler for the given IDT vector (must be in 64..=95).
/// The handler runs in interrupt context — keep it short and non-blocking.
/// Returns `Err(())` if the vector is out of range.
pub fn register_msix_handler(vector: u8, handler: fn()) -> Result<(), ()> {
    if vector < MSIX_BASE || vector >= MSIX_BASE + MSIX_COUNT as u8 {
        return Err(());
    }
    let idx = (vector - MSIX_BASE) as usize;
    MSIX_HANDLERS[idx].store(handler as usize, Ordering::Release);
    Ok(())
}

/// Clear a registered MSI-X handler. Safe to call on unregistered slots.
pub fn unregister_msix_handler(vector: u8) {
    if vector < MSIX_BASE || vector >= MSIX_BASE + MSIX_COUNT as u8 {
        return;
    }
    let idx = (vector - MSIX_BASE) as usize;
    MSIX_HANDLERS[idx].store(0, Ordering::Release);
}

/// Dispatch helper called from every MSI-X stub. Inlined into the stub
/// bodies by the macro expansion; factored out so the stub code stays
/// uniform and the compiler can share it.
#[inline(always)]
fn msix_dispatch(vector_idx: usize) {
    let ptr = MSIX_HANDLERS[vector_idx].load(Ordering::Acquire);
    if ptr != 0 {
        let f: fn() = unsafe { core::mem::transmute(ptr) };
        f();
    }
    super::apic::send_eoi();
}

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

        // MSI-X vectors (64-95) — dispatch through MSIX_HANDLERS at runtime
        idt[64].set_handler_fn(msix_handler_64);
        idt[65].set_handler_fn(msix_handler_65);
        idt[66].set_handler_fn(msix_handler_66);
        idt[67].set_handler_fn(msix_handler_67);
        idt[68].set_handler_fn(msix_handler_68);
        idt[69].set_handler_fn(msix_handler_69);
        idt[70].set_handler_fn(msix_handler_70);
        idt[71].set_handler_fn(msix_handler_71);
        idt[72].set_handler_fn(msix_handler_72);
        idt[73].set_handler_fn(msix_handler_73);
        idt[74].set_handler_fn(msix_handler_74);
        idt[75].set_handler_fn(msix_handler_75);
        idt[76].set_handler_fn(msix_handler_76);
        idt[77].set_handler_fn(msix_handler_77);
        idt[78].set_handler_fn(msix_handler_78);
        idt[79].set_handler_fn(msix_handler_79);
        idt[80].set_handler_fn(msix_handler_80);
        idt[81].set_handler_fn(msix_handler_81);
        idt[82].set_handler_fn(msix_handler_82);
        idt[83].set_handler_fn(msix_handler_83);
        idt[84].set_handler_fn(msix_handler_84);
        idt[85].set_handler_fn(msix_handler_85);
        idt[86].set_handler_fn(msix_handler_86);
        idt[87].set_handler_fn(msix_handler_87);
        idt[88].set_handler_fn(msix_handler_88);
        idt[89].set_handler_fn(msix_handler_89);
        idt[90].set_handler_fn(msix_handler_90);
        idt[91].set_handler_fn(msix_handler_91);
        idt[92].set_handler_fn(msix_handler_92);
        idt[93].set_handler_fn(msix_handler_93);
        idt[94].set_handler_fn(msix_handler_94);
        idt[95].set_handler_fn(msix_handler_95);

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

// ── MSI-X IRQ Handlers (vectors 64-95) ──────────────────────────────────
// Each stub dispatches through MSIX_HANDLERS and sends LAPIC EOI.
// MSI-X does not use IOAPIC masking — the device controls delivery via
// the per-entry vector_control mask bit, so stubs don't touch IOAPIC.

macro_rules! msix_irq_handler {
    ($name:ident, $vector:expr) => {
        extern "x86-interrupt" fn $name(_stack_frame: InterruptStackFrame) {
            msix_dispatch($vector - MSIX_BASE as usize);
        }
    };
}

msix_irq_handler!(msix_handler_64, 64);
msix_irq_handler!(msix_handler_65, 65);
msix_irq_handler!(msix_handler_66, 66);
msix_irq_handler!(msix_handler_67, 67);
msix_irq_handler!(msix_handler_68, 68);
msix_irq_handler!(msix_handler_69, 69);
msix_irq_handler!(msix_handler_70, 70);
msix_irq_handler!(msix_handler_71, 71);
msix_irq_handler!(msix_handler_72, 72);
msix_irq_handler!(msix_handler_73, 73);
msix_irq_handler!(msix_handler_74, 74);
msix_irq_handler!(msix_handler_75, 75);
msix_irq_handler!(msix_handler_76, 76);
msix_irq_handler!(msix_handler_77, 77);
msix_irq_handler!(msix_handler_78, 78);
msix_irq_handler!(msix_handler_79, 79);
msix_irq_handler!(msix_handler_80, 80);
msix_irq_handler!(msix_handler_81, 81);
msix_irq_handler!(msix_handler_82, 82);
msix_irq_handler!(msix_handler_83, 83);
msix_irq_handler!(msix_handler_84, 84);
msix_irq_handler!(msix_handler_85, 85);
msix_irq_handler!(msix_handler_86, 86);
msix_irq_handler!(msix_handler_87, 87);
msix_irq_handler!(msix_handler_88, 88);
msix_irq_handler!(msix_handler_89, 89);
msix_irq_handler!(msix_handler_90, 90);
msix_irq_handler!(msix_handler_91, 91);
msix_irq_handler!(msix_handler_92, 92);
msix_irq_handler!(msix_handler_93, 93);
msix_irq_handler!(msix_handler_94, 94);
msix_irq_handler!(msix_handler_95, 95);
