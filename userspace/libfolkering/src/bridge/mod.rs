//! Brain Bridge - Userspace Communication with Kernel
//!
//! Provides high-level API for writing context hints to the kernel's Neural Scheduler.
//!
//! # Overview
//!
//! The Brain Bridge is a shared memory page that enables sub-microsecond communication
//! between userspace AI systems (Smart Brain) and the kernel scheduler (Fast Brain).
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │  Smart Brain (Userspace)                │
//! │  - Synapse (knowledge graph)            │
//! │  - Neural Scheduler (phase 1)           │
//! │  - Application intent detection         │
//! │                                          │
//! │  Uses: BrainBridgeWriter                │
//! │    writer.write_hint(intent)            │
//! │    ~500ns latency                       │
//! └─────────────────┬───────────────────────┘
//!                   │
//!                   │ Shared Memory @ 0x4000_0000_0000
//!                   │ 4KB page, atomic version
//!                   │
//! ┌─────────────────▼───────────────────────┐
//! │  Fast Brain (Kernel)                    │
//! │  - Neural Scheduler                     │
//! │  - Real-time decisions                  │
//! │                                          │
//! │  Uses: read_hints()                     │
//! │    <1μs latency via HHDM                │
//! └─────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ## Basic Usage
//!
//! ```no_run
//! use libfolkering::bridge::{BrainBridgeWriter, Intent, IntentType};
//!
//! // Initialize writer (one-time)
//! let mut writer = BrainBridgeWriter::new()?;
//!
//! // Detect compilation starting
//! writer.write_hint(Intent::new(IntentType::Compiling)
//!     .with_duration(30)     // 30 seconds expected
//!     .with_cpu(85)          // 85% CPU usage
//!     .with_confidence(200)  // 78% confident
//! )?;
//!
//! // Kernel will boost CPU frequency proactively
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Advanced Usage
//!
//! ```no_run
//! # use libfolkering::bridge::{BrainBridgeWriter, Intent, IntentType, WorkloadType};
//! # let mut writer = BrainBridgeWriter::new()?;
//! // Detailed hint with semantic label
//! let intent = Intent::new(IntentType::Compiling)
//!     .with_duration(30)
//!     .with_workload(WorkloadType::CpuBound)
//!     .with_cpu(85)
//!     .with_memory(40)
//!     .with_io(10)
//!     .with_confidence(200)
//!     .with_task_type("cargo_build_release");
//!
//! writer.write_hint(intent)?;
//!
//! // Check statistics
//! let stats = writer.stats();
//! println!("Usage rate: {:.1}%", stats.usage_rate() * 100.0);
//! println!("Rejection rate: {:.1}%", stats.rejection_rate() * 100.0);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Monitoring Kernel Feedback
//!
//! ```no_run
//! # use libfolkering::bridge::BrainBridgeWriter;
//! # let writer = BrainBridgeWriter::new()?;
//! // Read kernel feedback
//! let cpu_freq = writer.current_cpu_freq_mhz();
//! let confidence = writer.scheduler_confidence();
//!
//! println!("Current CPU: {} MHz", cpu_freq);
//! println!("Scheduler confidence: {:.2}", confidence);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Integration Examples
//!
//! ## Synapse Integration
//!
//! ```no_run
//! use libfolkering::bridge::{BrainBridgeWriter, Intent, IntentType};
//!
//! struct SynapseContextWriter {
//!     writer: BrainBridgeWriter,
//! }
//!
//! impl SynapseContextWriter {
//!     pub fn on_file_access(&mut self, path: &str) {
//!         // Detect compile patterns
//!         if path.ends_with("Cargo.toml") || path.contains("/target/") {
//!             let _ = self.writer.write_hint(
//!                 Intent::new(IntentType::Compiling)
//!                     .with_duration(30)
//!                     .with_confidence(180)
//!             );
//!         }
//!     }
//! }
//! ```
//!
//! ## Neural Scheduler Integration
//!
//! ```ignore
//! use libfolkering::bridge::{BrainBridgeWriter, Intent};
//!
//! struct NeuralSchedulerWriter {
//!     writer: BrainBridgeWriter,
//! }
//!
//! impl NeuralSchedulerWriter {
//!     pub fn write_prediction(&mut self, prediction: &Prediction) {
//!         let intent = Intent::new(prediction.intent_type)
//!             .with_duration(prediction.duration_sec)
//!             .with_cpu(prediction.cpu_usage)
//!             .with_memory(prediction.memory_usage)
//!             .with_confidence((prediction.confidence * 255.0) as u8);
//!
//!         let _ = self.writer.write_hint(intent);
//!     }
//! }
//! ```

pub mod types;
pub mod writer;

// Re-export common types
pub use types::{Intent, IntentType, WorkloadType, BRAIN_BRIDGE_VIRT_ADDR};
pub use writer::{BrainBridgeWriter, WriterError, WriterStats};
