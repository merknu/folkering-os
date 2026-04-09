//! WASM Host API — Categorized host function registration
//!
//! Each sub-module registers its functions on a wasmi Linker<HostState>.
//! Called from `wasm_runtime::register_host_functions()`.

// Re-export parent types so sub-modules can use `super::` to access them
pub(in crate::wasm_runtime) use super::{
    HostState, DrawCmd, TextCmd, LineCmd, CircleCmd, PixelBlit,
    PendingAssetRequest, SURFACE_OFFSET,
    execute_shadow_test,
};

pub mod graphics;
pub mod network;
pub mod ai;
pub mod vfs;
pub mod system;
