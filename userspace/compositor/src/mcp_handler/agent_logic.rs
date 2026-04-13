//! Agent + Draug + AutoDream cycle orchestration + MCP poll dispatch.
//!
//! This module owns the bulk of the original `tick_ai_systems()`. It handles
//! agent timeout, Draug daemon ticking, AutoDream cycle initiation, driver
//! tick, pattern mining, and MCP poll routing for `ChatResponse`. WasmChunk
//! responses are forwarded to the `autodream` submodule which contains the
//! signature verify + dream evaluation logic.

extern crate alloc;

use libfolk::sys::io::write_str;

use compositor::agent::AgentSession;
use compositor::damage::DamageTracker;
use compositor::draug::DraugDaemon;
use compositor::framebuffer::FramebufferView;
use compositor::state::{McpState, StreamState, WasmState};
use compositor::window_manager::WindowManager;

use crate::util::format_usize;

use super::{autodream, knowledge_hunt, rdtsc, AiTickResult};

pub(super) fn tick(
    mcp: &mut McpState,
    wasm: &mut WasmState,
    wm: &mut WindowManager,
    stream: &mut StreamState,
    draug: &mut DraugDaemon,
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    active_agent: &mut Option<AgentSession>,
    drivers_seeded: &mut bool,
    tsc_per_us: u64,
) -> AiTickResult {
    let mut did_work = false;
    let mut need_redraw = false;

    // ===== WASM JIT TOOLSMITHING =====
    if let Some((tool_win_id, tool_prompt)) = mcp.deferred_tool_gen.take() {
        did_work = true;
        if libfolk::mcp::client::send_wasm_gen(&tool_prompt) {
            mcp.async_tool_gen = Some((tool_win_id, tool_prompt));
            write_str("[MCP] WasmGenRequest sent\n");
        } else {
            if let Some(win) = wm.get_window_mut(tool_win_id) {
                win.push_line("[AI] Error: failed to send WASM gen request");
            }
        }
    }

    // ===== Agent timeout check =====
    if let Some(agent) = &mut *active_agent {
        let timeout_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { libfolk::sys::uptime() };
        if agent.check_timeout(timeout_ms) {
            if let Some(win) = wm.get_window_mut(agent.window_id) {
                win.push_line("[Agent] Timeout: LLM did not respond in 120s");
            }
            *active_agent = None;
            need_redraw = true;
        }
    }

    // ===== Draug: Background AI daemon tick =====
    {
        let now_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { libfolk::sys::uptime() };
        if draug.should_tick(now_ms) {
            draug.tick(now_ms);
            let mut nb = [0u8; 16];
            if draug.observation_count() % 6 == 1 || draug.observation_count() <= 3 {
                write_str("[Draug] Tick #");
                write_str(format_usize(draug.observation_count(), &mut nb));
                let idle_ms = now_ms.saturating_sub(draug.last_input_ms());
                write_str(" | idle: ");
                write_str(format_usize((idle_ms / 1000) as usize, &mut nb));
                write_str("s | dreams: ");
                write_str(format_usize(draug.dream_count() as usize, &mut nb));
                write_str("/");
                write_str(format_usize(compositor::draug::DREAM_MAX_PER_SESSION as usize, &mut nb));
                write_str("\n");
            }
        }
        if draug.should_analyze(now_ms) && active_agent.is_none() {
            if draug.start_analysis(now_ms) {
                let mut nb = [0u8; 16];
                write_str("[Draug] Analysis #");
                write_str(format_usize(draug.analysis_count() as usize, &mut nb));
                write_str("/5 started\n");
            }
        }
    }

    // ===== Tick WASM Drivers =====
    if !wasm.active_drivers.is_empty() {
        let resumed = compositor::driver_runtime::tick_drivers(&mut wasm.active_drivers);
        if resumed > 0 {
            did_work = true;
        }
    }

    // ===== Draug/Dream timeout =====
    {
        let timeout_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
        if draug.check_waiting_timeout(timeout_ms) {
            write_str("[Draug] Timeout — giving up on LLM response\n");
        }
    }

    // ===== Pattern-Mining =====
    {
        let mine_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
        if draug.should_mine_patterns(mine_ms)
            && active_agent.is_none()
            && mcp.async_tool_gen.is_none()
            && !draug.should_yield_tokens(active_agent.is_some(), mine_ms)
        {
            if let Some(insight) = draug.mine_patterns(mine_ms) {
                write_str("[Draug] Insight: ");
                let show_len = insight.len().min(80);
                write_str(&insight[..show_len]);
                write_str("\n");
            }
        }
    }

    // ===== Phase 7 — Knowledge Hunt =====
    //
    // Fires once per boot when the user has been idle for ~15 s.
    // Gated before AutoDream because it's a single cheap fetch, and
    // we want it to land BEFORE the 45-minute AutoDream threshold so
    // the demo is visible without a long wait.
    {
        let hunt_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { libfolk::sys::uptime() };
        if draug.should_hunt_knowledge(hunt_ms)
            && active_agent.is_none()
            && mcp.async_tool_gen.is_none()
        {
            knowledge_hunt::run(draug);
            did_work = true;
        }
    }

    // ===== Phase 13 — Overnight refactor loop (async) =====
    //
    // Non-blocking: tick_async returns in <1ms via EAGAIN polling.
    // UI renders between phases. Falls back to blocking for Phase 15.
    {
        let refactor_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { libfolk::sys::uptime() };

        // Async state machine: <1ms per frame, never blocks UI.
        // Handles both skill tree (L1-L3) and Phase 15 Plan-and-Solve.
        if draug.async_phase != compositor::draug::AsyncPhase::Idle {
            // In-progress async operation — always poll (bypass gate)
            if super::draug_async::tick_async(draug, refactor_ms) {
                did_work = true;
            }
        } else if draug.should_run_refactor_step(refactor_ms)
            && active_agent.is_none()
            && mcp.async_tool_gen.is_none()
        {
            // Start new iteration via async path
            if super::draug_async::tick_async(draug, refactor_ms) {
                did_work = true;
            }
        }
    }

    // ===== Draug Bridge: push status to kernel for TCP shell =====
    // Rate-limited: every 60 ticks (~1/sec at 60Hz) to avoid
    // unnecessary syscall overhead.
    {
        static BRIDGE_COUNTER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        let count = BRIDGE_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if count % 60 == 0 {
            let paused = libfolk::sys::draug_bridge_update(
                draug.refactor_iter,
                draug.refactor_passed,
                draug.refactor_failed,
                draug.refactor_retries,
                draug.tasks_at_level(1) as u8,
                draug.tasks_at_level(2) as u8,
                draug.tasks_at_level(3) as u8,
                if draug.plan_mode_active { 1 } else { 0 },
                draug.complex_task_idx as u8,
                if draug.refactor_hibernating { 1 } else { 0 },
                draug.consecutive_skips.min(255) as u8,
            );
            if paused && draug.is_active() {
                draug.set_active(false);
                write_str("[Draug] PAUSED via remote shell\n");
            } else if !paused && !draug.is_active() {
                draug.set_active(true);
                write_str("[Draug] RESUMED via remote shell\n");
            }
        }
    }

    // ===== AutoDream cycle start (delegates to autodream module) =====
    // Phase 14: don't dream if the skill tree still has work to do —
    // the refactor loop takes priority over AutoDream.
    let dream_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
    if draug.should_dream(dream_ms) && active_agent.is_none() && mcp.async_tool_gen.is_none()
        && !draug.should_yield_tokens(active_agent.is_some(), dream_ms)
        && draug.next_task_and_level().is_none()
        && !draug.plan_mode_active
        && draug.complex_task_idx >= compositor::draug::COMPLEX_TASK_COUNT
    {
        autodream::start_dream_cycle(mcp, wasm, draug, fb, dream_ms);
    }

    // Wake Draug from dream if user interacts
    if did_work && draug.is_dreaming() {
        draug.wake_up();
        write_str("[AutoDream] User woke up — dream cancelled\n");
    }

    // Morning Briefing
    if did_work && draug.has_pending_creative() && !draug.is_dreaming() {
        let count = draug.pending_count();
        write_str("[Morning Briefing] Draug has ");
        let mut nb2 = [0u8; 16];
        write_str(format_usize(count, &mut nb2));
        write_str(" creative change(s) waiting for approval.\n");

        let brief_win = wm.create_terminal("Morning Briefing", 200, 100, 500, 250);
        if let Some(win) = wm.get_window_mut(brief_win) {
            win.push_line("Good morning! Draug dreamt overnight:");
            win.push_line("");
            for (i, p) in draug.pending_creative.iter().enumerate() {
                if p.accepted.is_none() {
                    let line = alloc::format!(
                        "  {}. '{}': {}",
                        i + 1,
                        &p.app_name[..p.app_name.len().min(20)],
                        &p.description[..p.description.len().min(50)]
                    );
                    win.push_line(&line);
                }
            }
            win.push_line("");
            win.push_line("Type in omnibar: 'dream accept all' or 'dream reject all'");
            win.push_line("Or: 'dream accept 1' / 'dream reject 2'");
        }
        need_redraw = true;
        damage.damage_full();
    }

    // ===== MCP: Poll for responses =====
    if mcp.tz_sync_pending || mcp.async_tool_gen.is_some() || active_agent.is_some()
        || draug.is_waiting() || mcp.pending_shell_jit.is_some()
    {
        if let Some(response) = libfolk::mcp::client::poll() {
            did_work = true;
            match response {
                libfolk::mcp::types::McpRequest::TimeSync {
                    year: _, month: _, day: _, hour: _, minute: _, second: _,
                    utc_offset_minutes,
                } => {
                    mcp.tz_offset_minutes = utc_offset_minutes as i32;
                    mcp.tz_synced = true;
                    mcp.tz_sync_pending = false;
                    write_str("[MCP] TimeSync: UTC+");
                    let mut nbuf = [0u8; 16];
                    write_str(format_usize((utc_offset_minutes / 60) as usize, &mut nbuf));
                    write_str("\n");
                }
                libfolk::mcp::types::McpRequest::ChatResponse { text } => {
                    if let Ok(resp_text) = core::str::from_utf8(&text) {
                        handle_chat_response(
                            resp_text, mcp, wasm, wm, draug, active_agent, tsc_per_us,
                            &mut need_redraw,
                        );
                    }
                }
                libfolk::mcp::types::McpRequest::WasmChunk {
                    total_chunks, chunk_index: _, data,
                } => {
                    // Delegate WasmChunk handling to autodream module
                    let result = autodream::handle_wasm_chunk(
                        total_chunks, &data[..], mcp, wasm, wm, draug, fb, damage,
                        drivers_seeded, tsc_per_us, &mut need_redraw,
                    );
                    if result.early_return {
                        return AiTickResult { did_work, need_redraw };
                    }
                }
                _ => {
                    write_str("[MCP] Unhandled response\n");
                }
            }
        }
    }

    AiTickResult { did_work, need_redraw }
}

/// Handle a `ChatResponse` from the LLM proxy. Routes to the active agent,
/// Draug analysis, dream error handling, or async tool gen clarification.
fn handle_chat_response(
    resp_text: &str,
    mcp: &mut McpState,
    wasm: &mut WasmState,
    wm: &mut WindowManager,
    draug: &mut DraugDaemon,
    active_agent: &mut Option<AgentSession>,
    tsc_per_us: u64,
    need_redraw: &mut bool,
) {
    if let Some(agent) = &mut *active_agent {
        write_str("[Agent] LLM responded\n");
        agent.on_llm_response(resp_text);

        match &agent.state {
            compositor::agent::AgentState::ExecutingTool { tool_name, tool_args } => {
                let tname = tool_name.clone();
                let targs = tool_args.clone();
                write_str("[Agent] Tool: ");
                write_str(&tname);
                write_str(" ");
                write_str(&targs[..targs.len().min(40)]);
                write_str("\n");
                if let Some(win) = wm.get_window_mut(agent.window_id) {
                    win.push_line(&alloc::format!("[Agent] Tool: {} {}",
                        &tname, &targs[..targs.len().min(40)]));
                }

                if tname == "generate_wasm" {
                    mcp.deferred_tool_gen = Some((agent.window_id, alloc::string::String::from(targs.as_str())));
                } else if tname == "list_cache" {
                    let mut cache_list = alloc::string::String::from("Cached WASM apps:\n");
                    for (name, wasm_data) in &wasm.cache {
                        cache_list.push_str(&alloc::format!("  - {} ({} bytes)\n", name, wasm_data.len()));
                    }
                    if wasm.cache.is_empty() {
                        cache_list.push_str("  (empty)\n");
                    }
                    agent.on_tool_result(&cache_list);
                    if let Some(win) = wm.get_window_mut(agent.window_id) {
                        win.push_line(&cache_list[..cache_list.len().min(200)]);
                        win.push_line("[Agent] Thinking...");
                    }
                } else {
                    let result = compositor::agent::execute_tool(&tname, &targs);
                    if let Some(win) = wm.get_window_mut(agent.window_id) {
                        let preview = &result[..result.len().min(80)];
                        win.push_line(&alloc::format!("[Tool] {}", preview));
                    }
                    agent.on_tool_result(&result);
                    if let Some(win) = wm.get_window_mut(agent.window_id) {
                        win.push_line("[Agent] Thinking...");
                    }
                }
            }
            compositor::agent::AgentState::Done { answer } => {
                write_str("[Agent] Done: ");
                write_str(&answer[..answer.len().min(80)]);
                write_str("\n");
                if let Some(win) = wm.get_window_mut(agent.window_id) {
                    win.push_line("[Agent] Done:");
                    for line in answer.split('\n') {
                        if !line.is_empty() {
                            win.push_line(&line[..line.len().min(100)]);
                        }
                    }
                }
                *active_agent = None;
            }
            compositor::agent::AgentState::Failed { reason } => {
                write_str("[Agent] Failed: ");
                write_str(&reason[..reason.len().min(80)]);
                write_str("\n");
                if let Some(win) = wm.get_window_mut(agent.window_id) {
                    win.push_line(&alloc::format!("[Agent] Failed: {}", &reason[..reason.len().min(80)]));
                }
                *active_agent = None;
            }
            _ => {}
        }
        *need_redraw = true;
    } else if draug.is_waiting() {
        if let Some(alert) = draug.on_analysis_response(resp_text) {
            write_str(&alert);
            write_str("\n");
        } else {
            write_str("[Draug] Analysis complete (no action needed)\n");
        }
    } else if draug.is_dreaming() {
        write_str("[AutoDream] Error from proxy: ");
        write_str(&resp_text[..resp_text.len().min(80)]);
        write_str("\n");
        let done_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
        draug.on_dream_complete(done_ms);
        if mcp.async_tool_gen.is_some() {
            mcp.async_tool_gen = None;
        }
    } else if mcp.async_tool_gen.is_some() {
        let (tool_win_id, _) = mcp.async_tool_gen.take().unwrap_or((0, alloc::string::String::new()));
        write_str("[MCP] WASM gen response: ");
        write_str(&resp_text[..resp_text.len().min(80)]);
        write_str("\n");

        let is_question = resp_text.starts_with("QUESTION:")
            || resp_text.starts_with("VARIANTS:")
            || resp_text.starts_with("EXISTING:");
        if let Some(win) = wm.get_window_mut(tool_win_id) {
            if is_question {
                win.push_line("[AI] Need more info:");
            } else if resp_text.starts_with("Error:") {
                win.push_line("[AI] Generation failed:");
            }
            for line in resp_text.split('\n') {
                if !line.is_empty() {
                    win.push_line(&line[..line.len().min(100)]);
                }
            }
            if is_question {
                win.push_line("");
                win.push_line("Refine your request and try again.");
            }
        }
        *need_redraw = true;
    } else {
        write_str("[MCP] ChatResponse (unrouted): ");
        write_str(&resp_text[..resp_text.len().min(60)]);
        write_str("\n");
    }
}
