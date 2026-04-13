//! Draug Async Tick — non-blocking refactor iteration via EAGAIN TCP.
//!
//! Replaces the blocking `run_refactor_step()` with a state machine
//! that returns in <1ms on every call. The compositor renders between
//! states — UI never freezes during LLM calls.
//!
//! State flow per iteration:
//!   Idle → pick task, build LLM request
//!   Connecting → tcp_connect_async(proxy) → EAGAIN until connected
//!   Sending → tcp_send_async(request) → EAGAIN until sent
//!   Reading → tcp_poll_recv(response) → EAGAIN until complete
//!   Processing → extract code, start PATCH (back to Connecting)
//!   Idle → advance level, save state

extern crate alloc;

use alloc::string::String;
use libfolk::sys::io::write_str;
use libfolk::sys::{tcp_connect_async, tcp_send_async, tcp_poll_recv, tcp_close_async, TCP_EAGAIN};
use compositor::draug::{AsyncPhase, AsyncOp, DraugDaemon};

use super::knowledge_hunt::{write_dec, extract_rust_code_block, REFACTOR_TASKS};

const PROXY_IP: [u8; 4] = [10, 0, 2, 2];
const PROXY_PORT: u16 = 14711;

/// Non-blocking Draug tick. Called every compositor frame (~60Hz).
/// Returns in <1ms regardless of network state.
pub(super) fn tick_async(draug: &mut DraugDaemon, now_ms: u64) -> bool {
    match draug.async_phase.clone() {
        AsyncPhase::Idle => tick_idle(draug, now_ms),
        AsyncPhase::Connecting => tick_connecting(draug),
        AsyncPhase::Sending => tick_sending(draug),
        AsyncPhase::Reading => tick_reading(draug),
        AsyncPhase::Processing => tick_processing(draug, now_ms),
    }
}

/// Idle: pick next task, build request, start connecting.
fn tick_idle(draug: &mut DraugDaemon, now_ms: u64) -> bool {
    // Gate check (same as blocking version)
    if !draug.should_run_refactor_step(now_ms) { return false; }

    draug.last_refactor_ms = now_ms;

    // Pick task
    let (task_idx, level) = match draug.next_task_and_level() {
        Some(t) => t,
        None => {
            // Skill tree complete — Phase 15 would go here
            // For now, do nothing in async mode for Phase 15
            return false;
        }
    };

    let (task_id, task_desc) = REFACTOR_TASKS[task_idx];

    // Build LLM prompt
    let model = compositor::draug::model_for_level(level);
    let prompt = super::knowledge_hunt::build_level_prompt(
        level, task_id, task_desc, draug.get_task_code(task_idx),
    );

    // Build the wire frame: LLM <model>\n<len>\n<prompt>
    let mut req = alloc::vec::Vec::with_capacity(prompt.len() + 64);
    req.extend_from_slice(b"LLM ");
    req.extend_from_slice(model.as_bytes());
    req.push(b'\n');
    // Decimal-encode prompt length
    let mut tmp = [0u8; 12];
    let mut n = prompt.len();
    let mut idx = 0;
    if n == 0 { tmp[0] = b'0'; idx = 1; }
    else { while n > 0 { tmp[idx] = b'0' + (n % 10) as u8; n /= 10; idx += 1; } }
    for i in 0..idx / 2 { tmp.swap(i, idx - 1 - i); }
    req.extend_from_slice(&tmp[..idx]);
    req.push(b'\n');
    req.extend_from_slice(prompt.as_bytes());

    // Save context
    draug.async_task_idx = task_idx;
    draug.async_level = level;
    draug.async_attempt = 0;
    draug.async_request = req;
    draug.async_sent = 0;
    draug.async_response.clear();
    draug.async_operation = AsyncOp::LlmGenerate;

    // Log
    write_str("\n[Draug-async] iter task=");
    write_str(task_id);
    write_str(" L");
    write_dec(level as u32);
    write_str(" connecting...\n");

    // Update bridge
    {
        let mut label = String::with_capacity(32);
        label.push_str(task_id);
        label.push_str(" L");
        super::knowledge_hunt::push_decimal(&mut label, level as u32);
        libfolk::sys::draug_bridge_set_task(&label);
    }

    // Start TCP connect
    let ip_packed = ((PROXY_IP[0] as u64) << 24) | ((PROXY_IP[1] as u64) << 16)
        | ((PROXY_IP[2] as u64) << 8) | (PROXY_IP[3] as u64);
    let result = tcp_connect_async(PROXY_IP, PROXY_PORT);
    if result == u64::MAX {
        write_str("[Draug-async] SKIP: connect failed\n");
        draug.record_skip();
        return true;
    }

    draug.async_tcp_slot = if result == TCP_EAGAIN { 0xFFFF } else { result };
    draug.async_phase = AsyncPhase::Connecting;
    true
}

/// Connecting: poll until TCP handshake completes.
fn tick_connecting(draug: &mut DraugDaemon) -> bool {
    if draug.async_tcp_slot == 0xFFFF {
        // Still waiting for slot assignment — retry connect
        let result = tcp_connect_async(PROXY_IP, PROXY_PORT);
        if result == TCP_EAGAIN {
            return false; // still connecting, <1ms
        } else if result == u64::MAX {
            write_str("[Draug-async] SKIP: connect failed\n");
            draug.async_phase = AsyncPhase::Idle;
            draug.record_skip();
            return true;
        }
        draug.async_tcp_slot = result;
    }

    // Slot assigned = connected
    draug.async_phase = AsyncPhase::Sending;
    draug.async_sent = 0;
    true
}

/// Sending: push request bytes to TCP socket.
fn tick_sending(draug: &mut DraugDaemon) -> bool {
    let remaining = &draug.async_request[draug.async_sent..];
    if remaining.is_empty() {
        draug.async_phase = AsyncPhase::Reading;
        draug.async_response.clear();
        return true;
    }

    let result = tcp_send_async(draug.async_tcp_slot, remaining);
    if result == TCP_EAGAIN {
        return false; // send buffer full, try next frame
    }
    if result == u64::MAX {
        write_str("[Draug-async] send error\n");
        tcp_close_async(draug.async_tcp_slot);
        draug.async_phase = AsyncPhase::Idle;
        draug.record_skip();
        return true;
    }
    draug.async_sent += result as usize;
    false // more to send, but <1ms
}

/// Reading: accumulate response bytes until peer closes or we have enough.
fn tick_reading(draug: &mut DraugDaemon) -> bool {
    let mut buf = [0u8; 4096];
    let result = tcp_poll_recv(draug.async_tcp_slot, &mut buf);

    if result == TCP_EAGAIN {
        return false; // no data yet, <1ms
    }
    if result == u64::MAX {
        write_str("[Draug-async] recv error\n");
        tcp_close_async(draug.async_tcp_slot);
        draug.async_phase = AsyncPhase::Idle;
        draug.record_skip();
        return true;
    }
    if result == 0 {
        // Peer closed — response complete
        tcp_close_async(draug.async_tcp_slot);
        draug.async_tcp_slot = 0xFFFF;
        draug.async_phase = AsyncPhase::Processing;
        return true;
    }

    // Accumulate data
    draug.async_response.extend_from_slice(&buf[..result as usize]);

    // Safety cap: don't buffer more than 64KB
    if draug.async_response.len() > 65536 {
        tcp_close_async(draug.async_tcp_slot);
        draug.async_tcp_slot = 0xFFFF;
        draug.async_phase = AsyncPhase::Processing;
        return true;
    }

    false // more data may come, <1ms
}

/// Processing: parse response, advance task state.
fn tick_processing(draug: &mut DraugDaemon, now_ms: u64) -> bool {
    let response = &draug.async_response;

    match draug.async_operation {
        AsyncOp::LlmGenerate => {
            // Parse LLM response: [u32 status][u32 len][text]
            if response.len() < 8 {
                write_str("[Draug-async] LLM: short response\n");
                draug.async_phase = AsyncPhase::Idle;
                draug.record_skip();
                return true;
            }
            let status = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
            let output_len = u32::from_le_bytes([response[4], response[5], response[6], response[7]]) as usize;

            if status != 0 {
                write_str("[Draug-async] SKIP: LLM error\n");
                draug.async_phase = AsyncPhase::Idle;
                draug.record_skip();
                return true;
            }

            let text_end = (8 + output_len).min(response.len());
            let raw = match core::str::from_utf8(&response[8..text_end]) {
                Ok(s) => s,
                Err(_) => {
                    write_str("[Draug-async] SKIP: non-UTF8\n");
                    draug.async_phase = AsyncPhase::Idle;
                    return true;
                }
            };

            let code = extract_rust_code_block(raw);
            if code.is_empty() {
                write_str("[Draug-async] SKIP: empty code\n");
                draug.async_phase = AsyncPhase::Idle;
                return true;
            }

            write_str("[Draug-async] LLM OK, ");
            write_dec(code.len() as u32);
            write_str(" bytes code → sending PATCH\n");

            // Build PATCH request
            let filename = b"draug_latest.rs";
            let mut req = alloc::vec::Vec::with_capacity(code.len() + 64);
            req.extend_from_slice(b"PATCH draug_latest.rs\n");
            // Decimal length
            let mut tmp = [0u8; 12];
            let mut n = code.len();
            let mut idx = 0;
            if n == 0 { tmp[0] = b'0'; idx = 1; }
            else { while n > 0 { tmp[idx] = b'0' + (n % 10) as u8; n /= 10; idx += 1; } }
            for i in 0..idx / 2 { tmp.swap(i, idx - 1 - i); }
            req.extend_from_slice(&tmp[..idx]);
            req.push(b'\n');
            req.extend_from_slice(code.as_bytes());

            // Save code for potential retry
            // (reuse async_request for the code text)
            draug.async_request = req;
            draug.async_sent = 0;
            draug.async_response.clear();
            draug.async_operation = AsyncOp::FbpPatch;

            // Store extracted code for L1 persistence
            if draug.async_level == 1 {
                draug.store_task_code(draug.async_task_idx, code);
                draug.save_task_code(draug.async_task_idx);
            }

            // Connect for PATCH
            let result = tcp_connect_async(PROXY_IP, PROXY_PORT);
            draug.async_tcp_slot = if result == TCP_EAGAIN { 0xFFFF } else { result };
            draug.async_phase = AsyncPhase::Connecting;
            return true;
        }

        AsyncOp::FbpPatch => {
            // Parse PATCH response: [u32 status][u32 len][text]
            if response.len() < 8 {
                write_str("[Draug-async] PATCH: short response\n");
                draug.async_phase = AsyncPhase::Idle;
                return true;
            }
            let status = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);

            let task_idx = draug.async_task_idx;
            let level = draug.async_level;
            let (task_id, _) = REFACTOR_TASKS[task_idx];

            if status == 0 {
                // SUCCESS
                let iter = draug.advance_refactor(now_ms);
                draug.record_refactor_pass();
                draug.advance_task_level(task_idx);
                draug.reset_skips();
                draug.clear_task_error(task_idx);
                draug.save_state();

                write_str("[Draug-async] ");
                write_str(task_id);
                write_str(" L");
                write_dec(level as u32);
                write_str(" PASS\n");

                let at_l1 = draug.tasks_at_level(1);
                let at_l2 = draug.tasks_at_level(2);
                let at_l3 = draug.tasks_at_level(3);
                write_str("[Draug-async] Skill tree: L1=");
                write_dec(at_l1 as u32);
                write_str("/20 L2=");
                write_dec(at_l2 as u32);
                write_str("/20 L3=");
                write_dec(at_l3 as u32);
                write_str("/20\n");
            } else {
                // FAIL
                draug.record_refactor_fail();
                write_str("[Draug-async] ");
                write_str(task_id);
                write_str(" L");
                write_dec(level as u32);
                write_str(" FAIL\n");
                // TODO: retry with error feedback (async retry loop)
            }

            draug.async_phase = AsyncPhase::Idle;
            draug.async_operation = AsyncOp::None;
            return true;
        }

        _ => {
            draug.async_phase = AsyncPhase::Idle;
            return true;
        }
    }
}
