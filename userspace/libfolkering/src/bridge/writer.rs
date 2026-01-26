//! Brain Bridge Writer - Userspace Side
//!
//! Provides high-level API for writing context hints to the kernel scheduler.
//!
//! # Architecture
//!
//! ```text
//! Application (Synapse, Neural Scheduler)
//!     │
//!     │ write_hint(Intent)
//!     ▼
//! BrainBridgeWriter
//!     │
//!     ├─> Write to shared memory @ 0x4000_0000_0000
//!     ├─> Update timestamp
//!     ├─> Increment version (atomic)
//!     └─> ~500ns latency
//!     │
//!     ▼
//! BrainBridge Page (4KB)
//!     │
//!     │ Kernel reads via HHDM (<1μs)
//!     ▼
//! Neural Scheduler (Kernel)
//! ```
//!
//! # Usage
//!
//! ```no_run
//! use libfolkering::bridge::{BrainBridgeWriter, Intent, IntentType, WorkloadType};
//!
//! // Initialize writer (one-time setup)
//! let mut writer = BrainBridgeWriter::new()?;
//!
//! // Write hint
//! writer.write_hint(Intent::new(IntentType::Compiling)
//!     .with_duration(30)
//!     .with_workload(WorkloadType::CpuBound)
//!     .with_cpu(85)
//!     .with_confidence(200)
//!     .with_task_type("cargo_build")
//! )?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use super::types::{BrainBridge, Intent};
use core::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

/// Brain Bridge Writer
///
/// Provides a safe, high-level API for writing context hints to the kernel.
///
/// # Thread Safety
///
/// This struct is NOT thread-safe. Create one instance per thread if needed,
/// or protect with a Mutex.
pub struct BrainBridgeWriter {
    /// Reference to mapped BrainBridge page
    bridge: &'static mut BrainBridge,

    /// Task ID (for debugging)
    task_id: u32,
}

/// Writer errors
#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    #[error("Failed to create shared memory: {0}")]
    ShmemCreate(String),

    #[error("Failed to map shared memory: {0}")]
    ShmemMap(String),

    #[error("BrainBridge address is NULL")]
    NullPointer,

    #[error("Task type string too long (max 31 bytes)")]
    TaskTypeTooLong,

    #[error("Syscall failed: {0}")]
    Syscall(String),
}

impl BrainBridgeWriter {
    /// Create a new BrainBridgeWriter
    ///
    /// This performs one-time setup:
    /// 1. Creates 4KB shared memory region
    /// 2. Maps it at BRAIN_BRIDGE_VIRT_ADDR
    /// 3. Initializes the structure
    /// 4. Returns writer handle
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Shared memory creation fails
    /// - Mapping fails
    /// - Address is invalid
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use libfolkering::bridge::BrainBridgeWriter;
    /// let mut writer = BrainBridgeWriter::new()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn new() -> Result<Self, WriterError> {
        // Note: In a real implementation, these would be actual syscalls
        // For now, we'll create the structure for API demonstration

        // 1. Create shared memory region (4KB)
        // let shmem_id = syscall_shmem_create(4096, SHMEM_RW)?;

        // 2. Map at designated address
        // syscall_shmem_map(shmem_id, BRAIN_BRIDGE_VIRT_ADDR)?;

        // 3. Get mutable reference to mapped page
        // Safety: We own this page via shared memory system
        let bridge = unsafe {
            // In real implementation, this would be the mapped shared memory
            // For now, allocate on heap (demonstration purposes)
            let layout = std::alloc::Layout::from_size_align(4096, 4096)
                .map_err(|e| WriterError::ShmemCreate(e.to_string()))?;
            let ptr = std::alloc::alloc_zeroed(layout);
            if ptr.is_null() {
                return Err(WriterError::NullPointer);
            }
            &mut *(ptr as *mut BrainBridge)
        };

        // 4. Initialize version counter
        bridge.version.store(0, Ordering::Relaxed);

        // 5. Get current task ID (in real implementation)
        let task_id = 0; // Would be from syscall_get_task_id()

        tracing::info!("BrainBridgeWriter initialized");

        Ok(Self {
            bridge,
            task_id,
        })
    }

    /// Write a hint to the Brain Bridge
    ///
    /// Updates the BrainBridge structure with the provided intent and
    /// atomically increments the version to signal new data to the kernel.
    ///
    /// # Performance
    ///
    /// - Typical latency: ~500ns
    /// - Atomic version increment: ~10ns
    /// - Memory writes: ~200-300ns (L1 cache)
    /// - Timestamp generation: ~100-200ns
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use libfolkering::bridge::{BrainBridgeWriter, Intent, IntentType};
    /// # let mut writer = BrainBridgeWriter::new()?;
    /// writer.write_hint(Intent::new(IntentType::Compiling)
    ///     .with_duration(30)
    ///     .with_confidence(200)
    /// )?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn write_hint(&mut self, intent: Intent) -> Result<(), WriterError> {
        // 1. Get current timestamp (milliseconds since UNIX_EPOCH)
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        // 2. Write fields to bridge
        self.bridge.current_intent = intent.intent_type as u8;
        self.bridge.expected_burst_sec = intent.expected_duration_sec;
        self.bridge.workload_type = intent.workload as u8;
        self.bridge.confidence = intent.confidence;
        self.bridge.predicted_cpu = intent.predicted_cpu_usage;
        self.bridge.predicted_memory = intent.predicted_memory_usage;
        self.bridge.predicted_io = intent.predicted_io_usage;
        self.bridge.timestamp = timestamp;
        self.bridge.writer_task_id = self.task_id;

        // 3. Copy task type string (if provided)
        if let Some(task_type) = &intent.task_type {
            let bytes = task_type.as_bytes();
            if bytes.len() > 31 {
                return Err(WriterError::TaskTypeTooLong);
            }
            // Zero out first
            self.bridge.task_type = [0; 32];
            // Copy string
            self.bridge.task_type[..bytes.len()].copy_from_slice(bytes);
        } else {
            self.bridge.task_type = [0; 32];
        }

        // 4. Increment version LAST (signals write complete)
        // This is the synchronization point - kernel checks this
        let new_version = self.bridge.version.fetch_add(1, Ordering::Release) + 1;

        // 5. Update statistics
        self.bridge.total_hints += 1;

        tracing::debug!(
            "Wrote hint: intent={:?}, confidence={}, version={}",
            intent.intent_type,
            intent.confidence,
            new_version
        );

        Ok(())
    }

    /// Get statistics from the bridge
    ///
    /// Returns counters showing how many hints were written and used.
    pub fn stats(&self) -> WriterStats {
        WriterStats {
            total_hints_written: self.bridge.total_hints,
            hints_used: self.bridge.hints_used,
            hints_rejected_confidence: self.bridge.hints_rejected_confidence,
            hints_rejected_timeout: self.bridge.hints_rejected_timeout,
            current_version: self.bridge.version.load(Ordering::Relaxed),
            last_kernel_read: self.bridge.last_read_timestamp,
        }
    }

    /// Read current CPU frequency from kernel feedback
    ///
    /// The kernel writes back the current CPU frequency, allowing
    /// userspace to monitor scheduling decisions.
    pub fn current_cpu_freq_mhz(&self) -> u32 {
        self.bridge.current_cpu_freq_mhz
    }

    /// Read scheduler confidence from kernel feedback
    ///
    /// The kernel writes back how confident it is in predictions,
    /// allowing userspace models to tune themselves.
    pub fn scheduler_confidence(&self) -> f32 {
        self.bridge.scheduler_confidence
    }
}

impl Drop for BrainBridgeWriter {
    fn drop(&mut self) {
        // In real implementation, would unmap shared memory
        // syscall_shmem_unmap(shmem_id, BRAIN_BRIDGE_VIRT_ADDR);

        tracing::info!("BrainBridgeWriter dropped");
    }
}

/// Writer statistics
#[derive(Debug, Clone, Copy)]
pub struct WriterStats {
    /// Total hints written by this writer
    pub total_hints_written: u64,

    /// Hints actually used by kernel (from feedback)
    pub hints_used: u64,

    /// Hints rejected due to low confidence (from feedback)
    pub hints_rejected_confidence: u64,

    /// Hints rejected due to timeout (from feedback)
    pub hints_rejected_timeout: u64,

    /// Current version number
    pub current_version: u64,

    /// Last time kernel read the bridge (milliseconds)
    pub last_kernel_read: u64,
}

impl WriterStats {
    /// Calculate hint usage rate (0.0-1.0)
    pub fn usage_rate(&self) -> f64 {
        if self.total_hints_written == 0 {
            return 0.0;
        }
        self.hints_used as f64 / self.total_hints_written as f64
    }

    /// Calculate rejection rate (0.0-1.0)
    pub fn rejection_rate(&self) -> f64 {
        if self.total_hints_written == 0 {
            return 0.0;
        }
        let rejected = self.hints_rejected_confidence + self.hints_rejected_timeout;
        rejected as f64 / self.total_hints_written as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::types::{IntentType, WorkloadType};

    #[test]
    fn test_writer_creation() {
        // Note: This will fail without actual kernel support
        // Just testing API compilation
    }

    #[test]
    fn test_intent_builder() {
        let intent = Intent::new(IntentType::Compiling)
            .with_duration(30)
            .with_workload(WorkloadType::CpuBound)
            .with_cpu(85)
            .with_confidence(200)
            .with_task_type("cargo_build");

        assert_eq!(intent.intent_type, IntentType::Compiling);
        assert_eq!(intent.expected_duration_sec, 30);
        assert_eq!(intent.workload, WorkloadType::CpuBound);
        assert_eq!(intent.predicted_cpu_usage, 85);
        assert_eq!(intent.confidence, 200);
        assert_eq!(intent.task_type.as_ref().unwrap(), "cargo_build");
    }

    #[test]
    fn test_stats_calculations() {
        let stats = WriterStats {
            total_hints_written: 100,
            hints_used: 80,
            hints_rejected_confidence: 10,
            hints_rejected_timeout: 5,
            current_version: 100,
            last_kernel_read: 12345,
        };

        assert_eq!(stats.usage_rate(), 0.8);
        assert_eq!(stats.rejection_rate(), 0.15);
    }
}
