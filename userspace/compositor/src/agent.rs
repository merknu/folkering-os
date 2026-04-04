//! Agentic Loop — ReAct (Reason + Act) engine for Folkering OS
//!
//! Implements the core while(tool_call) loop from the architectural blueprint.
//! The agent sends prompts to the LLM via MCP, receives tool call requests,
//! executes them locally, and feeds results back until the LLM is done.
//!
//! # Architecture
//!
//! ```text
//! User prompt → Agent → MCP ChatRequest → LLM
//!                 ↑                          ↓
//!                 └── ToolResult ← tool_call ┘
//!                        (loop until no more tool calls)
//! ```

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

/// Maximum number of tool calls per agent session (prevents infinite loops)
pub const MAX_TOOL_CALLS: usize = 10;

/// Maximum conversation history entries before compaction
pub const MAX_HISTORY: usize = 16;

/// Timeout for waiting on LLM response (milliseconds)
pub const LLM_TIMEOUT_MS: u64 = 120_000; // 2 minutes

// ── Tool Registry ───────────────────────────────────────────────────────

/// A tool the OS exposes to the LLM
#[derive(Clone)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
}

/// All tools available to the LLM agent
pub const OS_TOOLS: &[Tool] = &[
    Tool { name: "list_files", description: "List files in the ramdisk VFS. Returns filenames." },
    Tool { name: "read_file", description: "Read a file from the ramdisk VFS. Args: filename" },
    Tool { name: "list_tasks", description: "List running OS tasks with PIDs and names." },
    Tool { name: "system_info", description: "Get system uptime, memory usage, and CPU info." },
    Tool { name: "generate_wasm", description: "Generate and compile a WASM visual tool. Args: description of what to draw" },
    Tool { name: "list_cache", description: "List all cached WASM apps with names and sizes" },
    Tool { name: "revert_app", description: "Rollback a WASM app to a previous version. Args: app_name version_number" },
    Tool { name: "run_command", description: "Execute a shell command. Args: command string" },
    Tool { name: "query_intent", description: "Search files by concept/purpose. Args: natural language query (e.g. 'calculator', 'animation')" },
    Tool { name: "pci_devices", description: "List all PCI hardware devices with vendor/device IDs, class, BARs, and interrupt lines" },
];

/// Build the system prompt that includes tool descriptions
pub fn build_system_prompt() -> String {
    let mut prompt = String::from(
        "You are an AI agent for Folkering OS. You MUST respond with JSON only.\n\
         \n\
         To call a tool:\n\
         {\"tool\": \"tool_name\", \"args\": \"arguments\"}\n\
         \n\
         To give your final answer:\n\
         {\"answer\": \"your response\"}\n\
         \n\
         RULES:\n\
         - Output ONLY one JSON object per response, nothing else.\n\
         - No markdown, no explanation, no text outside the JSON.\n\
         - Call tools one at a time. Wait for the result before calling the next.\n\
         - After getting tool results, either call another tool or give your answer.\n\
         \n\
         Available tools:\n"
    );
    for tool in OS_TOOLS {
        prompt.push_str("- ");
        prompt.push_str(tool.name);
        prompt.push_str(": ");
        prompt.push_str(tool.description);
        prompt.push('\n');
    }
    prompt.push_str("\nExample: {\"tool\": \"system_info\", \"args\": \"\"}\n");
    prompt.push_str("\nEPHEMERAL UI:\n\
        - For simple questions (math, conversions, quick facts), use generate_wasm\n\
          to create a micro-widget that shows the answer visually.\n\
        - These widgets are disposable — they vanish when the user closes them.\n\
        - Example: user asks 'what is 45*340?' → generate_wasm a calculator widget\n\
          showing the result 15300 in large text.\n");
    prompt.push_str("\nPERFORMANCE:\n\
        - When asked to optimize something, generate multiple versions and compare.\n\
        - Always explain which version you chose and why.\n");
    prompt
}

// ── Agent State Machine ─────────────────────────────────────────────────

/// Current state of the agentic loop
#[derive(Debug, Clone, PartialEq)]
pub enum AgentState {
    /// No active agent session
    Idle,
    /// Waiting for LLM response via MCP
    WaitingForLlm,
    /// LLM requested a tool call — needs execution
    ExecutingTool { tool_name: String, tool_args: String },
    /// Tool executed — sending result back to LLM
    SendingToolResult,
    /// Agent finished — final answer ready
    Done { answer: String },
    /// Agent failed after MAX_TOOL_CALLS
    Failed { reason: String },
}

/// Conversation history entry
pub struct HistoryEntry {
    pub role: &'static str, // "user", "assistant", "tool_result"
    pub content: String,
}

/// The agent session
pub struct AgentSession {
    pub state: AgentState,
    pub history: Vec<HistoryEntry>,
    pub tool_calls: usize,
    pub window_id: u32,
    pub waiting_since_ms: u64, // timestamp when we started waiting for LLM
}

impl AgentSession {
    pub fn new(user_prompt: &str, window_id: u32) -> Self {
        let mut history = Vec::new();
        history.push(HistoryEntry {
            role: "system",
            content: build_system_prompt(),
        });
        history.push(HistoryEntry {
            role: "user",
            content: String::from(user_prompt),
        });
        Self {
            state: AgentState::Idle,
            history,
            tool_calls: 0,
            window_id,
            waiting_since_ms: 0,
        }
    }

    /// Build the full prompt from conversation history
    pub fn build_prompt(&self) -> String {
        let mut prompt = String::new();
        for entry in &self.history {
            prompt.push_str("[");
            prompt.push_str(entry.role);
            prompt.push_str("]\n");
            prompt.push_str(&entry.content);
            prompt.push_str("\n\n");
        }
        prompt
    }

    /// Start the agent — send first prompt to LLM
    pub fn start(&mut self) -> bool {
        let prompt = self.build_prompt();
        if libfolk::mcp::client::send_chat(&prompt).is_some() {
            self.state = AgentState::WaitingForLlm;
            self.waiting_since_ms = libfolk::sys::uptime();
            true
        } else {
            self.state = AgentState::Failed {
                reason: String::from("Failed to send MCP ChatRequest"),
            };
            false
        }
    }

    /// Check if we've been waiting too long for the LLM.
    pub fn check_timeout(&mut self, now_ms: u64) -> bool {
        if self.state == AgentState::WaitingForLlm
            && self.waiting_since_ms > 0
            && now_ms.saturating_sub(self.waiting_since_ms) > LLM_TIMEOUT_MS
        {
            self.state = AgentState::Failed {
                reason: String::from("LLM response timeout (120s)"),
            };
            return true;
        }
        false
    }

    /// Process an LLM response. Parses for tool calls or final answer.
    pub fn on_llm_response(&mut self, response: &str) {
        self.history.push(HistoryEntry {
            role: "assistant",
            content: String::from(response),
        });

        // Strip whitespace and find the JSON object within the response.
        // LLMs often wrap JSON in markdown or add explanatory text around it.
        let trimmed = response.trim();

        // Try to find a JSON object anywhere in the response (not just at the start)
        let json_str = if let Some(start) = trimmed.find('{') {
            if let Some(end) = trimmed.rfind('}') {
                &trimmed[start..=end]
            } else { trimmed }
        } else { trimmed };

        // Check for {"tool": "...", "args": "..."} — tool call (check FIRST)
        if let Some(tool_name) = extract_json_field(json_str, "tool") {
            let tool_args = extract_json_field(json_str, "args").unwrap_or_default();
            self.tool_calls += 1;

            if self.tool_calls > MAX_TOOL_CALLS {
                self.state = AgentState::Failed {
                    reason: format!("Exceeded {} tool calls", MAX_TOOL_CALLS),
                };
                return;
            }

            self.state = AgentState::ExecutingTool { tool_name, tool_args };
            return;
        }

        // Check for {"answer": "..."} — final answer
        if let Some(answer) = extract_json_field(json_str, "answer") {
            self.state = AgentState::Done { answer };
            return;
        }

        // No JSON found — treat the entire response as plain text answer.
        // This handles cases where the LLM ignores JSON formatting instructions.
        self.state = AgentState::Done {
            answer: String::from(trimmed),
        };
    }

    /// Feed tool execution result back to LLM and continue the loop
    pub fn on_tool_result(&mut self, result: &str) {
        self.history.push(HistoryEntry {
            role: "tool_result",
            content: String::from(result),
        });

        // Send updated conversation back to LLM
        let prompt = self.build_prompt();
        if libfolk::mcp::client::send_chat(&prompt).is_some() {
            self.state = AgentState::WaitingForLlm;
            self.waiting_since_ms = libfolk::sys::uptime();
        } else {
            self.state = AgentState::Failed {
                reason: String::from("Failed to send tool result to LLM"),
            };
        }
    }
}

// ── Tool Executor ───────────────────────────────────────────────────────

/// Execute an OS tool and return the result as a string.
pub fn execute_tool(name: &str, args: &str) -> String {
    match name {
        "list_files" => {
            match libfolk::sys::shell::list_files() {
                Ok(resp) => format!("Found {} files (shmem={})", resp.count, resp.shmem_handle),
                Err(e) => format!("Error: {:?}", e),
            }
        }
        "read_file" => {
            match libfolk::sys::shell::cat_file(args.trim()) {
                Ok(resp) => format!("File size: {} bytes (shmem={})", resp.size, resp.shmem_handle),
                Err(e) => format!("Error reading '{}': {:?}", args.trim(), e),
            }
        }
        "list_tasks" => {
            match libfolk::sys::shell::ps() {
                Ok(count) => format!("{} tasks running", count),
                Err(e) => format!("Error: {:?}", e),
            }
        }
        "system_info" => {
            let (total_mb, used_mb, pct) = libfolk::sys::memory_stats();
            let uptime = libfolk::sys::uptime();
            format!(
                "Uptime: {}s, Memory: {}/{}MB ({}%)",
                uptime / 1000, used_mb, total_mb, pct
            )
        }
        "generate_wasm" => {
            format!("__WASM_GEN__{}", args.trim())
        }
        "list_cache" => {
            format!("__LIST_CACHE__")
        }
        "revert_app" => {
            // Marker — compositor handles rollback via proxy
            format!("__REVERT__{}", args.trim())
        }
        "run_command" => {
            // Search is the closest we have to a generic command
            match libfolk::sys::shell::search(args.trim()) {
                Ok(resp) => format!("{} results (shmem={})", resp.count, resp.shmem_handle),
                Err(e) => format!("Error: {:?}", e),
            }
        }
        "query_intent" => {
            match libfolk::sys::synapse::query_intent(args.trim()) {
                Ok(info) => format!("Found: file_id={}, size={} bytes", info.file_id, info.size),
                Err(_) => format!("No files match '{}'", args.trim()),
            }
        }
        "pci_devices" => {
            let mut devices: [libfolk::sys::pci::PciDeviceInfo; 32] = unsafe { core::mem::zeroed() };
            let count = libfolk::sys::pci::enumerate(&mut devices);
            let mut result = format!("Found {} PCI devices:\n", count);
            for i in 0..count {
                let d = &devices[i];
                result.push_str(&format!(
                    "  {:02x}:{:02x}.{} {:04x}:{:04x} class={:02x}.{:02x} irq={} {} [BAR0={}B BAR1={}B]\n",
                    d.bus, d.device_num, d.function,
                    d.vendor_id, d.device_id,
                    d.class_code, d.subclass,
                    d.interrupt_line,
                    d.class_name(),
                    d.bar_sizes[0], d.bar_sizes[1]
                ));
            }
            result
        }
        _ => format!("Error: unknown tool '{}'", name),
    }
}

// ── JSON Parsing (minimal, no_std) ──────────────────────────────────────

/// Extract a string value from JSON — delegates to shared libfolk::json parser.
fn extract_json_field(json: &str, key: &str) -> Option<String> {
    libfolk::json::extract(json, key).map(String::from)
}
