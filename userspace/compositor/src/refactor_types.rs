//! Shared Phase 17 refactor-loop types.
//!
//! `DraugDaemon` lives in the lib (`compositor::draug`) but the
//! task_store / refactor_loop logic lives in the bin's
//! `mcp_handler` module. To let the lib hold a `Vec<RefactorTask>`
//! field without forming a lib→bin dependency, the data carriers
//! live here in the lib and the bin's `task_store` re-exports them.

extern crate alloc;

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
