//! MCP Handler — AI orchestration, MCP polling, IPC processing,
//! token streaming, and think overlay logic.
//!
//! Extracted from main.rs to reduce the monolithic main loop.
//! Contains two public entry points:
//!
//! - `tick_ai_systems`: WASM JIT, agent, Draug, drivers, AutoDream, MCP polling
//! - `tick_ipc_and_streaming`: IPC messages, token streaming, think overlay

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

use compositor::Compositor;
use compositor::framebuffer::FramebufferView;
use compositor::window_manager::WindowManager;
use compositor::state::{McpState, WasmState, StreamState};
use compositor::draug::DraugDaemon;
use compositor::agent::AgentSession;
use compositor::damage::DamageTracker;

use libfolk::sys::io::{write_char, write_str};
use libfolk::sys::ipc::{recv_async, reply_with_token, IpcError};
use libfolk::sys::{shmem_map, shmem_unmap, shmem_destroy};

use crate::util::*;
use crate::ipc_helpers::*;

// ── Constants ──────────────────────────────────────────────────────────────

/// Maximum number of WASM apps in the warm cache
const MAX_CACHE_ENTRIES: usize = 4;

/// Maximum number of view adapters in the cache
const MAX_ADAPTER_ENTRIES: usize = 8;

/// Virtual address for mapping shared memory received from shell
const COMPOSITOR_SHMEM_VADDR: usize = 0x30000000;

/// Virtual address for mapping TokenRing shmem (ULTRA 43: isolated from ask shmem)
const RING_VADDR: usize = 0x32000000;

/// TokenRing header — must match inference-server's TokenRing layout (ULTRA 37, 40)
const RING_HEADER_SIZE: usize = 16;

/// Token tag constants for stream parsing
const TOOL_OPEN: &[u8] = b"<|tool|>";    // 8 bytes
const TOOL_CLOSE: &[u8] = b"<|/tool|>";  // 9 bytes
const THINK_BUF_SIZE: usize = 1024;
const THINK_OPEN: &[u8] = b"<think>";    // 7 bytes
const THINK_CLOSE: &[u8] = b"</think>";  // 8 bytes
const RESULT_OPEN: &[u8] = b"<|tool_result|>";   // 15 bytes
const RESULT_CLOSE: &[u8] = b"<|/tool_result|>"; // 16 bytes

// ── Result type ────────────────────────────────────────────────────────────

/// Result of an AI tick — signals whether work was done and redraw is needed.
pub struct AiTickResult {
    pub did_work: bool,
    pub need_redraw: bool,
}

// ── Inline RDTSC ───────────────────────────────────────────────────────────

#[inline(always)]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

// ════════════════════════════════════════════════════════════════════════════
// tick_ai_systems — AI orchestration + MCP polling
// ════════════════════════════════════════════════════════════════════════════

/// Tick AI systems: WASM JIT, agent, Draug, drivers, AutoDream, MCP polling.
///
/// This is the big AI heartbeat that runs every frame in the compositor loop.
/// Returns whether any work was done and whether the framebuffer needs redraw.
pub fn tick_ai_systems(
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

    // ===== WASM JIT TOOLSMITHING — MCP-based async generation =====
    // Frame 1: mcp.deferred_tool_gen set → send McpResponse::WasmGenRequest via COBS
    // Frame N: MCP poll returns McpRequest::WasmBinary → execute directly (no base64!)
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
        // Use RDTSC for timing (uptime_ms is broken under WHPX — APIC timer death)
        let now_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { libfolk::sys::uptime() };
        // Only count actual user input (mouse/keyboard) as activity, not rendering
        // did_work is too broad — clock ticks, MCP polls, etc. are not user input
        if draug.should_tick(now_ms) {
            draug.tick(now_ms);
            let mut nb = [0u8; 16];
            // Log every 6th tick (~1 min) to avoid spam but show liveness
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

    // ===== Tick WASM Drivers: poll IRQs and resume suspended drivers =====
    if !wasm.active_drivers.is_empty() {
        let resumed = compositor::driver_runtime::tick_drivers(&mut wasm.active_drivers);
        if resumed > 0 {
            did_work = true;
        }
    }

    // ===== Draug/Dream timeout — prevent permanent waiting_for_llm =====
    {
        let timeout_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
        if draug.check_waiting_timeout(timeout_ms) {
            write_str("[Draug] Timeout — giving up on LLM response\n");
        }
    }

    // ===== Pattern-Mining: Phase 1 of AutoDream Cycle =====
    // Runs BEFORE app dreams — analyzes telemetry for system insights.
    {
        let mine_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
        if draug.should_mine_patterns(mine_ms)
            && active_agent.is_none()
            && mcp.async_tool_gen.is_none()
            && !draug.should_yield_tokens(active_agent.is_some(), mine_ms)
        {
            if let Some(insight) = draug.mine_patterns(mine_ms) {
                // Show insight in status bar briefly
                write_str("[Draug] Insight: ");
                let show_len = insight.len().min(80);
                write_str(&insight[..show_len]);
                write_str("\n");
            }
        }
    }

    // ===== AutoDream: Two-Hemisphere Self-Improving Software =====
    let dream_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
    if draug.should_dream(dream_ms) && active_agent.is_none() && mcp.async_tool_gen.is_none()
        && !draug.should_yield_tokens(active_agent.is_some(), dream_ms) {
        let keys: alloc::vec::Vec<&str> = wasm.cache.keys().map(|k| k.as_str()).collect();
        if let Some((target, mode)) = draug.start_dream(&keys, dream_ms) {
            // Dream target found — proceed with generation
            let mode_str = match mode {
                compositor::draug::DreamMode::Refactor => "Refactor",
                compositor::draug::DreamMode::Creative => "Creative",
                compositor::draug::DreamMode::Nightmare => "Nightmare",
                compositor::draug::DreamMode::DriverRefactor => "DriverRefactor",
                compositor::draug::DreamMode::DriverNightmare => "DriverNightmare",
            };

            // State Migration: snapshot WASM memory if active app is the dream target
            wasm.state_snapshot = None;
            if let Some(ref app) = wasm.active_app {
                if let Some(ref k) = wasm.active_app_key {
                    if k.as_str() == target.as_str() {
                        if let Some(mem) = app.get_memory_slice() {
                            let snap_len = mem.len().min(1024);
                            wasm.state_snapshot = Some(alloc::vec::Vec::from(&mem[..snap_len]));
                            write_str("[StateMigration] Captured ");
                            let mut nb2 = [0u8; 16];
                            write_str(format_usize(snap_len, &mut nb2));
                            write_str(" bytes of app state\n");
                        }
                    }
                }
            }

            // Log dream start to both serial AND COM3 telemetry
            write_str("[AutoDream] ========================================\n");
            write_str("[AutoDream] DREAM #");
            let mut nb = [0u8; 16];
            write_str(format_usize(draug.dream_count() as usize, &mut nb));
            write_str(" | Mode: ");
            write_str(mode_str);
            write_str(" | Target: ");
            write_str(&target[..target.len().min(40)]);
            write_str("\n");
            // RTC timestamp for overnight log correlation
            {
                let dt = libfolk::sys::get_rtc();
                let mut ts = [0u8; 19]; // "2026-04-03 02:15:30"
                ts[0] = b'0'+((dt.year/1000)%10) as u8; ts[1] = b'0'+((dt.year/100)%10) as u8;
                ts[2] = b'0'+((dt.year/10)%10) as u8; ts[3] = b'0'+(dt.year%10) as u8;
                ts[4] = b'-'; ts[5] = b'0'+dt.month/10; ts[6] = b'0'+dt.month%10;
                ts[7] = b'-'; ts[8] = b'0'+dt.day/10; ts[9] = b'0'+dt.day%10;
                ts[10] = b' '; ts[11] = b'0'+dt.hour/10; ts[12] = b'0'+dt.hour%10;
                ts[13] = b':'; ts[14] = b'0'+dt.minute/10; ts[15] = b'0'+dt.minute%10;
                ts[16] = b':'; ts[17] = b'0'+dt.second/10; ts[18] = b'0'+dt.second%10;
                write_str("[AutoDream] Time: ");
                if let Ok(s) = core::str::from_utf8(&ts) { write_str(s); }
                write_str("\n");
            }
            // Cache size
            write_str("[AutoDream] Cache: ");
            write_str(format_usize(wasm.cache.len(), &mut nb));
            write_str(" apps | Draug dreams: ");
            write_str(format_usize(draug.dream_count() as usize, &mut nb));
            write_str("/");
            write_str(format_usize(compositor::draug::DREAM_MAX_PER_SESSION as usize, &mut nb));
            write_str("\n");

            let tweak = match mode {
                compositor::draug::DreamMode::Refactor =>
                    alloc::format!("--tweak \"refactor for fewer CPU cycles, no new features\" {}", target),
                compositor::draug::DreamMode::Nightmare => {
                    // Nightmare: ask LLM to harden the code against edge cases
                    alloc::format!("--tweak \"harden against edge cases: zero division, overflow, OOB\" {}", target)
                }
                compositor::draug::DreamMode::Creative => {
                    // For Creative mode: run the app headless to get render summary
                    let render_desc = if let Some(cached_wasm) = wasm.cache.get(&target) {
                        let cfg = compositor::wasm_runtime::WasmConfig {
                            screen_width: fb.width as u32,
                            screen_height: fb.height as u32,
                            uptime_ms: 0,
                        };
                        let (_, output) = compositor::wasm_runtime::execute_wasm(cached_wasm, cfg);
                        compositor::wasm_runtime::render_summary(&output)
                    } else {
                        alloc::string::String::from("(no cached binary)")
                    };
                    alloc::format!("--tweak \"add one visual improvement. Current output: {}\" {}", render_desc, target)
                }
                compositor::draug::DreamMode::DriverRefactor => {
                    alloc::format!("--tweak \"optimize driver for fewer CPU cycles, preserve IRQ loop\" {}", target)
                }
                compositor::draug::DreamMode::DriverNightmare => {
                    alloc::format!("--tweak \"harden driver against SFI violations, IRQ storms, DMA failures\" {}", target)
                }
            };

            if libfolk::mcp::client::send_wasm_gen(&tweak) {
                mcp.async_tool_gen = Some((0, target));
                write_str("[AutoDream] Request sent\n");
            } else {
                // send failed — cancel dream to prevent retry spam
                write_str("[AutoDream] Send failed — cancelling dream\n");
                draug.on_dream_complete(dream_ms);
            }
        } else {
            // Digital Homeostasis: all apps stable, no dreams needed
            write_str("[AutoDream] All systems stable. Sleeping.\n");
        }
    }

    // Wake Draug from dream if user interacts
    if did_work && draug.is_dreaming() {
        draug.wake_up();
        write_str("[AutoDream] User woke up — dream cancelled\n");
    }

    // Morning Briefing: show pending creative changes when user returns
    if did_work && draug.has_pending_creative() && !draug.is_dreaming() {
        let count = draug.pending_count();
        write_str("[Morning Briefing] Draug has ");
        let mut nb2 = [0u8; 16];
        write_str(format_usize(count, &mut nb2));
        write_str(" creative change(s) waiting for approval.\n");

        // Show in a terminal window
        let brief_win = wm.create_terminal("Morning Briefing", 200, 100, 500, 250);
        if let Some(win) = wm.get_window_mut(brief_win) {
            win.push_line("Good morning! Draug dreamt overnight:");
            win.push_line("");
            for (i, p) in draug.pending_creative.iter().enumerate() {
                if p.accepted.is_none() {
                    let line = alloc::format!("  {}. '{}': {}", i + 1, &p.app_name[..p.app_name.len().min(20)], &p.description[..p.description.len().min(50)]);
                    win.push_line(&line);
                }
            }
            win.push_line("");
            win.push_line("Type in omnibar: 'dream accept all' or 'dream reject all'");
            win.push_line("Or: 'dream accept 1' / 'dream reject 2'");
        }
        need_redraw = true;
        damage.damage_full();
        // Only show once per batch — mark as shown
        // (pending_creative stays until user decides)
    }

    // ===== MCP: Poll for responses from Python proxy =====
    if mcp.tz_sync_pending || mcp.async_tool_gen.is_some() || active_agent.is_some() || draug.is_waiting() || mcp.pending_shell_jit.is_some() {
        if let Some(response) = libfolk::mcp::client::poll() {
            did_work = true;
            match response {
                libfolk::mcp::types::McpRequest::TimeSync {
                    year: _, month: _, day: _,
                    hour: _, minute: _, second: _,
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
                        // Route to active agent if present
                        if let Some(agent) = &mut *active_agent {
                            write_str("[Agent] LLM responded\n");
                            agent.on_llm_response(resp_text);

                            // Process agent state
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
                                        win.push_line(&alloc::format!("[Agent] Tool: {} {}", &tname, &targs[..targs.len().min(40)]));
                                    }

                                    // Check for WASM gen (special case — async)
                                    if tname == "generate_wasm" {
                                        mcp.deferred_tool_gen = Some((agent.window_id, alloc::string::String::from(targs.as_str())));
                                    } else if tname == "list_cache" {
                                        // List OS-side WASM cache
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
                                        // Execute tool synchronously
                                        let result = compositor::agent::execute_tool(&tname, &targs);
                                        if let Some(win) = wm.get_window_mut(agent.window_id) {
                                            let preview = &result[..result.len().min(80)];
                                            win.push_line(&alloc::format!("[Tool] {}", preview));
                                        }
                                        // Feed result back to LLM
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
                                _ => {} // WaitingForLlm, etc.
                            }
                            need_redraw = true;
                        } else if draug.is_waiting() {
                            // Route to Draug daemon (analysis response)
                            if let Some(alert) = draug.on_analysis_response(resp_text) {
                                write_str(&alert);
                                write_str("\n");
                            } else {
                                write_str("[Draug] Analysis complete (no action needed)\n");
                            }
                        } else if draug.is_dreaming() {
                            // Dream error response (e.g., budget exhausted, compile fail)
                            write_str("[AutoDream] Error from proxy: ");
                            write_str(&resp_text[..resp_text.len().min(80)]);
                            write_str("\n");
                            let done_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
                            draug.on_dream_complete(done_ms);
                            // Clear mcp.async_tool_gen if dream was pending
                            if mcp.async_tool_gen.is_some() {
                                mcp.async_tool_gen = None;
                            }
                        } else if mcp.async_tool_gen.is_some() {
                            // Response during WASM gen — likely clarification or error
                            let (tool_win_id, _) = mcp.async_tool_gen.take().unwrap_or((0, alloc::string::String::new()));
                            write_str("[MCP] WASM gen response: ");
                            write_str(&resp_text[..resp_text.len().min(80)]);
                            write_str("\n");

                            // Check for clarification types
                            let is_question = resp_text.starts_with("QUESTION:") || resp_text.starts_with("VARIANTS:") || resp_text.starts_with("EXISTING:");
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
                            need_redraw = true;
                        } else {
                            write_str("[MCP] ChatResponse (unrouted): ");
                            write_str(&resp_text[..resp_text.len().min(60)]);
                            write_str("\n");
                        }
                    }
                }
                libfolk::mcp::types::McpRequest::WasmChunk { total_chunks, chunk_index, data } => {
                    let mut nbuf = [0u8; 16];
                    // client::poll() handles reassembly. The last chunk triggers this match.
                    // Get assembled WASM data from client
                    let assembled = if libfolk::mcp::client::wasm_assembly_complete() {
                        let d = libfolk::mcp::client::wasm_assembly_data();
                        write_str("[MCP] WASM assembled: ");
                        write_str(format_usize(d.len(), &mut nbuf));
                        write_str(" bytes (");
                        write_str(format_usize(total_chunks as usize, &mut nbuf));
                        write_str(" chunks)\n");
                        Some(alloc::vec::Vec::from(d))
                    } else {
                        // Single chunk (total=1) — use data directly
                        write_str("[MCP] WASM single chunk: ");
                        write_str(format_usize(data.len(), &mut nbuf));
                        write_str(" bytes\n");
                        Some(alloc::vec::Vec::from(data.as_slice()))
                    };
                    libfolk::mcp::client::wasm_assembly_reset();

                    let raw_bytes = match assembled {
                        Some(v) => v,
                        None => {
                            return AiTickResult { did_work, need_redraw };
                        }
                    };

                    // ═══════ Cryptographic Lineage: Strip + Verify Signature ═══════
                    // Signed WASM format: FOLK\x00 (5 bytes) + SHA256 sig (32 bytes) + WASM
                    let wasm_bytes = if raw_bytes.len() > 37
                        && raw_bytes[0] == b'F' && raw_bytes[1] == b'O'
                        && raw_bytes[2] == b'L' && raw_bytes[3] == b'K'
                        && raw_bytes[4] == 0x00
                    {
                        let sig = &raw_bytes[5..37];
                        let wasm_data = &raw_bytes[37..];
                        // Verify: hash the WASM binary
                        let wasm_hash = libfolk::crypto::sha256(wasm_data);
                        let mut sig_hex = [0u8; 64];
                        libfolk::crypto::hash_to_hex(&wasm_hash, &mut sig_hex);
                        write_str("[CRYPTO] Signed WASM: hash=");
                        if let Ok(s) = core::str::from_utf8(&sig_hex[..16]) { write_str(s); }
                        write_str("... sig=");
                        // Show first 8 bytes of signature as hex
                        for i in 0..4 {
                            let b = sig[i];
                            let hi = b"0123456789abcdef"[(b >> 4) as usize];
                            let lo = b"0123456789abcdef"[(b & 0xf) as usize];
                            let buf = [hi, lo];
                            if let Ok(s) = core::str::from_utf8(&buf) { write_str(s); }
                        }
                        write_str("...\n");
                        alloc::vec::Vec::from(wasm_data)
                    } else {
                        // Unsigned WASM — allow for now (boot apps, legacy)
                        // TODO: reject unsigned WASM once all paths sign
                        if raw_bytes.len() > 4 && raw_bytes[0] == 0x00
                            && raw_bytes[1] == b'a' && raw_bytes[2] == b's' && raw_bytes[3] == b'm'
                        {
                            write_str("[CRYPTO] Unsigned WASM (legacy)\n");
                        }
                        raw_bytes
                    };

                    // Extract tool context if this was from mcp.async_tool_gen
                    let (tool_win_id, tool_prompt) = if let Some(ctx) = mcp.async_tool_gen.take() {
                        ctx
                    } else {
                        (0u32, alloc::string::String::new())
                    };
                    wasm.last_bytes = Some(wasm_bytes.clone());

                    // Live Patching: if this WASM is a response to mcp.immune_patching request
                    if let Some(ref patch_key) = mcp.immune_patching.clone() {
                        let config = compositor::wasm_runtime::WasmConfig {
                            screen_width: fb.width as u32,
                            screen_height: fb.height as u32,
                            uptime_ms: libfolk::sys::uptime() as u32,
                        };
                        match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                            Ok(app) => {
                                write_str("[IMMUNE] Patched '");
                                write_str(&patch_key[..patch_key.len().min(30)]);
                                write_str("' live!\n");
                                wasm.active_app = Some(app);
                                wasm.fuel_fail_count = 0;
                                // Update cache with fixed version
                                wasm.cache.insert(patch_key.clone(), wasm_bytes.clone());
                            }
                            Err(e) => {
                                write_str("[IMMUNE] Patch failed to load: ");
                                write_str(&e[..e.len().min(60)]);
                                write_str("\n");
                            }
                        }
                        mcp.immune_patching = None;
                        return AiTickResult { did_work, need_redraw };
                    }

                    // View Adapter: if this WASM is a response to adapter generation
                    if let Some(ref adapter_key) = mcp.pending_adapter.clone() {
                        // Validate the adapter compiles
                        let config = compositor::wasm_runtime::WasmConfig {
                            screen_width: fb.width as u32,
                            screen_height: fb.height as u32,
                            uptime_ms: libfolk::sys::uptime() as u32,
                        };
                        let (result, _) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, config);
                        match result {
                            compositor::wasm_runtime::WasmResult::Ok |
                            compositor::wasm_runtime::WasmResult::OutOfFuel => {
                                // Adapter compiled and runs — cache it
                                if mcp.adapter_cache.len() >= MAX_ADAPTER_ENTRIES {
                                    if let Some(oldest) = mcp.adapter_cache.keys().next().cloned() {
                                        mcp.adapter_cache.remove(&oldest);
                                    }
                                }
                                mcp.adapter_cache.insert(adapter_key.clone(), wasm_bytes.clone());
                                write_str("[ViewAdapter] Cached adapter: ");
                                write_str(&adapter_key[..adapter_key.len().min(40)]);
                                write_str("\n");
                            }
                            _ => {
                                write_str("[ViewAdapter] Adapter generation failed — discarding\n");
                            }
                        }
                        mcp.pending_adapter = None;
                        return AiTickResult { did_work, need_redraw };
                    }

                    // Autonomous Driver: if this WASM is a driver response
                    if let Some(pci_dev) = mcp.pending_driver_device.take() {
                        // ── Persist to Synapse VFS before loading ──
                        let next_v = compositor::driver_runtime::find_latest_version(
                            pci_dev.vendor_id, pci_dev.device_id) + 1;
                        if compositor::driver_runtime::store_driver_vfs(
                            pci_dev.vendor_id, pci_dev.device_id, next_v,
                            &wasm_bytes, compositor::driver_runtime::DriverSource::Jit
                        ) {
                            write_str(&alloc::format!("[DRV] Persisted to VFS as v{}\n", next_v));
                        }

                        let mut cap = compositor::driver_runtime::DriverCapability::from_pci(&pci_dev);
                        let name = alloc::format!("drv_{:04x}_{:04x}", pci_dev.vendor_id, pci_dev.device_id);
                        cap.set_name(&name);

                        // Map MMIO BARs into our address space
                        let mapped = compositor::driver_runtime::map_device_bars(&mut cap);
                        write_str("[DRV] Mapped ");
                        let mut nb4 = [0u8; 16];
                        write_str(format_usize(mapped, &mut nb4));
                        write_str(" MMIO BARs\n");

                        // Instantiate the WASM driver
                        match compositor::driver_runtime::WasmDriver::new(&wasm_bytes, cap) {
                            Ok(mut driver) => {
                                driver.meta.version = next_v;
                                driver.meta.source = compositor::driver_runtime::DriverSource::Jit;
                                // Bind IRQ
                                let _ = driver.bind_irq();

                                // Start driver execution
                                write_str("[DRV] Starting driver: ");
                                write_str(&name[..name.len().min(30)]);
                                write_str("\n");
                                match driver.start() {
                                    compositor::driver_runtime::DriverResult::WaitingForIrq => {
                                        write_str("[DRV] Driver yielded (waiting for IRQ)\n");
                                        wasm.active_drivers.push(driver);
                                    }
                                    compositor::driver_runtime::DriverResult::Completed => {
                                        write_str("[DRV] Driver completed immediately\n");
                                    }
                                    compositor::driver_runtime::DriverResult::OutOfFuel => {
                                        write_str("[DRV] Driver preempted (fuel) — scheduling\n");
                                        wasm.active_drivers.push(driver);
                                    }
                                    compositor::driver_runtime::DriverResult::Trapped(msg) => {
                                        write_str("[DRV] Driver TRAPPED: ");
                                        write_str(&msg[..msg.len().min(60)]);
                                        write_str("\n");
                                    }
                                    compositor::driver_runtime::DriverResult::LoadError(e) => {
                                        write_str("[DRV] Load error: ");
                                        write_str(&e[..e.len().min(60)]);
                                        write_str("\n");
                                    }
                                }
                            }
                            Err(e) => {
                                write_str("[DRV] Failed to instantiate: ");
                                write_str(&e[..e.len().min(60)]);
                                write_str("\n");
                            }
                        }
                        return AiTickResult { did_work, need_redraw };
                    }

                    // FolkShell JIT: if shell is waiting for a synthesized command
                    if let Some(ref jit_name) = mcp.pending_shell_jit.clone() {
                        wasm.cache.insert(jit_name.clone(), wasm_bytes.clone());
                        write_str("[FolkShell] JIT command ready: ");
                        write_str(&jit_name[..jit_name.len().min(30)]);
                        write_str("\n");

                        // Resume pipeline from where it stopped
                        if let Some((pipeline, stage, pipe_input)) = mcp.shell_jit_pipeline.take() {
                            let result = compositor::folkshell::execute_pipeline(
                                &pipeline, stage, pipe_input, &wasm.cache
                            );
                            match result {
                                compositor::folkshell::ShellState::Done(output) => {
                                    // Display output in the most recent window
                                    write_str("[FolkShell] Pipeline output:\n");
                                    write_str(&output[..output.len().min(200)]);
                                    write_str("\n");
                                }
                                compositor::folkshell::ShellState::WaitingForJIT {
                                    command_name, pipeline: p, stage: s, pipe_input: pi
                                } => {
                                    write_str("[FolkShell] Chaining JIT: ");
                                    write_str(&command_name[..command_name.len().min(30)]);
                                    write_str("\n");
                                    let prompt = compositor::folkshell::jit_prompt(&command_name, &pi);
                                    if libfolk::mcp::client::send_wasm_gen(&prompt) {
                                        mcp.pending_shell_jit = Some(command_name);
                                        mcp.shell_jit_pipeline = Some((p, s, pi));
                                    }
                                }
                                compositor::folkshell::ShellState::Widget { wasm_bytes: w, title: t } => {
                                    // JIT produced a visual widget — launch it
                                    write_str("[FolkShell] JIT widget: ");
                                    write_str(&t[..t.len().min(30)]);
                                    write_str("\n");
                                    let config = compositor::wasm_runtime::WasmConfig {
                                        screen_width: fb.width as u32,
                                        screen_height: fb.height as u32,
                                        uptime_ms: libfolk::sys::uptime() as u32,
                                    };
                                    if let Ok(app) = compositor::wasm_runtime::PersistentWasmApp::new(&w, config) {
                                        wasm.active_app = Some(app);
                                        wasm.active_app_key = Some(t);
                                        wasm.app_open_since_ms = libfolk::sys::uptime();
                                        wasm.fuel_fail_count = 0;
                                        damage.damage_full();
                                    }
                                }
                                _ => {}
                            }
                        }
                        if !matches!(mcp.pending_shell_jit.as_deref(), Some(_)) || mcp.shell_jit_pipeline.is_none() {
                            mcp.pending_shell_jit = None;
                        }
                        return AiTickResult { did_work, need_redraw };
                    }

                    // AutoDream: two-hemisphere evaluation
                    if draug.is_dreaming() && !tool_prompt.is_empty() {
                        // Use dream target as cache key (copy to avoid borrow conflict)
                        let orig_key_owned = draug.dream_target()
                            .map(alloc::string::String::from)
                            .unwrap_or_else(|| alloc::string::String::from(
                                tool_prompt.rsplit(' ').next().unwrap_or(&tool_prompt)
                            ));
                        let orig_key = orig_key_owned.as_str();
                        let dream_mode = draug.current_dream_mode();
                        let mut nb = [0u8; 16];

                        match dream_mode {
                            compositor::draug::DreamMode::Refactor => {
                                write_str("[AutoDream] ---- REFACTOR RESULT ----\n");
                                // Amnesia fix: if V1 not in RAM cache, try loading from Synapse VFS
                                if !wasm.cache.contains_key(orig_key) {
                                    let vfs_name = alloc::format!("{}.wasm", orig_key);
                                    const VFS_DREAM_VADDR: usize = 0x50070000;
                                    if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(&vfs_name) {
                                        if shmem_map(resp.shmem_handle, VFS_DREAM_VADDR).is_ok() {
                                            let data = unsafe {
                                                core::slice::from_raw_parts(VFS_DREAM_VADDR as *const u8, resp.size as usize)
                                            };
                                            wasm.cache.insert(alloc::string::String::from(orig_key), alloc::vec::Vec::from(data));
                                            let _ = shmem_unmap(resp.shmem_handle, VFS_DREAM_VADDR);
                                            let _ = shmem_destroy(resp.shmem_handle);
                                            write_str("[AutoDream] Recovered V1 from Synapse VFS\n");
                                        } else {
                                            let _ = shmem_destroy(resp.shmem_handle);
                                        }
                                    }
                                }
                                if let Some(v1_wasm) = wasm.cache.get(orig_key) {
                                    let bench_config = compositor::wasm_runtime::WasmConfig {
                                        screen_width: fb.width as u32, screen_height: fb.height as u32, uptime_ms: 0,
                                    };

                                    // Lobotomy check: compare draw command counts
                                    let (_, v1_out) = compositor::wasm_runtime::execute_wasm(v1_wasm, bench_config.clone());
                                    let v1_cmds = v1_out.draw_commands.len() + v1_out.circle_commands.len()
                                        + v1_out.line_commands.len() + v1_out.text_commands.len();
                                    let (_, v2_out) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, bench_config.clone());
                                    let v2_cmds = v2_out.draw_commands.len() + v2_out.circle_commands.len()
                                        + v2_out.line_commands.len() + v2_out.text_commands.len();

                                    if v1_cmds > 0 && v2_cmds == 0 {
                                        // V2 draws NOTHING — lobotomized!
                                        write_str("[AutoDream] VERDICT: STRIKE (Lobotomy — V2 draws 0 commands vs V1:");
                                        write_str(format_usize(v1_cmds, &mut nb));
                                        write_str(")\n");
                                        draug.add_strike(orig_key);
                                    } else if v1_cmds > 0 && (v2_cmds * 2) < v1_cmds {
                                        // V2 draws less than half of V1 — functional degradation
                                        write_str("[AutoDream] VERDICT: STRIKE (Degradation — V2:");
                                        write_str(format_usize(v2_cmds, &mut nb));
                                        write_str(" cmds vs V1:");
                                        write_str(format_usize(v1_cmds, &mut nb));
                                        write_str(")\n");
                                        draug.add_strike(orig_key);
                                    } else {
                                        // Passed sanity check — now benchmark
                                        write_str("[AutoDream] Sanity: V1=");
                                        write_str(format_usize(v1_cmds, &mut nb));
                                        write_str(" V2=");
                                        write_str(format_usize(v2_cmds, &mut nb));
                                        write_str(" cmds (OK)\n");

                                        write_str("[AutoDream] Benchmarking (10 iterations)...\n");
                                        let t1 = rdtsc();
                                        for _ in 0..10 { let _ = compositor::wasm_runtime::execute_wasm(v1_wasm, bench_config.clone()); }
                                        let v1_us = (rdtsc() - t1) / tsc_per_us / 10;
                                        let t2 = rdtsc();
                                        for _ in 0..10 { let _ = compositor::wasm_runtime::execute_wasm(&wasm_bytes, bench_config.clone()); }
                                        let v2_us = (rdtsc() - t2) / tsc_per_us / 10;

                                        write_str("[AutoDream] V1:");
                                        write_str(format_usize(v1_us as usize, &mut nb));
                                        write_str("us V2:");
                                        write_str(format_usize(v2_us as usize, &mut nb));
                                        write_str("us\n");

                                        if v2_us < v1_us {
                                            // ── Edge-case fuzz test before accepting ──
                                            // Run V2 with extreme inputs to catch crashes
                                            let fuzz_configs = [
                                                compositor::wasm_runtime::WasmConfig { screen_width: 0, screen_height: 0, uptime_ms: 0 },
                                                compositor::wasm_runtime::WasmConfig { screen_width: 1, screen_height: 1, uptime_ms: u32::MAX },
                                                compositor::wasm_runtime::WasmConfig { screen_width: 9999, screen_height: 9999, uptime_ms: 0 },
                                            ];
                                            let mut fuzz_pass = true;
                                            for fc in &fuzz_configs {
                                                let (fr, _) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, fc.clone());
                                                if let compositor::wasm_runtime::WasmResult::Trap(_) = fr {
                                                    write_str("[AutoDream] FUZZ FAIL: V2 crashes on edge input\n");
                                                    fuzz_pass = false;
                                                    break;
                                                }
                                            }
                                            if !fuzz_pass {
                                                write_str("[AutoDream] VERDICT: STRIKE (failed edge-case fuzz)\n");
                                                draug.add_strike(orig_key);
                                            } else {
                                            let pct = ((v1_us - v2_us) * 100 / v1_us.max(1)) as usize;
                                            write_str("[AutoDream] VERDICT: EVOLVED! ");
                                            write_str(format_usize(pct, &mut nb));
                                            write_str("% faster (fuzz: OK)\n");
                                            wasm.cache.insert(alloc::string::String::from(orig_key), wasm_bytes.clone());
                                            draug.reset_strikes(orig_key);
                                            } // end fuzz_pass
                                        } else {
                                            write_str("[AutoDream] VERDICT: STRIKE (V2 not faster)\n");
                                            draug.add_strike(orig_key);
                                        }
                                    }
                                    if draug.is_perfected(orig_key) {
                                        write_str("[AutoDream] STATUS: PERFECTED\n");
                                    }
                                } else {
                                    write_str("[AutoDream] ERROR: V1 not in cache, cannot compare\n");
                                }
                            }
                            compositor::draug::DreamMode::Creative => {
                                write_str("[AutoDream] ---- CREATIVE RESULT ----\n");
                                write_str("[AutoDream] New version: ");
                                write_str(format_usize(wasm_bytes.len(), &mut nb));
                                write_str(" bytes\n");
                                let preview_cfg = compositor::wasm_runtime::WasmConfig {
                                    screen_width: fb.width as u32, screen_height: fb.height as u32, uptime_ms: 0,
                                };
                                let (_, preview_out) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, preview_cfg);
                                let summary = compositor::wasm_runtime::render_summary(&preview_out);
                                write_str("[AutoDream] New render: ");
                                write_str(&summary[..summary.len().min(200)]);
                                write_str("\n");
                                // Queue for Morning Briefing — user decides
                                write_str("[AutoDream] VERDICT: QUEUED for user approval (Morning Briefing)\n");
                                draug.queue_creative(orig_key, &summary[..summary.len().min(100)], wasm_bytes.clone());
                            }
                            compositor::draug::DreamMode::Nightmare => {
                                write_str("[AutoDream] ---- NIGHTMARE RESULT ----\n");
                                write_str("[AutoDream] Fuzzing hardened version (w=0,h=0,t=MAX)...\n");
                                let fuzz_config = compositor::wasm_runtime::WasmConfig {
                                    screen_width: 0, screen_height: 0, uptime_ms: u32::MAX,
                                };
                                let (fuzz_result, _) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, fuzz_config);
                                match fuzz_result {
                                    compositor::wasm_runtime::WasmResult::Ok => {
                                        write_str("[AutoDream] VERDICT: SURVIVED (Ok) — app vaccinated!\n");
                                        wasm.cache.insert(alloc::string::String::from(orig_key), wasm_bytes.clone());
                                    }
                                    compositor::wasm_runtime::WasmResult::OutOfFuel => {
                                        write_str("[AutoDream] VERDICT: SURVIVED (fuel exhausted, but no crash) — accepted\n");
                                        wasm.cache.insert(alloc::string::String::from(orig_key), wasm_bytes.clone());
                                    }
                                    compositor::wasm_runtime::WasmResult::Trap(ref msg) => {
                                        write_str("[AutoDream] VERDICT: CRASHED! Trap: ");
                                        write_str(&msg[..msg.len().min(80)]);
                                        write_str("\n[AutoDream] Keeping original (V2 too fragile)\n");
                                    }
                                    compositor::wasm_runtime::WasmResult::LoadError(ref msg) => {
                                        write_str("[AutoDream] VERDICT: LOAD FAILED: ");
                                        write_str(&msg[..msg.len().min(80)]);
                                        write_str("\n");
                                    }
                                }
                            }
                            compositor::draug::DreamMode::DriverRefactor |
                            compositor::draug::DreamMode::DriverNightmare => {
                                write_str("[AutoDream] ---- DRIVER DREAM RESULT ----\n");
                                // For driver dreams, store as next version in VFS
                                // Parse vendor:device from orig_key (format: "drv_8086_100e")
                                // For now, just cache the improved WASM
                                write_str("[AutoDream] Driver dream result received\n");
                                wasm.cache.insert(alloc::string::String::from(orig_key), wasm_bytes.clone());
                            }
                        }

                        write_str("[AutoDream] ========== DREAM COMPLETE ==========\n");
                        let done_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
                        draug.on_dream_complete(done_ms);

                        // State Migration: if active app was the dream target, hot-swap with evolved version
                        if let Some(ref snapshot) = wasm.state_snapshot {
                            if let Some(ref k) = wasm.active_app_key {
                                if k.as_str() == orig_key {
                                    if let Some(evolved_wasm) = wasm.cache.get(orig_key) {
                                        let config = compositor::wasm_runtime::WasmConfig {
                                            screen_width: fb.width as u32,
                                            screen_height: fb.height as u32,
                                            uptime_ms: libfolk::sys::uptime() as u32,
                                        };
                                        if let Ok(mut new_app) = compositor::wasm_runtime::PersistentWasmApp::new(evolved_wasm, config) {
                                            new_app.write_memory(0, snapshot);
                                            wasm.active_app = Some(new_app);
                                            wasm.fuel_fail_count = 0;
                                            write_str("[StateMigration] Hot-swapped running app with evolved version + restored state\n");
                                        }
                                    }
                                }
                            }
                            wasm.state_snapshot = None;
                        }
                    }
                    // Normal cache storage (non-dream)
                    else if !tool_prompt.is_empty() {
                        if wasm.cache.len() >= MAX_CACHE_ENTRIES {
                            if let Some(oldest) = wasm.cache.keys().next().cloned() {
                                wasm.cache.remove(&oldest);
                            }
                        }
                        wasm.cache.insert(tool_prompt.clone(), wasm_bytes.clone());
                        write_str("[Cache] Stored WASM for: ");
                        write_str(&tool_prompt[..tool_prompt.len().min(40)]);
                        write_str("\n");

                        // Semantic VFS: auto-tag intent metadata
                        let clean_name = {
                            let mut n = tool_prompt.as_str();
                            for pfx in &["gemini generate ", "gemini gen ", "generate "] {
                                if n.len() > pfx.len() && n.as_bytes()[..pfx.len()].eq_ignore_ascii_case(pfx.as_bytes()) {
                                    n = &n[pfx.len()..];
                                    break;
                                }
                            }
                            n.trim()
                        };
                        // Write WASM to Synapse — returns rowid on success
                        let wasm_filename = alloc::format!("{}.wasm", clean_name);
                        let write_ret = libfolk::sys::synapse::write_file(&wasm_filename, &wasm_bytes);
                        if write_ret.is_ok() {
                            // Synapse now returns rowid directly in the reply
                            // Use file_count as fallback rowid estimate
                            let rowid = if let Ok(count) = libfolk::sys::synapse::file_count() {
                                count as u32
                            } else { 0 };
                            if rowid > 0 {
                                let intent_json = alloc::format!(
                                    "{{\"purpose\":\"{}\",\"type\":\"wasm_app\",\"size\":{}}}",
                                    clean_name, wasm_bytes.len()
                                );
                                let _ = libfolk::sys::synapse::write_intent(
                                    rowid, "application/wasm", &intent_json,
                                );
                                write_str("[Synapse] Intent tagged: ");
                                write_str(clean_name);
                                write_str("\n");
                            }
                        }
                    }

                    let config = compositor::wasm_runtime::WasmConfig {
                        screen_width: fb.width as u32,
                        screen_height: fb.height as u32,
                        uptime_ms: libfolk::sys::uptime() as u32,
                    };

                    let interactive = {
                        let p = tool_prompt.as_bytes();
                        find_ci(p, b"interactive") || find_ci(p, b"game")
                            || find_ci(p, b"app") || find_ci(p, b"click")
                            || find_ci(p, b"mouse") || find_ci(p, b"tetris")
                            || find_ci(p, b"follow") || find_ci(p, b"cursor")
                    };
                    wasm.last_interactive = interactive;

                    if interactive {
                        match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                            Ok(app) => {
                                write_str("[MCP] Interactive WASM app launched!\n");
                                if let Some(win) = wm.get_window_mut(tool_win_id) {
                                    win.push_line("[AI] Interactive app launched! Press ESC to exit.");
                                }
                                wasm.active_app = Some(app);
                                wasm.active_app_key = Some(tool_prompt.clone());
                                wasm.app_open_since_ms = libfolk::sys::uptime();
                                wasm.fuel_fail_count = 0;
                            }
                            Err(e) => {
                                if let Some(win) = wm.get_window_mut(tool_win_id) {
                                    win.push_line(&alloc::format!("[AI] App error: {}", &e[..e.len().min(80)]));
                                }
                            }
                        }
                    } else {
                        let (result, output) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, config);
                        let total_cmds = output.draw_commands.len()
                            + output.line_commands.len()
                            + output.circle_commands.len()
                            + output.text_commands.len()
                            + if output.fill_screen.is_some() { 1 } else { 0 };
                        if let Some(win) = wm.get_window_mut(tool_win_id) {
                            match &result {
                                compositor::wasm_runtime::WasmResult::Ok =>
                                    win.push_line(&alloc::format!("[AI] Tool: {} cmds", total_cmds)),
                                compositor::wasm_runtime::WasmResult::OutOfFuel =>
                                    win.push_line("[AI] Halted: fuel exhausted"),
                                compositor::wasm_runtime::WasmResult::Trap(msg) =>
                                    win.push_line(&alloc::format!("[AI] Trap: {}", &msg[..msg.len().min(80)])),
                                compositor::wasm_runtime::WasmResult::LoadError(msg) =>
                                    win.push_line(&alloc::format!("[AI] Load: {}", &msg[..msg.len().min(80)])),
                            }
                        }
                        if let Some(color) = output.fill_screen { fb.clear(fb.color_from_rgb24(color)); }
                        for cmd in &output.draw_commands {
                            fb.fill_rect(cmd.x as usize, cmd.y as usize, cmd.w as usize, cmd.h as usize, fb.color_from_rgb24(cmd.color));
                        }
                        for cmd in &output.line_commands {
                            let c = fb.color_from_rgb24(cmd.color);
                            compositor::graphics::draw_line(&mut *fb, cmd.x1, cmd.y1, cmd.x2, cmd.y2, c);
                        }
                        for cmd in &output.circle_commands {
                            let c = fb.color_from_rgb24(cmd.color);
                            compositor::graphics::draw_circle(&mut *fb, cmd.cx, cmd.cy, cmd.r, c);
                        }
                        for cmd in &output.text_commands {
                            fb.draw_string(cmd.x as usize, cmd.y as usize, &cmd.text, fb.color_from_rgb24(cmd.color), fb.color_from_rgb24(0));
                        }
                        if total_cmds > 0 { damage.damage_full(); }
                    }
                    need_redraw = true;
                    damage.damage_full();
                }
                _ => {
                    write_str("[MCP] Unhandled response\n");
                }
            }
        }
    }

    AiTickResult { did_work, need_redraw }
}

// ════════════════════════════════════════════════════════════════════════════
// tick_ipc_and_streaming — IPC messages + token streaming + think overlay
// ════════════════════════════════════════════════════════════════════════════

/// Process IPC messages, token streaming, and the AI think overlay.
///
/// Handles:
/// - Non-blocking IPC message reception (window creation, compositor updates)
/// - TokenRing polling for streaming LLM output
/// - Think tag filtering (`<think>...</think>`)
/// - Tool tag filtering (`<|tool|>...<|/tool|>`)
/// - Tool result filtering (`<|tool_result|>...<|/tool_result|>`)
/// - AI Think overlay rendering (semi-transparent panel)
pub fn tick_ipc_and_streaming(
    wm: &mut WindowManager,
    stream: &mut StreamState,
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    compositor: &mut Compositor,
) -> AiTickResult {
    let mut did_work = false;
    let mut need_redraw = false;

    // ===== Process IPC messages (non-blocking) =====
    match recv_async() {
        Ok(msg) => {
            did_work = true;
            let opcode = msg.payload0 & 0xFF;

            if opcode == MSG_CREATE_UI_WINDOW {
                // Create UI window from shmem widget description
                let shmem_handle = ((msg.payload0 >> 8) & 0xFFFFFFFF) as u32;
                let mut response = u64::MAX;

                if shmem_handle != 0 {
                    if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                        // Read shmem to get UI description size (max 4KB)
                        let buf = unsafe {
                            core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, 4096)
                        };

                        if let Some(header) = libfolk::ui::parse_header(buf) {
                            // Create App window
                            let win_count = wm.windows.len() as i32;
                            let wx = 100 + win_count * 30;
                            let wy = 80 + win_count * 30;
                            let win_id = wm.create_terminal(
                                header.title,
                                wx, wy,
                                header.width as u32,
                                header.height as u32,
                            );

                            if let Some(win) = wm.get_window_mut(win_id) {
                                win.kind = compositor::window_manager::WindowKind::App;
                                win.owner_task = msg.sender;

                                // Parse widget tree recursively
                                let (root_widget, _) = parse_widget_tree(header.widget_data);
                                if let Some(widget) = root_widget {
                                    win.widgets.push(widget);
                                }
                            }

                            write_str("[WM] Created UI window: ");
                            write_str(header.title);
                            write_str("\n");
                            response = win_id as u64;
                        }

                        let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
                        let _ = shmem_destroy(shmem_handle);
                    }
                }
                let _ = reply_with_token(msg.token, response, 0);
                need_redraw = true;
            } else {
                let response = handle_message(compositor, msg.payload0);
                let _ = reply_with_token(msg.token, response, 0);
            }
        }
        Err(IpcError::WouldBlock) => {}
        Err(_) => {}
    }

    // ===== Token Streaming: Poll TokenRing (ULTRA 37, 38, 46, 47) =====
    if stream.ring_handle != 0 {
        use core::sync::atomic::Ordering;
        // Read ring header atomically
        let ring_ptr = RING_VADDR as *const u32;
        let write_idx_atomic = unsafe { &*(ring_ptr as *const core::sync::atomic::AtomicU32) };
        let status_atomic = unsafe { &*((ring_ptr as *const core::sync::atomic::AtomicU32).add(1)) };

        let new_write = write_idx_atomic.load(Ordering::Acquire) as usize;
        if new_write > stream.ring_read_idx {
            did_work = true;
            // ULTRA 38: Batch-read ALL new bytes at once
            let data_ptr = unsafe { (RING_VADDR as *const u8).add(RING_HEADER_SIZE) };
            let new_data = unsafe {
                core::slice::from_raw_parts(
                    data_ptr.add(stream.ring_read_idx),
                    new_write - stream.ring_read_idx,
                )
            };
            // ULTRA 47: Data guaranteed valid UTF-8 by inference server
            // Tool call interception: scan for <|tool|>...<|/tool|> tags
            let mut visible_buf: [u8; 512] = [0; 512];
            let mut vis_len: usize = 0;

            for &byte in new_data.iter() {
                // ── Layer 1: Think tag filter ──
                // Intercepts <think>...</think> blocks and drops them entirely.
                // Bytes inside a think block never reach the tool/visible layer.
                if stream.think_state == 0 {
                    // Scanning for THINK_OPEN
                    if byte == THINK_OPEN[stream.think_open_match] {
                        stream.think_pending[stream.think_pending_len] = byte;
                        stream.think_pending_len += 1;
                        stream.think_open_match += 1;
                        if stream.think_open_match == THINK_OPEN.len() {
                            // Entered think block — capture to overlay
                            stream.think_state = 1;
                            stream.think_open_match = 0;
                            stream.think_pending_len = 0;
                            stream.think_active = true;
                            stream.think_display_len = 0; // clear previous
                            stream.think_fade_timer = 0;
                            need_redraw = true;
                        }
                        continue; // Don't pass to tool/visible layer yet
                    } else if stream.think_open_match > 0 {
                        // Partial match failed — flush pending to tool/visible layer below
                        // (fall through with pending bytes + current byte)
                        // We need to process each pending byte through tool layer
                        let pending_count = stream.think_pending_len;
                        stream.think_open_match = 0;
                        stream.think_pending_len = 0;
                        // Process each pending byte through tool/visible layer
                        for j in 0..pending_count {
                            let pb = stream.think_pending[j];
                            // (inline the tool/visible logic for flushed bytes)
                            match stream.tool_state {
                                0 => {
                                    if pb == TOOL_OPEN[stream.tool_open_match] {
                                        stream.tool_pending[stream.tool_pending_len] = pb;
                                        stream.tool_pending_len += 1;
                                        stream.tool_open_match += 1;
                                        if stream.tool_open_match == TOOL_OPEN.len() {
                                            stream.tool_state = 1; stream.tool_open_match = 0;
                                            stream.tool_pending_len = 0; stream.tool_buf_len = 0;
                                        }
                                    } else if stream.tool_open_match > 0 {
                                        for k in 0..stream.tool_pending_len {
                                            if vis_len < visible_buf.len() { visible_buf[vis_len] = stream.tool_pending[k]; vis_len += 1; }
                                        }
                                        stream.tool_open_match = 0; stream.tool_pending_len = 0;
                                        if vis_len < visible_buf.len() { visible_buf[vis_len] = pb; vis_len += 1; }
                                    } else if vis_len < visible_buf.len() { visible_buf[vis_len] = pb; vis_len += 1; }
                                }
                                1 => {
                                    if pb == TOOL_CLOSE[stream.tool_close_match] {
                                        stream.tool_close_match += 1;
                                        if stream.tool_close_match == TOOL_CLOSE.len() { stream.tool_state = 3; stream.tool_close_match = 0; }
                                    } else {
                                        for k in 0..stream.tool_close_match { if stream.tool_buf_len < stream.tool_buf.len() { stream.tool_buf[stream.tool_buf_len] = TOOL_CLOSE[k]; stream.tool_buf_len += 1; } }
                                        stream.tool_close_match = 0;
                                        if stream.tool_buf_len < stream.tool_buf.len() { stream.tool_buf[stream.tool_buf_len] = pb; stream.tool_buf_len += 1; }
                                    }
                                }
                                _ => { if vis_len < visible_buf.len() { visible_buf[vis_len] = pb; vis_len += 1; } }
                            }
                        }
                        // Now fall through to process current byte normally
                    }
                    // else: no partial match, byte falls through to tool/visible
                } else {
                    // stream.think_state == 1: Inside <think> block — scan for </think>
                    if byte == THINK_CLOSE[stream.think_close_match] {
                        stream.think_close_match += 1;
                        if stream.think_close_match == THINK_CLOSE.len() {
                            // Exited think block — keep overlay visible for 120 frames (~2s)
                            stream.think_state = 0;
                            stream.think_close_match = 0;
                            stream.think_active = false;
                            stream.think_fade_timer = 120;
                            need_redraw = true;
                        }
                    } else {
                        // Flush partial close-match bytes to think buffer
                        for k in 0..stream.think_close_match {
                            if stream.think_display_len < THINK_BUF_SIZE {
                                stream.think_display[stream.think_display_len] = THINK_CLOSE[k];
                                stream.think_display_len += 1;
                            }
                        }
                        stream.think_close_match = 0;
                        // Store current byte in think display buffer
                        if stream.think_display_len < THINK_BUF_SIZE {
                            stream.think_display[stream.think_display_len] = byte;
                            stream.think_display_len += 1;
                        }
                        need_redraw = true;
                    }
                    continue; // Don't pass think bytes to tool/visible layer
                }

                // ── Layer 1.5: Tool result filter ──
                // Hides <|tool_result|>...<|/tool_result|> from display
                if stream.result_state == 0 {
                    if byte == RESULT_OPEN[stream.result_open_match] {
                        stream.result_open_match += 1;
                        if stream.result_open_match == RESULT_OPEN.len() {
                            stream.result_state = 1;
                            stream.result_open_match = 0;
                        }
                        continue;
                    } else if stream.result_open_match > 0 {
                        // Partial match failed — these bytes were '<|tool_r...' which
                        // isn't a real result tag. They fall through to tool/visible.
                        // For simplicity, just reset and let the current byte through.
                        stream.result_open_match = 0;
                        // Fall through to process current byte
                    }
                } else {
                    // stream.result_state == 1: Inside result block — scan for close tag
                    if byte == RESULT_CLOSE[stream.result_close_match] {
                        stream.result_close_match += 1;
                        if stream.result_close_match == RESULT_CLOSE.len() {
                            stream.result_state = 0;
                            stream.result_close_match = 0;
                        }
                    } else {
                        stream.result_close_match = 0;
                    }
                    continue; // Drop bytes inside result block
                }

                // ── Layer 2: Tool tag filter + visible output ──
                match stream.tool_state {
                    0 => {
                        // Scanning for TOOL_OPEN tag
                        if byte == TOOL_OPEN[stream.tool_open_match] {
                            stream.tool_pending[stream.tool_pending_len] = byte;
                            stream.tool_pending_len += 1;
                            stream.tool_open_match += 1;
                            if stream.tool_open_match == TOOL_OPEN.len() {
                                stream.tool_state = 1;
                                stream.tool_open_match = 0;
                                stream.tool_pending_len = 0;
                                stream.tool_buf_len = 0;
                            }
                        } else if stream.tool_open_match > 0 {
                            for j in 0..stream.tool_pending_len {
                                if vis_len < visible_buf.len() {
                                    visible_buf[vis_len] = stream.tool_pending[j];
                                    vis_len += 1;
                                }
                            }
                            stream.tool_open_match = 0;
                            stream.tool_pending_len = 0;
                            if vis_len < visible_buf.len() {
                                visible_buf[vis_len] = byte;
                                vis_len += 1;
                            }
                        } else {
                            if vis_len < visible_buf.len() {
                                visible_buf[vis_len] = byte;
                                vis_len += 1;
                            }
                        }
                    }
                    1 => {
                        // Buffering tool body, scanning for TOOL_CLOSE
                        if byte == TOOL_CLOSE[stream.tool_close_match] {
                            stream.tool_close_match += 1;
                            if stream.tool_close_match == TOOL_CLOSE.len() {
                                stream.tool_state = 3;
                                stream.tool_close_match = 0;
                            }
                        } else {
                            for j in 0..stream.tool_close_match {
                                if stream.tool_buf_len < stream.tool_buf.len() {
                                    stream.tool_buf[stream.tool_buf_len] = TOOL_CLOSE[j];
                                    stream.tool_buf_len += 1;
                                }
                            }
                            stream.tool_close_match = 0;
                            if stream.tool_buf_len < stream.tool_buf.len() {
                                stream.tool_buf[stream.tool_buf_len] = byte;
                                stream.tool_buf_len += 1;
                            }
                        }
                    }
                    _ => {
                        if vis_len < visible_buf.len() {
                            visible_buf[vis_len] = byte;
                            vis_len += 1;
                        }
                    }
                }
            }

            // Append visible (non-tool) text to window
            if vis_len > 0 {
                if let Some(win) = wm.get_window_mut(stream.win_id) {
                    win.append_text(&visible_buf[..vis_len]);
                }
            }

            // Execute completed tool call + write result back to ring
            if stream.tool_state == 3 {
                let tool_content = core::str::from_utf8(&stream.tool_buf[..stream.tool_buf_len]).unwrap_or("");
                // Pass ring info so result can be written back for AI feedback
                let ring_va = if stream.ring_handle != 0 { RING_VADDR } else { 0 };
                let ring_write = new_write; // current write position in ring
                if let Some(win) = wm.get_window_mut(stream.win_id) {
                    execute_tool_call(tool_content, win, ring_va, ring_write);
                }
                stream.tool_state = 0;
                stream.tool_buf_len = 0;
                need_redraw = true;
            }
            stream.ring_read_idx = new_write;
            need_redraw = true;
        }

        let status = status_atomic.load(Ordering::Acquire);
        if status != 0 {
            // DONE (1) or ERROR (2) — cleanup
            did_work = true;
            let _ = shmem_unmap(stream.ring_handle, RING_VADDR);
            let _ = shmem_destroy(stream.ring_handle);
            let _ = shmem_destroy(stream.query_handle);
            stream.ring_handle = 0;
            stream.query_handle = 0;
            // Flush incomplete tool tag if generation ended mid-tag
            if stream.tool_state != 0 {
                stream.tool_state = 0;
                stream.tool_open_match = 0;
                stream.tool_close_match = 0;
                stream.tool_buf_len = 0;
                stream.tool_pending_len = 0;
            }
            if let Some(win) = wm.get_window_mut(stream.win_id) {
                win.typing = false;
                win.push_line(""); // new line after AI response
                if status == 2 {
                    win.push_line("[AI] Error during generation");
                }
            }
            need_redraw = true;
        }
    }

    // ===== AI Think Overlay =====
    // Semi-transparent panel showing AI reasoning in real-time
    if (stream.think_active || stream.think_fade_timer > 0) && stream.think_display_len > 0 {
        // Overlay dimensions: top-right corner, 400px wide
        let overlay_w = 400usize;
        let overlay_x = fb.width.saturating_sub(overlay_w + 16);
        let overlay_y = 40usize;

        // Extract last N lines from think buffer (show most recent reasoning)
        let think_text = unsafe {
            core::str::from_utf8_unchecked(&stream.think_display[..stream.think_display_len])
        };

        // Count lines and find start of last 8 lines
        let max_lines = 8usize;
        let mut line_starts = [0usize; 9]; // up to 8 lines + sentinel
        let mut line_count = 0usize;
        let bytes = think_text.as_bytes();
        line_starts[0] = 0;
        for i in 0..bytes.len() {
            if bytes[i] == b'\n' && line_count < max_lines {
                line_count += 1;
                line_starts[line_count] = i + 1;
            }
        }
        if line_count == 0 { line_count = 1; } // at least 1 line

        // Show last max_lines lines
        let first_line = if line_count > max_lines { line_count - max_lines } else { 0 };
        let display_lines = line_count - first_line;
        let overlay_h = 28 + display_lines * 18;

        // Alpha for fade-out effect
        let alpha = if stream.think_active { 200u8 } else {
            (stream.think_fade_timer as u16 * 200 / 120).min(200) as u8
        };

        // Draw semi-transparent background
        fb.fill_rect_alpha(overlay_x, overlay_y, overlay_w, overlay_h, 0x0a0a1e, alpha);

        // Header: "AI Thinking..." or "AI Thought"
        let header = if stream.think_active { "AI Thinking..." } else { "AI Thought" };
        let header_color = if stream.think_active { 0x00ccff } else { 0x666688 };
        fb.draw_string(overlay_x + 8, overlay_y + 6, header,
            fb.color_from_rgb24(header_color), fb.color_from_rgb24(0));

        // Draw reasoning lines
        let text_color = fb.color_from_rgb24(if stream.think_active { 0xaaaacc } else { 0x666688 });
        let bg_color = fb.color_from_rgb24(0);
        for li in 0..display_lines {
            let idx = first_line + li;
            let start = line_starts[idx];
            let end = if idx + 1 <= line_count {
                line_starts[idx + 1].min(stream.think_display_len)
            } else {
                stream.think_display_len
            };
            if start < end {
                // Truncate long lines
                let line_end = end.min(start + 48);
                let line = unsafe {
                    core::str::from_utf8_unchecked(&stream.think_display[start..line_end])
                };
                let line_trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                if !line_trimmed.is_empty() {
                    fb.draw_string(overlay_x + 8, overlay_y + 24 + li * 18,
                        line_trimmed, text_color, bg_color);
                }
            }
        }

        let overlay_w_u32 = 400;
        damage.add_damage(compositor::damage::Rect::new(
            overlay_x as u32, overlay_y as u32, overlay_w_u32, overlay_h as u32));
        need_redraw = true;
    }

    // Decrement fade timer
    if stream.think_fade_timer > 0 {
        stream.think_fade_timer -= 1;
        if stream.think_fade_timer == 0 {
            need_redraw = true; // final redraw to clear overlay
        }
    }

    AiTickResult { did_work, need_redraw }
}
