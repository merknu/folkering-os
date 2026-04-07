//! Timer Subsystem

use core::sync::atomic::{AtomicU64, Ordering};

static UPTIME_MS: AtomicU64 = AtomicU64::new(0);

/// Get system uptime in milliseconds
pub fn uptime_ms() -> u64 {
    UPTIME_MS.load(Ordering::Relaxed)
}

/// Increment uptime (called by timer interrupt)
pub fn tick() {
    UPTIME_MS.fetch_add(10, Ordering::Relaxed);

    // NOTE: Network polling is NOT done from the timer ISR.
    // Calling net::poll() from ISR context causes #GP faults because:
    // 1. ISR stack may not be 16-byte aligned (SSE in smoltcp checksums)
    // 2. NET_STATE lock reentrancy if a syscall already holds it
    // Instead, the compositor drives network polling via its idle syscall (0x99),
    // and blocking syscalls (TLS, DNS, Gemini) poll inline.
}
