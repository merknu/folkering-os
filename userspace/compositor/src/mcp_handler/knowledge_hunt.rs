//! Re-export shim — actual implementation moved to
//! `draug-daemon::knowledge_hunt` in Phase A.4. Existing call sites
//! (`mcp_handler::knowledge_hunt::*`, `super::knowledge_hunt::*`,
//! `crate::mcp_handler::knowledge_hunt::*`) keep resolving via this
//! re-export until A.5 rewrites them.

pub use draug_daemon::knowledge_hunt::*;
