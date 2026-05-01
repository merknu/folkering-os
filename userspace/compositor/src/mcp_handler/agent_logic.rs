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
use compositor::briefing::BriefingState;
use compositor::draug::DraugDaemon;
use compositor::framebuffer::FramebufferView;
use compositor::state::{McpState, StreamState, WasmState};
use compositor::window_manager::WindowManager;

use crate::util::format_usize;

use super::{autodream, rdtsc, AiTickResult};

pub(super) fn tick(
    mcp: &mut McpState,
    wasm: &mut WasmState,
    wm: &mut WindowManager,
    stream: &mut StreamState,
    draug: &mut DraugDaemon,
    briefing: &mut BriefingState,
    draug_status: Option<&'static libfolk::sys::draug::DraugStatus>,
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

    // ===== Phase A.5 (Path A): Draug self-analysis moved to daemon
    // (direct TCP via libfolk::sys::llm_generate, see
    // draug_async::start_analysis_via_tcp). Compositor's local
    // DraugDaemon no longer drives `should_tick` / `should_analyze`
    // / `check_waiting_timeout` — those run in the daemon's tick
    // loop, alongside refactor/knowledge-hunt/pattern-mining.
    //
    // The dream timeout housekeeping that lived here also moved
    // (the daemon owns the analysis-wait state machine now).

    // ===== Tick WASM Drivers =====
    if !wasm.active_drivers.is_empty() {
        let resumed = compositor::driver_runtime::tick_drivers(&mut wasm.active_drivers);
        if resumed > 0 {
            did_work = true;
        }
    }

    // ===== Phase A.5 step 2.2: pattern mining / knowledge hunt /
    // refactor loop / kernel-bridge update all moved to draug-daemon.
    // The daemon's main loop (`run_draug_tick`) drives those paths
    // and writes status into the shmem region. Compositor's local
    // DraugDaemon no longer participates in any TCP-bound agent
    // work; only the MCP-routed `start_analysis` path below stays
    // here, because compositor still owns the MCP poll/dispatch.
    //
    // Removing these blocks eliminates the duplicate-LLM-cost issue
    // that A.5 step 1 introduced (daemon and compositor both
    // ticking).

    // ===== AutoDream cycle start (delegates to autodream module) =====
    // Phase 14: don't dream if the skill tree still has work to do —
    // the refactor loop takes priority over AutoDream.
    //
    // Gate state lives in the daemon's status shmem (Phase A.5 step 4):
    // `DRAUG_FLAG_DREAM_READY` collapses `should_dream` + `should_yield_tokens`,
    // `DRAUG_FLAG_SKILL_TREE_HAS_WORK` and `DRAUG_FLAG_PLAN_MODE_ACTIVE`
    // gate against the refactor / planner loops, and `complex_task_idx`
    // gates the boot-time complex-task warmup. Compositor-local
    // `DraugDaemon` is consulted only as a cold-boot fallback before the
    // daemon's shmem region is attached.
    let dream_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
    let dream_gate_open = if active_agent.is_some() || mcp.async_tool_gen.is_some() {
        false
    } else if let Some(s) = draug_status {
        let flags = s.flags.load(core::sync::atomic::Ordering::Acquire);
        let dream_ready    = flags & libfolk::sys::draug::DRAUG_FLAG_DREAM_READY != 0;
        let plan_active    = flags & libfolk::sys::draug::DRAUG_FLAG_PLAN_MODE_ACTIVE != 0;
        let skill_has_work = flags & libfolk::sys::draug::DRAUG_FLAG_SKILL_TREE_HAS_WORK != 0;
        let complex_done = s.complex_task_idx.load(core::sync::atomic::Ordering::Relaxed)
            >= compositor::draug::COMPLEX_TASK_COUNT as u32;
        dream_ready && !plan_active && !skill_has_work && complex_done
    } else {
        // Fallback: shmem not attached yet (boot-order race with
        // draug-daemon). Read from compositor-local DraugDaemon — its
        // values can drift from the daemon's, but for this gate the
        // cost of being wrong is one wasted DREAM_DECIDE IPC, which the
        // daemon then short-circuits with SKIP. Acceptable.
        draug.should_dream(dream_ms)
            && !draug.should_yield_tokens(active_agent.is_some(), dream_ms)
            && draug.next_task_and_level().is_none()
            && !draug.plan_mode_active
            && draug.complex_task_idx >= compositor::draug::COMPLEX_TASK_COUNT
    };
    if dream_gate_open {
        autodream::start_dream_cycle(mcp, wasm, fb, dream_ms);
    }

    // Wake Draug from dream if user interacts. Authoritative dream
    // context lives in `mcp.current_dream` since A.5 step 2.
    if did_work && mcp.current_dream.is_some() {
        libfolk::sys::draug::notify_dream_result(libfolk::sys::draug::DREAM_RESULT_CANCEL);
        mcp.current_dream = None;
        write_str("[AutoDream] User woke up — dream cancelled\n");
    }

    // Morning Briefing
    if did_work && briefing.has_pending() && mcp.current_dream.is_none() {
        let count = briefing.pending_count();
        write_str("[Morning Briefing] Draug has ");
        let mut nb2 = [0u8; 16];
        write_str(format_usize(count, &mut nb2));
        write_str(" creative change(s) waiting for approval.\n");

        let brief_win = wm.create_terminal("Morning Briefing", 200, 100, 500, 250);
        if let Some(win) = wm.get_window_mut(brief_win) {
            win.push_line("Good morning! Draug dreamt overnight:");
            win.push_line("");
            for (i, p) in briefing.items.iter().enumerate() {
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
    let daemon_waiting = if let Some(s) = draug_status {
        let flags = s.flags.load(core::sync::atomic::Ordering::Acquire);
        flags & libfolk::sys::draug::DRAUG_FLAG_WAITING_FOR_LLM != 0
    } else {
        draug.is_waiting()
    };
    if mcp.tz_sync_pending || mcp.async_tool_gen.is_some() || active_agent.is_some()
        || daemon_waiting || mcp.pending_shell_jit.is_some()
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
                            resp_text, mcp, wasm, wm, active_agent,
                            &mut need_redraw,
                        );
                    }
                }
                libfolk::mcp::types::McpRequest::WasmChunk {
                    total_chunks, chunk_index: _, data,
                } => {
                    // Delegate WasmChunk handling to autodream module
                    let result = autodream::handle_wasm_chunk(
                        total_chunks, &data[..], mcp, wasm, wm, briefing, fb, damage,
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
    active_agent: &mut Option<AgentSession>,
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
    } else if mcp.current_dream.is_some() {
        write_str("[AutoDream] Error from proxy: ");
        write_str(&resp_text[..resp_text.len().min(80)]);
        write_str("\n");
        libfolk::sys::draug::notify_dream_result(libfolk::sys::draug::DREAM_RESULT_CANCEL);
        mcp.current_dream = None;
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
