//! Synapse-VFS-backed persistent store for Draug refactor tasks.
//!
//! Replaces the static `REFACTOR_TASKS` / `COMPLEX_TASKS` arrays
//! with a list of [`RefactorTask`] entries that survive across
//! reboots. Each task carries id + target fn/file + goal +
//! attempts + last status, so the autonomous loop can pick up
//! where it left off without re-trying tasks it already settled.
//!
//! Storage layout:
//! - One file in Synapse VFS at `draug/refactor_tasks.txt`
//! - Custom line-based format (no JSON / TOML deps in compositor's
//!   `no_std` target). Format below.
//!
//! ```text
//! v1
//! N
//! <task block 1>
//! <task block 2>
//! ...
//! ```
//!
//! Each task block is exactly six lines:
//! ```text
//! ID: <task_id>
//! TARGET_FILE: <repo-relative path>
//! TARGET_FN: <fn-name>
//! ATTEMPTS: <integer>
//! LAST_STATUS: <Pending|Pass|FailCompile|FailCallerCompat|Skip>
//! GOAL: <single-line goal text — newlines escaped as \n>
//! ```
//!
//! Tasks are loaded once at boot via [`load`] and saved after every
//! attempt via [`save`]. Failures are tolerated: a missing file
//! returns `Ok(empty)`, a parse error returns `Err`.
//!
//! This is the compile-time scaffolding. The autonomous loop's
//! actual call sites (`tick_idle` selecting the next pending
//! refactor) are stubbed in `draug_async.rs` and will be wired
//! once the proxy CARGO_CHECK command lands and we boot-test.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

const STORE_PATH: &str = "draug/refactor_tasks.txt";
const FORMAT_VERSION: &str = "v1";

/// One task in the autonomous refactor queue. Fixture data carried
/// in static arrays before; persisted in Synapse VFS now so the
/// autonomous loop survives reboots.
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
    /// Last attempt produced code that didn't compile (any cargo
    /// check error in the target file itself).
    FailCompile,
    /// Last attempt compiled but broke a caller. Strongest signal
    /// for "the LLM changed the signature without updating callers".
    FailCallerCompat,
    /// Skipped because some pre-flight check failed (target fn not
    /// in the graph, source extraction failed, etc).
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

#[derive(Debug)]
pub enum StoreError {
    Synapse(libfolk::sys::synapse::SynapseError),
    BadFormat(String),
    UnsupportedVersion(String),
}

impl core::fmt::Display for StoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StoreError::Synapse(e) => {
                f.write_str("synapse: ")?;
                f.write_fmt(format_args!("{e:?}"))
            }
            StoreError::BadFormat(s) => {
                f.write_str("bad format: ")?;
                f.write_str(s)
            }
            StoreError::UnsupportedVersion(v) => {
                f.write_str("unsupported version: ")?;
                f.write_str(v)
            }
        }
    }
}

/// Load the task list from Synapse VFS. Missing-file is the cold-
/// boot case — returns an empty vec, not an error, so the caller
/// can seed defaults from the static fixtures.
pub fn load() -> Result<Vec<RefactorTask>, StoreError> {
    let resp = match libfolk::sys::synapse::read_file_shmem(STORE_PATH) {
        Ok(r) => r,
        Err(libfolk::sys::synapse::SynapseError::NotFound) => return Ok(Vec::new()),
        Err(e) => return Err(StoreError::Synapse(e)),
    };

    if resp.size == 0 {
        // Empty file — same as missing.
        let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
        return Ok(Vec::new());
    }

    // Map the shmem region into our address space, copy bytes out,
    // then unmap+destroy. Same pattern as DraugDaemon::restore_state.
    const VADDR: usize = 0x3000_5000;
    if libfolk::sys::shmem_map(resp.shmem_handle, VADDR).is_err() {
        let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
        return Err(StoreError::BadFormat("shmem_map failed".to_string()));
    }
    let raw = unsafe {
        core::slice::from_raw_parts(VADDR as *const u8, resp.size as usize)
    };
    let parsed = match core::str::from_utf8(raw) {
        Ok(s) => parse(s),
        Err(_) => Err(StoreError::BadFormat("file contents not UTF-8".to_string())),
    };
    let _ = libfolk::sys::shmem_unmap(resp.shmem_handle, VADDR);
    let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
    parsed
}

/// Persist the task list back to Synapse VFS. Caller is responsible
/// for serialising concurrent writers — DraugDaemon owns the loop
/// in compositor so this is single-threaded by construction.
pub fn save(tasks: &[RefactorTask]) -> Result<(), StoreError> {
    let serialised = serialise(tasks);
    libfolk::sys::synapse::write_file(STORE_PATH, serialised.as_bytes())
        .map_err(StoreError::Synapse)
}

/// Seed the store from a static (id, target_file, target_fn, goal)
/// list when there's no persisted data yet. Idempotent: tasks that
/// already exist by id are kept unchanged so attempt counts don't
/// reset on reboot.
pub fn seed_or_merge(
    existing: &[RefactorTask],
    fixtures: &[(&str, &str, &str, &str)],
) -> Vec<RefactorTask> {
    let mut out: Vec<RefactorTask> = existing.to_vec();
    for &(id, target_file, target_fn, goal) in fixtures {
        if !out.iter().any(|t| t.id == id) {
            out.push(RefactorTask {
                id: id.to_string(),
                target_file: target_file.to_string(),
                target_fn: target_fn.to_string(),
                goal: goal.to_string(),
                attempts: 0,
                last_status: TaskStatus::Pending,
            });
        }
    }
    out
}

// ── Serialisation ───────────────────────────────────────────────────

fn serialise(tasks: &[RefactorTask]) -> String {
    let mut out = String::with_capacity(64 + tasks.len() * 256);
    out.push_str(FORMAT_VERSION);
    out.push('\n');
    push_u32(&mut out, tasks.len() as u32);
    out.push('\n');
    for t in tasks {
        out.push_str("ID: ");           out.push_str(&t.id);          out.push('\n');
        out.push_str("TARGET_FILE: ");  out.push_str(&t.target_file); out.push('\n');
        out.push_str("TARGET_FN: ");    out.push_str(&t.target_fn);   out.push('\n');
        out.push_str("ATTEMPTS: ");     push_u32(&mut out, t.attempts); out.push('\n');
        out.push_str("LAST_STATUS: ");  out.push_str(t.last_status.as_str()); out.push('\n');
        out.push_str("GOAL: ");         push_escaped_line(&mut out, &t.goal); out.push('\n');
    }
    out
}

fn parse(raw: &str) -> Result<Vec<RefactorTask>, StoreError> {
    let mut lines = raw.lines();
    let header = lines.next().ok_or_else(|| StoreError::BadFormat("empty file".into()))?;
    if header.trim() != FORMAT_VERSION {
        return Err(StoreError::UnsupportedVersion(header.trim().to_string()));
    }
    let count_line = lines.next().ok_or_else(|| StoreError::BadFormat("missing count".into()))?;
    let count: u32 = count_line.trim().parse()
        .map_err(|_| StoreError::BadFormat("count not a number".into()))?;

    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let id           = require_field(&mut lines, "ID")?;
        let target_file  = require_field(&mut lines, "TARGET_FILE")?;
        let target_fn    = require_field(&mut lines, "TARGET_FN")?;
        let attempts_str = require_field(&mut lines, "ATTEMPTS")?;
        let attempts: u32 = attempts_str.trim().parse()
            .map_err(|_| StoreError::BadFormat("attempts not a number".into()))?;
        let status_str   = require_field(&mut lines, "LAST_STATUS")?;
        let last_status  = TaskStatus::parse(status_str.trim()).ok_or_else(||
            StoreError::BadFormat({
                let mut s = String::from("unknown status: ");
                s.push_str(status_str.trim());
                s
            }))?;
        let goal_raw     = require_field(&mut lines, "GOAL")?;
        let goal = unescape_line(&goal_raw);
        out.push(RefactorTask {
            id, target_file, target_fn, goal, attempts, last_status,
        });
    }
    Ok(out)
}

fn require_field(
    lines: &mut core::str::Lines<'_>,
    key: &str,
) -> Result<String, StoreError> {
    let line = lines.next().ok_or_else(|| {
        let mut s = String::from("EOF before field ");
        s.push_str(key);
        StoreError::BadFormat(s)
    })?;
    let prefix = {
        let mut p = String::with_capacity(key.len() + 2);
        p.push_str(key);
        p.push_str(": ");
        p
    };
    if !line.starts_with(&prefix) {
        let mut msg = String::with_capacity(64);
        msg.push_str("expected '");
        msg.push_str(&prefix);
        msg.push_str("' got '");
        msg.push_str(&line[..line.len().min(40)]);
        msg.push('\'');
        return Err(StoreError::BadFormat(msg));
    }
    Ok(line[prefix.len()..].to_string())
}

fn push_u32(out: &mut String, mut v: u32) {
    if v == 0 { out.push('0'); return; }
    let mut buf = [0u8; 10];
    let mut i = 0;
    while v > 0 {
        buf[i] = (v % 10) as u8 + b'0';
        v /= 10;
        i += 1;
    }
    while i > 0 { i -= 1; out.push(buf[i] as char); }
}

/// Goal text may contain newlines; we escape them so the line-based
/// parser doesn't get confused. `\n` ↔ literal newline; `\\` ↔
/// literal backslash. Other characters pass through unchanged.
fn push_escaped_line(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\n' => { out.push('\\'); out.push('n'); }
            '\\' => { out.push('\\'); out.push('\\'); }
            other => out.push(other),
        }
    }
}

fn unescape_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek().copied() {
                Some('n')  => { chars.next(); out.push('\n'); }
                Some('\\') => { chars.next(); out.push('\\'); }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tasks() -> Vec<RefactorTask> {
        alloc::vec![
            RefactorTask {
                id: "01_pop_i32_slot".into(),
                target_file: "tools/a64-encoder/src/wasm_lower/stack.rs".into(),
                target_fn: "pop_i32_slot".into(),
                goal: "Refactor pop_i32_slot to return Result instead of panicking.".into(),
                attempts: 2,
                last_status: TaskStatus::FailCallerCompat,
            },
            RefactorTask {
                id: "03_alloc_pages".into(),
                target_file: "kernel/src/memory/physical.rs".into(),
                target_fn: "alloc_pages".into(),
                // Multi-line goal — exercises the escape round-trip.
                goal: "Add a Layout-style API.\nKeep the original signature.".into(),
                attempts: 0,
                last_status: TaskStatus::Pending,
            },
        ]
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let original = sample_tasks();
        let serialised = serialise(&original);
        let parsed = parse(&serialised).expect("parse");
        assert_eq!(parsed, original);
    }

    #[test]
    fn parse_rejects_unknown_version() {
        let err = parse("v9\n0\n").unwrap_err();
        match err {
            StoreError::UnsupportedVersion(v) => assert_eq!(v, "v9"),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_truncated_block() {
        let truncated = "v1\n1\nID: foo\nTARGET_FILE: bar\n";
        assert!(parse(truncated).is_err());
    }

    #[test]
    fn seed_or_merge_idempotent_for_known_ids() {
        let existing = alloc::vec![
            RefactorTask {
                id: "01".into(),
                target_file: "a.rs".into(),
                target_fn: "f".into(),
                goal: "g".into(),
                attempts: 5,
                last_status: TaskStatus::FailCompile,
            },
        ];
        let fixtures = &[
            ("01", "a.rs", "f", "should NOT overwrite"),
            ("02", "b.rs", "g", "should be added fresh"),
        ];
        let merged = seed_or_merge(&existing, fixtures);
        assert_eq!(merged.len(), 2);
        // Existing task's attempts + status preserved.
        let t1 = merged.iter().find(|t| t.id == "01").unwrap();
        assert_eq!(t1.attempts, 5);
        assert_eq!(t1.last_status, TaskStatus::FailCompile);
        assert_eq!(t1.goal, "g"); // not the new fixture text
        // New task seeded with default state.
        let t2 = merged.iter().find(|t| t.id == "02").unwrap();
        assert_eq!(t2.attempts, 0);
        assert_eq!(t2.last_status, TaskStatus::Pending);
    }

    #[test]
    fn status_round_trips_via_string() {
        for s in [TaskStatus::Pending, TaskStatus::Pass,
                  TaskStatus::FailCompile, TaskStatus::FailCallerCompat,
                  TaskStatus::Skip] {
            assert_eq!(TaskStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(TaskStatus::parse("NopeBad"), None);
    }

    #[test]
    fn escape_round_trip_handles_backslash_and_newline() {
        let s = "line1\nline2\\with\\backslash";
        let mut buf = String::new();
        push_escaped_line(&mut buf, s);
        assert!(!buf.contains('\n'), "escaped form must be single-line");
        let restored = unescape_line(&buf);
        assert_eq!(restored, s);
    }
}
