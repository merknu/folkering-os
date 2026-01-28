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
}
