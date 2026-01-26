//! Brain Bridge Types - Userspace Mirror
//!
//! These types mirror the kernel types defined in `kernel/src/bridge/types.rs`.
//! Must be kept in sync to ensure ABI compatibility.

use core::sync::atomic::AtomicU64;

/// Page size (4KB)
pub const PAGE_SIZE: usize = 4096;

/// Virtual address for BrainBridge in userspace
pub const BRAIN_BRIDGE_VIRT_ADDR: usize = 0x4000_0000_0000;

/// User intent classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IntentType {
    Idle = 0,
    Gaming = 1,
    Coding = 2,
    Rendering = 3,
    Compiling = 4,
    VideoEncoding = 5,
    MLTraining = 6,
    Browsing = 7,
    VideoPlayback = 8,
}

/// Workload type classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WorkloadType {
    CpuBound = 0,
    IoBound = 1,
    Mixed = 2,
    MemoryBound = 3,
    GpuBound = 4,
}

/// Brain Bridge structure (must match kernel layout exactly)
#[repr(C, align(4096))]
pub struct BrainBridge {
    pub current_intent: u8,
    pub expected_burst_sec: u32,
    pub workload_type: u8,
    pub confidence: u8,
    pub predicted_cpu: u8,
    pub predicted_memory: u8,
    pub predicted_io: u8,
    pub _padding1: [u8; 3],
    pub task_type: [u8; 32],
    pub version: AtomicU64,
    pub timestamp: u64,
    pub writer_task_id: u32,
    pub _padding2: [u8; 4],
    pub current_cpu_freq_mhz: u32,
    pub scheduler_confidence: f32,
    pub last_read_timestamp: u64,
    pub total_hints: u64,
    pub hints_used: u64,
    pub hints_rejected_confidence: u64,
    pub hints_rejected_timeout: u64,
    pub _padding3: [u8; 3976],
}

// Compile-time assertion
const _: () = assert!(core::mem::size_of::<BrainBridge>() == PAGE_SIZE);

/// Intent information for writing hints
#[derive(Debug, Clone)]
pub struct Intent {
    pub intent_type: IntentType,
    pub expected_duration_sec: u32,
    pub workload: WorkloadType,
    pub predicted_cpu_usage: u8,       // 0-100
    pub predicted_memory_usage: u8,    // 0-100
    pub predicted_io_usage: u8,        // 0-100
    pub confidence: u8,                // 0-255
    pub task_type: Option<String>,     // Semantic label (max 31 bytes)
}

impl Default for Intent {
    fn default() -> Self {
        Self {
            intent_type: IntentType::Idle,
            expected_duration_sec: 0,
            workload: WorkloadType::Mixed,
            predicted_cpu_usage: 0,
            predicted_memory_usage: 0,
            predicted_io_usage: 0,
            confidence: 0,
            task_type: None,
        }
    }
}

impl Intent {
    /// Create a new intent with common defaults
    pub fn new(intent_type: IntentType) -> Self {
        Self {
            intent_type,
            ..Default::default()
        }
    }

    /// Set expected duration
    pub fn with_duration(mut self, seconds: u32) -> Self {
        self.expected_duration_sec = seconds;
        self
    }

    /// Set workload type
    pub fn with_workload(mut self, workload: WorkloadType) -> Self {
        self.workload = workload;
        self
    }

    /// Set predicted CPU usage (0-100)
    pub fn with_cpu(mut self, usage: u8) -> Self {
        self.predicted_cpu_usage = usage.min(100);
        self
    }

    /// Set predicted memory usage (0-100)
    pub fn with_memory(mut self, usage: u8) -> Self {
        self.predicted_memory_usage = usage.min(100);
        self
    }

    /// Set predicted I/O usage (0-100)
    pub fn with_io(mut self, usage: u8) -> Self {
        self.predicted_io_usage = usage.min(100);
        self
    }

    /// Set confidence (0-255)
    pub fn with_confidence(mut self, confidence: u8) -> Self {
        self.confidence = confidence;
        self
    }

    /// Set semantic task type label
    pub fn with_task_type(mut self, task_type: impl Into<String>) -> Self {
        self.task_type = Some(task_type.into());
        self
    }
}
