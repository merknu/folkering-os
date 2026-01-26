//! Neural Scheduler Types
//!
//! Defines the data structures for predictive task scheduling.

use serde::{Deserialize, Serialize};

/// Task ID (same as used in Intent Bus)
pub type TaskId = u32;

/// Timestamp in milliseconds since epoch
pub type Timestamp = u64;

/// System metrics snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    /// Timestamp when metrics were collected
    pub timestamp: Timestamp,

    /// CPU usage (0.0 - 1.0)
    pub cpu_usage: f32,

    /// Memory usage (0.0 - 1.0)
    pub memory_usage: f32,

    /// I/O operations per second
    pub io_ops: u32,

    /// Network throughput (bytes/sec)
    pub network_throughput: u64,

    /// Number of active tasks
    pub active_tasks: u32,

    /// Average task duration (ms)
    pub avg_task_duration: f32,
}

/// Task execution event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    /// Task ID
    pub task_id: TaskId,

    /// Event type
    pub event_type: TaskEventType,

    /// Timestamp
    pub timestamp: Timestamp,

    /// CPU time consumed (ms)
    pub cpu_time: u64,

    /// Memory used (bytes)
    pub memory_used: u64,
}

/// Task event types
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskEventType {
    Created,
    Started,
    Blocked,
    Resumed,
    Completed,
    Killed,
}

/// Prediction for future resource usage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourcePrediction {
    /// Timestamp for prediction
    pub timestamp: Timestamp,

    /// Predicted CPU usage (0.0 - 1.0)
    pub predicted_cpu: f32,

    /// Predicted memory usage (0.0 - 1.0)
    pub predicted_memory: f32,

    /// Predicted I/O load
    pub predicted_io: f32,

    /// Confidence (0.0 - 1.0)
    pub confidence: f32,
}

/// Scheduling decision
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SchedulingDecision {
    /// Increase CPU frequency
    ScaleCpuUp { target_freq_mhz: u32 },

    /// Decrease CPU frequency (save power)
    ScaleCpuDown { target_freq_mhz: u32 },

    /// Pre-warm cache for predicted task
    PrefetchData { task_id: TaskId, pages: Vec<u64> },

    /// Prepare I/O buffers
    PreallocateBuffers { size_bytes: u64 },

    /// Wake up sleeping core
    WakeCore { core_id: u8 },

    /// Put core to sleep
    SleepCore { core_id: u8 },

    /// No action needed
    NoAction,
}

/// Task pattern (for learning user behavior)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPattern {
    /// Time of day (0-23)
    pub hour: u8,

    /// Day of week (0-6, 0 = Sunday)
    pub day_of_week: u8,

    /// Tasks typically running at this time
    pub common_tasks: Vec<TaskId>,

    /// Average system load
    pub avg_load: f32,

    /// Confidence in this pattern
    pub confidence: f32,
}

/// Neural scheduler configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// History window size (samples to keep)
    pub history_window: usize,

    /// Prediction horizon (ms into future)
    pub prediction_horizon_ms: u64,

    /// Minimum confidence for decisions
    pub min_confidence: f32,

    /// Enable aggressive power management
    pub aggressive_power_saving: bool,

    /// Enable predictive prefetching
    pub predictive_prefetch: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            history_window: 1000,              // Keep 1000 samples
            prediction_horizon_ms: 1000,       // Predict 1 second ahead
            min_confidence: 0.7,                // 70% confidence threshold
            aggressive_power_saving: false,     // Conservative by default
            predictive_prefetch: true,          // Enable prefetching
        }
    }
}
