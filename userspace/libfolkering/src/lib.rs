//! LibFolkering - Userspace Library for Folkering OS
//!
//! Provides high-level APIs for interacting with Folkering OS kernel features:
//! - **Brain Bridge**: Context hint communication with kernel scheduler
//! - **IPC**: Inter-process communication (future)
//! - **Shared Memory**: Zero-copy data transfer (future)
//!
//! # Overview
//!
//! LibFolkering is the standard userspace library for Folkering OS applications.
//! It provides safe, ergonomic wrappers around kernel syscalls and shared memory.
//!
//! # Features
//!
//! ## Brain Bridge (Available)
//!
//! Write context hints to the kernel's Neural Scheduler:
//!
//! ```no_run
//! use libfolkering::bridge::{BrainBridgeWriter, Intent, IntentType};
//!
//! let mut writer = BrainBridgeWriter::new()?;
//!
//! writer.write_hint(Intent::new(IntentType::Compiling)
//!     .with_duration(30)
//!     .with_confidence(200)
//! )?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Future Features
//!
//! - **IPC**: Send/receive messages between tasks
//! - **Shared Memory**: Zero-copy bulk data transfer
//! - **Capabilities**: Capability-based security API
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │  Application (Synapse, Neural Scheduler)│
//! │                                          │
//! │  Uses: LibFolkering                     │
//! │    - BrainBridgeWriter                  │
//! │    - IPC (future)                       │
//! │    - SharedMemory (future)              │
//! └─────────────────┬───────────────────────┘
//!                   │
//!                   │ Syscalls + Shared Memory
//!                   │
//! ┌─────────────────▼───────────────────────┐
//! │  Folkering OS Kernel                    │
//! │  - Microkernel                          │
//! │  - Neural Scheduler                     │
//! │  - IPC                                  │
//! └─────────────────────────────────────────┘
//! ```

pub mod bridge;

// Re-export commonly used types at crate root
pub use bridge::{
    BrainBridgeWriter,
    Intent,
    IntentType,
    WorkloadType,
    WriterError,
    WriterStats,
};
