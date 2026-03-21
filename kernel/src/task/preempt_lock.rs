//! Preemption Lock Mechanism
//!
//! Fine-grained preemption control that allows the timer interrupt to continue
//! firing (for timekeeping and EOI) while preventing task switches.
//! This is superior to CLI/STI which blocks ALL interrupts.

use core::sync::atomic::{AtomicU32, Ordering};

/// Per-CPU preemption disable counter (single-core for now).
/// Accessed from irq_timer assembly via `sym`, so must be `pub`.
#[no_mangle]
pub static PREEMPT_DISABLE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Disable preemptive task switching (timer still fires for timekeeping).
/// Nestable: each disable must have a matching enable.
#[inline]
pub fn preempt_disable() {
    PREEMPT_DISABLE_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Re-enable preemptive task switching.
/// Only actually enables when count drops to 0.
#[inline]
pub fn preempt_enable() {
    let prev = PREEMPT_DISABLE_COUNT.fetch_sub(1, Ordering::Relaxed);
    debug_assert!(prev > 0, "preempt_enable() called without matching disable");
}

/// Check if preemption is currently allowed.
#[inline]
pub fn is_preemption_enabled() -> bool {
    PREEMPT_DISABLE_COUNT.load(Ordering::Relaxed) == 0
}
