# Folkering OS — Agentic Architecture Blueprint

## Gap Analysis: Current State vs. Blueprint

### What We Have
| Component | Current Implementation | Status |
|-----------|----------------------|--------|
| COM2 Bridge | `@@GEMINI_REQ@@{json}@@END@@` ASCII delimiters | Working but fragile |
| Serialization | Raw JSON strings over serial | Slow, no framing |
| Tool System | WASM toolsmithing, shell commands, ask_gemini | Ad-hoc, not discoverable |
| Permissions | None — LLM can execute anything | No safety boundary |
| Context Management | None — raw prompt in, raw response out | No compression |
| Agentic Loop | Single request/response per ask_gemini call | No chaining |
| MCP | Host-side only (folkering-debug server) | Not in OS |
| Sub-agents | None | Not implemented |
| Memory/Persistence | Synapse SQLite VFS | Could be leveraged |
| Async I/O | New async COM2 syscalls (0x96-0x98) | Partially working |

### What the Blueprint Requires
1. **COBS framing** on COM2 (replace ASCII delimiters)
2. **Postcard serialization** (replace JSON, zero-alloc)
3. **MCP Server in Rust OS** (expose tools via protocol)
4. **Permission Engine** (5-level cascade)
5. **Context Manager** (3-tier compaction)
6. **ReAct Loop** (while(tool_call) with hook validation)
7. **Sub-agent Coordinator** (isolated context)
8. **Draug Daemon** (background tick-driven AI)

## Implementation Phases

### Phase A: Serial Bridge Upgrade (Foundation) ✅ COMPLETE
- [x] COBS framing (libfolk/src/mcp/frame.rs + tools/mcp_bridge.py)
- [x] Postcard serialization (libfolk/src/mcp/types.rs — McpRequest/McpResponse enums)
- [x] CRC-16 integrity checks (both Rust and Python, verified matching)
- [x] Kernel dual-mode: syscall 0x97 supports COBS sentinel (arg1=0) and legacy @@END@@ (arg1=1)
- [x] Python proxy dual-mode: handle_mcp_frame() alongside legacy @@GEMINI_REQ@@

### Phase B: MCP Integration (In Progress)
- [x] MCP client API (libfolk/src/mcp/client.rs — send_chat, send_time_sync, send_wasm_gen, poll)
- [x] TIME_SYNC converted from blocking ask_gemini to async MCP
- [x] Python proxy handles McpResponse::TimeSyncRequest → McpRequest::TimeSync
- [x] Python proxy handles McpResponse::ChatRequest → LLM → McpRequest::ChatResponse
- [x] Python proxy handles McpResponse::WasmGenRequest → compile → McpRequest::WasmBinary
- [ ] Convert WASM gen flow in compositor to MCP
- [ ] Convert regular gemini queries to MCP
- [ ] Convert LOAD_WASM to MCP
- [ ] Tool discovery (tools/list)
- [ ] Capability negotiation

### Phase C: Agentic Loop ✅ COMPLETE
- [x] ReAct while(tool_call) loop (compositor/src/agent.rs)
- [x] AgentSession state machine: Idle → WaitingForLlm → ExecutingTool → SendingToolResult → Done/Failed
- [x] Tool registry: list_files, read_file, list_tasks, system_info, generate_wasm, run_command
- [x] JSON tool call parsing: {"tool": "name", "args": "..."} and {"answer": "..."}
- [x] Multi-step execution: LLM calls tool → result fed back → LLM calls another tool → repeat
- [x] Circuit breaker: MAX_TOOL_CALLS = 10 prevents infinite loops
- [x] Compositor integration: `agent <prompt>` command, MCP poll routes ChatResponse to agent
- [x] WASM gen interop: generate_wasm tool routes through existing deferred_tool_gen pipeline
- [ ] Hook system (PostToolUse → auto-validation) — future enhancement

### Phase D: Context Management ✅ COMPLETE
- [x] ContextManager class (tools/context_manager.py)
- [x] MicroCompact: truncate large tool outputs, collapse duplicates (per-turn, zero API)
- [x] AutoCompact: at 75% → summarize old messages via LLM, keep last 4
- [x] FullCompact: at 95% → nuclear reset, keep only last user + tool result
- [x] Circuit breaker: max 3 auto-compact retries
- [x] Token counting: chars/4 heuristic
- [x] Integrated into serial-gemini-proxy.py handle_mcp_frame ChatRequest handler
- [x] All self-tests passing

### Phase E: Draug Daemon ✅ COMPLETE
- [x] DraugDaemon state machine (compositor/src/draug.rs)
- [x] Timer-driven tick every 30s — collects memory, uptime, idle state
- [x] Circular observation log (20 entries, compact summaries)
- [x] Idle detection (60s threshold) — only analyzes when user is inactive
- [x] Analysis via MCP: sends observations to LLM, parses {"action":"alert"} or {"action":"none"}
- [x] Memory warning detection (>85% triggers MemoryWarning event)
- [x] Integrated into compositor main loop (tick + analyze before MCP poll)
- [x] Non-intrusive: never runs during active user interaction or agent sessions
- [ ] Synapse VFS persistence (future — write observations to SQLite)
- [ ] Dream Mode consolidation (future — summarize daily logs during idle)
