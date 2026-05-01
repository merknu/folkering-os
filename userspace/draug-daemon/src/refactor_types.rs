//! Shared Phase 17 refactor-loop types.
//!
//! Originally lived in `compositor::refactor_types` so the bin
//! (`mcp_handler`) and lib (`compositor::draug`) could share carrier
//! types without forming a lib→bin dependency. Phase A.4 moved them
//! into the daemon crate so the agent code that owns them can travel
//! along with the data structures it manipulates. Compositor still
//! re-exports the module so existing call sites (`compositor::refactor_types::RefactorTask`)
//! keep resolving — that shim disappears in Phase A.5 once the
//! compositor stops referring to these types directly.

use alloc::string::String;

/// One task in the autonomous refactor queue. Matches the eval-runner's
/// fixture shape (`tools/draug-eval-runner/tasks.toml`) so we can seed
/// the in-OS queue from the same data the host harness scores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefactorTask {
    pub id: String,
    pub target_file: String,
    pub target_fn: String,
    pub goal: String,
    pub attempts: u32,
    pub last_status: TaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// Never attempted. Initial state for fresh tasks.
    Pending,
    /// Last attempt produced a refactor that compiled and kept
    /// callers compiling. Locked in.
    Pass,
    /// Last attempt produced code that didn't compile.
    FailCompile,
    /// Last attempt compiled but broke a caller. Strongest signal
    /// for "the LLM changed the signature without updating callers".
    FailCallerCompat,
    /// Skipped because some pre-flight check failed.
    Skip,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending          => "Pending",
            TaskStatus::Pass             => "Pass",
            TaskStatus::FailCompile      => "FailCompile",
            TaskStatus::FailCallerCompat => "FailCallerCompat",
            TaskStatus::Skip             => "Skip",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Pending"          => Some(TaskStatus::Pending),
            "Pass"             => Some(TaskStatus::Pass),
            "FailCompile"      => Some(TaskStatus::FailCompile),
            "FailCallerCompat" => Some(TaskStatus::FailCallerCompat),
            "Skip"             => Some(TaskStatus::Skip),
            _ => None,
        }
    }
}
