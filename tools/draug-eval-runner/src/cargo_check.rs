//! Run `cargo check` in the workspace that owns a target file, and
//! report whether the build still passes after the patch was applied.
//!
//! Workspace detection is path-prefix based — the repo's structure is
//! stable enough that hard-coding the four roots is simpler and more
//! reliable than walking up looking for `[workspace]` Cargo.tomls.
//!
//! `compile_module`-style tasks have callers in `examples/`, so we
//! pass `--all-targets` to make sure those compile too.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub workspace: String,
    pub exit_code: i32,
    pub elapsed: Duration,
    /// Counted from `^error\b` lines in stderr.
    pub error_count: u32,
    /// Counted from `^warning\b` lines in stderr.
    pub warning_count: u32,
    /// First N lines of stderr, kept for the JSON report so a human can
    /// see what broke without re-running cargo by hand.
    pub stderr_excerpt: String,
}

impl CheckOutcome {
    pub fn passed(&self) -> bool { self.exit_code == 0 }
}

/// Determine which workspace owns `target_file_rel` (a path relative to
/// the repo root) and run `cargo check --all-targets` in that workspace.
/// `sandbox_root` is the absolute path to the worktree.
pub fn check(
    sandbox_root: &Path,
    target_file_rel: &Path,
) -> Result<CheckOutcome, String> {
    let ws = workspace_for(target_file_rel)
        .ok_or_else(|| format!("no workspace mapped for {}", target_file_rel.display()))?;
    let ws_dir: PathBuf = sandbox_root.join(&ws);
    let cargo_args = cargo_args_for(&ws);

    let t0 = Instant::now();
    let out = Command::new("cargo")
        .current_dir(&ws_dir)
        .args(&cargo_args)
        .output()
        .map_err(|e| format!("spawn cargo: {e}"))?;
    let elapsed = t0.elapsed();

    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let (error_count, warning_count) = count_diagnostics(&stderr);
    let excerpt = diagnostic_excerpt(&stderr);
    let exit_code = out.status.code().unwrap_or(-1);

    Ok(CheckOutcome {
        workspace: ws,
        exit_code,
        elapsed,
        error_count,
        warning_count,
        stderr_excerpt: excerpt,
    })
}

/// Map a repo-relative file path to the workspace root that should be
/// the cargo-check cwd. Returns `None` for paths outside known workspaces
/// (top-level scripts, screenshots, etc).
pub fn workspace_for(path: &Path) -> Option<String> {
    let p = path.to_string_lossy().replace('\\', "/");

    // Known workspace roots, longest prefix first so nested paths resolve
    // correctly (e.g. tools/folkering-codegraph beats tools).
    const WORKSPACES: &[&str] = &[
        "tools/draug-eval-runner",
        "tools/folkering-codegraph",
        "tools/a64-encoder",
        "tools/a64-streamer",
        "tools/folk-pack",
        "tools/fbp-rs",
        "tools/proxy",
        "kernel",
        "userspace",
    ];

    WORKSPACES.iter()
        .find(|root| p.starts_with(&format!("{root}/")))
        .map(|s| (*s).to_string())
}

/// Pick cargo args appropriate for the workspace. The kernel + userspace
/// crates are `#![no_std]` and `--all-targets` would try to compile tests
/// against the missing `test` crate (E0463). Host-side tool crates (a64-
/// encoder etc.) have real tests + examples and benefit from --all-targets
/// to also catch caller breakage in `examples/`.
fn cargo_args_for(workspace: &str) -> Vec<&'static str> {
    let no_std_workspaces = ["kernel", "userspace"];
    if no_std_workspaces.contains(&workspace) {
        vec!["check"]
    } else {
        vec!["check", "--all-targets"]
    }
}

fn count_diagnostics(stderr: &str) -> (u32, u32) {
    let mut errors = 0;
    let mut warnings = 0;
    for line in stderr.lines() {
        if line.starts_with("error[") || line.starts_with("error:") || line == "error" {
            errors += 1;
        } else if line.starts_with("warning[") || line.starts_with("warning:") {
            warnings += 1;
        }
    }
    (errors, warnings)
}

fn head(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("\n")
}

/// Pull just the diagnostic-relevant lines out of cargo's stderr.
/// Cargo emits hundreds of `Compiling foo`/`Downloading bar` progress
/// lines before the actual errors; a head-of-file excerpt typically
/// misses every error. We instead capture each `error[…]:` /
/// `warning[…]:` block plus its immediate context (the indented `-->`
/// pointer, source code preview, and `note:`/`help:` follow-ups), up
/// to a cap that keeps the JSON report readable.
fn diagnostic_excerpt(stderr: &str) -> String {
    const MAX_LINES: usize = 120;
    let mut out: Vec<&str> = Vec::new();
    let mut in_block = false;

    for line in stderr.lines() {
        let starts_diag = line.starts_with("error[")
            || line.starts_with("error:")
            || line.starts_with("warning[")
            || line.starts_with("warning:");

        if starts_diag {
            in_block = true;
            out.push(line);
            if out.len() >= MAX_LINES { break; }
            continue;
        }

        if in_block {
            // Continue capturing while the block's body is still going:
            // - `   -->`/`   |` source pointers (indented + leading space)
            // - `note:`/`help:` follow-ups
            // - blank lines (block separators)
            let trimmed = line.trim_start();
            let is_continuation = trimmed.starts_with("-->")
                || trimmed.starts_with("|")
                || trimmed.starts_with("=")
                || trimmed.starts_with("note:")
                || trimmed.starts_with("help:")
                || line.is_empty();
            if is_continuation {
                out.push(line);
                if out.len() >= MAX_LINES { break; }
            } else {
                in_block = false;
            }
        }
    }

    if out.is_empty() {
        // Fall back to the tail — when cargo aborts before any
        // diagnostic, the actual reason is usually in the last lines.
        return stderr.lines().rev().take(40)
            .collect::<Vec<_>>().into_iter().rev()
            .collect::<Vec<_>>().join("\n");
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_for_kernel_path() {
        assert_eq!(
            workspace_for(Path::new("kernel/src/memory/physical.rs")),
            Some("kernel".to_string()),
        );
    }

    #[test]
    fn workspace_for_a64_encoder_paths() {
        assert_eq!(
            workspace_for(Path::new("tools/a64-encoder/src/wasm_lower/stack.rs")),
            Some("tools/a64-encoder".to_string()),
        );
        assert_eq!(
            workspace_for(Path::new("tools/a64-encoder/examples/bench_mlp_ablation.rs")),
            Some("tools/a64-encoder".to_string()),
        );
    }

    #[test]
    fn workspace_for_userspace_path() {
        assert_eq!(
            workspace_for(Path::new("userspace/compositor/src/main.rs")),
            Some("userspace".to_string()),
        );
    }

    #[test]
    fn workspace_for_unknown_path_returns_none() {
        assert_eq!(workspace_for(Path::new("README.md")), None);
        assert_eq!(workspace_for(Path::new("screenshots/x.png")), None);
    }

    #[test]
    fn workspace_for_handles_windows_separators() {
        assert_eq!(
            workspace_for(Path::new(r"kernel\src\memory\physical.rs")),
            Some("kernel".to_string()),
        );
    }

    #[test]
    fn count_diagnostics_recognises_error_and_warning_lines() {
        let stderr = "\
warning: unused variable
error[E0308]: mismatched types
note: required by ...
warning: dead_code
error: aborting due to 1 previous error
";
        let (errs, warns) = count_diagnostics(stderr);
        assert_eq!(errs, 2);
        assert_eq!(warns, 2);
    }
}
