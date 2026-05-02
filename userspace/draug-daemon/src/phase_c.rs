//! Phase C — autonomous multi-file project authoring.
//!
//! The skill-tree path (L1-L3) is single-file: each task is a
//! standalone `fn`, the LLM writes one fenced ```rust block, the
//! proxy stores it as `draug_latest.rs`. That ceiling is real — the
//! demo "Draug builds apps overnight" needs more than one file at a
//! time. This module wires the FIRST step toward that:
//!
//! 1. Pick a multi-file project from `MULTI_FILE_PROJECTS` (one entry
//!    for now — a tiny calculator crate).
//! 2. Build a prompt that asks the LLM for a Rust crate split into
//!    multiple files using `// === FILE: <path>` markers.
//! 3. Parse the response, extracting each `(path, content)` pair.
//! 4. Write each file via `Project::write` under
//!    `proj/<project_name>/`.
//! 5. Log the resulting project listing via `Project::list` so the
//!    operator can see "Draug authored these files autonomously".
//!
//! What's deliberately NOT here yet:
//! - **Compilation / cargo test.** The proxy's PATCH command writes
//!   one file (`draug_latest.rs`); a multi-file CARGO_CHECK is the
//!   next PR's worth of work. For now this is store-only — Draug
//!   produces the layout, we eyeball it.
//! - **Iterative refinement.** Single-shot generation. A failed
//!   parse just resets and tries again on the next cycle.
//! - **Per-file deletion.** Project's `delete()` is soft (overwrite-
//!   with-empty); real cleanup waits on Issue #100.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libfolk::sys::io::write_str;

use crate::knowledge_hunt::write_dec;
use crate::project::Project;

/// Library of multi-file project specs. Each entry is
/// `(project_id, prompt_body)` — the prompt body is interpolated
/// into the LLM template; the project_id becomes the prefix under
/// `proj/<id>/`. Keep names short and ASCII — Synapse's wire format
/// truncates at 24 bytes (combined with the `proj/<id>/` prefix and
/// the per-file path, names get tight fast).
pub const MULTI_FILE_PROJECTS: &[(&str, &str)] = &[
    (
        "demo-calc",
        "a tiny no_std calculator library. Two files: \
         `src/lib.rs` defines `pub fn add(a: i32, b: i32) -> i32` \
         and `pub fn sub(a: i32, b: i32) -> i32`; \
         `src/tests.rs` is a `#[cfg(test)] mod` with three tests \
         that verify add and sub via assert_eq! including a negative \
         case. Both files compile as parts of a no_std `lib.rs` crate.",
    ),
];

/// Build the LLM prompt for a multi-file project request.
///
/// Asks for a single response with `// === FILE: <path>` markers
/// separating each file. The marker shape is unique enough that
/// regex-free parsing (line-prefix match) is reliable on the kinds
/// of output qwen2.5-coder produces — model-conditional templating
/// can come later if needed.
pub fn build_multi_file_prompt(_project_id: &str, body: &str) -> String {
    let mut p = String::with_capacity(1024);
    p.push_str("Write ");
    p.push_str(body);
    p.push_str("\n\n");
    p.push_str("Format your response as a single Rust source listing ");
    p.push_str("with each file separated by a marker line of the exact form:\n");
    p.push_str("    // === FILE: <relative-path>\n");
    p.push_str("Example:\n");
    p.push_str("    // === FILE: src/lib.rs\n");
    p.push_str("    pub fn foo() {}\n");
    p.push_str("    // === FILE: src/tests.rs\n");
    p.push_str("    #[cfg(test)]\n");
    p.push_str("    mod tests { /* ... */ }\n\n");
    p.push_str("Rules: include only file content between markers, ");
    p.push_str("no explanation outside, no Cargo.toml (we'll generate one), ");
    p.push_str("no top-level `fn main`. Wrap the whole thing in one ");
    p.push_str("```rust fenced block.");
    p
}

/// Parsed file from a multi-file LLM response.
#[derive(Debug, Clone)]
pub struct MultiFileEntry {
    pub path: String,
    pub content: String,
}

/// Split an LLM response into individual files using the
/// `// === FILE: <path>` marker convention. Returns an empty Vec
/// if no markers are present (caller treats that as a parse failure).
///
/// Trims leading/trailing whitespace from each file's content. The
/// path is taken verbatim after `// === FILE:` up to the end of the
/// line, then trimmed.
pub fn parse_multi_file_response(raw: &str) -> Vec<MultiFileEntry> {
    // Strip any outer ```rust fence first — the prompt asks for it
    // wrapped, but we shouldn't trip on the closing ``` showing up
    // inside a file's content (Rust comments wouldn't, but defensive).
    let body = match raw.find("```rust") {
        Some(start) => {
            let after = &raw[start + "```rust".len()..];
            let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
            let inner = &after[body_start..];
            match inner.rfind("```") {
                Some(end) => &inner[..end],
                None => inner,
            }
        }
        None => raw,
    };

    const MARKER: &str = "// === FILE:";
    let mut out: Vec<MultiFileEntry> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_content = String::new();

    for line in body.lines() {
        if let Some(after) = line.trim_start().strip_prefix(MARKER) {
            // Flush previous entry
            if let Some(path) = current_path.take() {
                out.push(MultiFileEntry {
                    path,
                    content: core::mem::take(&mut current_content)
                        .trim()
                        .into(),
                });
            }
            current_path = Some(after.trim().into());
            continue;
        }
        if current_path.is_some() {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }
    if let Some(path) = current_path.take() {
        out.push(MultiFileEntry {
            path,
            content: current_content.trim().into(),
        });
    }
    out
}

/// Persist a parsed multi-file project to Synapse via `Project::write`,
/// then log the resulting `Project::list` for operator visibility.
///
/// Returns the count of files actually written. Soft-fails on any
/// individual write error (logs and continues) — the daemon's
/// autonomous loop is best-effort, not transactional.
pub fn persist_multi_file_project(project_id: &str, files: &[MultiFileEntry]) -> usize {
    let proj = Project::new(project_id);

    write_str("[Phase-C] persisting ");
    write_dec(files.len() as u32);
    write_str(" file(s) under proj/");
    write_str(project_id);
    write_str("/\n");

    let mut written = 0usize;
    for f in files {
        match proj.write(&f.path, f.content.as_bytes()) {
            Ok(_) => {
                written += 1;
                write_str("[Phase-C]   ✓ ");
                write_str(&f.path);
                write_str(" (");
                write_dec(f.content.len() as u32);
                write_str(" bytes)\n");
            }
            Err(_) => {
                write_str("[Phase-C]   ✗ ");
                write_str(&f.path);
                write_str(" — Synapse write failed\n");
            }
        }
    }

    // Log the final state via Project::list — same code path the
    // daemon would use to walk a project later (e.g. to read its own
    // earlier work on the next iteration).
    match proj.list() {
        Ok(listing) => {
            write_str("[Phase-C] project state after write: ");
            write_dec(listing.len() as u32);
            write_str(" entr");
            if listing.len() == 1 { write_str("y"); } else { write_str("ies"); }
            write_str("\n");
            for entry in &listing {
                write_str("[Phase-C]   • ");
                write_str(&entry.name);
                write_str(" (");
                write_dec(entry.size);
                if entry.is_deleted() {
                    write_str(" bytes, tombstone)\n");
                } else {
                    write_str(" bytes)\n");
                }
            }
        }
        Err(_) => {
            write_str("[Phase-C] project listing failed (Synapse unavailable?)\n");
        }
    }

    written
}

/// Format a one-line summary suitable for the autonomous loop's
/// status print after a Phase C cycle completes.
pub fn summary_line(project_id: &str, written: usize, parsed: usize) -> String {
    format!(
        "[Phase-C] {} done: {} of {} file(s) persisted",
        project_id, written, parsed
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn parser_extracts_two_files() {
        let raw = "```rust\n\
                   // === FILE: src/lib.rs\n\
                   pub fn add(a: i32, b: i32) -> i32 { a + b }\n\
                   // === FILE: src/tests.rs\n\
                   #[cfg(test)]\n\
                   mod tests {\n\
                       use super::*;\n\
                       #[test]\n\
                       fn it_adds() { assert_eq!(add(2, 3), 5); }\n\
                   }\n\
                   ```";
        let files = parse_multi_file_response(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/lib.rs".to_string());
        assert!(files[0].content.contains("pub fn add"));
        assert_eq!(files[1].path, "src/tests.rs".to_string());
        assert!(files[1].content.contains("it_adds"));
    }

    #[test]
    fn parser_handles_no_outer_fence() {
        let raw = "// === FILE: a.rs\nA\n// === FILE: b.rs\nB";
        let files = parse_multi_file_response(raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].content, "A".to_string());
        assert_eq!(files[1].content, "B".to_string());
    }

    #[test]
    fn parser_returns_empty_when_no_markers() {
        let raw = "```rust\nfn main() {}\n```";
        let files = parse_multi_file_response(raw);
        assert_eq!(files.len(), 0);
    }
}
