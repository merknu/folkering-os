//! Command Dispatch — omnibar + terminal command execution.
//!
//! Originally extracted from main.rs (Phase 8 refactor) as a single 2243-line
//! file with two mega-functions. Phase C1 split it into focused submodules:
//!
//! - `mod.rs` (this file) — `DispatchContext` struct + `dispatch_omnibar`
//!   orchestrator that calls into the sub-stages in order
//! - `preprocess` — COM3 god-mode inject, FolkShell pipe pre-processor,
//!   semantic intent matching (run BEFORE legacy dispatch)
//! - `legacy_dispatch` — the giant if-else for builtins (ls, ps, cat, find,
//!   open, run, save, dream, drivers, lspci, https, dns, ai, gemini, agent,
//!   load, etc.)
//! - `deferred` — post-dispatch actions (deferred intent moves/closes,
//!   deferred app window creation from shmem handle)
//!
//! The 13-parameter call signature was replaced by a single
//! `&mut DispatchContext`, which is a thin record of borrows the caller
//! (compositor main loop) already has live for one frame.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use compositor::agent::AgentSession;
use compositor::damage::DamageTracker;
use compositor::draug::DraugDaemon;
use compositor::framebuffer::FramebufferView;
use compositor::state::{CursorState, InputState, McpState, StreamState, WasmState};
use compositor::window_manager::WindowManager;

mod preprocess;
mod legacy_dispatch;
mod deferred;

/// Virtual address for mapping shared memory received from shell
pub(super) const COMPOSITOR_SHMEM_VADDR: usize = 0x30000000;

/// Virtual address for mapping TokenRing shmem
pub(super) const RING_VADDR: usize = 0x32000000;

/// Virtual address for query shmem to inference
pub(super) const ASK_QUERY_VADDR: usize = 0x30000000;

pub(super) const THINK_BUF_SIZE: usize = 1024;

/// Result returned by `dispatch_omnibar` to the compositor main loop.
pub struct DispatchResult {
    pub need_redraw: bool,
    pub did_work: bool,
    pub deferred_app_handle: u32,
}

/// All mutable state needed by the omnibar dispatch path, packed into a
/// single struct so the call site doesn't have to thread 13 parameters.
///
/// Each field is a `&'a mut` borrow that the compositor main loop already
/// holds open for one frame. The lifetime `'a` is bound to that frame.
pub struct DispatchContext<'a> {
    pub input: &'a mut InputState,
    pub wasm: &'a mut WasmState,
    pub wm: &'a mut WindowManager,
    pub mcp: &'a mut McpState,
    pub stream: &'a mut StreamState,
    pub draug: &'a mut DraugDaemon,
    pub briefing: &'a mut compositor::briefing::BriefingState,
    pub fb: &'a mut FramebufferView,
    pub damage: &'a mut DamageTracker,
    pub com3_queue: &'a mut Vec<String>,
    pub active_agent: &'a mut Option<AgentSession>,
    pub drivers_seeded: &'a mut bool,
    pub cursor: &'a mut CursorState,
}

/// Dispatch omnibar commands.
///
/// Pipeline (each stage may early-out / mark folkshell_handled):
/// 1. **COM3 god-mode inject** — dequeue one command from COM3 → input buffer
/// 2. **FolkShell pre-processor** — handle `|>` and `~>` pipe syntax
/// 3. **Legacy dispatch** — the big if-else for `open`, `ls`, `gemini`, …
/// 4. **Deferred actions** — execute AI intent moves/closes after window borrow drops
/// 5. **Deferred app window** — create UI window from shmem handle if dispatch produced one
///
/// Returns `DispatchResult` describing what happened this frame.
pub fn dispatch_omnibar(ctx: &mut DispatchContext, execute_command: bool) -> DispatchResult {
    let mut need_redraw = false;
    let mut did_work = false;
    let mut deferred_app_handle: u32 = 0;

    // Stage 1 — COM3 god-mode inject
    if preprocess::inject_god_mode_command(ctx.input, ctx.com3_queue) {
        need_redraw = true;
    }

    // Re-evaluate execute_command — same shadowing logic as the original.
    // If COM3 injected this frame, text_len is now > 0 and we want to execute.
    let execute_command = if !ctx.com3_queue.is_empty() || execute_command {
        execute_command || (ctx.input.text_len > 0 && need_redraw)
    } else {
        execute_command
    };

    if execute_command && ctx.input.text_len > 0 {
        // Copy cmd_str into a local buffer so we can drop the input borrow
        // before calling the dispatch stages (which need their own &mut input).
        let mut local_buf = [0u8; 256];
        let cmd_len = ctx.input.text_len.min(256);
        local_buf[..cmd_len].copy_from_slice(&ctx.input.text_buffer[..cmd_len]);

        if let Ok(cmd_str) = core::str::from_utf8(&local_buf[..cmd_len]) {
            // Stage 2 — FolkShell pre-processor (handles |> and ~>)
            let folkshell_handled = preprocess::handle_folkshell(cmd_str, ctx, &mut need_redraw);

            if !folkshell_handled {
                // Stage 3 — Legacy dispatch (open, run, builtins, gemini, agent, …)
                let dr = legacy_dispatch::dispatch_legacy_command(
                    cmd_str, ctx, &mut need_redraw,
                );
                deferred_app_handle = dr;
            }

            // Clear the omnibar input after executing
            ctx.input.text_len = 0;
            ctx.input.cursor_pos = 0;
            for i in 0..256 {
                ctx.input.text_buffer[i] = 0;
            }
            ctx.input.show_results = false;
            ctx.cursor.bg_dirty = true;
        }
    }

    // Stage 5 — Deferred app window creation from shmem handle
    if deferred_app_handle != 0 {
        if deferred::create_deferred_app_window(deferred_app_handle, ctx) {
            need_redraw = true;
        }
    }

    DispatchResult {
        need_redraw,
        did_work,
        deferred_app_handle,
    }
}
