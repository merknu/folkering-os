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

/// Abstract Syntax Tree node
#[derive(Clone, Debug)]
pub enum AstNode {
    /// Single command invocation
    Cmd(Command),
    /// Deterministic pipe: left |> right
    Pipe { left: Box<AstNode>, right: Box<AstNode> },
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
    /// Execution completed with output
    Done(String),
    /// Shell could not handle this input (fall through to legacy dispatch)
    Passthrough,
}

// ── Parser ──────────────────────────────────────────────────────────────

/// Parse a shell input string into an AST.
/// Splits on `|>` for pipes, then splits each segment on whitespace.
/// Handles double-quoted arguments: `cmd "multi word arg"`.
pub fn parse(input: &str) -> Result<AstNode, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(String::from("empty input"));
    }

    // Split on |> pipe operator
    let segments: Vec<&str> = split_pipe(trimmed);

    if segments.is_empty() {
        return Err(String::from("empty pipeline"));
    }

    // Parse each segment into a Command
    let mut commands = Vec::new();
    for seg in &segments {
        let cmd = parse_command(seg.trim())?;
        commands.push(cmd);
    }

    // Build AST: left-to-right pipe chain
    let mut ast = AstNode::Cmd(commands.remove(0));
    for cmd in commands {
        ast = AstNode::Pipe {
            left: Box::new(ast),
            right: Box::new(AstNode::Cmd(cmd)),
        };
    }

    Ok(ast)
}

/// Split input on `|>` delimiter, respecting quoted strings.
fn split_pipe(input: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_quotes = false;

    while i < bytes.len() {
        if bytes[i] == b'"' {
            in_quotes = !in_quotes;
        } else if !in_quotes && i + 1 < bytes.len() && bytes[i] == b'|' && bytes[i + 1] == b'>' {
            segments.push(&input[start..i]);
            i += 2; // skip |>
            start = i;
            continue;
        }
        i += 1;
    }
    // Last segment
    if start < input.len() {
        segments.push(&input[start..]);
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

/// Flatten an AST into a linear pipeline of commands (left to right).
pub fn flatten_pipeline(ast: &AstNode) -> Vec<Command> {
    match ast {
        AstNode::Cmd(cmd) => alloc::vec![cmd.clone()],
        AstNode::Pipe { left, right } => {
            let mut cmds = flatten_pipeline(left);
            cmds.extend(flatten_pipeline(right));
            cmds
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
    if pipeline.len() == 1 && is_builtin_or_legacy(&pipeline[0].name) {
        return ShellState::Passthrough;
    }

    // If ANY command in the pipeline is a builtin, passthrough entire thing
    // (legacy dispatch handles gemini, open, run, etc.)
    if pipeline.iter().any(|cmd| is_builtin_or_legacy(&cmd.name)) && pipeline.len() == 1 {
        return ShellState::Passthrough;
    }

    // Execute pipeline stage by stage
    execute_pipeline(&pipeline, 0, String::new(), wasm_cache)
}

/// Execute pipeline from a given stage with accumulated pipe input.
pub fn execute_pipeline(
    pipeline: &[Command],
    from_stage: usize,
    mut pipe_input: String,
    wasm_cache: &BTreeMap<String, Vec<u8>>,
) -> ShellState {
    for stage in from_stage..pipeline.len() {
        let cmd = &pipeline[stage];

        // 1. Check builtins (inline execution)
        if is_builtin_or_legacy(&cmd.name) {
            // For piped builtins, execute inline
            pipe_input = execute_builtin(cmd, &pipe_input);
            continue;
        }

        // 2. Check wasm_cache (RAM)
        if let Some(wasm_bytes) = wasm_cache.get(&cmd.name) {
            pipe_input = execute_wasm_command(wasm_bytes, cmd, &pipe_input);
            continue;
        }

        // 3. Check Synapse VFS
        let vfs_name = format!("{}.wasm", cmd.name);
        const VFS_SHELL_VADDR: usize = 0x50090000;
        if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(&vfs_name) {
            if libfolk::sys::shmem_map(resp.shmem_handle, VFS_SHELL_VADDR).is_ok() {
                let data = unsafe {
                    core::slice::from_raw_parts(VFS_SHELL_VADDR as *const u8, resp.size as usize)
                };
                let wasm_bytes = Vec::from(data);
                let _ = libfolk::sys::shmem_unmap(resp.shmem_handle, VFS_SHELL_VADDR);
                let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
                pipe_input = execute_wasm_command(&wasm_bytes, cmd, &pipe_input);
                continue;
            } else {
                let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
            }
        }

        // 4. Not found → JIT synthesis needed
        return ShellState::WaitingForJIT {
            command_name: cmd.name.clone(),
            pipeline: pipeline.to_vec(),
            stage,
            pipe_input,
        };
    }

    // All stages completed
    ShellState::Done(pipe_input)
}

/// Execute a WASM command (one-shot) and return its text output.
fn execute_wasm_command(wasm_bytes: &[u8], cmd: &Command, _pipe_input: &str) -> String {
    let config = crate::wasm_runtime::WasmConfig {
        screen_width: 1280,
        screen_height: 800,
        uptime_ms: libfolk::sys::uptime() as u32,
    };
    let (result, output) = crate::wasm_runtime::execute_wasm(wasm_bytes, config);
    match result {
        crate::wasm_runtime::WasmResult::Ok => {
            // Collect text output from draw commands
            let mut text = String::new();
            for tc in &output.text_commands {
                text.push_str(&tc.text);
                text.push('\n');
            }
            if text.is_empty() {
                format!("[{}] OK ({} draw cmds)", cmd.name, output.draw_commands.len())
            } else {
                text
            }
        }
        crate::wasm_runtime::WasmResult::OutOfFuel => {
            format!("[{}] Halted: fuel exhausted", cmd.name)
        }
        crate::wasm_runtime::WasmResult::Trap(msg) => {
            format!("[{}] Trap: {}", cmd.name, &msg[..msg.len().min(60)])
        }
        crate::wasm_runtime::WasmResult::LoadError(msg) => {
            format!("[{}] Load error: {}", cmd.name, &msg[..msg.len().min(60)])
        }
    }
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
