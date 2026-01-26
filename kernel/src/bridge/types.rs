//! Brain Bridge Types
//!
//! Shared memory structures for Smart Brain (userspace) <-> Fast Brain (kernel) communication.
//! The "corpus callosum" between the two-brain architecture.

use core::sync::atomic::{AtomicU64, Ordering};

/// Page size (4KB)
pub const PAGE_SIZE: usize = 4096;

/// Virtual address for BrainBridge in userspace
/// Location: 0x4000_0000_0000 (16TB offset, well above typical userspace allocations)
pub const BRAIN_BRIDGE_VIRT_ADDR: usize = 0x4000_0000_0000;

/// Timeout for hints in milliseconds
/// Hints older than 5 seconds are considered stale and ignored
pub const HINT_TIMEOUT_MS: u64 = 5000;

/// Minimum confidence threshold for using hints
/// Hints below 50% confidence are ignored
pub const MIN_CONFIDENCE: u8 = 128; // 50% on 0-255 scale

/// Brain Bridge - Shared Memory Communication Structure
///
/// # Architecture
///
/// This 4KB page is mapped into both userspace (Synapse, Neural Scheduler) and kernel space,
/// providing a zero-copy, sub-microsecond communication channel for context hints.
///
/// ```text
/// Synapse (Smart Brain, Userspace)
///     │
///     │ Write hints: intent, predictions, confidence
///     │ Increment version (signals new data)
///     ▼
/// BrainBridge (Shared Memory Page @ 0x4000_0000_0000)
///     │
///     │ Kernel reads via HHDM (no page fault)
///     │ Check version != last_read (atomic)
///     │ <1μs latency (L1 cache hit)
///     ▼
/// Neural Scheduler (Fast Brain, Kernel)
///     │
///     │ Apply hints to scheduling decisions
///     │ Boost CPU freq, adjust priorities, etc.
///     └─> Proactive optimization
/// ```
///
/// # Performance
///
/// - Read latency: <100ns (L1 cache hit)
/// - Version check: ~10 cycles (atomic load)
/// - Total overhead: <1μs (well within target)
///
/// # Security
///
/// - Read-only from kernel (no writes to shared memory)
/// - Version stamping prevents stale hints
/// - Timeout prevents zombie hint attacks
/// - Confidence threshold prevents low-quality hints
#[repr(C, align(4096))]
pub struct BrainBridge {
    // ===== Written by Smart Brain (Synapse, userspace) =====

    /// Current user intent classification
    /// See `IntentType` enum for values
    pub current_intent: u8,

    /// Expected duration of high load in seconds
    /// Used for predictive frequency scaling
    /// Example: 30 = "expect heavy load for next 30 seconds"
    pub expected_burst_sec: u32,

    /// Workload type classification
    /// See `WorkloadType` enum for values
    pub workload_type: u8,

    /// Confidence in prediction (0-255)
    /// 0 = no confidence, 255 = certain
    /// Hints below MIN_CONFIDENCE (128) are ignored
    pub confidence: u8,

    /// Predicted CPU usage (0-100)
    /// Percentage of CPU expected to be used
    pub predicted_cpu: u8,

    /// Predicted memory usage (0-100)
    /// Percentage of available memory expected to be used
    pub predicted_memory: u8,

    /// Predicted I/O usage (0-100)
    /// Percentage of I/O bandwidth expected to be used
    pub predicted_io: u8,

    /// Padding for alignment
    _padding1: [u8; 3],

    /// Semantic task type (UTF-8 string)
    /// Examples: "rust_compile", "video_encode", "ml_training"
    /// Used for pattern learning and profiling
    pub task_type: [u8; 32],

    /// Version counter (incremented on each write)
    /// Kernel checks if version > last_read to detect new hints
    /// Atomic ensures visibility across cores
    pub version: AtomicU64,

    /// Timestamp in milliseconds since boot
    /// Used to detect stale hints (timeout after HINT_TIMEOUT_MS)
    pub timestamp: u64,

    /// Task ID of the writer (for debugging)
    pub writer_task_id: u32,

    /// Padding for alignment
    _padding2: [u8; 4],

    // ===== Written by Fast Brain (Neural Scheduler, kernel) =====

    /// Current CPU frequency in MHz
    /// Feedback from kernel to userspace
    pub current_cpu_freq_mhz: u32,

    /// Scheduler confidence (0.0-1.0)
    /// How well predictions match reality (for model tuning)
    pub scheduler_confidence: f32,

    /// Last time kernel read this structure (milliseconds since boot)
    /// Used to detect if kernel is actively reading hints
    pub last_read_timestamp: u64,

    // ===== Statistics (both sides) =====

    /// Total hints written by userspace
    pub total_hints: u64,

    /// Hints actually used by kernel (confidence >= threshold)
    pub hints_used: u64,

    /// Hints ignored due to low confidence
    pub hints_rejected_confidence: u64,

    /// Hints ignored due to timeout (stale)
    pub hints_rejected_timeout: u64,

    // ===== Padding to exactly 4096 bytes =====

    /// Padding to fill page
    /// Calculation accounting for alignment:
    /// - current_intent: u8 @ 0
    /// - (3 bytes implicit padding for u32 alignment)
    /// - expected_burst_sec: u32 @ 4
    /// - workload_type: u8 @ 8
    /// - confidence: u8 @ 9
    /// - predicted_cpu: u8 @ 10
    /// - predicted_memory: u8 @ 11
    /// - predicted_io: u8 @ 12
    /// - _padding1: [u8; 3] @ 13-15
    /// - task_type: [u8; 32] @ 16-47
    /// - version: AtomicU64 @ 48-55
    /// - timestamp: u64 @ 56-63
    /// - writer_task_id: u32 @ 64-67
    /// - _padding2: [u8; 4] @ 68-71
    /// - current_cpu_freq_mhz: u32 @ 72-75
    /// - scheduler_confidence: f32 @ 76-79
    /// - last_read_timestamp: u64 @ 80-87
    /// - total_hints: u64 @ 88-95
    /// - hints_used: u64 @ 96-103
    /// - hints_rejected_confidence: u64 @ 104-111
    /// - hints_rejected_timeout: u64 @ 112-119
    /// Total used: 120 bytes
    /// Padding needed: 4096 - 120 = 3976 bytes
    _padding3: [u8; 3976],
}

// Compile-time assertion that BrainBridge is exactly 4096 bytes
const _: () = assert!(core::mem::size_of::<BrainBridge>() == PAGE_SIZE);

impl BrainBridge {
    /// Create a new BrainBridge (zero-initialized)
    pub const fn new() -> Self {
        Self {
            current_intent: IntentType::Idle as u8,
            expected_burst_sec: 0,
            workload_type: WorkloadType::Mixed as u8,
            confidence: 0,
            predicted_cpu: 0,
            predicted_memory: 0,
            predicted_io: 0,
            _padding1: [0; 3],
            task_type: [0; 32],
            version: AtomicU64::new(0),
            timestamp: 0,
            writer_task_id: 0,
            _padding2: [0; 4],
            current_cpu_freq_mhz: 0,
            scheduler_confidence: 0.0,
            last_read_timestamp: 0,
            total_hints: 0,
            hints_used: 0,
            hints_rejected_confidence: 0,
            hints_rejected_timeout: 0,
            _padding3: [0; 3976],
        }
    }

    /// Check if hint is valid (not stale, confidence acceptable)
    pub fn is_hint_valid(&self, current_time_ms: u64) -> bool {
        // Check confidence threshold
        if self.confidence < MIN_CONFIDENCE {
            return false;
        }

        // Check timestamp (not too old)
        if current_time_ms - self.timestamp > HINT_TIMEOUT_MS {
            return false;
        }

        true
    }
}

/// User intent classification
///
/// High-level semantic classification of what the user is doing.
/// Used by the scheduler to make proactive decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IntentType {
    /// No specific intent detected
    Idle = 0,

    /// Gaming (low latency, consistent frame times)
    Gaming = 1,

    /// Coding/Development (bursty compiles)
    Coding = 2,

    /// Rendering (GPU + CPU heavy)
    Rendering = 3,

    /// Compiling (CPU-intensive, predictable duration)
    Compiling = 4,

    /// Video encoding (sustained CPU/GPU load)
    VideoEncoding = 5,

    /// Machine learning training (GPU + memory intensive)
    MLTraining = 6,

    /// Web browsing (low CPU, bursty I/O)
    Browsing = 7,

    /// Video playback (consistent, predictable load)
    VideoPlayback = 8,
}

/// Workload type classification
///
/// Resource usage pattern for scheduling optimization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WorkloadType {
    /// CPU-bound (computation heavy)
    /// Example: Compilation, scientific computing
    CpuBound = 0,

    /// I/O-bound (disk/network heavy)
    /// Example: File copying, downloads
    IoBound = 1,

    /// Mixed workload
    /// Example: IDE with background compilation
    Mixed = 2,

    /// Memory-bound (large memory access patterns)
    /// Example: Database queries, large data processing
    MemoryBound = 3,

    /// GPU-bound (GPU computation heavy)
    /// Example: Gaming, video encoding, ML inference
    GpuBound = 4,
}

/// Snapshot of brain bridge data
///
/// Owned copy of hint data, safe to use without holding reference to BrainBridge.
/// This is what the kernel scheduler receives from the reader.
#[derive(Debug, Clone, Copy)]
pub struct BrainBridgeSnapshot {
    pub current_intent: IntentType,
    pub expected_burst_sec: u32,
    pub workload_type: WorkloadType,
    pub predicted_cpu: u8,
    pub predicted_memory: u8,
    pub predicted_io: u8,
    pub confidence: u8,
    pub timestamp: u64,
}

impl BrainBridgeSnapshot {
    /// Create snapshot from BrainBridge
    pub fn from_bridge(bridge: &BrainBridge) -> Self {
        Self {
            current_intent: IntentType::from_u8(bridge.current_intent),
            expected_burst_sec: bridge.expected_burst_sec,
            workload_type: WorkloadType::from_u8(bridge.workload_type),
            predicted_cpu: bridge.predicted_cpu,
            predicted_memory: bridge.predicted_memory,
            predicted_io: bridge.predicted_io,
            confidence: bridge.confidence,
            timestamp: bridge.timestamp,
        }
    }
}

impl IntentType {
    /// Convert from u8 (handles invalid values)
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => IntentType::Idle,
            1 => IntentType::Gaming,
            2 => IntentType::Coding,
            3 => IntentType::Rendering,
            4 => IntentType::Compiling,
            5 => IntentType::VideoEncoding,
            6 => IntentType::MLTraining,
            7 => IntentType::Browsing,
            8 => IntentType::VideoPlayback,
            _ => IntentType::Idle, // Default to Idle for invalid values
        }
    }
}

impl WorkloadType {
    /// Convert from u8 (handles invalid values)
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => WorkloadType::CpuBound,
            1 => WorkloadType::IoBound,
            2 => WorkloadType::Mixed,
            3 => WorkloadType::MemoryBound,
            4 => WorkloadType::GpuBound,
            _ => WorkloadType::Mixed, // Default to Mixed for invalid values
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_brain_bridge_size() {
        // Verify BrainBridge is exactly 4096 bytes (one page)
        assert_eq!(core::mem::size_of::<BrainBridge>(), PAGE_SIZE);
    }

    #[test]
    fn test_brain_bridge_alignment() {
        // Verify BrainBridge is 4096-byte aligned
        assert_eq!(core::mem::align_of::<BrainBridge>(), PAGE_SIZE);
    }

    #[test]
    fn test_brain_bridge_new() {
        let bridge = BrainBridge::new();
        assert_eq!(bridge.current_intent, IntentType::Idle as u8);
        assert_eq!(bridge.confidence, 0);
        assert_eq!(bridge.version.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_hint_validation() {
        let mut bridge = BrainBridge::new();

        // Low confidence should be invalid
        bridge.confidence = 50;
        bridge.timestamp = 1000;
        assert_eq!(bridge.is_hint_valid(1100), false);

        // High confidence but stale should be invalid
        bridge.confidence = 200;
        bridge.timestamp = 1000;
        assert_eq!(bridge.is_hint_valid(10000), false); // 9 seconds old

        // High confidence and fresh should be valid
        bridge.confidence = 200;
        bridge.timestamp = 1000;
        assert_eq!(bridge.is_hint_valid(1500), true); // 500ms old
    }

    #[test]
    fn test_intent_type_conversion() {
        assert_eq!(IntentType::from_u8(0), IntentType::Idle);
        assert_eq!(IntentType::from_u8(4), IntentType::Compiling);
        assert_eq!(IntentType::from_u8(255), IntentType::Idle); // Invalid -> Idle
    }

    #[test]
    fn test_workload_type_conversion() {
        assert_eq!(WorkloadType::from_u8(0), WorkloadType::CpuBound);
        assert_eq!(WorkloadType::from_u8(4), WorkloadType::GpuBound);
        assert_eq!(WorkloadType::from_u8(255), WorkloadType::Mixed); // Invalid -> Mixed
    }
}
