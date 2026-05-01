//! Re-export shim: the real types now live in the `draug-daemon` crate
//! (Phase A.4). Existing call sites — both `crate::refactor_types::*`
//! from inside `compositor` and `compositor::refactor_types::*` from
//! the bin's `mcp_handler` — keep resolving through this shim until
//! Phase A.5 rewrites them to talk Draug over IPC instead of holding
//! the carrier types directly. Once that rewrite lands, this file
//! and its `pub mod refactor_types;` line in `lib.rs` go away.

pub use draug_daemon::refactor_types::*;
