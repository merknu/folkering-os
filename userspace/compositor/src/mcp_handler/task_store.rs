//! Re-export shim — actual implementation moved to
//! `draug-daemon::task_store` in Phase A.4. Existing call sites
//! (`mcp_handler::task_store::*`, `super::task_store::*`) keep
//! resolving through this shim until A.5 rewrites them to talk Draug
//! over IPC. Then this file (and its `pub(crate) mod task_store;`
//! line in `mod.rs`) goes away.

pub use draug_daemon::task_store::*;
