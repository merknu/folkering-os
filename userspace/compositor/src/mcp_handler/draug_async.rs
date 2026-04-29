//! Draug Async Tick — non-blocking refactor iteration via EAGAIN TCP.
//!
//! Handles both Skill Tree (L1-L3) and Phase 15 (Plan-and-Solve).
//! Every call returns in <1ms. UI renders between calls.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use libfolk::sys::io::write_str;
use libfolk::sys::{tcp_connect_async, tcp_send_async, tcp_poll_recv, tcp_close_async, TCP_EAGAIN};
use compositor::draug::{AsyncPhase, AsyncOp, DraugDaemon, PlanStep};

use super::knowledge_hunt::{write_dec, extract_rust_code_block, push_decimal, REFACTOR_TASKS};
use super::agent_planner::COMPLEX_TASKS;

const PROXY_IP: [u8; 4] = [10, 0, 2, 2];
const PROXY_PORT: u16 = 14711;

/// Non-blocking Draug tick. Called every compositor frame (~60Hz).
pub(super) fn tick_async(draug: &mut DraugDaemon, now_ms: u64) -> bool {
    // Timeout check: if any non-Idle phase exceeds 90s, force abort.
    // Prevents permanent hang when proxy stops responding mid-transfer.
    if draug.async_phase != AsyncPhase::Idle && draug.async_phase != AsyncPhase::Processing {
        let elapsed = now_ms.saturating_sub(draug.async_phase_started_ms);
        if elapsed > compositor::draug::ASYNC_TIMEOUT_MS {
            write_str("[Draug-async] TIMEOUT after ");
            write_dec((elapsed / 1000) as u32);
            write_str("s in ");
            let phase_name = match &draug.async_phase {
                AsyncPhase::Connecting => "Connecting",
                AsyncPhase::Sending => "Sending",
                AsyncPhase::Reading => "Reading",
                _ => "?",
            };
            write_str(phase_name);
            write_str(" — aborting\n");

            // Clean up TCP slot
            if draug.async_tcp_slot != 0xFFFF {
                tcp_close_async(draug.async_tcp_slot);
                draug.async_tcp_slot = 0xFFFF;
            }
            draug.async_phase = AsyncPhase::Idle;
            draug.async_operation = AsyncOp::None;
            draug.record_skip();
            return true;
        }
    }

    match draug.async_phase.clone() {
        AsyncPhase::Idle => tick_idle(draug, now_ms),
        // Connecting is handled by tick_sending — tcp_send_async returns
        // EAGAIN until the TCP handshake completes, which auto-promotes
        // the slot from Connecting→Connected.
        AsyncPhase::Connecting | AsyncPhase::Sending => tick_sending(draug),
        AsyncPhase::Reading => tick_reading(draug),
        AsyncPhase::Processing => tick_processing(draug, now_ms),
    }
}

// ── IDLE: pick task, build request, start TCP connect ───────────────

fn tick_idle(draug: &mut DraugDaemon, now_ms: u64) -> bool {
    // Reclaim heap from previous async operation. shrink_to(0) releases
    // excess capacity so we don't leak ~8KB per iteration over 24h.
    if draug.async_response.capacity() > 0 {
        draug.async_response = alloc::vec::Vec::new();
    }
    if draug.async_request.capacity() > 0 {
        draug.async_request = alloc::vec::Vec::new();
    }
    if !draug.should_run_refactor_step(now_ms) { return false; }
    draug.last_refactor_ms = now_ms;

    match draug.next_task_and_level() {
        Some((task_idx, level)) => start_skill_tree(draug, task_idx, level, now_ms),
        None => {
            // Skill-tree complete — try Phase 17 autonomous refactor
            // before falling through to Phase 15. Refactor work is
            // gated on (a) the task queue being loaded and (b) the
            // per-boot iteration cap. `start_refactor_iteration`
            // returns false when no refactor work is available, so
            // we cleanly fall through.
            if draug.refactor_budget_remaining()
                && start_refactor_iteration(draug, now_ms)
            {
                return true;
            }
            start_phase15(draug, now_ms)
        }
    }
}

/// Skill tree L1-L3: build LLM prompt, start async TCP.
fn start_skill_tree(draug: &mut DraugDaemon, task_idx: usize, level: u8, now_ms: u64) -> bool {
    let (task_id, task_desc) = REFACTOR_TASKS[task_idx];
    let model = compositor::draug::model_for_level(level);
    let prompt = super::knowledge_hunt::build_level_prompt(
        level, task_id, task_desc, draug.get_task_code(task_idx),
    );

    // Set task label for shell
    {
        let mut label = String::with_capacity(32);
        label.push_str(task_id);
        label.push_str(" L");
        push_decimal(&mut label, level as u32);
        libfolk::sys::draug_bridge_set_task(&label);
    }

    write_str("\n[Draug-async] ");
    write_str(task_id);
    write_str(" L");
    write_dec(level as u32);
    write_str(" → LLM\n");

    draug.async_task_idx = task_idx;
    draug.async_level = level;
    draug.async_attempt = 0;
    draug.async_operation = AsyncOp::LlmGenerate;
    draug.async_phase_started_ms = now_ms;
    start_llm_request(draug, model, &prompt)
}

/// Phase 15: decide whether to plan a new task or execute next step.
fn start_phase15(draug: &mut DraugDaemon, now_ms: u64) -> bool {
    if !draug.plan_mode_active {
        draug.plan_mode_active = true;
        draug.save_state();
        write_str("\n[Draug-async] *** PHASE 15 activated ***\n");
    }

    // Check if active plan has pending steps
    if let Some(ref plan) = draug.active_plan {
        if !plan.completed {
            if let Some(step_idx) = plan.steps.iter().position(|s| !s.done) {
                return start_executor_step(draug, step_idx, now_ms);
            }
        }
    }

    // Need a new plan
    if draug.complex_task_idx >= COMPLEX_TASKS.len() {
        write_str("[Draug-async] All complex tasks done!\n");
        return false;
    }

    let (task_id, task_desc) = COMPLEX_TASKS[draug.complex_task_idx];
    write_str("\n[Draug-async] [PLAN-NEW] ");
    write_str(task_id);
    write_str("\n");

    libfolk::sys::draug_bridge_set_task(task_id);

    // Build planner prompt
    let mut prompt = String::with_capacity(512);
    prompt.push_str("You are a chief software architect. Break down the following ");
    prompt.push_str("coding task into 3 to 5 implementation steps. ");
    prompt.push_str("Respond with ONLY one step per line, formatted exactly as: ");
    prompt.push_str("STEP|Short description of the step\n");
    prompt.push_str("No other text, no numbering, no blank lines.\n\n");
    prompt.push_str("Task: Write ");
    prompt.push_str(task_desc);

    draug.async_operation = AsyncOp::PlannerLlm;
    draug.async_phase_started_ms = now_ms;
    start_llm_request(draug, compositor::draug::PLANNER_MODEL, &prompt)
}

/// Phase 15: build executor prompt for a specific step.
fn start_executor_step(draug: &mut DraugDaemon, step_idx: usize, now_ms: u64) -> bool {
    let plan = draug.active_plan.as_ref().unwrap();
    let step_desc = &plan.steps[step_idx].description;

    write_str("[Draug-async] [EXEC] step ");
    write_dec((step_idx + 1) as u32);
    write_str("/");
    write_dec(plan.steps.len() as u32);
    write_str(": ");
    write_str(&step_desc[..step_desc.len().min(50)]);
    write_str("\n");

    // Gather prior code
    let mut prior_code = String::with_capacity(4096);
    for prev in &plan.steps[..step_idx] {
        if let Some(ref code) = prev.code {
            if prior_code.len() + code.len() > 8192 { break; }
            if !prior_code.is_empty() { prior_code.push('\n'); }
            prior_code.push_str(code);
        }
    }

    let mut prompt = String::with_capacity(2048);
    prompt.push_str("You are building: ");
    prompt.push_str(&plan.task_desc);
    prompt.push_str("\n\n");
    if !prior_code.is_empty() {
        prompt.push_str("Here is the code written so far:\n```rust\n");
        prompt.push_str(&prior_code);
        prompt.push_str("\n```\n\n");
    }
    prompt.push_str("Current step: ");
    prompt.push_str(step_desc);
    prompt.push_str("\n\nWrite ONLY the code in a ```rust fenced block. ");
    if !prior_code.is_empty() {
        prompt.push_str("Include all previous code plus additions. ");
    }
    prompt.push_str("Must compile as lib.rs. No explanation.");

    draug.async_task_idx = step_idx;
    draug.async_operation = AsyncOp::ExecutorLlm;
    draug.async_phase_started_ms = now_ms;
    start_llm_request(draug, compositor::draug::EXECUTOR_MODEL, &prompt)
}

/// Phase 17 — pick the next pending refactor task, fetch its source
/// from the host via the FETCH_SOURCE syscall, build the refactor
/// prompt (with model-conditional caller list), and fire LlmGenerate.
///
/// Returns true if the iteration started (or terminally short-
/// circuited — no work, fetch_source failed, etc). The caller in
/// `tick_idle` interprets `true` as "we did something this tick".
pub(super) fn start_refactor_iteration(draug: &mut DraugDaemon, now_ms: u64) -> bool {
    let task_idx = match draug.pick_next_refactor() {
        Some(i) => i,
        None => return false, // No work — caller falls through to phase15.
    };

    // Snapshot the task fields so we can drop the immutable borrow
    // before mutating draug below.
    let (task_id, target_file, target_fn, goal, attempts) = {
        let tasks = draug.refactor_tasks.as_ref().unwrap();
        let t = &tasks[task_idx];
        (
            t.id.clone(),
            t.target_file.clone(),
            t.target_fn.clone(),
            t.goal.clone(),
            t.attempts,
        )
    };

    write_str("\n[Draug-async] [REFACTOR] ");
    write_str(&task_id);
    write_str(" attempt ");
    write_dec(attempts + 1);
    write_str(" → FETCH_SOURCE\n");

    libfolk::sys::draug_bridge_set_task(&task_id);

    // Fetch the original source from the host. Synchronous syscall —
    // fast (single tcp_request, ≪ 1 s on the LAN). For files larger
    // than 64 KB we'd need a chunked path; the fixture targets are
    // all well below that.
    let mut fetch_buf = alloc::vec::Vec::with_capacity(64 * 1024);
    fetch_buf.resize(64 * 1024, 0u8);
    let fetch_res = libfolk::sys::fetch_source(&target_file, &mut fetch_buf);
    let source = match fetch_res {
        Some(p) if p.status == libfolk::sys::FS_STATUS_OK => {
            fetch_buf.truncate(p.output_len);
            match alloc::string::String::from_utf8(fetch_buf) {
                Ok(s) => s,
                Err(_) => {
                    write_str("[Draug-async] FETCH_SOURCE: non-UTF-8 body\n");
                    record_refactor_failure(draug, task_idx, "non-UTF-8 source");
                    return true;
                }
            }
        }
        Some(p) => {
            write_str("[Draug-async] FETCH_SOURCE failed status=");
            write_dec(p.status);
            write_str("\n");
            record_refactor_failure(draug, task_idx, "fetch_source non-OK");
            return true;
        }
        None => {
            write_str("[Draug-async] FETCH_SOURCE: TCP/syscall failure\n");
            record_refactor_failure(draug, task_idx, "fetch_source transport");
            return true;
        }
    };

    write_str("[Draug-async] FETCH_SOURCE OK ");
    write_dec(source.len() as u32);
    write_str("B\n");

    // Build prompt with the same shape the eval-runner uses. The
    // caller list is pulled in only when `codegraph_for_model` says
    // so — for qwen-coder:7b that's "yes", which improves pass-rate
    // by +20 pp on the fixture set per cross-model trial 001.
    let task = compositor::refactor_types::RefactorTask {
        id: task_id.clone(),
        target_file: target_file.clone(),
        target_fn,
        goal,
        attempts,
        last_status: compositor::refactor_types::TaskStatus::Pending,
    };
    let prompt = super::refactor_loop::build_refactor_prompt(
        &task, &source, compositor::draug::REFACTOR_MODEL,
    );

    write_str("[Draug-async] prompt ");
    write_dec(prompt.len() as u32);
    write_str("B → LLM\n");

    draug.current_refactor_idx = task_idx;
    draug.current_refactor_target = target_file;
    draug.async_operation = AsyncOp::RefactorLlm;
    draug.async_phase_started_ms = now_ms;
    draug.refactor_iterations_done = draug.refactor_iterations_done.saturating_add(1);
    start_llm_request(draug, compositor::draug::REFACTOR_MODEL, &prompt)
}

/// Persist a refactor failure that hit before the LLM was even
/// queried (FETCH_SOURCE failed, etc). Increments attempts +
/// records Skip so the loop moves on instead of retrying forever.
fn record_refactor_failure(draug: &mut DraugDaemon, task_idx: usize, _reason: &str) {
    if let Some(tasks) = draug.refactor_tasks.as_mut() {
        if task_idx < tasks.len() {
            tasks[task_idx].attempts = tasks[task_idx].attempts.saturating_add(1);
            tasks[task_idx].last_status = compositor::refactor_types::TaskStatus::Skip;
        }
    }
    if let Some(ref tasks) = draug.refactor_tasks {
        let _ = super::task_store::save(tasks);
    }
    draug.record_skip();
}

// ── Shared: build LLM wire frame and start TCP connect ──────────────

fn start_llm_request(draug: &mut DraugDaemon, model: &str, prompt: &str) -> bool {
    let mut req = Vec::with_capacity(prompt.len() + 64);
    req.extend_from_slice(b"LLM ");
    req.extend_from_slice(model.as_bytes());
    req.push(b'\n');
    encode_decimal(&mut req, prompt.len());
    req.push(b'\n');
    req.extend_from_slice(prompt.as_bytes());

    draug.async_request = req;
    draug.async_sent = 0;
    // Pre-allocate 8KB to avoid repeated realloc during reads
    if draug.async_response.capacity() < 8192 {
        draug.async_response.reserve(8192);
    }
    draug.async_response.clear();
    // Timestamp set by caller (tick_idle passes now_ms from compositor clock)

    let result = tcp_connect_async(PROXY_IP, PROXY_PORT);
    if result == u64::MAX {
        write_str("[Draug-async] connect failed (no slots)\n");
        draug.async_phase = AsyncPhase::Idle;
        draug.record_skip();
        return true;
    }
    draug.async_tcp_slot = result;
    draug.async_phase = AsyncPhase::Sending;
    true
}

fn start_patch_request(draug: &mut DraugDaemon, code: &str) -> bool {
    let mut req = Vec::with_capacity(code.len() + 64);
    req.extend_from_slice(b"PATCH draug_latest.rs\n");
    encode_decimal(&mut req, code.len());
    req.push(b'\n');
    req.extend_from_slice(code.as_bytes());

    draug.async_request = req;
    draug.async_sent = 0;
    // Pre-allocate 8KB to avoid repeated realloc during reads
    if draug.async_response.capacity() < 8192 {
        draug.async_response.reserve(8192);
    }
    draug.async_response.clear();
    draug.async_operation = AsyncOp::FbpPatch;
    // Timestamp set by caller (tick_idle passes now_ms from compositor clock)

    let result = tcp_connect_async(PROXY_IP, PROXY_PORT);
    if result == u64::MAX {
        write_str("[Draug-async] connect failed (no slots)\n");
        draug.async_phase = AsyncPhase::Idle;
        draug.record_skip();
        return true;
    }
    draug.async_tcp_slot = result;
    draug.async_phase = AsyncPhase::Sending;
    true
}

// ── SENDING / READING ────────────────────────────────────────────────

fn tick_sending(draug: &mut DraugDaemon) -> bool {
    // Guard: if slot is invalid (shouldn't happen), reset to idle
    if draug.async_tcp_slot == 0xFFFF {
        write_str("[Draug-async] BUG: sending with invalid slot\n");
        draug.async_phase = AsyncPhase::Idle;
        draug.record_skip();
        return true;
    }
    let remaining = &draug.async_request[draug.async_sent..];
    if remaining.is_empty() {
        draug.async_phase = AsyncPhase::Reading;
        // Pre-allocate 8KB to avoid repeated realloc during reads
    if draug.async_response.capacity() < 8192 {
        draug.async_response.reserve(8192);
    }
    draug.async_response.clear();
        return true;
    }
    let result = tcp_send_async(draug.async_tcp_slot, remaining);
    if result == TCP_EAGAIN { return false; }
    if result == u64::MAX {
        tcp_close_async(draug.async_tcp_slot);
        draug.async_phase = AsyncPhase::Idle;
        draug.record_skip();
        return true;
    }
    draug.async_sent += result as usize;
    false
}

fn tick_reading(draug: &mut DraugDaemon) -> bool {
    let mut buf = [0u8; 4096];
    let result = tcp_poll_recv(draug.async_tcp_slot, &mut buf);
    if result == TCP_EAGAIN { return false; }
    if result == u64::MAX {
        tcp_close_async(draug.async_tcp_slot);
        draug.async_phase = AsyncPhase::Idle;
        draug.record_skip();
        return true;
    }
    if result == 0 {
        tcp_close_async(draug.async_tcp_slot);
        draug.async_tcp_slot = 0xFFFF;
        draug.async_phase = AsyncPhase::Processing;
        return true;
    }
    draug.async_response.extend_from_slice(&buf[..result as usize]);
    if draug.async_response.len() > 65536 {
        tcp_close_async(draug.async_tcp_slot);
        draug.async_tcp_slot = 0xFFFF;
        draug.async_phase = AsyncPhase::Processing;
    }
    false
}

// ── PROCESSING: parse response based on async_operation ─────────────

fn tick_processing(draug: &mut DraugDaemon, now_ms: u64) -> bool {
    let response = core::mem::take(&mut draug.async_response);

    match draug.async_operation.clone() {
        AsyncOp::LlmGenerate => process_skill_llm(draug, &response, now_ms),
        AsyncOp::FbpPatch => process_patch_result(draug, &response, now_ms),
        AsyncOp::PlannerLlm => process_planner_response(draug, &response),
        AsyncOp::ExecutorLlm => process_executor_llm(draug, &response, now_ms),
        // Phase 17 — autonomous refactor loop. Both arms are
        // explicitly handled (rather than falling into the `_ =>`
        // catch-all) so the next session can wire tick_idle into
        // them without revisiting the dispatch shape.
        AsyncOp::RefactorLlm => process_refactor_llm(draug, &response, now_ms),
        AsyncOp::CargoCheck => process_cargo_check_result(draug, &response, now_ms),
        _ => { draug.async_phase = AsyncPhase::Idle; true }
    }
}

/// Phase 17 — handle the LLM's response to a refactor prompt.
/// Extracts the rust code block, builds a CARGO_CHECK request frame,
/// and fires another async TCP round-trip — same shape as the skill-
/// tree LLM→PATCH transition.
fn process_refactor_llm(draug: &mut DraugDaemon, response: &[u8], now_ms: u64) -> bool {
    let code = match parse_llm_response(response) {
        Some(c) => c,
        None => {
            write_str("[Draug-async] Refactor LLM: parse failed\n");
            persist_refactor_skip(draug);
            draug.async_phase = AsyncPhase::Idle;
            draug.async_operation = AsyncOp::None;
            draug.record_skip();
            return true;
        }
    };

    write_str("[Draug-async] Refactor LLM OK → ");
    write_dec(code.len() as u32);
    write_str("B → CARGO_CHECK\n");

    // Build the proxy request. `build_cargo_check_request` is unit-
    // tested in refactor_loop so the wire shape stays in sync with
    // what the proxy parses.
    let target = draug.current_refactor_target.clone();
    let req = super::refactor_loop::build_cargo_check_request(&target, &code);

    draug.async_request = req;
    draug.async_sent = 0;
    if draug.async_response.capacity() < 8192 {
        draug.async_response.reserve(8192);
    }
    draug.async_response.clear();
    draug.async_operation = AsyncOp::CargoCheck;
    draug.async_phase_started_ms = now_ms;

    let result = libfolk::sys::tcp_connect_async(PROXY_IP, PROXY_PORT);
    if result == u64::MAX {
        write_str("[Draug-async] CARGO_CHECK connect failed (no slots)\n");
        persist_refactor_skip(draug);
        draug.async_phase = AsyncPhase::Idle;
        draug.async_operation = AsyncOp::None;
        draug.record_skip();
        return true;
    }
    draug.async_tcp_slot = result;
    draug.async_phase = AsyncPhase::Sending;
    true
}

/// Persist a Skip verdict for the in-flight refactor. Used when
/// the loop hits an infrastructure problem (LLM parse failure,
/// connect-no-slots, etc) where we don't want the failure to count
/// against the model's own retry budget.
fn persist_refactor_skip(draug: &mut DraugDaemon) {
    let idx = draug.current_refactor_idx;
    if idx == usize::MAX { return; }
    if let Some(tasks) = draug.refactor_tasks.as_mut() {
        if idx < tasks.len() {
            tasks[idx].attempts = tasks[idx].attempts.saturating_add(1);
            tasks[idx].last_status = compositor::refactor_types::TaskStatus::Skip;
        }
    }
    if let Some(ref tasks) = draug.refactor_tasks {
        let _ = super::task_store::save(tasks);
    }
    draug.current_refactor_idx = usize::MAX;
    draug.current_refactor_target.clear();
}

/// Phase 17 — handle the proxy's CARGO_CHECK reply for a refactor
/// task. Maps status to a verdict, calls `record_attempt`, persists
/// the queue back to Synapse VFS, and clears the in-flight pointer.
fn process_cargo_check_result(
    draug: &mut DraugDaemon,
    response: &[u8],
    _now_ms: u64,
) -> bool {
    let idx = draug.current_refactor_idx;

    let header = super::refactor_loop::parse_cargo_check_header(response);
    let status = match header {
        Some((s, output_len)) => {
            write_str("[Draug-async] CARGO_CHECK status=");
            write_dec(s);
            write_str(" output=");
            write_dec(output_len);
            write_str("B\n");
            s
        }
        None => {
            write_str("[Draug-async] CARGO_CHECK: short/empty reply\n");
            draug.record_skip();
            // Treat short reply as Skip — protocol error, not the model's fault.
            persist_refactor_skip(draug);
            draug.async_phase = AsyncPhase::Idle;
            draug.async_operation = AsyncOp::None;
            return true;
        }
    };

    if idx == usize::MAX {
        write_str("[Draug-async] CARGO_CHECK: stale (no in-flight idx)\n");
        draug.async_phase = AsyncPhase::Idle;
        draug.async_operation = AsyncOp::None;
        return true;
    }

    let verdict = super::refactor_loop::verdict_from_cargo_check_status(status);
    if let Some(tasks) = draug.refactor_tasks.as_mut() {
        if idx < tasks.len() {
            super::refactor_loop::record_attempt(&mut tasks[idx], verdict);
        }
    }

    // Persist immediately so a crash mid-loop doesn't lose the verdict.
    if let Some(ref tasks) = draug.refactor_tasks {
        if let Err(e) = super::task_store::save(tasks) {
            write_str("[Draug-async] task_store::save failed: ");
            // StoreError doesn't implement no_std `core::fmt::Display`
            // beyond Debug, so just stringify a tag.
            let _ = e; // suppress unused warning when not displayed
            write_str("(see prior log)\n");
        }
    }

    // Verdict-aware tracking: bump pass/fail counters for the shell badge.
    use super::refactor_loop::AttemptVerdict;
    match verdict {
        AttemptVerdict::Pass             => draug.record_refactor_pass(),
        AttemptVerdict::FailCompile
        | AttemptVerdict::FailCallerCompat => draug.record_refactor_fail(),
        AttemptVerdict::Skip             => draug.record_skip(),
    }

    draug.current_refactor_idx = usize::MAX;
    draug.current_refactor_target.clear();
    draug.async_phase = AsyncPhase::Idle;
    draug.async_operation = AsyncOp::None;
    true
}

/// Skill tree: LLM returned code → extract → start PATCH.
fn process_skill_llm(draug: &mut DraugDaemon, response: &[u8], now_ms: u64) -> bool {
    let code = match parse_llm_response(response) {
        Some(c) => c,
        None => {
            write_str("[Draug-async] LLM parse failed\n");
            draug.async_phase = AsyncPhase::Idle;
            draug.record_skip();
            return true;
        }
    };

    write_str("[Draug-async] LLM OK → ");
    write_dec(code.len() as u32);
    write_str("B → PATCH\n");

    if draug.async_level == 1 {
        draug.store_task_code(draug.async_task_idx, code.clone());
        draug.save_task_code(draug.async_task_idx);
    }

    draug.async_phase_started_ms = now_ms;
    start_patch_request(draug, &code);
    true
}

/// Skill tree / Phase 15: PATCH result → advance or fail.
fn process_patch_result(draug: &mut DraugDaemon, response: &[u8], now_ms: u64) -> bool {
    if response.len() < 8 {
        write_str("[Draug-async] PATCH: short/empty response\n");
        // Treat as transient failure — don't penalize task
        draug.record_skip();
        draug.async_phase = AsyncPhase::Idle;
        return true;
    }
    let status = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);

    match draug.async_operation.clone() {
        // Could be skill tree or Phase 15 executor
        _ => {}
    }

    // Check which context we're in
    let is_phase15 = draug.plan_mode_active && draug.active_plan.is_some();

    if status == 0 {
        if is_phase15 {
            // Mark step done in plan
            let step_idx = draug.async_task_idx;
            if let Some(ref mut plan) = draug.active_plan {
                if step_idx < plan.steps.len() {
                    // Extract code from the request (it was the PATCH payload)
                    let code = extract_code_from_patch_request(&draug.async_request);
                    plan.steps[step_idx].code = Some(code);
                    plan.steps[step_idx].done = true;

                    write_str("[Draug-async] ");
                    write_str(&plan.task_id);
                    write_str(" step ");
                    write_dec((step_idx + 1) as u32);
                    write_str(" PASS\n");

                    if plan.steps.iter().all(|s| s.done) {
                        plan.completed = true;
                        write_str("[Draug-async] === ");
                        write_str(&plan.task_id);
                        write_str(" COMPLETE ===\n");
                    }
                }
            }
        } else {
            // Skill tree PASS
            let iter = draug.advance_refactor(now_ms);
            draug.record_refactor_pass();
            draug.advance_task_level(draug.async_task_idx);
            draug.reset_skips();
            draug.clear_task_error(draug.async_task_idx);
            draug.save_state();

            let (task_id, _) = REFACTOR_TASKS[draug.async_task_idx];
            write_str("[Draug-async] ");
            write_str(task_id);
            write_str(" L");
            write_dec(draug.async_level as u32);
            write_str(" PASS\n");

            let at_l1 = draug.tasks_at_level(1);
            let at_l2 = draug.tasks_at_level(2);
            let at_l3 = draug.tasks_at_level(3);
            write_str("[Draug-async] Skill: L1=");
            write_dec(at_l1 as u32);
            write_str("/20 L2=");
            write_dec(at_l2 as u32);
            write_str("/20 L3=");
            write_dec(at_l3 as u32);
            write_str("/20\n");
        }
    } else {
        // FAIL — attempt error-driven retry (max 2)
        if draug.async_attempt < 2 {
            draug.async_attempt += 1;

            // Extract error from PATCH response
            let err_len = response.len().saturating_sub(8).min(1024);
            let error_text = if err_len > 0 {
                core::str::from_utf8(&response[8..8 + err_len]).unwrap_or("(parse error)")
            } else {
                "(no error text)"
            };

            write_str("[Draug-async] FAIL → retry #");
            write_dec(draug.async_attempt as u32);
            write_str(" with compiler feedback\n");

            // Extract the code we sent (from the PATCH request)
            let failed_code = extract_code_from_patch_request(&draug.async_request);

            // Build retry prompt
            let mut retry_prompt = String::with_capacity(failed_code.len() + 512);
            retry_prompt.push_str("Your previous code failed compilation.\n\n[YOUR CODE]\n```rust\n");
            retry_prompt.push_str(&failed_code);
            retry_prompt.push_str("\n```\n\n[COMPILER ERROR]\n```\n");
            retry_prompt.push_str(&error_text[..error_text.len().min(1024)]);
            retry_prompt.push_str("\n```\n\nFix the errors. Respond with the FIXED code in a ```rust block.");

            let model = if is_phase15 {
                compositor::draug::EXECUTOR_MODEL
            } else {
                compositor::draug::model_for_level(draug.async_level)
            };

            draug.async_phase_started_ms = now_ms;
            start_llm_request(draug, model, &retry_prompt);
            // Keep the same async_operation (LlmGenerate or ExecutorLlm)
            if is_phase15 {
                draug.async_operation = AsyncOp::ExecutorLlm;
            } else {
                draug.async_operation = AsyncOp::LlmGenerate;
            }
            return true;
        }

        // Final fail after retries — FORCE ADVANCE to prevent infinite loop.
        // The task at this level is unresolvable. Skip it so the system
        // can progress to the next task instead of retrying forever.
        if is_phase15 {
            write_str("[Draug-async] Phase 15 step FAIL (after retries)\n");
            increment_step_fail(draug);
        } else {
            draug.record_refactor_fail();
            let task_idx = draug.async_task_idx;
            let (task_id, _) = REFACTOR_TASKS[task_idx];
            write_str("[Draug-async] ");
            write_str(task_id);
            write_str(" L");
            write_dec(draug.async_level as u32);
            write_str(" SKIP (unresolvable after ");
            write_dec(draug.async_attempt as u32);
            write_str(" retries)\n");

            // Store the error for future context
            let err_len = response.len().saturating_sub(8).min(512);
            if err_len > 0 {
                if let Ok(s) = core::str::from_utf8(&response[8..8 + err_len]) {
                    draug.store_task_error(task_idx, String::from(&s[..s.len().min(512)]));
                }
            }

            // Force-advance the level so next_task_and_level moves on
            draug.advance_task_level(task_idx);
            draug.save_state();
        }
    }

    draug.async_phase = AsyncPhase::Idle;
    draug.async_operation = AsyncOp::None;
    true
}

/// Phase 15 Planner: parse STEP| lines → create TaskPlan.
fn process_planner_response(draug: &mut DraugDaemon, response: &[u8]) -> bool {
    let raw = match parse_llm_response(response) {
        Some(text) => text,
        None => {
            write_str("[Draug-async] Planner LLM failed\n");
            draug.complex_task_idx += 1; // skip this task
            draug.async_phase = AsyncPhase::Idle;
            return true;
        }
    };

    // Parse STEP| lines
    let mut steps = Vec::new();
    for line in raw.split('\n') {
        let trimmed = line.trim();
        if let Some(desc) = trimmed.strip_prefix("STEP|") {
            let desc = desc.trim();
            if !desc.is_empty() && steps.len() < 5 {
                steps.push(PlanStep {
                    description: String::from(desc),
                    code: None,
                    done: false,
                    fail_count: 0,
                });
            }
        }
    }

    if steps.is_empty() {
        write_str("[Draug-async] No STEP| lines → skip\n");
        draug.complex_task_idx += 1;
        draug.async_phase = AsyncPhase::Idle;
        return true;
    }

    let (task_id, task_desc) = COMPLEX_TASKS[draug.complex_task_idx];
    write_str("[Draug-async] Planned ");
    write_dec(steps.len() as u32);
    write_str(" steps for ");
    write_str(task_id);
    write_str("\n");

    drop(draug.active_plan.take());
    draug.active_plan = Some(compositor::draug::TaskPlan {
        task_id: String::from(task_id),
        task_desc: String::from(task_desc),
        steps,
        current_step: 0,
        completed: false,
    });
    draug.complex_task_idx += 1;
    draug.save_state();

    draug.async_phase = AsyncPhase::Idle;
    true
}

/// Phase 15 Executor: LLM returned code → start PATCH.
fn process_executor_llm(draug: &mut DraugDaemon, response: &[u8], now_ms: u64) -> bool {
    let code = match parse_llm_response(response) {
        Some(text) => {
            let extracted = extract_rust_code_block(&text);
            if extracted.is_empty() { text } else { extracted }
        }
        None => {
            write_str("[Draug-async] Executor LLM failed\n");
            // Increment fail_count so we eventually abandon the step
            increment_step_fail(draug);
            draug.async_phase = AsyncPhase::Idle;
            return true;
        }
    };

    write_str("[Draug-async] Executor → ");
    write_dec(code.len() as u32);
    write_str("B → PATCH\n");

    draug.async_phase_started_ms = now_ms;
    start_patch_request(draug, &code);
    true
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Parse [u32 status][u32 len][text] from LLM response.
/// Returns None on any parse failure — never panics.
fn parse_llm_response(response: &[u8]) -> Option<String> {
    if response.len() < 8 { return None; }
    let status = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
    if status != 0 { return None; }
    let output_len = u32::from_le_bytes([response[4], response[5], response[6], response[7]]) as usize;
    // Guard: cap at actual response length (prevents overflow if output_len is corrupt)
    let text_end = 8usize.saturating_add(output_len).min(response.len());
    let raw = core::str::from_utf8(&response[8..text_end]).ok()?;
    let code = extract_rust_code_block(raw);
    if code.is_empty() && raw.contains("STEP|") {
        // Planner response — return raw text
        Some(String::from(raw))
    } else if code.is_empty() {
        None
    } else {
        Some(code)
    }
}

/// Extract the code payload from a PATCH request (after "PATCH name\nlen\n").
fn extract_code_from_patch_request(request: &[u8]) -> String {
    // Find second \n (after "PATCH draug_latest.rs\nNNN\n")
    let mut newlines = 0;
    for (i, &b) in request.iter().enumerate() {
        if b == b'\n' {
            newlines += 1;
            if newlines == 2 {
                return core::str::from_utf8(&request[i+1..])
                    .map(String::from)
                    .unwrap_or_default();
            }
        }
    }
    String::new()
}

/// Increment fail_count on the current Phase 15 step.
/// After 3 fails, abandon the entire task (prevent infinite loop).
fn increment_step_fail(draug: &mut DraugDaemon) {
    let step_idx = draug.async_task_idx;
    if let Some(ref mut plan) = draug.active_plan {
        if step_idx < plan.steps.len() {
            plan.steps[step_idx].fail_count += 1;
            write_str("[Draug-async] step fail_count=");
            write_dec(plan.steps[step_idx].fail_count as u32);
            write_str("/3\n");
            if plan.steps[step_idx].fail_count >= 3 {
                plan.completed = true;
                write_str("[Draug-async] === ");
                write_str(&plan.task_id);
                write_str(" ABANDONED ===\n");
            }
        }
    }
}

/// Encode a usize as decimal ASCII into a Vec.
fn encode_decimal(out: &mut Vec<u8>, mut n: usize) {
    if n == 0 { out.push(b'0'); return; }
    let mut tmp = [0u8; 12];
    let mut i = 0;
    while n > 0 { tmp[i] = b'0' + (n % 10) as u8; n /= 10; i += 1; }
    for j in 0..i / 2 { tmp.swap(j, i - 1 - j); }
    out.extend_from_slice(&tmp[..i]);
}
