//! Interrupt Management

/// Enable interrupts
pub fn enable() {
    x86_64::instructions::interrupts::enable();
}

/// Disable interrupts
pub fn disable() {
    x86_64::instructions::interrupts::disable();
}
