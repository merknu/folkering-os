//! Re-export shim — the `DraugDaemon` struct, friction sensor, hibernation
//! state machine, and all helpers moved to `draug-daemon::draug` in
//! Phase A.4. External call sites (`compositor::draug::DraugDaemon`,
//! `compositor::draug::FRICTION_QUICK_CLOSE`, …) keep resolving via
//! this re-export until A.5 rewrites them to talk Draug over IPC.

pub use draug_daemon::draug::*;
