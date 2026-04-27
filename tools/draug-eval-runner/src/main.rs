//! Draug refactor-flow eval runner.
//!
//! Three subcommands:
//!
//!   * `verify`  — load `tasks.toml`, build a fresh CSR, confirm every
//!                 task's frozen caller count + file set still matches.
//!                 Catches CodeGraph regressions. Default subcommand.
//!
//!   * `prompt <task-id>`  — assemble the LLM-facing refactor prompt for
//!                 a single task and write it to `output/<id>/prompt.md`.
//!                 No LLM call. Useful for inspecting / hand-editing the
//!                 prompt before paying for tokens.
//!
//!   * `refactor <task-id>` — assemble the prompt, ship it to the
//!                 host-side `folkering-proxy` LLM endpoint, save the
//!                 response. Pulls a code block out of the response if
//!                 there is one.
//!
//! Verifying the actual refactor (apply patch + cargo check + caller-
//! compat scoring) is Phase 2B, deferred to its own PR. This crate's
//! README documents the full plan.

mod apply;
mod cargo_check;
mod prompt;
mod proxy;
mod sandbox;
mod source_extract;

use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
struct TasksFile {
    task: Vec<Task>,
}

#[derive(Debug, Deserialize)]
struct Task {
    id: String,
    description: String,
    target_fn: String,
    target_file: String,
    expected_caller_count: u32,
    expected_caller_files: Vec<String>,
}

#[derive(Debug)]
struct GlobalArgs {
    tasks_path: PathBuf,
    root_path: PathBuf,
    output_dir: PathBuf,
    proxy_host: String,
    proxy_port: u16,
    /// LLM model used by the `refactor` subcommand. The L1 default in
    /// Draug is qwen2.5-coder:7b, which is local + fast — keep that
    /// here so the eval doesn't burn cloud tokens on every run.
    llm_model: String,
}

impl GlobalArgs {
    fn defaults() -> Self {
        GlobalArgs {
            tasks_path: PathBuf::from("tools/draug-eval-runner/tasks.toml"),
            root_path: PathBuf::from("."),
            output_dir: PathBuf::from("tools/draug-eval-runner/output"),
            proxy_host: proxy::DEFAULT_HOST.to_string(),
            proxy_port: proxy::DEFAULT_PORT,
            llm_model: "qwen2.5-coder:7b".to_string(),
        }
    }
}

fn main() -> ExitCode {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let mut g = GlobalArgs::defaults();
    let mut subcommand: Option<String> = None;
    let mut subcommand_args: Vec<String> = Vec::new();

    let mut i = 0;
    while i < raw_args.len() {
        let a = &raw_args[i];
        match a.as_str() {
            "-h" | "--help" => return print_help(),
            "--tasks" => { g.tasks_path = next_or_die(&raw_args, &mut i, "--tasks").into(); }
            "--root"  => { g.root_path  = next_or_die(&raw_args, &mut i, "--root").into(); }
            "--output" => { g.output_dir = next_or_die(&raw_args, &mut i, "--output").into(); }
            "--proxy-host" => { g.proxy_host = next_or_die(&raw_args, &mut i, "--proxy-host"); }
            "--proxy-port" => {
                let s = next_or_die(&raw_args, &mut i, "--proxy-port");
                g.proxy_port = match s.parse() {
                    Ok(p) => p,
                    Err(_) => { eprintln!("--proxy-port: not a u16: {s}"); return ExitCode::from(2); }
                };
            }
            "--model" => { g.llm_model = next_or_die(&raw_args, &mut i, "--model"); }
            other if subcommand.is_none() => {
                subcommand = Some(other.to_string());
            }
            other => {
                subcommand_args.push(other.to_string());
            }
        }
        i += 1;
    }

    let cmd = subcommand.as_deref().unwrap_or("verify");
    match cmd {
        "verify"   => cmd_verify(&g),
        "prompt"   => cmd_prompt(&g, &subcommand_args),
        "refactor" => cmd_refactor(&g, &subcommand_args),
        "score"    => cmd_score(&g, &subcommand_args),
        "eval"     => cmd_eval(&g, &subcommand_args),
        other => {
            eprintln!("unknown subcommand: {other}");
            print_help();
            ExitCode::from(2)
        }
    }
}

fn next_or_die(args: &[String], i: &mut usize, flag: &str) -> String {
    *i += 1;
    if *i >= args.len() {
        eprintln!("flag '{flag}' needs a value");
        std::process::exit(2);
    }
    args[*i].clone()
}

fn print_help() -> ExitCode {
    println!("draug-eval — refactor-flow evaluation harness for Draug");
    println!();
    println!("USAGE:");
    println!("  draug-eval [GLOBAL FLAGS] <subcommand> [ARGS]");
    println!();
    println!("SUBCOMMANDS:");
    println!("  verify                  Verify CSR against frozen task fixtures (default)");
    println!("  prompt <task-id>        Build refactor prompt → output/<id>/prompt.md");
    println!("  refactor <task-id>      Build prompt + LLM call → output/<id>/refactor.md");
    println!("  score <task-id>         Apply existing refactor.md to sandbox + cargo check");
    println!("  eval [task-id|--all]    Refactor + score in one go; --all runs every task");
    println!();
    println!("GLOBAL FLAGS:");
    println!("  --tasks PATH            tasks.toml location");
    println!("  --root PATH             repo root for CSR build");
    println!("  --output DIR            where prompt/refactor results land");
    println!("  --proxy-host HOST       folkering-proxy address (default 127.0.0.1)");
    println!("  --proxy-port PORT       (default 14711)");
    println!("  --model NAME            LLM model name (default qwen2.5-coder:7b)");
    ExitCode::SUCCESS
}

// ── verify ──────────────────────────────────────────────────────────

fn cmd_verify(g: &GlobalArgs) -> ExitCode {
    let tasks = match load_tasks(&g.tasks_path) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let graph = match build_graph(&g.root_path) {
        Ok(g) => g,
        Err(code) => return code,
    };
    println!(
        "[verify] {} task(s); CSR {} verts / {} edges / {} bytes",
        tasks.task.len(),
        graph.names.len(),
        graph.col_indices.len(),
        graph.csr_bytes(),
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
    println!("\n[verify] summary: {passed} passed, {failed} failed");
    if failed > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS }
}

// ── prompt ──────────────────────────────────────────────────────────

fn cmd_prompt(g: &GlobalArgs, args: &[String]) -> ExitCode {
    let task_id = match args.first() {
        Some(s) => s,
        None => {
            eprintln!("prompt: needs <task-id>");
            return ExitCode::from(2);
        }
    };
    let tasks = match load_tasks(&g.tasks_path) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let task = match tasks.task.iter().find(|t| t.id == *task_id) {
        Some(t) => t,
        None => {
            eprintln!("prompt: task '{task_id}' not in {}", g.tasks_path.display());
            return ExitCode::from(2);
        }
    };
    let graph = match build_graph(&g.root_path) {
        Ok(g) => g,
        Err(code) => return code,
    };

    let target_file = g.root_path.join(&task.target_file);
    let input = prompt::RefactorPromptInput {
        task_id: &task.id,
        goal: &task.description,
        target_fn: &task.target_fn,
        target_file: &target_file,
        graph: &graph,
    };
    let built = match prompt::build(&input) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("prompt: build failed: {e}");
            return ExitCode::from(1);
        }
    };

    let task_out = g.output_dir.join(&task.id);
    if let Err(e) = std::fs::create_dir_all(&task_out) {
        eprintln!("prompt: mkdir {}: {e}", task_out.display());
        return ExitCode::from(2);
    }
    let prompt_path = task_out.join("prompt.md");
    if let Err(e) = std::fs::write(&prompt_path, &built.markdown) {
        eprintln!("prompt: write {}: {e}", prompt_path.display());
        return ExitCode::from(2);
    }

    println!("[prompt] {} bytes → {}", built.markdown.len(), prompt_path.display());
    println!("[prompt] {} caller(s) across {} file(s)",
        built.caller_count, built.caller_files.len());
    ExitCode::SUCCESS
}

// ── refactor ────────────────────────────────────────────────────────

fn cmd_refactor(g: &GlobalArgs, args: &[String]) -> ExitCode {
    let task_id = match args.first() {
        Some(s) => s,
        None => {
            eprintln!("refactor: needs <task-id>");
            return ExitCode::from(2);
        }
    };
    let tasks = match load_tasks(&g.tasks_path) {
        Ok(t) => t,
        Err(code) => return code,
    };
    let task = match tasks.task.iter().find(|t| t.id == *task_id) {
        Some(t) => t,
        None => {
            eprintln!("refactor: task '{task_id}' not in {}", g.tasks_path.display());
            return ExitCode::from(2);
        }
    };
    let graph = match build_graph(&g.root_path) {
        Ok(g) => g,
        Err(code) => return code,
    };

    let target_file = g.root_path.join(&task.target_file);
    let input = prompt::RefactorPromptInput {
        task_id: &task.id,
        goal: &task.description,
        target_fn: &task.target_fn,
        target_file: &target_file,
        graph: &graph,
    };
    let built = match prompt::build(&input) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("refactor: prompt build failed: {e}");
            return ExitCode::from(1);
        }
    };

    let task_out = g.output_dir.join(&task.id);
    if let Err(e) = std::fs::create_dir_all(&task_out) {
        eprintln!("refactor: mkdir {}: {e}", task_out.display());
        return ExitCode::from(2);
    }
    let prompt_path = task_out.join("prompt.md");
    let _ = std::fs::write(&prompt_path, &built.markdown);

    println!("[refactor] task={} model={} prompt_bytes={}",
        task.id, g.llm_model, built.markdown.len());
    println!("[refactor] calling proxy {}:{} ...", g.proxy_host, g.proxy_port);

    let t0 = Instant::now();
    // 3-min timeout matches the proxy's OLLAMA_TIMEOUT_SECS for cloud-
    // backed models. Local 7b should answer in ≤30s once warm.
    let resp = match proxy::llm_generate(
        (&g.proxy_host, g.proxy_port),
        &g.llm_model,
        &built.markdown,
        Duration::from_secs(180),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("refactor: proxy call failed: {e}");
            return ExitCode::from(1);
        }
    };
    let elapsed = t0.elapsed();

    println!("[refactor] status={} body_bytes={} elapsed={:.1}s",
        resp.status, resp.body.len(), elapsed.as_secs_f32());

    if resp.status != 0 {
        eprintln!("[refactor] proxy returned non-OK status — see body for details");
        let response_path = task_out.join("response.txt");
        let _ = std::fs::write(&response_path, &resp.body);
        eprintln!("[refactor] saved raw body → {}", response_path.display());
        return ExitCode::from(1);
    }

    // Persist raw + extracted artefacts.
    let response_path = task_out.join("response.txt");
    if let Err(e) = std::fs::write(&response_path, &resp.body) {
        eprintln!("refactor: write {}: {e}", response_path.display());
        return ExitCode::from(2);
    }

    let body = match resp.body_str() {
        Some(s) => s,
        None => {
            eprintln!("refactor: response not valid UTF-8 (saved to response.txt as raw bytes)");
            return ExitCode::from(1);
        }
    };
    let code = extract_rust_code_block(body);
    let refactor_path = task_out.join("refactor.md");
    let mut report = String::with_capacity(body.len() + 256);
    report.push_str("# Refactor result for ");
    report.push_str(&task.id);
    report.push_str("\n\n");
    report.push_str("Model: `");
    report.push_str(&g.llm_model);
    report.push_str("`\n");
    report.push_str("Prompt: see `prompt.md`\n");
    report.push_str("Raw response: see `response.txt`\n\n");
    if let Some(rs) = &code {
        report.push_str("## Extracted refactored function\n\n```rust\n");
        report.push_str(rs);
        if !rs.ends_with('\n') { report.push('\n'); }
        report.push_str("```\n");
    } else {
        report.push_str("## No fenced ```rust block found\n\n");
        report.push_str("Raw response was saved verbatim to `response.txt`. ");
        report.push_str("Inspect manually — the model didn't follow the format constraint.\n");
    }
    if let Err(e) = std::fs::write(&refactor_path, &report) {
        eprintln!("refactor: write {}: {e}", refactor_path.display());
        return ExitCode::from(2);
    }

    println!("[refactor] saved → {}", refactor_path.display());
    if code.is_none() {
        println!("[refactor] WARNING: no ```rust block extracted — check response.txt");
    }
    ExitCode::SUCCESS
}

// ── score ───────────────────────────────────────────────────────────

fn cmd_score(g: &GlobalArgs, args: &[String]) -> ExitCode {
    let task_id = match args.first() {
        Some(s) => s.clone(),
        None => {
            eprintln!("score: needs <task-id>");
            return ExitCode::from(2);
        }
    };
    let tasks = match load_tasks(&g.tasks_path) { Ok(t) => t, Err(c) => return c };
    let task = match tasks.task.iter().find(|t| t.id == task_id) {
        Some(t) => t,
        None => {
            eprintln!("score: task '{task_id}' not in {}", g.tasks_path.display());
            return ExitCode::from(2);
        }
    };

    // Pull the previously-saved refactor.md and re-extract its rust block.
    let task_out = g.output_dir.join(&task.id);
    let refactor_path = task_out.join("refactor.md");
    let refactor_md = match std::fs::read_to_string(&refactor_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("score: read {}: {e}\n  did you run `draug-eval refactor {task_id}` first?",
                refactor_path.display());
            return ExitCode::from(2);
        }
    };
    let patch_code = match extract_rust_code_block(&refactor_md) {
        Some(s) => s,
        None => {
            eprintln!("score: no ```rust block in {} — refactor produced no usable patch",
                refactor_path.display());
            return ExitCode::from(1);
        }
    };

    score_one(g, task, &patch_code)
}

fn score_one(g: &GlobalArgs, task: &Task, patch_code: &str) -> ExitCode {
    let task_out = g.output_dir.join(&task.id);
    let _ = std::fs::create_dir_all(&task_out);

    println!("[score] task={} patch_bytes={}", task.id, patch_code.len());

    // (1) Spin up / reset the sandbox worktree.
    println!("[score] preparing sandbox ...");
    let sandbox = match sandbox::Sandbox::prepare(
        &g.root_path,
        Path::new("tools/draug-eval-runner/sandbox"),
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("score: sandbox prepare failed: {e}");
            return ExitCode::from(2);
        }
    };

    // (2) Apply the patch in the sandbox.
    let target_rel = Path::new(&task.target_file);
    let target_abs = sandbox.inside(target_rel);
    let applied = match apply::apply(&target_abs, &task.target_fn, patch_code) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("score: apply failed: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = std::fs::write(&target_abs, &applied.patched) {
        eprintln!("score: write {}: {e}", target_abs.display());
        return ExitCode::from(2);
    }
    println!("[score] applied: {:?} (lines {}–{})",
        applied.strategy, applied.start_line, applied.end_line);

    // (3) Run cargo check in the workspace that owns this file.
    let outcome = match cargo_check::check(&sandbox.path, target_rel) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("score: cargo check failed to launch: {e}");
            return ExitCode::from(2);
        }
    };
    println!(
        "[score] cargo check: workspace={} exit={} errors={} warnings={} elapsed={:.1}s",
        outcome.workspace, outcome.exit_code, outcome.error_count,
        outcome.warning_count, outcome.elapsed.as_secs_f32(),
    );

    // (4) Restore the file so the sandbox is clean for the next task.
    let _ = sandbox.restore(target_rel);

    // (5) Write a JSON report so future tooling can aggregate.
    let report = TaskReport {
        task_id: task.id.clone(),
        target_fn: task.target_fn.clone(),
        target_file: task.target_file.clone(),
        patch_strategy: format!("{:?}", applied.strategy).to_lowercase(),
        patch_chars: patch_code.len(),
        cargo_check: CargoReport {
            workspace: outcome.workspace.clone(),
            exit_code: outcome.exit_code,
            error_count: outcome.error_count,
            warning_count: outcome.warning_count,
            elapsed_secs: outcome.elapsed.as_secs_f32(),
            stderr_excerpt: outcome.stderr_excerpt.clone(),
        },
        verdict: if outcome.passed() { "PASS".into() } else { "FAIL".into() },
    };
    let json = serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into());
    let json_path = task_out.join("score.json");
    if let Err(e) = std::fs::write(&json_path, &json) {
        eprintln!("score: write {}: {e}", json_path.display());
    } else {
        println!("[score] report → {}", json_path.display());
    }

    println!("[score] verdict: {}", report.verdict);
    if outcome.passed() { ExitCode::SUCCESS } else { ExitCode::from(1) }
}

// ── eval ────────────────────────────────────────────────────────────

fn cmd_eval(g: &GlobalArgs, args: &[String]) -> ExitCode {
    let tasks = match load_tasks(&g.tasks_path) { Ok(t) => t, Err(c) => return c };
    let want_all = args.iter().any(|a| a == "--all");
    let single_id = args.iter().find(|a| !a.starts_with("--")).cloned();

    let chosen: Vec<&Task> = if want_all {
        tasks.task.iter().collect()
    } else if let Some(id) = single_id {
        match tasks.task.iter().find(|t| t.id == id) {
            Some(t) => vec![t],
            None => {
                eprintln!("eval: task '{id}' not in {}", g.tasks_path.display());
                return ExitCode::from(2);
            }
        }
    } else {
        eprintln!("eval: needs <task-id> or --all");
        return ExitCode::from(2);
    };

    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;
    for task in &chosen {
        println!("\n=== eval: {} ===", task.id);
        // Re-run refactor to get fresh LLM output.
        let rc = cmd_refactor(g, &[task.id.clone()]);
        if rc != ExitCode::SUCCESS {
            eprintln!("[eval] {} refactor step failed — skipping score", task.id);
            skipped += 1;
            continue;
        }
        let rc = cmd_score(g, &[task.id.clone()]);
        if rc == ExitCode::SUCCESS { passed += 1; }
        else if rc == ExitCode::from(1) { failed += 1; }
        else { skipped += 1; }
    }

    println!("\n[eval] summary: {passed} pass, {failed} fail, {skipped} skipped");
    if failed > 0 || skipped > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS }
}

// ── JSON report shapes ──────────────────────────────────────────────

#[derive(serde::Serialize)]
struct TaskReport {
    task_id: String,
    target_fn: String,
    target_file: String,
    patch_strategy: String,
    patch_chars: usize,
    cargo_check: CargoReport,
    verdict: String,
}

#[derive(serde::Serialize)]
struct CargoReport {
    workspace: String,
    exit_code: i32,
    error_count: u32,
    warning_count: u32,
    elapsed_secs: f32,
    stderr_excerpt: String,
}

/// Pull the first ```rust ... ``` fenced block out of an LLM response.
/// Falls back to ``` (no language tag) if no rust-tagged fence appears.
fn extract_rust_code_block(body: &str) -> Option<String> {
    for tag in ["```rust", "```"] {
        if let Some(open) = body.find(tag) {
            let rest = &body[open + tag.len()..];
            // Skip optional newline/spaces after the opening fence.
            let after_fence = rest.trim_start_matches(|c: char| c != '\n')
                .strip_prefix('\n')
                .unwrap_or(rest);
            if let Some(close) = after_fence.find("\n```") {
                return Some(after_fence[..close].to_string());
            }
        }
    }
    None
}

// ── shared plumbing ─────────────────────────────────────────────────

fn load_tasks(path: &Path) -> Result<TasksFile, ExitCode> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        eprintln!("error: read {}: {}", path.display(), e);
        ExitCode::from(2)
    })?;
    toml::from_str(&raw).map_err(|e| {
        eprintln!("error: parse {}: {}", path.display(), e);
        ExitCode::from(2)
    })
}

fn build_graph(root: &Path) -> Result<folkering_codegraph::CallGraph, ExitCode> {
    folkering_codegraph::build_from_dir(root).map_err(|e| {
        eprintln!("error: build_from_dir({}): {e:?}", root.display());
        ExitCode::from(2)
    })
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
            msg.push_str("  missing (expected, not found):\n");
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

fn qualified_to_file(qualified: &str) -> String {
    let path = qualified.split("::").next().unwrap_or(qualified);
    normalize_path(path)
}

fn normalize_path(p: &str) -> String {
    let mut s = p.replace('\\', "/");
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

    #[test]
    fn fixture_parses() {
        let path = Path::new("tasks.toml");
        if !path.exists() { return; }
        let raw = std::fs::read_to_string(path).unwrap();
        let parsed: TasksFile = toml::from_str(&raw).unwrap();
        assert!(!parsed.task.is_empty());
        for t in &parsed.task {
            assert!(!t.id.is_empty());
            assert!(!t.target_fn.is_empty());
        }
    }

    #[test]
    fn extract_rust_code_block_pulls_first_fence() {
        let body = "Some preamble\n```rust\nfn foo() {}\n```\nTrailing.";
        assert_eq!(extract_rust_code_block(body).as_deref(), Some("fn foo() {}"));
    }

    #[test]
    fn extract_rust_code_block_falls_back_to_unlabelled() {
        let body = "```\nfn foo() {}\n```";
        assert_eq!(extract_rust_code_block(body).as_deref(), Some("fn foo() {}"));
    }

    #[test]
    fn extract_rust_code_block_returns_none_for_unfenced() {
        assert_eq!(extract_rust_code_block("plain text"), None);
    }
}
