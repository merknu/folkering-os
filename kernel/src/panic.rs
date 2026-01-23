//! Kernel Panic Handler
//!
//! Handles kernel panics with detailed diagnostics and safe shutdown.

use core::panic::PanicInfo;

/// Panic handler for kernel panics
///
/// Displays detailed error information and halts the system.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    use x86_64::instructions::interrupts;

    // Disable interrupts to prevent further damage
    interrupts::disable();

    // Print panic banner
    crate::drivers::serial::_print(format_args!("\n\n"));
    crate::drivers::serial::_print(format_args!("╔════════════════════════════════════════╗\n"));
    crate::drivers::serial::_print(format_args!("║     KERNEL PANIC                       ║\n"));
    crate::drivers::serial::_print(format_args!("╚════════════════════════════════════════╝\n\n"));

    // Print location
    if let Some(location) = info.location() {
        crate::drivers::serial::_print(format_args!(
            "Location: {}:{}:{}\n",
            location.file(),
            location.line(),
            location.column()
        ));
    }

    // Print message
    let message = info.message();
    crate::drivers::serial::_print(format_args!("Message: {}\n", message));

    crate::drivers::serial::_print(format_args!("\nStack trace:\n"));
    // TODO: Stack unwinding

    crate::drivers::serial::_print(format_args!("\nSystem halted.\n"));

    // Halt CPU
    loop {
        x86_64::instructions::hlt();
    }
}
