//! WASM Runtime Library
//!
//! Provides WASM/WASI execution environment for Folkering OS applications
//! with Intent Bus integration.

pub mod types;
pub mod host;
pub mod runtime;

pub use types::*;
pub use host::HostState;
pub use runtime::{WasmRuntime, RuntimeStats};
