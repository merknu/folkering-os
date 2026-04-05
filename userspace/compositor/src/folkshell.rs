//! FolkShell — AI-Native Semantic Object Shell
//!
//! Replaces the monolithic omnibar dispatch with an AST-based shell that
//! can pipe data between WASM commands and auto-generate missing commands
//! via LLM JIT synthesis.
//!
//! # Syntax
//!
//! ```text
//! command arg1 arg2           — single command
//! cmd1 |> cmd2 |> cmd3        — deterministic pipe (text for Phase 1)
//! cmd1 ~> "semantic query"    — fuzzy pipe (Phase 2, not yet implemented)
//! ```
//!
//! # JIT Synthesis
//!
//! When a command is not found in wasm_cache or Synapse VFS, the shell
//! transitions to `WaitingForJIT` and sends a generation request to the
//! LLM proxy. When the WASM arrives, the shell resumes the pipeline.

extern crate alloc;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use alloc::collections::BTreeMap;

// ── AST ─────────────────────────────────────────────────────────────────

/// A parsed command in the pipeline
#[derive(Clone, Debug)]
pub struct Command {
    pub name: String,
    pub args: Vec<String>,
}

/// Confidence level for semantic matching
#[derive(Clone, Debug, Copy)]
pub enum Confidence {
    Default,   // τ = 0.70
    High,      // τ = 0.90
    Low,       // τ = 0.50
}

impl Confidence {
    pub fn threshold(&self) -> u32 {
        match self {
            Confidence::Default => 70,
            Confidence::High => 90,
            Confidence::Low => 50,
        }
    }
}

/// A pipeline segment — either a command or a semantic query
#[derive(Clone, Debug)]
pub enum PipeSegment {
    Cmd(Command),
    /// Semantic match: ~> "query string" [Confidence: High]
    Semantic { query: String, confidence: Confidence },
}

/// Abstract Syntax Tree node
#[derive(Clone, Debug)]
pub enum AstNode {
    /// Single command invocation
    Cmd(Command),
    /// Deterministic pipe: left |> right
    Pipe { left: Box<AstNode>, right: Box<AstNode> },
    /// Fuzzy semantic pipe: left ~> "query"
    FuzzyPipe { left: Box<AstNode>, query: String, confidence: Confidence },
}

// ── Shell State Machine ─────────────────────────────────────────────────

/// Current state of the shell execution
#[derive(Clone, Debug)]
pub enum ShellState {
    /// No active execution
    Idle,
    /// Pipeline suspended — waiting for LLM to synthesize a missing command
    WaitingForJIT {
        command_name: String,
        /// Flattened pipeline commands (for resumption)
        pipeline: Vec<Command>,
        /// Index of the command we're waiting for
        stage: usize,
        /// Accumulated output from previous stages
        pipe_input: String,
    },
    /// Execution completed with text output
    Done(String),
    /// Execution completed with a visual WASM widget (Holographic Output).
    /// The compositor should launch this as a PersistentWasmApp in a floating window.
    Widget {
        /// Compiled WASM bytes for the visual widget
        wasm_bytes: Vec<u8>,
        /// Title for the widget window
        title: String,
    },
    /// Shell could not handle this input (fall through to legacy dispatch)
    Passthrough,
}

// ── Parser ──────────────────────────────────────────────────────────────

/// Operator type between pipeline segments
#[derive(Clone, Debug, Copy, PartialEq)]
enum PipeOp {
    /// |> deterministic pipe
    Deterministic,
    /// ~> fuzzy semantic pipe
    Fuzzy,
}

/// A raw pipeline segment: (text, operator_to_next)
struct RawSegment<'a> {
    text: &'a str,
    op: PipeOp,
}

/// Parse a shell input string into an AST.
/// Supports `|>` (deterministic) and `~>` (fuzzy semantic) pipes.
/// Handles double-quoted arguments and [Confidence: High/Low] annotations.
pub fn parse(input: &str) -> Result<AstNode, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(String::from("empty input"));
    }

    // Split on |> and ~> operators
    let segments = split_pipeline(trimmed);

    if segments.is_empty() {
        return Err(String::from("empty pipeline"));
    }

    // Build AST from segments
    let first = parse_segment(&segments[0])?;
    let mut ast = first;

    for i in 1..segments.len() {
        let prev_op = segments[i - 1].op;
        let seg = &segments[i];

        match prev_op {
            PipeOp::Deterministic => {
                let right = parse_segment(seg)?;
                ast = AstNode::Pipe {
                    left: Box::new(ast),
                    right: Box::new(right),
                };
            }
            PipeOp::Fuzzy => {
                // Fuzzy pipe: right side is a semantic query string
                let text = seg.text.trim();
                let (query, confidence) = parse_semantic_query(text);
                ast = AstNode::FuzzyPipe {
                    left: Box::new(ast),
                    query: String::from(query),
                    confidence,
                };
            }
        }
    }

    Ok(ast)
}

/// Parse a segment into an AstNode (always a Command for now)
fn parse_segment(seg: &RawSegment) -> Result<AstNode, String> {
    let cmd = parse_command(seg.text.trim())?;
    Ok(AstNode::Cmd(cmd))
}

/// Parse a semantic query with optional confidence annotation.
/// Input: `"software subscriptions" [Confidence: High]`
/// Returns: ("software subscriptions", Confidence::High)
fn parse_semantic_query(text: &str) -> (&str, Confidence) {
    let trimmed = text.trim();

    // Check for [Confidence: X] suffix
    if let Some(bracket_start) = trimmed.rfind('[') {
        let annotation = &trimmed[bracket_start..];
        let query = trimmed[..bracket_start].trim();
        // Strip quotes from query
        let query = query.trim_matches('"');

        let confidence = if annotation.contains("High") || annotation.contains("high") {
            Confidence::High
        } else if annotation.contains("Low") || annotation.contains("low") {
            Confidence::Low
        } else {
            Confidence::Default
        };
        return (query, confidence);
    }

    // No annotation — strip quotes and use default confidence
    let query = trimmed.trim_matches('"');
    (query, Confidence::Default)
}

/// Split input on `|>` and `~>` delimiters, respecting quoted strings.
fn split_pipeline(input: &str) -> Vec<RawSegment> {
    let mut segments = Vec::new();
    let mut start = 0;
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_quotes = false;

    while i < bytes.len() {
        if bytes[i] == b'"' {
            in_quotes = !in_quotes;
        } else if !in_quotes && i + 1 < bytes.len() {
            if bytes[i] == b'|' && bytes[i + 1] == b'>' {
                segments.push(RawSegment {
                    text: &input[start..i],
                    op: PipeOp::Deterministic,
                });
                i += 2;
                start = i;
                continue;
            }
            if bytes[i] == b'~' && bytes[i + 1] == b'>' {
                segments.push(RawSegment {
                    text: &input[start..i],
                    op: PipeOp::Fuzzy,
                });
                i += 2;
                start = i;
                continue;
            }
        }
        i += 1;
    }
    // Last segment (op doesn't matter for last)
    if start < input.len() {
        segments.push(RawSegment {
            text: &input[start..],
            op: PipeOp::Deterministic,
        });
    }
    segments
}

/// Parse a single command segment: `name arg1 "quoted arg" arg3`
fn parse_command(segment: &str) -> Result<Command, String> {
    let trimmed = segment.trim();
    if trimmed.is_empty() {
        return Err(String::from("empty command"));
    }

    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in trimmed.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
            }
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    tokens.push(core::mem::replace(&mut current, String::new()));
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    if tokens.is_empty() {
        return Err(String::from("no command name"));
    }

    let name = tokens.remove(0);
    Ok(Command { name, args: tokens })
}

/// A flattened pipeline step (command or semantic filter)
#[derive(Clone, Debug)]
pub enum PipelineStep {
    Cmd(Command),
    Semantic { query: String, confidence: Confidence },
}

/// Flatten an AST into a linear pipeline of steps (left to right).
pub fn flatten_pipeline(ast: &AstNode) -> Vec<PipelineStep> {
    match ast {
        AstNode::Cmd(cmd) => alloc::vec![PipelineStep::Cmd(cmd.clone())],
        AstNode::Pipe { left, right } => {
            let mut steps = flatten_pipeline(left);
            steps.extend(flatten_pipeline(right));
            steps
        }
        AstNode::FuzzyPipe { left, query, confidence } => {
            let mut steps = flatten_pipeline(left);
            steps.push(PipelineStep::Semantic {
                query: query.clone(),
                confidence: *confidence,
            });
            steps
        }
    }
}

// ── Builtins ────────────────────────────────────────────────────────────

/// Known builtin commands that are handled inline (not via WASM).
const BUILTINS: &[&str] = &[
    "ls", "files", "ps", "tasks", "cat", "uptime", "lspci", "drivers",
    "poweroff", "shutdown", "help", "dream", "revert", "save", "load",
    "ai-status", "ask", "infer", "agent",
    // gemini/generate are handled specially
];

/// Check if a command name is a builtin or a gemini/generate prefix.
pub fn is_builtin_or_legacy(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    BUILTINS.iter().any(|b| lower == *b)
        || lower.starts_with("gemini")
        || lower.starts_with("generate")
        || lower.starts_with("open")
        || lower.starts_with("run")
        || lower.starts_with("find")
        || lower.starts_with("search")
}

// ── Evaluator ───────────────────────────────────────────────────────────

/// Evaluate a parsed pipeline against the WASM cache and Synapse VFS.
///
/// Returns:
/// - `Done(output)` if pipeline completed successfully
/// - `WaitingForJIT { .. }` if a command was missing (needs LLM synthesis)
/// - `Passthrough` if input should be handled by legacy omnibar dispatch
pub fn eval(
    input: &str,
    wasm_cache: &BTreeMap<String, Vec<u8>>,
) -> ShellState {
    // Parse
    let ast = match parse(input) {
        Ok(ast) => ast,
        Err(_) => return ShellState::Passthrough,
    };

    let pipeline = flatten_pipeline(&ast);

    // Single command that's a builtin → passthrough to legacy
    if pipeline.len() == 1 {
        if let PipelineStep::Cmd(ref cmd) = pipeline[0] {
            if is_builtin_or_legacy(&cmd.name) {
                return ShellState::Passthrough;
            }
        }
    }

    // Execute pipeline stage by stage
    execute_pipeline_steps(&pipeline, 0, String::new(), wasm_cache)
}

/// Execute pipeline from a given stage with accumulated pipe input.
pub fn execute_pipeline_steps(
    pipeline: &[PipelineStep],
    from_stage: usize,
    mut pipe_input: String,
    wasm_cache: &BTreeMap<String, Vec<u8>>,
) -> ShellState {
    for stage in from_stage..pipeline.len() {
        match &pipeline[stage] {
            PipelineStep::Cmd(cmd) => {
                // 1. Check builtins
                if is_builtin_or_legacy(&cmd.name) {
                    pipe_input = execute_builtin(cmd, &pipe_input);
                    continue;
                }

                // 2. Check wasm_cache (RAM)
                if let Some(wasm_bytes) = wasm_cache.get(&cmd.name) {
                    match execute_wasm_command(wasm_bytes, cmd, &pipe_input) {
                        WasmCommandResult::Text(t) => { pipe_input = t; continue; }
                        WasmCommandResult::Visual(w, title) => {
                            return ShellState::Widget { wasm_bytes: w, title };
                        }
                    }
                }

                // 3. Check Synapse VFS
                let vfs_name = format!("{}.wasm", cmd.name);
                const VFS_SHELL_VADDR: usize = 0x50090000;
                if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(&vfs_name) {
                    if libfolk::sys::shmem_map(resp.shmem_handle, VFS_SHELL_VADDR).is_ok() {
                        let data = unsafe {
                            ::core::slice::from_raw_parts(VFS_SHELL_VADDR as *const u8, resp.size as usize)
                        };
                        let wasm_bytes = Vec::from(data);
                        let _ = libfolk::sys::shmem_unmap(resp.shmem_handle, VFS_SHELL_VADDR);
                        let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
                        match execute_wasm_command(&wasm_bytes, cmd, &pipe_input) {
                            WasmCommandResult::Text(t) => { pipe_input = t; continue; }
                            WasmCommandResult::Visual(w, title) => {
                                return ShellState::Widget { wasm_bytes: w, title };
                            }
                        }
                    } else {
                        let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
                    }
                }

                // 4. Not found → JIT synthesis needed
                // Convert pipeline steps back to Commands for JIT state
                let cmd_pipeline: Vec<Command> = pipeline.iter().filter_map(|s| {
                    if let PipelineStep::Cmd(c) = s { Some(c.clone()) } else { None }
                }).collect();
                return ShellState::WaitingForJIT {
                    command_name: cmd.name.clone(),
                    pipeline: cmd_pipeline,
                    stage,
                    pipe_input,
                };
            }

            PipelineStep::Semantic { query, confidence } => {
                // ── Fuzzy Semantic Matching (~>) ──────────────────
                // Use Synapse VFS intent query to find files matching the semantic query.
                // The pipe_input from the previous stage is filtered/routed based on
                // cosine similarity to the query.
                pipe_input = execute_semantic_match(query, *confidence, &pipe_input);
            }
        }
    }

    // All stages completed
    ShellState::Done(pipe_input)
}

/// Execute a semantic match: query Synapse VFS for entities matching the query.
/// Returns text describing matching files/intents.
fn execute_semantic_match(query: &str, confidence: Confidence, pipe_input: &str) -> String {
    let threshold = confidence.threshold();

    // Use Synapse intent query to find matching files
    match libfolk::sys::synapse::query_intent(query) {
        Ok(info) => {
            let mut result = format!(
                "[~> Match] '{}' → file_id={} size={}B (threshold={}%)\n",
                query, info.file_id, info.size, threshold
            );
            // Append the pipe_input as context
            if !pipe_input.is_empty() {
                result.push_str("Input data:\n");
                // Only include lines that conceptually match the query
                // (simple keyword filter for Phase 1 — full cosine similarity in Phase 2)
                let query_lower = query.to_ascii_lowercase();
                let query_words: Vec<&str> = query_lower.split_whitespace().collect();
                let mut matched_lines = 0;
                for line in pipe_input.lines() {
                    let line_lower = line.to_ascii_lowercase();
                    let score = query_words.iter()
                        .filter(|w| line_lower.contains(**w))
                        .count();
                    // Simple relevance: if any query word matches, include the line
                    if score > 0 || query_words.is_empty() {
                        result.push_str("  ");
                        result.push_str(line);
                        result.push('\n');
                        matched_lines += 1;
                    }
                }
                if matched_lines == 0 {
                    result.push_str("  (no matching lines — full vector search needs embeddings)\n");
                }
            }
            result
        }
        Err(_) => {
            // No VFS match — fall back to keyword filtering on pipe_input
            let mut result = format!("[~> Filter] '{}' (keyword match, threshold={}%)\n", query, threshold);
            let query_lower = query.to_ascii_lowercase();
            let query_words: Vec<&str> = query_lower.split_whitespace().collect();
            for line in pipe_input.lines() {
                let line_lower = line.to_ascii_lowercase();
                let hits = query_words.iter()
                    .filter(|w| line_lower.contains(**w))
                    .count();
                if hits > 0 {
                    result.push_str("  ");
                    result.push_str(line);
                    result.push('\n');
                }
            }
            result
        }
    }
}

// Legacy compat wrapper for JIT resume (uses Command-only pipeline)
pub fn execute_pipeline(
    pipeline: &[Command],
    from_stage: usize,
    pipe_input: String,
    wasm_cache: &BTreeMap<String, Vec<u8>>,
) -> ShellState {
    let steps: Vec<PipelineStep> = pipeline.iter()
        .map(|c| PipelineStep::Cmd(c.clone()))
        .collect();
    execute_pipeline_steps(&steps, from_stage, pipe_input, wasm_cache)
}

/// Execute a WASM command (one-shot) and return its text output.
/// Result of executing a WASM command — either text or a visual widget
enum WasmCommandResult {
    Text(String),
    Visual(Vec<u8>, String), // (wasm_bytes, title)
}

fn execute_wasm_command(wasm_bytes: &[u8], cmd: &Command, _pipe_input: &str) -> WasmCommandResult {
    let config = crate::wasm_runtime::WasmConfig {
        screen_width: 1280,
        screen_height: 800,
        uptime_ms: libfolk::sys::uptime() as u32,
    };
    let (result, output) = crate::wasm_runtime::execute_wasm(wasm_bytes, config);
    match result {
        crate::wasm_runtime::WasmResult::Ok => {
            // Count visual draw commands
            let draw_count = output.draw_commands.len()
                + output.circle_commands.len()
                + output.line_commands.len();
            let has_text = !output.text_commands.is_empty();
            let has_fill = output.fill_screen.is_some();

            // Holographic Output: if the command produces significant visuals
            // (more than just text), return it as a live widget
            if (draw_count >= 3 || has_fill) && is_visual_command(&cmd.name) {
                return WasmCommandResult::Visual(
                    Vec::from(wasm_bytes),
                    cmd.name.clone(),
                );
            }

            // Text output path
            let mut text = String::new();
            for tc in &output.text_commands {
                text.push_str(&tc.text);
                text.push('\n');
            }
            if text.is_empty() && draw_count > 0 {
                // Has draws but no text — describe them
                WasmCommandResult::Text(
                    format!("[{}] Visual: {} rects, {} circles, {} lines",
                        cmd.name, output.draw_commands.len(),
                        output.circle_commands.len(), output.line_commands.len())
                )
            } else if text.is_empty() {
                WasmCommandResult::Text(format!("[{}] OK", cmd.name))
            } else {
                WasmCommandResult::Text(text)
            }
        }
        crate::wasm_runtime::WasmResult::OutOfFuel => {
            WasmCommandResult::Text(format!("[{}] Halted: fuel exhausted", cmd.name))
        }
        crate::wasm_runtime::WasmResult::Trap(msg) => {
            WasmCommandResult::Text(format!("[{}] Trap: {}", cmd.name, &msg[..msg.len().min(60)]))
        }
        crate::wasm_runtime::WasmResult::LoadError(msg) => {
            WasmCommandResult::Text(format!("[{}] Load error: {}", cmd.name, &msg[..msg.len().min(60)]))
        }
    }
}

/// Check if a command name suggests visual/graphical output.
/// These commands get the visual JIT prompt and return WidgetHandles.
fn is_visual_command(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("dashboard") || lower.contains("chart") || lower.contains("graph")
        || lower.contains("widget") || lower.contains("visual") || lower.contains("monitor")
        || lower.contains("render") || lower.contains("display") || lower.contains("gauge")
        || lower.contains("meter") || lower.contains("heatmap") || lower.contains("plot")
        || lower.starts_with("show-") || lower.starts_with("draw-")
        || lower.starts_with("format-")
}

/// Execute a builtin command and return text output.
fn execute_builtin(cmd: &Command, pipe_input: &str) -> String {
    let name = cmd.name.as_str();
    match name {
        "ls" | "files" => {
            let mut entries: [libfolk::sys::fs::DirEntry; 32] = unsafe { ::core::mem::zeroed() };
            let count = libfolk::sys::fs::read_dir(&mut entries);
            let mut out = String::new();
            for i in 0..count {
                let e = &entries[i];
                out.push_str(e.name_str());
                out.push('\t');
                // Size as string
                let mut nb = [0u8; 12];
                let mut n = e.size as usize;
                let mut pos = nb.len();
                if n == 0 { pos -= 1; nb[pos] = b'0'; }
                while n > 0 && pos > 0 { pos -= 1; nb[pos] = b'0' + (n % 10) as u8; n /= 10; }
                if let Ok(s) = ::core::str::from_utf8(&nb[pos..]) { out.push_str(s); }
                out.push('\n');
            }
            out
        }
        "uptime" => {
            let ms = libfolk::sys::uptime();
            format!("{}s", ms / 1000)
        }
        "lspci" => {
            let mut pci_buf: [libfolk::sys::pci::PciDeviceInfo; 32] = unsafe { ::core::mem::zeroed() };
            let count = libfolk::sys::pci::enumerate(&mut pci_buf);
            let mut out = format!("{} PCI devices:\n", count);
            for i in 0..count {
                let d = &pci_buf[i];
                out.push_str(&format!(
                    "  {:02x}:{:02x}.{} {:04x}:{:04x} {} irq={}\n",
                    d.bus, d.device_num, d.function,
                    d.vendor_id, d.device_id, d.class_name(), d.interrupt_line
                ));
            }
            out
        }
        "help" => {
            String::from(
                "FolkShell — AI-Native Semantic Object Shell\n\
                 \n\
                 Built-in commands:\n\
                 ls, ps, uptime, lspci, drivers, help\n\
                 \n\
                 AI commands:\n\
                 gemini generate <desc>   — generate WASM app\n\
                 generate driver [vid:did] — generate hardware driver\n\
                 \n\
                 Pipe syntax:\n\
                 <cmd1> |> <cmd2>   — pipe output between commands\n\
                 Unknown commands are auto-generated via LLM JIT\n\
                 \n\
                 WASM API (for generated apps):\n\
                 folk_http_get(url, buf)      — HTTP GET via host\n\
                 folk_intent_fetch(query, buf) — semantic data fetch\n\
                 folk_list_files(buf, len)    — list VFS files\n\
                 folk_write_file(path, data)  — save to VFS\n\
                 folk_query_files(query, buf) — semantic file search\n"
            )
        }
        _ => {
            // Unknown builtin — return pipe_input unchanged
            String::from(pipe_input)
        }
    }
}

// ── JIT Prompt ──────────────────────────────────────────────────────────

/// Build the LLM prompt for JIT synthesis of a missing command.
pub fn jit_prompt(command_name: &str, pipe_context: &str) -> String {
    if is_visual_command(command_name) {
        // Holographic Output: generate a visual WASM widget
        format!(
            "Generate a Rust no_std WASM visual widget called '{}' for Folkering OS.\n\n\
             This is a GRAPHICAL dashboard/widget, NOT a text tool.\n\
             It must draw a beautiful, colorful visualization using the Folkering color palette.\n\n\
             Pipeline context (data to visualize): {}\n\n\
             AVAILABLE DRAWING FUNCTIONS (extern \"C\"):\n\
             fn folk_fill_screen(color: i32);          // fill background\n\
             fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32); // filled rectangle\n\
             fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32); // text\n\
             fn folk_draw_line(x1: i32, y1: i32, x2: i32, y2: i32, color: i32); // line\n\
             fn folk_draw_circle(cx: i32, cy: i32, r: i32, color: i32); // circle\n\
             fn folk_screen_width() -> i32;\n\
             fn folk_screen_height() -> i32;\n\
             fn folk_get_time() -> i32;  // uptime ms\n\n\
             COLOR PALETTE (0x00RRGGBB):\n\
             Background: 0x001a1a2e, Surface: 0x00252540, Blue: 0x003498db,\n\
             Purple: 0x009b59b6, Green: 0x0044FF44, Orange: 0x00FFAA00,\n\
             Red: 0x00FF4444, White: 0x00FFFFFF\n\n\
             CONSTRAINTS:\n\
             - #![no_std] #![no_main]\n\
             - Export: #[no_mangle] pub extern \"C\" fn run()\n\
             - Use folk_fill_screen for background, then draw rects/text/circles\n\
             - Create a visually rich dashboard with bars, labels, and data\n\
             - All extern calls in unsafe {{}}\n\
             - NO allocation, NO imports\n\
             - Return ONLY the Rust code.",
            command_name,
            if pipe_context.is_empty() { "(system overview data)" } else { pipe_context }
        )
    } else {
        // Text output tool
        format!(
            "Generate a Rust no_std WASM tool called '{}' for Folkering OS.\n\n\
             This tool is part of a shell pipeline. It receives text input and \
             produces text output via folk_draw_text.\n\n\
             Pipeline context (data flowing in): {}\n\n\
             CONSTRAINTS:\n\
             - #![no_std] #![no_main]\n\
             - Export: #[no_mangle] pub extern \"C\" fn run()\n\
             - Use folk_draw_text(x, y, ptr, len, color) to output results\n\
             - Use folk_screen_width()/folk_screen_height() for layout\n\
             - NO allocation, NO imports, NO loops\n\
             - Return ONLY the Rust code.",
            command_name,
            if pipe_context.is_empty() { "(none — first command)" } else { pipe_context }
        )
    }
}
