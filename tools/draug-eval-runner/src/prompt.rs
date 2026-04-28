//! Refactor-prompt assembly.
//!
//! A refactor prompt is the LLM-facing artifact step 3 of the post-CodeGraph
//! plan produces. It folds together three things the agent needs to do
//! the work without hallucinating:
//!
//!   1. The original source for the target fn (extracted verbatim from
//!      the tree, layout preserved).
//!   2. The list of callers from CodeGraph — Draug's "blast radius" so
//!      she knows whose interfaces she must not break.
//!   3. The refactor goal + a small set of constraints (no signature
//!      changes without listing every caller, return only one fenced
//!      block, etc).
//!
//! The output is a Markdown-formatted string. Markdown because the LLM
//! reliably parses it, and we get readable diff output when we save the
//! prompt to a file for human inspection.

use folkering_codegraph::CallGraph;
use std::collections::BTreeSet;
use std::path::Path;

use crate::source_extract::{self, ExtractError};

/// Inputs needed to build a refactor prompt for one task.
pub struct RefactorPromptInput<'a> {
    pub task_id: &'a str,
    pub goal: &'a str,
    pub target_fn: &'a str,
    pub target_file: &'a Path,
    /// Loaded call-graph. The builder queries it for the caller list
    /// instead of trusting whatever's in tasks.toml — that way the
    /// prompt always reflects current reality, not stale fixture data.
    pub graph: &'a CallGraph,
    /// When true, suppress the "Blast radius — callers from the static
    /// call-graph" section. Used by the `--no-codegraph` ablation to
    /// measure whether feeding the caller list to the LLM actually
    /// improves refactor quality, or whether the model ignores it.
    pub include_callers: bool,
}

#[derive(Debug)]
pub enum PromptError {
    Extract(ExtractError),
    TargetNotInGraph(String),
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptError::Extract(e) => write!(f, "source extract: {e}"),
            PromptError::TargetNotInGraph(n) =>
                write!(f, "target fn '{n}' not in CodeGraph (rebuild CSR?)"),
        }
    }
}

impl std::error::Error for PromptError {}

pub struct BuiltPrompt {
    pub markdown: String,
    /// Distinct caller files (file granularity, normalised) the prompt
    /// surfaces to the LLM. Caller of the prompt builder uses this to
    /// decide which files to `cargo check` after applying a patch.
    pub caller_files: Vec<String>,
    /// Total caller fns (vertex-granularity) — included in the prompt
    /// header so the LLM knows the blast radius scale.
    pub caller_count: usize,
}

pub fn build(input: &RefactorPromptInput<'_>) -> Result<BuiltPrompt, PromptError> {
    let extracted = source_extract::extract_fn(input.target_file, input.target_fn)
        .map_err(PromptError::Extract)?;

    let target_idx = input.graph.lookup(input.target_fn)
        .ok_or_else(|| PromptError::TargetNotInGraph(input.target_fn.to_string()))?;
    let caller_indices = input.graph.callers_of(target_idx);
    let caller_count = caller_indices.len();

    // Per-caller display: `file::QualifiedName`. Sorted + deduped so
    // the prompt is stable across runs (helps prompt-cache hits). Path
    // separators are normalised (CodeGraph emits `.\foo\bar.rs` on
    // Windows; the LLM reads forward slashes more naturally).
    let caller_lines: BTreeSet<String> = caller_indices.iter()
        .filter_map(|idx| input.graph.names.get(*idx as usize))
        .map(|q| {
            if let Some((file, rest)) = q.split_once("::") {
                format!("{}::{}", normalise(file), rest)
            } else {
                normalise(q)
            }
        })
        .collect();
    let caller_files: BTreeSet<String> = caller_lines.iter()
        .map(|q| {
            let file = q.split("::").next().unwrap_or(q);
            normalise(file)
        })
        .collect();

    let mut md = String::with_capacity(2048 + extracted.source.len());

    md.push_str("# Refactor task: ");
    md.push_str(input.task_id);
    md.push_str("\n\n");

    md.push_str("## Goal\n\n");
    md.push_str(input.goal.trim());
    md.push_str("\n\n");

    md.push_str("## Target\n\n");
    md.push_str("- Function: `");
    md.push_str(input.target_fn);
    md.push_str("`\n");
    md.push_str("- File: `");
    md.push_str(&normalise(&input.target_file.display().to_string()));
    md.push_str("` (lines ");
    md.push_str(&extracted.start_line.to_string());
    md.push('–');
    md.push_str(&extracted.end_line.to_string());
    md.push_str(")\n\n");

    if input.include_callers {
        md.push_str("## Blast radius — callers from the static call-graph\n\n");
        md.push_str(&caller_count.to_string());
        md.push_str(" caller(s) across ");
        md.push_str(&caller_files.len().to_string());
        md.push_str(" file(s):\n\n");
        for c in &caller_lines {
            md.push_str("- `");
            md.push_str(c);
            md.push_str("`\n");
        }
        md.push('\n');
    } else {
        // Ablation mode: no blast-radius section. We still know the
        // count (from CSR) and surface it as a single number so the
        // post-hoc analysis can see whether the model used the absence
        // as a signal to take more risks. Caller-files are still
        // returned in BuiltPrompt for downstream tooling.
        md.push_str("## Blast radius\n\n");
        md.push_str("(call-graph context redacted for this ablation run)\n\n");
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
    md.push_str(&extracted.source);
    if !extracted.source.ends_with('\n') { md.push('\n'); }
    md.push_str("```\n");

    Ok(BuiltPrompt {
        markdown: md,
        caller_files: caller_files.into_iter().collect(),
        caller_count,
    })
}

fn normalise(p: &str) -> String {
    let mut s = p.replace('\\', "/");
    if let Some(stripped) = s.strip_prefix("./") { s = stripped.to_string(); }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn folkering_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().parent().unwrap().to_path_buf()
    }

    #[test]
    fn builds_prompt_for_real_task() {
        let root = folkering_root();
        let graph = folkering_codegraph::build_from_dir(&root).expect("graph");
        let target_file = root.join("kernel/src/memory/physical.rs");
        let input = RefactorPromptInput {
            task_id: "smoke",
            goal: "Add a Layout-style API alongside `alloc_pages` that takes raw bytes + alignment.",
            target_fn: "alloc_pages",
            target_file: &target_file,
            graph: &graph,
            include_callers: true,
        };
        let built = build(&input).expect("build");
        assert!(built.markdown.contains("# Refactor task: smoke"));
        assert!(built.markdown.contains("## Goal"));
        assert!(built.markdown.contains("## Blast radius — callers from the static call-graph"));
        assert!(built.markdown.contains("## Original source"));
        assert!(built.markdown.contains("fn alloc_pages"),
            "prompt must include the original fn body");
        assert!(built.caller_count >= 1, "expected ≥1 caller from real graph");
        assert!(!built.caller_files.is_empty());
    }

    /// Ablation mode: no caller list, but the rest of the prompt
    /// stays intact and `caller_files`/`caller_count` still surface.
    #[test]
    fn ablation_suppresses_caller_list() {
        let root = folkering_root();
        let graph = folkering_codegraph::build_from_dir(&root).expect("graph");
        let target_file = root.join("kernel/src/memory/physical.rs");
        let input = RefactorPromptInput {
            task_id: "smoke-ablation",
            goal: "Add a Layout-style API alongside `alloc_pages`.",
            target_fn: "alloc_pages",
            target_file: &target_file,
            graph: &graph,
            include_callers: false,
        };
        let built = build(&input).expect("build");
        assert!(built.markdown.contains("call-graph context redacted"),
            "ablation mode should advertise itself");
        assert!(!built.markdown.contains("## Blast radius — callers from the"),
            "caller-list section must NOT appear");
        // But the metadata is still available to the runner — the
        // ablation only redacts the LLM-facing prompt, not what we
        // know internally about callers.
        assert!(built.caller_count >= 1);
        assert!(!built.caller_files.is_empty());
    }
}
