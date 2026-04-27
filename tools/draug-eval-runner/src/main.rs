//! Draug refactor-flow eval runner.
//!
//! Loads `tasks.toml`, builds a CodeGraph CSR over the monorepo, and verifies
//! that each task's frozen caller count + file set still matches reality.
//!
//! Phase 1 (now): regression check on CodeGraph — if the CSR drifts away
//! from the locked-in expectations, the runner fails and the user decides
//! whether the fixture or the graph is wrong.
//!
//! Phase 2 (lands with Draug refactor flow in step 3): each task additionally
//! gets fed to Draug, the resulting patch is applied to a sandbox copy of
//! the monorepo, `cargo check` is run on the target file + every caller
//! file, and the score is reported. Compile + caller-compat is the headline
//! metric — that's what CodeGraph integration is supposed to enable.
//!
//! Usage:
//!     draug-eval                   # build CSR, verify all tasks
//!     draug-eval --tasks PATH      # use a different tasks.toml
//!     draug-eval --root PATH       # build CSR from PATH instead of CWD

use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

#[derive(Debug, Deserialize)]
struct TasksFile {
    task: Vec<Task>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // `description` + `target_file` aren't checked yet but
                    // are surfaced when step 3's refactor flow lands.
struct Task {
    id: String,
    description: String,
    target_fn: String,
    target_file: String,
    expected_caller_count: u32,
    expected_caller_files: Vec<String>,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut tasks_path = PathBuf::from("tools/draug-eval-runner/tasks.toml");
    let mut root_path = PathBuf::from(".");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tasks" => tasks_path = args.next().expect("--tasks needs a path").into(),
            "--root"  => root_path  = args.next().expect("--root needs a path").into(),
            "-h" | "--help" => {
                println!("draug-eval — verify CodeGraph against frozen task fixtures");
                println!();
                println!("Usage: draug-eval [--tasks tasks.toml] [--root .]");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown arg: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let raw = match std::fs::read_to_string(&tasks_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: read {}: {}", tasks_path.display(), e);
            return ExitCode::from(2);
        }
    };
    let tasks: TasksFile = match toml::from_str(&raw) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: parse {}: {}", tasks_path.display(), e);
            return ExitCode::from(2);
        }
    };

    println!("[draug-eval] {} task(s) loaded from {}", tasks.task.len(), tasks_path.display());
    println!("[draug-eval] building CSR from {} ...", root_path.display());

    let t0 = Instant::now();
    let graph = match folkering_codegraph::build_from_dir(&root_path) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: build_from_dir: {e:?}");
            return ExitCode::from(2);
        }
    };
    let build_ms = t0.elapsed().as_millis();
    println!(
        "[draug-eval] CSR ready ({} vertices, {} edges, {} bytes) in {} ms\n",
        graph.names.len(),
        graph.col_indices.len(),
        graph.csr_bytes(),
        build_ms,
    );

    let mut passed = 0;
    let mut failed = 0;

    for task in &tasks.task {
        match check_task(task, &graph) {
            Ok(()) => {
                println!("[PASS] {} ({} callers across {} files)",
                    task.id, task.expected_caller_count,
                    task.expected_caller_files.len());
                passed += 1;
            }
            Err(reason) => {
                println!("[FAIL] {}", task.id);
                for line in reason.lines() {
                    println!("       {line}");
                }
                failed += 1;
            }
        }
    }

    println!("\n[draug-eval] summary: {passed} passed, {failed} failed");
    if failed > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS }
}

fn check_task(task: &Task, graph: &folkering_codegraph::CallGraph) -> Result<(), String> {
    let target_idx = graph
        .lookup(&task.target_fn)
        .ok_or_else(|| format!("target_fn '{}' not in graph (from {})",
            task.target_fn, task.target_file))?;

    let caller_indices = graph.callers_of(target_idx);
    let actual_count = caller_indices.len() as u32;

    if actual_count != task.expected_caller_count {
        return Err(format!(
            "caller count drift: expected {}, got {}",
            task.expected_caller_count, actual_count,
        ));
    }

    let actual_files: BTreeSet<String> = caller_indices.iter()
        .filter_map(|i| graph.names.get(*i as usize))
        .map(|qualified| qualified_to_file(qualified))
        .collect();

    let expected_files: BTreeSet<String> = task.expected_caller_files.iter()
        .map(|s| normalize_path(s))
        .collect();

    if actual_files != expected_files {
        let missing: Vec<&str> = expected_files.difference(&actual_files)
            .map(|s| s.as_str()).collect();
        let extra: Vec<&str> = actual_files.difference(&expected_files)
            .map(|s| s.as_str()).collect();
        let mut msg = String::from("caller-file set drift\n");
        if !missing.is_empty() {
            msg.push_str(&format!("  missing (expected, not found):\n"));
            for f in &missing { msg.push_str(&format!("    - {f}\n")); }
        }
        if !extra.is_empty() {
            msg.push_str("  extra (found, not expected):\n");
            for f in &extra { msg.push_str(&format!("    + {f}\n")); }
        }
        return Err(msg);
    }

    Ok(())
}

/// Pull the file path out of a qualified vertex name like
/// `.\tools\a64-encoder\src\wasm_lower\call.rs::Lowerer::lower_call`
/// → `tools/a64-encoder/src/wasm_lower/call.rs`.
fn qualified_to_file(qualified: &str) -> String {
    let path = qualified.split("::").next().unwrap_or(qualified);
    normalize_path(&path.to_string())
}

fn normalize_path(p: &str) -> String {
    let mut s = p.to_string();
    s = s.replace('\\', "/");
    if let Some(stripped) = s.strip_prefix("./") { s = stripped.to_string(); }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_leading_dot_and_swaps_separators() {
        assert_eq!(normalize_path(r".\tools\a64-encoder\src\wasm_lower\call.rs"),
            "tools/a64-encoder/src/wasm_lower/call.rs");
        assert_eq!(normalize_path("tools/foo.rs"), "tools/foo.rs");
    }

    #[test]
    fn qualified_to_file_extracts_file_part() {
        assert_eq!(
            qualified_to_file(r".\tools\a64-encoder\src\wasm_lower\call.rs::Lowerer::lower_call"),
            "tools/a64-encoder/src/wasm_lower/call.rs",
        );
    }

    /// Real fixture parses cleanly. Catches `tasks.toml` schema regressions.
    #[test]
    fn fixture_parses() {
        let path = Path::new("tasks.toml");
        if !path.exists() { return; } // skip when not run from crate dir
        let raw = std::fs::read_to_string(path).unwrap();
        let parsed: TasksFile = toml::from_str(&raw).unwrap();
        assert!(!parsed.task.is_empty(), "tasks.toml must have at least one task");
        for t in &parsed.task {
            assert!(!t.id.is_empty());
            assert!(!t.target_fn.is_empty());
            assert_eq!(t.expected_caller_files.iter().filter(|s| s.is_empty()).count(), 0);
        }
    }
}
