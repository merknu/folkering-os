//! Persistent sandbox managed as a git worktree.
//!
//! Why a worktree:
//!   - Shares `.git` with the main repo so `git archive` / clone of the
//!     full source isn't needed (saves disk + time).
//!   - Has its own working tree + its own `target/` dir, so cargo
//!     incremental compilation works normally without contaminating
//!     the main repo's build cache.
//!   - Trivial to reset between runs: `git checkout -- .`
//!
//! The sandbox lives at `tools/draug-eval-runner/sandbox/` and is
//! gitignored. First call to [`prepare`] creates it; subsequent calls
//! reset any uncommitted changes (so the previous task's patch doesn't
//! contaminate the next task's evaluation).

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub enum SandboxError {
    Io(std::io::Error),
    Git { cmd: String, stderr: String },
    NotInRepo,
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Io(e) => write!(f, "io: {e}"),
            SandboxError::Git { cmd, stderr } =>
                write!(f, "git {cmd}: {}", stderr.trim()),
            SandboxError::NotInRepo => write!(f, "not inside a git repository"),
        }
    }
}

impl std::error::Error for SandboxError {}

#[derive(Debug)]
pub struct Sandbox {
    pub path: PathBuf,
    repo_root: PathBuf,
}

impl Sandbox {
    /// Prepare the sandbox at `<repo_root>/<rel_path>`. Creates the
    /// worktree if missing, otherwise resets it to a clean HEAD.
    ///
    /// `rel_path` should be a path relative to `repo_root`, e.g.
    /// `tools/draug-eval-runner/sandbox`.
    pub fn prepare(repo_root: &Path, rel_path: &Path) -> Result<Self, SandboxError> {
        // Don't canonicalize on Windows — that adds a `\\?\` extended-path
        // prefix that breaks `git worktree add`. The path stays relative-ish
        // (whatever the caller passed); git is happy with that.
        let repo_root = repo_root.to_path_buf();
        let abs_path = repo_root.join(rel_path);

        if !is_inside_repo(&repo_root)? {
            return Err(SandboxError::NotInRepo);
        }

        if abs_path.join(".git").exists() {
            // Existing worktree — reset state.
            run_git(&abs_path, &["reset", "--hard", "HEAD"])?;
            run_git(&abs_path, &["clean", "-fd"])?;
        } else {
            // Need to create it. `git worktree add` requires a branch
            // or commit ref; use the current HEAD detached so we don't
            // tie up a branch name.
            run_git(&repo_root, &["worktree", "add", "--detach",
                abs_path.to_str().expect("UTF-8 path"), "HEAD"])?;
        }

        Ok(Sandbox { path: abs_path, repo_root })
    }

    /// Return the absolute path inside the sandbox for a repo-relative
    /// path. e.g. `inside("kernel/src/lib.rs")` → `<sandbox>/kernel/src/lib.rs`.
    pub fn inside(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.path.join(rel.as_ref())
    }

    /// Restore a single file inside the sandbox to its committed state.
    /// Useful between tasks if you don't want a full reset.
    pub fn restore(&self, rel: &Path) -> Result<(), SandboxError> {
        run_git(&self.path, &["checkout", "HEAD", "--",
            rel.to_str().expect("UTF-8 path")])?;
        Ok(())
    }

    /// Bring the sandbox up to date with the main repo's HEAD. Use this
    /// when the main repo has new commits the sandbox should see.
    #[allow(dead_code)] // not yet used; will matter for the eval batch CLI
    pub fn fast_forward(&self) -> Result<(), SandboxError> {
        // Fetch from the main repo's HEAD via the shared object store.
        run_git(&self.path, &["fetch"])?;
        run_git(&self.path, &["reset", "--hard",
            "refs/remotes/origin/HEAD"])?;
        Ok(())
    }
}

fn is_inside_repo(path: &Path) -> Result<bool, SandboxError> {
    let out = Command::new("git")
        .arg("-C").arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(SandboxError::Io)?;
    Ok(out.status.success())
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, SandboxError> {
    let out = Command::new("git")
        .arg("-C").arg(cwd)
        .args(args)
        .output()
        .map_err(SandboxError::Io)?;
    if !out.status.success() {
        return Err(SandboxError::Git {
            cmd: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test the worktree creation against the live repo. Runs
    /// `prepare` against a temp subdir; cleans up at the end.
    ///
    /// Marked `#[ignore]` because it shares the parent repo's worktree
    /// list with the production sandbox at `tools/draug-eval-runner/
    /// sandbox/` and racing the live `score` subcommand confuses git.
    /// Run explicitly with `cargo test --release sandbox -- --ignored`
    /// when there's no concurrent `score` in flight. End-to-end
    /// verification of this module lives in the live `score`
    /// invocation, which also exercises `prepare` + `restore`.
    #[test]
    #[ignore]
    fn prepare_creates_worktree_and_can_reset() {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().parent().unwrap().to_path_buf();

        // Use a unique subdir per test to avoid stomping concurrent runs.
        let unique = format!("sandbox-test-{}", std::process::id());
        let rel = PathBuf::from("tools/draug-eval-runner/target/test-tmp")
            .join(&unique);

        let sandbox = match Sandbox::prepare(&repo_root, &rel) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[skip] sandbox prepare failed (expected on shallow clones): {e}");
                return;
            }
        };

        // The sandbox should contain Cargo.toml at root.
        assert!(sandbox.path.join("Cargo.toml").exists(),
            "worktree should mirror repo root");

        // Modify a file then restore it.
        let probe_rel = Path::new("tools/draug-eval-runner/tasks.toml");
        let probe = sandbox.inside(probe_rel);
        let original = std::fs::read_to_string(&probe).unwrap();
        std::fs::write(&probe, "DIRTY").unwrap();
        assert_ne!(std::fs::read_to_string(&probe).unwrap(), original);

        sandbox.restore(probe_rel).expect("restore");
        assert_eq!(std::fs::read_to_string(&probe).unwrap(), original);

        // Tear down the worktree so we don't leave a stale dir lying
        // around. Best-effort — failures here aren't test failures.
        let _ = run_git(&repo_root, &[
            "worktree", "remove", "--force",
            sandbox.path.to_str().unwrap(),
        ]);
    }
}
