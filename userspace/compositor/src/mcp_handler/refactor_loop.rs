//! Phase 17 — Autonomous refactor loop.
//!
//! Wires together everything we built in the eval-runner research:
//!
//!   1. `task_store::load()` — pick the next `Pending` refactor
//!      task that survived the last reboot
//!   2. Build the refactor prompt with the model-conditional
//!      caller list (`agent_planner::fetch_callers_summary`,
//!      gated by `codegraph_for_model(model)`)
//!   3. Ship the prompt to the proxy LLM endpoint via the existing
//!      `LlmGenerate` async-op path
//!   4. Apply the LLM's output via the proxy's new
//!      [`CARGO_CHECK`](../../../../../folkering-proxy/src/cargo_check.rs)
//!      command — pass/fail verdict comes back as a status code +
//!      diagnostic excerpt
//!   5. Update `task.attempts` + `task.last_status` and persist
//!      via `task_store::save()` so the next boot picks up where
//!      this attempt left off
//!
//! ## Status today: SCAFFOLDING ONLY
//!
//! The state-transition wiring lives in [`pick_next_refactor_task`]
//! and [`build_refactor_prompt`]. The actual TCP request/response
//! plumbing for `RefactorLlm` and `CargoCheck` async-ops is the
//! follow-up, gated on:
//!
//!   - libfolk gaining a `cargo_check(target_file, source)` syscall
//!     that mirrors the kernel's existing `fbp_patch` path
//!   - boot-test against a live proxy that responds to `CARGO_CHECK`
//!
//! Both are deferred to the session that does the boot-verify, so
//! we don't ship placebo-integration today.

extern crate alloc;

use alloc::string::{String, ToString};

use super::agent_planner::{codegraph_for_model, fetch_callers_summary};
use super::task_store::{RefactorTask, TaskStatus};

/// Find the next refactor task that's eligible for an attempt.
/// Scans in declared order and returns the first `Pending` entry
/// (or any failed entry whose attempt count is below the retry cap).
///
/// Returns `None` when every task has either passed or been retried
/// past the cap. The autonomous loop interprets that as "Phase 17
/// complete" and idles.
pub fn pick_next_refactor_task(tasks: &[RefactorTask]) -> Option<usize> {
    const MAX_ATTEMPTS_PER_TASK: u32 = 3;
    for (idx, t) in tasks.iter().enumerate() {
        match t.last_status {
            TaskStatus::Pending => return Some(idx),
            TaskStatus::Pass | TaskStatus::Skip => continue,
            TaskStatus::FailCompile | TaskStatus::FailCallerCompat => {
                if t.attempts < MAX_ATTEMPTS_PER_TASK {
                    return Some(idx);
                }
            }
        }
    }
    None
}

/// Build the LLM-facing refactor prompt for a single task.
///
/// Parallel to `tools/draug-eval-runner/src/prompt.rs::build` —
/// folds together goal, target metadata, source, constraints, and
/// (model-conditionally) the caller list. The eval-runner has the
/// reference implementation; this is its in-OS sibling.
///
/// `original_source` is the verbatim text of the target fn,
/// extracted via the source-extract syscall (or the kernel-side
/// helper, depending on whose hands we're in). For the scaffolding
/// commit we accept it as a pre-fetched string; the syscall itself
/// is the next-session task.
pub fn build_refactor_prompt(
    task: &RefactorTask,
    original_source: &str,
    model: &str,
) -> String {
    let mut md = String::with_capacity(2048 + original_source.len());

    md.push_str("# Refactor task: ");
    md.push_str(&task.id);
    md.push_str("\n\n");

    md.push_str("## Goal\n\n");
    md.push_str(task.goal.trim());
    md.push_str("\n\n");

    md.push_str("## Target\n\n");
    md.push_str("- Function: `");
    md.push_str(&task.target_fn);
    md.push_str("`\n");
    md.push_str("- File: `");
    md.push_str(&task.target_file);
    md.push_str("`\n\n");

    // Model-conditional caller list. `fetch_callers_summary` already
    // gates on `codegraph_for_model(model)` internally; we surface
    // the gate explicitly here for readability + to skip the syscall
    // entirely when the policy says exclude.
    if codegraph_for_model(model) {
        if let Some(callers) = fetch_callers_summary(&task.target_fn, model) {
            md.push_str("## Blast radius — callers from the static call-graph\n\n");
            md.push_str(&callers);
            md.push('\n');
        }
        // No callers / graph not loaded / proxy unreachable: prompt
        // proceeds without the section. The model still sees the
        // source + goal + constraints.
    }

    md.push_str("## Constraints\n\n");
    md.push_str(
        "- Preserve the public signature of the target fn unless the goal \
         explicitly authorizes changing it. If you must change it, list \
         every caller you would need to update, file by file.\n\
         - Don't introduce new external dependencies.\n\
         - Match the existing surrounding style (no_std discipline, error \
         types, lifetime patterns).\n\
         - Output only the refactored function inside a single fenced \
         ```rust block. No prose outside the block, no `// Before:`/`// After:` \
         comments, no diff format.\n\n",
    );

    md.push_str("## Original source\n\n```rust\n");
    md.push_str(original_source);
    if !original_source.ends_with('\n') { md.push('\n'); }
    md.push_str("```\n");

    md
}

/// Update a task with the verdict from a CARGO_CHECK round-trip and
/// return the new state. The autonomous loop persists this via
/// `task_store::save()` after every attempt.
///
/// Status mapping mirrors the proxy's `CC_STATUS_*` codes from
/// `folkering-proxy/src/cargo_check.rs`. `error_count` and
/// `caller_breakage` are the runner's heuristics for distinguishing
/// "doesn't compile at all" from "compiles but breaks callers".
pub fn record_attempt(
    task: &mut RefactorTask,
    verdict: AttemptVerdict,
) {
    task.attempts = task.attempts.saturating_add(1);
    task.last_status = match verdict {
        AttemptVerdict::Pass             => TaskStatus::Pass,
        AttemptVerdict::FailCompile      => TaskStatus::FailCompile,
        AttemptVerdict::FailCallerCompat => TaskStatus::FailCallerCompat,
        AttemptVerdict::Skip             => TaskStatus::Skip,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptVerdict {
    Pass,
    FailCompile,
    FailCallerCompat,
    Skip,
}

/// Map a proxy CARGO_CHECK status code to an [`AttemptVerdict`].
/// Mirrors `folkering-proxy/src/cargo_check.rs` constants:
///
/// - `0 OK` → Pass
/// - `1 BUILD_FAILED` → FailCompile (we don't yet differentiate
///   target-file errors from caller-file errors here; that takes
///   parsing the stderr excerpt, which is its own task)
/// - others → Skip (infrastructure failure, not an LLM problem)
pub fn verdict_from_cargo_check_status(status: u32) -> AttemptVerdict {
    match status {
        0 => AttemptVerdict::Pass,
        1 => AttemptVerdict::FailCompile,
        _ => AttemptVerdict::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn t(id: &str, attempts: u32, status: TaskStatus) -> RefactorTask {
        RefactorTask {
            id: id.to_string(),
            target_file: "x.rs".to_string(),
            target_fn: "f".to_string(),
            goal: "g".to_string(),
            attempts,
            last_status: status,
        }
    }

    #[test]
    fn pick_picks_first_pending() {
        let tasks = vec![
            t("a", 0, TaskStatus::Pass),
            t("b", 0, TaskStatus::Pending),
            t("c", 0, TaskStatus::Pending),
        ];
        assert_eq!(pick_next_refactor_task(&tasks), Some(1));
    }

    #[test]
    fn pick_retries_failed_under_cap() {
        let tasks = vec![
            t("a", 0, TaskStatus::Pass),
            t("b", 1, TaskStatus::FailCompile),
        ];
        assert_eq!(pick_next_refactor_task(&tasks), Some(1));
    }

    #[test]
    fn pick_skips_failed_at_cap() {
        let tasks = vec![
            t("a", 3, TaskStatus::FailCompile),
            t("b", 3, TaskStatus::FailCallerCompat),
        ];
        assert_eq!(pick_next_refactor_task(&tasks), None);
    }

    #[test]
    fn pick_returns_none_when_all_passed_or_skipped() {
        let tasks = vec![
            t("a", 1, TaskStatus::Pass),
            t("b", 2, TaskStatus::Skip),
        ];
        assert_eq!(pick_next_refactor_task(&tasks), None);
    }

    #[test]
    fn record_attempt_increments_and_sets_status() {
        let mut task = t("a", 0, TaskStatus::Pending);
        record_attempt(&mut task, AttemptVerdict::FailCompile);
        assert_eq!(task.attempts, 1);
        assert_eq!(task.last_status, TaskStatus::FailCompile);
        record_attempt(&mut task, AttemptVerdict::Pass);
        assert_eq!(task.attempts, 2);
        assert_eq!(task.last_status, TaskStatus::Pass);
    }

    #[test]
    fn verdict_mapping() {
        assert_eq!(verdict_from_cargo_check_status(0), AttemptVerdict::Pass);
        assert_eq!(verdict_from_cargo_check_status(1), AttemptVerdict::FailCompile);
        assert_eq!(verdict_from_cargo_check_status(99), AttemptVerdict::Skip);
    }

    #[test]
    fn build_prompt_includes_required_sections() {
        let task = t("01_test", 0, TaskStatus::Pending);
        let src = "fn f() {}";
        let prompt = build_refactor_prompt(&task, src, "qwen2.5-coder:7b");
        assert!(prompt.contains("# Refactor task: 01_test"));
        assert!(prompt.contains("## Goal"));
        assert!(prompt.contains("## Target"));
        assert!(prompt.contains("## Constraints"));
        assert!(prompt.contains("fn f() {}"));
    }

    /// Large-model path: `codegraph_for_model` returns false, so the
    /// blast-radius section never appears. (We don't reach
    /// `fetch_callers_summary` either, which makes this test
    /// deterministic — no syscall round-trip needed.)
    #[test]
    fn build_prompt_skips_blast_for_large_model() {
        let task = t("01_test", 0, TaskStatus::Pending);
        let src = "fn f() {}";
        let prompt = build_refactor_prompt(&task, src, "gemma4:31b-cloud");
        assert!(!prompt.contains("Blast radius"),
            "31b model should get no blast-radius section under by-model policy");
    }
}
