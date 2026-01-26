//! Brain Bridge - Communication Channel Between Two Brains
//!
//! The Brain Bridge provides a shared memory communication channel between:
//! - **Smart Brain** (Synapse, userspace): Understands user intent, predicts workloads
//! - **Fast Brain** (Neural Scheduler, kernel): Makes real-time scheduling decisions
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │  Smart Brain (Userspace)                │
//! │  - Synapse (knowledge graph)            │
//! │  - Neural Scheduler (phase 1)           │
//! │  - Intent detection from file access    │
//! │                                          │
//! │  Writes to BrainBridge:                 │
//! │    • current_intent = COMPILING         │
//! │    • expected_burst_sec = 30            │
//! │    • confidence = 200 (78%)             │
//! │    • version++                          │
//! └─────────────────┬───────────────────────┘
//!                   │
//!                   │ Shared Memory @ 0x4000_0000_0000
//!                   │ 4KB page, cache-aligned
//!                   │
//! ┌─────────────────▼───────────────────────┐
//! │  Fast Brain (Kernel)                    │
//! │  - Neural Scheduler (integrated)        │
//! │  - Real-time scheduling decisions       │
//! │                                          │
//! │  Reads from BrainBridge:                │
//! │    • Check version != last_read         │
//! │    • Read hints (<1μs via HHDM)         │
//! │    • Apply to scheduler                 │
//! │    • Boost CPU freq, adjust priorities  │
//! └─────────────────────────────────────────┘
//! ```
//!
//! # Performance
//!
//! - **Read latency**: <100ns (L1 cache hit, atomic version check)
//! - **Write latency**: <500ns (userspace write + atomic increment)
//! - **No syscalls**: Direct memory access for reads
//! - **Lock-free**: Atomic version ensures consistency
//!
//! # Communication Pattern
//!
//! ## Context Injection
//!
//! 1. **Synapse detects**: User runs `cargo build`
//! 2. **Pattern recognition**: Heavy CPU-bound compile workload
//! 3. **Prediction**: 30 seconds of high CPU usage
//! 4. **Write hint**: `intent=COMPILING, burst=30s, confidence=80%`
//! 5. **Scheduler reads**: <1μs latency via HHDM
//! 6. **Proactive action**: Boost CPU to 3.5GHz BEFORE load spikes
//!
//! ## Result
//!
//! - Compilation starts at full speed (no ramp-up delay)
//! - Better power management (predictive, not reactive)
//! - Improved user experience (faster builds)
//!
//! # Security
//!
//! - **Read-only kernel access**: Kernel never writes to shared page
//! - **Version stamping**: Detects races and stale data
//! - **Timeout protection**: Ignores hints older than 5 seconds
//! - **Confidence threshold**: Ignores low-quality predictions
//! - **NO_EXECUTE**: Shared page cannot contain executable code
//!
//! # Usage
//!
//! ## Kernel Side (Reading Hints)
//!
//! ```rust
//! use crate::bridge::reader::read_hints;
//!
//! // In scheduler (called every tick)
//! if let Some(snapshot) = read_hints() {
//!     match snapshot.current_intent {
//!         IntentType::Compiling if snapshot.confidence > 200 => {
//!             // Boost CPU frequency
//!             set_cpu_freq(3500); // 3.5GHz
//!         },
//!         _ => {}
//!     }
//! }
//! ```
//!
//! ## Userspace Side (Writing Hints)
//!
//! ```rust
//! use folkering_userspace::bridge::BrainBridgeWriter;
//!
//! let mut writer = BrainBridgeWriter::new()?;
//!
//! writer.write_hint(Intent {
//!     intent_type: IntentType::Compiling,
//!     expected_duration_sec: 30,
//!     confidence: 200,
//!     ..Default::default()
//! });
//! ```

pub mod types;
pub mod reader;

// Re-export commonly used types
pub use types::{
    BrainBridge,
    BrainBridgeSnapshot,
    IntentType,
    WorkloadType,
    BRAIN_BRIDGE_VIRT_ADDR,
    MIN_CONFIDENCE,
    HINT_TIMEOUT_MS,
};

// Re-export reader functions
pub use reader::{
    init as reader_init,
    read_hints,
    stats as reader_stats,
    ReaderStats,
};
