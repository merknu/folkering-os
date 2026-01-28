//! Safe syscall wrappers for Folkering OS
//!
//! This module provides safe, ergonomic wrappers around the raw syscall interface.

pub mod task;
pub mod io;
pub mod ipc;
pub mod memory;
pub mod system;
pub mod fs;
pub mod synapse;

// Re-export commonly used functions at the sys level
pub use task::{exit, yield_cpu, get_pid, spawn};
pub use io::{read_key, write_char};
pub use ipc::{send, receive, reply};
pub use memory::{shmem_create, shmem_map};
pub use system::{task_list, uptime};

// Re-export Synapse protocol
pub use synapse::{SYNAPSE_TASK_ID, SynapseError, SynapseResult};
