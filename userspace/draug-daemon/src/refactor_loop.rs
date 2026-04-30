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

use alloc::string::{String, ToString};

use crate::agent_planner::{codegraph_for_model, fetch_callers_summary};
use crate::task_store::{RefactorTask, TaskStatus};

/// Default fixture seed for [`task_store::seed_or_merge`]. Mirrors
/// `tools/draug-eval-runner/tasks.toml` — five real refactor targets
/// the eval harness has scored multiple times. When the autonomous
/// loop boots into a clean Synapse VFS (no `draug/refactor_tasks.txt`)
/// it seeds the queue from this list so Draug always has work
/// available, without re-implementing the toml parser inside the
/// no_std compositor.
///
/// Tuple shape: `(id, target_file, target_fn, goal)`.
///
/// Add new entries by mirroring the eval-runner fixture; the runner
/// remains the source of truth, this is the in-OS shadow.
pub const REFACTOR_FIXTURES: &[(&str, &str, &str, &str)] = &[
    (
        "01_pop_i32_slot",
        "tools/a64-encoder/src/wasm_lower/stack.rs",
        "pop_i32_slot",
        "Refactor `pop_i32_slot` to return `Result<Reg, LowerError>` instead \
         of panicking on stack underflow. 29 call sites across 8 files in \
         the wasm_lower module — all callers need to handle the new Result.",
    ),
    (
        "02_maybe_bounds_check",
        "tools/a64-encoder/src/wasm_lower/memory.rs",
        "maybe_bounds_check",
        "Extract the elision-decision logic in `maybe_bounds_check` into a \
         separate `BoundsCheckDecision` enum + helper, so call sites can \
         match on intent rather than chase a boolean. 10 call sites across \
         memory.rs + simd.rs.",
    ),
    (
        "03_alloc_pages",
        "kernel/src/memory/physical.rs",
        "alloc_pages",
        "The kernel buddy allocator's `alloc_pages` accepts an `order` \
         (log2 page count). Add a `Layout`-style API alongside that takes \
         raw byte size + alignment and computes order internally. 4 call \
         sites all in physical.rs.",
    ),
    (
        "04_compile_module",
        "tools/a64-encoder/src/wasm_lower/module_lower.rs",
        "compile_module",
        "`compile_module` is the host-side WASM-to-AArch64 entry point. \
         Add an explicit `CompileOptions` struct (currently positional \
         args). 5 call sites across jit_cache + 3 example bins — examples \
         must keep working.",
    ),
    (
        "05_push_dec",
        "kernel/src/net/tcp_shell.rs",
        "push_dec",
        "`push_dec` formats a u32 into a String for the kernel TCP shell. \
         Refactor to use the kernel's existing `core::fmt::Write` machinery \
         instead of manual digit pushing. 12 call sites all in tcp_shell.rs.",
    ),
];

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

/// Build the `CARGO_CHECK <target>\n<len>\n<source>` request frame
/// the proxy expects on the wire. Same shape as `start_patch_request`
/// in `draug_async.rs` — pulled out here so the formatting is unit-
/// testable without booting the OS.
///
/// `target_file` is the repo-relative path (e.g.
/// `kernel/src/memory/physical.rs`). `source` is the candidate Rust
/// text the proxy will overwrite the file with before running
/// `cargo check`.
pub fn build_cargo_check_request(target_file: &str, source: &str) -> alloc::vec::Vec<u8> {
    let mut req = alloc::vec::Vec::with_capacity(target_file.len() + source.len() + 32);
    req.extend_from_slice(b"CARGO_CHECK ");
    req.extend_from_slice(target_file.as_bytes());
    req.push(b'\n');
    push_decimal(&mut req, source.len());
    req.push(b'\n');
    req.extend_from_slice(source.as_bytes());
    req
}

fn push_decimal(out: &mut alloc::vec::Vec<u8>, mut v: usize) {
    if v == 0 { out.push(b'0'); return; }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while v > 0 {
        buf[i] = (v % 10) as u8 + b'0';
        v /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        out.push(buf[i]);
    }
}

/// Parse the 8-byte `[u32 status LE][u32 output_len LE]` reply header
/// the proxy emits at the front of a CARGO_CHECK response. Returns
/// `None` when the buffer is too short. `output_len` is what the
/// proxy claims the body length is — the caller is responsible for
/// reconciling that with the actual bytes it has buffered.
pub fn parse_cargo_check_header(reply: &[u8]) -> Option<(u32, u32)> {
    if reply.len() < 8 { return None; }
    let status = u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]);
    let len    = u32::from_le_bytes([reply[4], reply[5], reply[6], reply[7]]);
    Some((status, len))
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

    #[test]
    fn fixtures_have_unique_ids() {
        let mut ids: Vec<&str> = REFACTOR_FIXTURES.iter().map(|(id, _, _, _)| *id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), REFACTOR_FIXTURES.len(),
            "REFACTOR_FIXTURES has duplicate ids — task_store::seed_or_merge \
             would silently keep only the first instance");
    }

    #[test]
    fn fixtures_target_files_are_repo_relative() {
        for (id, target_file, _, _) in REFACTOR_FIXTURES {
            assert!(!target_file.starts_with('/'),
                "fixture `{id}`: target_file `{target_file}` is absolute");
            assert!(!target_file.contains(".."),
                "fixture `{id}`: target_file `{target_file}` has parent traversal");
            assert!(target_file.ends_with(".rs"),
                "fixture `{id}`: target_file `{target_file}` is not a .rs file");
        }
    }

    #[test]
    fn cargo_check_request_frame_matches_proxy_wire_format() {
        let req = build_cargo_check_request("kernel/src/foo.rs", "fn x() {}");
        // Wire layout: `CARGO_CHECK <target>\n<len>\n<source>`
        let s = core::str::from_utf8(&req).unwrap();
        assert!(s.starts_with("CARGO_CHECK kernel/src/foo.rs\n"));
        assert!(s.contains("\n9\n")); // 9 = len("fn x() {}")
        assert!(s.ends_with("fn x() {}"));
    }

    #[test]
    fn cargo_check_header_parses_status_and_len() {
        // status=1 (BUILD_FAILED), output_len=42, both little-endian
        let reply = [
            0x01, 0x00, 0x00, 0x00,
            0x2A, 0x00, 0x00, 0x00,
            b'p', b'a', b'd',
        ];
        let (status, len) = parse_cargo_check_header(&reply).unwrap();
        assert_eq!(status, 1);
        assert_eq!(len, 42);
    }

    #[test]
    fn cargo_check_header_rejects_short_buffer() {
        let short = [0u8; 4];
        assert!(parse_cargo_check_header(&short).is_none());
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
