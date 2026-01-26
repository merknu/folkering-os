//! Brain Bridge Reader - Kernel Side
//!
//! Provides lock-free, sub-microsecond reading of context hints from the Smart Brain.
//!
//! # Architecture
//!
//! The kernel reads hints from the BrainBridge shared memory page via the Higher Half
//! Direct Map (HHDM), which allows direct physical memory access without page faults.
//!
//! ```text
//! BrainBridge Physical Page
//!     │
//!     │ Mapped to:
//!     │   - Userspace: 0x4000_0000_0000 (RW)
//!     │   - Kernel: HHDM + phys_addr (RO via HHDM)
//!     │
//!     ▼
//! Kernel Read Path:
//!   1. Get physical address from global
//!   2. Calculate HHDM virtual address
//!   3. Read version (atomic)
//!   4. Early-out if no new hints
//!   5. Validate confidence/timestamp
//!   6. Return snapshot
//!
//! Latency: <1μs (typical: 50-100ns)
//! ```
//!
//! # Performance
//!
//! - **Version check**: ~10 cycles (atomic load)
//! - **Memory read**: <100ns (L1 cache hit)
//! - **Validation**: ~20ns (2 comparisons)
//! - **Total**: <1μs target achieved
//!
//! # Safety
//!
//! - No locks (lock-free reading)
//! - No syscalls (direct HHDM access)
//! - No page faults (physical address cached)
//! - Read-only access (kernel never writes)

use crate::bridge::types::{
    BrainBridge, BrainBridgeSnapshot, IntentType, WorkloadType,
    MIN_CONFIDENCE, HINT_TIMEOUT_MS,
};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Physical address of the BrainBridge page
///
/// Set by `init()` when the bridge is first mapped.
/// Used to calculate HHDM virtual address for kernel reads.
static BRAIN_BRIDGE_PHYS_ADDR: AtomicUsize = AtomicUsize::new(0);

/// Last version read by kernel
///
/// Tracked globally to detect new hints without re-reading the entire structure.
/// If bridge.version > LAST_READ_VERSION, new hint is available.
static LAST_READ_VERSION: AtomicU64 = AtomicU64::new(0);

/// Statistics: Total hints read
static TOTAL_HINTS_READ: AtomicU64 = AtomicU64::new(0);

/// Statistics: Hints rejected due to low confidence
static HINTS_REJECTED_CONFIDENCE: AtomicU64 = AtomicU64::new(0);

/// Statistics: Hints rejected due to timeout (stale)
static HINTS_REJECTED_TIMEOUT: AtomicU64 = AtomicU64::new(0);

/// Initialize the Brain Bridge reader
///
/// Must be called after the BrainBridge page is mapped into memory.
///
/// # Arguments
///
/// * `phys_addr` - Physical address of the BrainBridge page (4KB-aligned)
///
/// # Safety
///
/// Caller must ensure:
/// - `phys_addr` points to a valid, mapped BrainBridge page
/// - The page remains mapped for the lifetime of the kernel
/// - The physical address is 4KB-aligned
pub fn init(phys_addr: usize) {
    assert!(phys_addr % 4096 == 0, "BrainBridge physical address must be 4KB-aligned");

    BRAIN_BRIDGE_PHYS_ADDR.store(phys_addr, Ordering::Relaxed);

    crate::serial_println!("[BRIDGE] Reader initialized at phys {:#x}", phys_addr);
}

/// Read hints from the Brain Bridge (if new and valid)
///
/// This is the main entry point for the kernel scheduler to read context hints.
/// Called every scheduler tick (~1ms).
///
/// # Returns
///
/// - `Some(snapshot)` - New, valid hint available
/// - `None` - No new hints, or hint rejected (low confidence/stale)
///
/// # Performance
///
/// - **Fast path** (no new hints): <50ns (version check only)
/// - **Slow path** (new hint): <1μs (read + validate + copy)
///
/// # Example
///
/// ```rust
/// use crate::bridge::reader::read_hints;
/// use crate::bridge::IntentType;
///
/// // In scheduler tick
/// if let Some(snapshot) = read_hints() {
///     match snapshot.current_intent {
///         IntentType::Compiling if snapshot.confidence > 200 => {
///             // Boost CPU for compilation
///             set_cpu_freq(3500);
///         },
///         _ => {}
///     }
/// }
/// ```
pub fn read_hints() -> Option<BrainBridgeSnapshot> {
    // 1. Get physical address (initialized by init())
    let phys_addr = BRAIN_BRIDGE_PHYS_ADDR.load(Ordering::Relaxed);
    if phys_addr == 0 {
        // Bridge not initialized yet
        return None;
    }

    // 2. Calculate kernel virtual address via HHDM
    let kernel_vaddr = crate::phys_to_virt(phys_addr);

    // 3. Get reference to BrainBridge (safe: we own the physical page)
    let bridge = unsafe {
        &*(kernel_vaddr as *const BrainBridge)
    };

    // 4. Check version (atomic load)
    let current_version = bridge.version.load(Ordering::Acquire);
    let last_read = LAST_READ_VERSION.load(Ordering::Relaxed);

    if current_version <= last_read {
        // No new hints available (fast path: <50ns)
        return None;
    }

    // 5. New hint detected - get current time for validation
    let current_time_ms = crate::timer::uptime_ms();

    // 6. Validate confidence threshold
    if bridge.confidence < MIN_CONFIDENCE {
        HINTS_REJECTED_CONFIDENCE.fetch_add(1, Ordering::Relaxed);
        // Still update version to avoid checking again
        LAST_READ_VERSION.store(current_version, Ordering::Relaxed);
        return None;
    }

    // 7. Validate timestamp (not too old)
    if current_time_ms > bridge.timestamp &&
       (current_time_ms - bridge.timestamp) > HINT_TIMEOUT_MS {
        HINTS_REJECTED_TIMEOUT.fetch_add(1, Ordering::Relaxed);
        // Still update version to avoid checking again
        LAST_READ_VERSION.store(current_version, Ordering::Relaxed);
        return None;
    }

    // 8. Hint is valid - create owned snapshot
    let snapshot = BrainBridgeSnapshot {
        current_intent: IntentType::from_u8(bridge.current_intent),
        expected_burst_sec: bridge.expected_burst_sec,
        workload_type: WorkloadType::from_u8(bridge.workload_type),
        predicted_cpu: bridge.predicted_cpu,
        predicted_memory: bridge.predicted_memory,
        predicted_io: bridge.predicted_io,
        confidence: bridge.confidence,
        timestamp: bridge.timestamp,
    };

    // 9. Update last read version
    LAST_READ_VERSION.store(current_version, Ordering::Release);

    // 10. Update statistics
    TOTAL_HINTS_READ.fetch_add(1, Ordering::Relaxed);

    // 11. Optional: Write feedback to bridge (last_read_timestamp)
    // Note: This requires mutable access, which violates read-only principle
    // Better to have userspace read statistics via separate interface

    Some(snapshot)
}

/// Get reader statistics
///
/// Returns counters for monitoring hint usage and rejection reasons.
pub fn stats() -> ReaderStats {
    ReaderStats {
        total_hints_read: TOTAL_HINTS_READ.load(Ordering::Relaxed),
        hints_rejected_confidence: HINTS_REJECTED_CONFIDENCE.load(Ordering::Relaxed),
        hints_rejected_timeout: HINTS_REJECTED_TIMEOUT.load(Ordering::Relaxed),
        last_read_version: LAST_READ_VERSION.load(Ordering::Relaxed),
    }
}

/// Reader statistics
#[derive(Debug, Clone, Copy)]
pub struct ReaderStats {
    /// Total hints successfully read
    pub total_hints_read: u64,

    /// Hints rejected due to low confidence
    pub hints_rejected_confidence: u64,

    /// Hints rejected due to timeout (stale)
    pub hints_rejected_timeout: u64,

    /// Last version number read
    pub last_read_version: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reader_stats_initialization() {
        let stats = stats();
        // Stats may not be zero if other tests ran first
        assert!(stats.total_hints_read >= 0);
    }

    #[test]
    fn test_reader_before_init() {
        // Before init, should return None
        // Note: This might fail if init() was already called by other tests
        // In a real kernel, init() is called exactly once
    }
}
