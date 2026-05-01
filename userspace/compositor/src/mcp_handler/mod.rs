//! MCP Handler — AI orchestration, MCP polling, IPC processing,
//! token streaming, and think overlay logic.
//!
//! Phase C3 split this 1518-line file into focused submodules:
//!
//! - `mod.rs` (this file) — `tick_ai_systems` and `tick_ipc_and_streaming`
//!   orchestrators that call into the sub-stages
//! - `agent_logic` — Agent timeout, Draug ticking, AutoDream cycle start,
//!   MCP poll dispatch, ChatResponse routing, single-chunk WASM responses
//!   that aren't dream evaluations
//! - `autodream` — WasmChunk receive + signature verify + dream evaluation
//!   (Refactor / Creative / Nightmare / Driver variants), state migration,
//!   normal cache storage
//! - `token_stream` — TokenRing polling + tag FSM (`<think>`, `<|tool|>`,
//!   `<|tool_result|>`) + AI Think overlay rendering

extern crate alloc;

use compositor::Compositor;
use compositor::agent::AgentSession;
use compositor::damage::DamageTracker;
use compositor::draug::DraugDaemon;
use compositor::framebuffer::FramebufferView;
use compositor::state::{McpState, StreamState, WasmState};
use compositor::window_manager::WindowManager;

mod agent_logic;
pub(crate) mod agent_planner;
mod autodream;
pub(crate) mod draug_async;
pub(crate) mod knowledge_hunt;
pub(crate) mod refactor_loop;
pub(crate) mod task_store;
mod token_stream;

// ── Constants (shared between submodules) ─────────────────────────────

/// Maximum number of WASM apps in the warm cache
pub(crate) const MAX_CACHE_ENTRIES: usize = 4;

/// Maximum number of view adapters in the cache
pub(crate) const MAX_ADAPTER_ENTRIES: usize = 8;

/// Virtual address for mapping shared memory received from shell
pub(crate) const COMPOSITOR_SHMEM_VADDR: usize = 0x30000000;

/// Virtual address for mapping TokenRing shmem (ULTRA 43)
pub(crate) const RING_VADDR: usize = 0x32000000;

/// TokenRing header size — must match inference-server's TokenRing layout
pub(crate) const RING_HEADER_SIZE: usize = 16;

/// Token tag constants for stream parsing
pub(crate) const TOOL_OPEN: &[u8] = b"<|tool|>";
pub(crate) const TOOL_CLOSE: &[u8] = b"<|/tool|>";
pub(crate) const THINK_BUF_SIZE: usize = 1024;
pub(crate) const THINK_OPEN: &[u8] = b"<think>";
pub(crate) const THINK_CLOSE: &[u8] = b"</think>";
pub(crate) const RESULT_OPEN: &[u8] = b"<|tool_result|>";
pub(crate) const RESULT_CLOSE: &[u8] = b"<|/tool_result|>";

/// Result of an AI tick — signals whether work was done and redraw is needed.
pub struct AiTickResult {
    pub did_work: bool,
    pub need_redraw: bool,
}

/// Inline rdtsc — used for monotonic timing across the AI loop.
#[inline(always)]
pub(crate) fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

// ── Public entry points ───────────────────────────────────────────────

/// Tick AI systems: WASM JIT, agent, Draug, drivers, AutoDream, MCP polling.
///
/// Pipeline:
/// 1. Pending tool gen → send WasmGenRequest (`agent_logic`)
/// 2. Agent timeout check + Draug daemon tick (`agent_logic`)
/// 3. WASM driver tick (`agent_logic`)
/// 4. Pattern mining + AutoDream cycle start (`agent_logic`)
/// 5. MCP poll → dispatch responses (`agent_logic` for ChatResponse,
///    `autodream` for WasmChunk)
pub fn tick_ai_systems(
    mcp: &mut McpState,
    wasm: &mut WasmState,
    wm: &mut WindowManager,
    stream: &mut StreamState,
    draug: &mut DraugDaemon,
    briefing: &mut compositor::briefing::BriefingState,
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    active_agent: &mut Option<AgentSession>,
    drivers_seeded: &mut bool,
    tsc_per_us: u64,
) -> AiTickResult {
    agent_logic::tick(
        mcp, wasm, wm, stream, draug, briefing, fb, damage, active_agent, drivers_seeded, tsc_per_us,
    )
}

/// Process IPC messages, token streaming, and the AI think overlay.
pub fn tick_ipc_and_streaming(
    wm: &mut WindowManager,
    stream: &mut StreamState,
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    compositor: &mut Compositor,
) -> AiTickResult {
    token_stream::tick(wm, stream, fb, damage, compositor)
}
