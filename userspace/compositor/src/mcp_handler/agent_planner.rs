//! Phase 15 — Agentic Plan-and-Solve.
//!
//! Instead of round-robin execution of simple tasks, Draug can now
//! tackle complex multi-step projects. The flow is:
//!
//! 1. **Planner persona** — LLM breaks a task into 3-5 steps
//!    formatted as `STEP|description`. Steps are stored in the
//!    knowledge graph as TODO_STEP entities with `pending` status.
//!
//! 2. **Executor persona** — finds the first pending step, gathers
//!    code from prior completed steps as context, prompts the LLM,
//!    compiles via fbp_patch, and marks the step `done` on success.
//!
//! 3. **Context chaining** — each step's code is accumulated so the
//!    LLM sees everything built so far. This lets Draug write 50
//!    lines, verify they compile, then feed them as context for the
//!    next 50 lines.
//!
//! The knowledge graph becomes Draug's project management board:
//!
//! ```text
//! task_ringbuffer --has_step--> step_ringbuffer_1 --status--> done
//!                 --has_step--> step_ringbuffer_2 --status--> pending
//!                 --has_step--> step_ringbuffer_3 --status--> pending
//! ```

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use libfolk::sys::io::write_str;
// Graph writes removed — see eviction policy note in plan_task()
use libfolk::sys::{fbp_patch, llm_generate};

use compositor::draug::{TaskPlan, PlanStep};
use super::knowledge_hunt::{extract_rust_code_block, write_dec};

/// Complex tasks that require multi-step planning.
/// Each entry is (task_id, task_description).
/// Length must match `draug::COMPLEX_TASK_COUNT`.
pub const COMPLEX_TASKS: &[(&str, &str)] = &[
    ("ringbuffer", "a lock-free Single-Producer Single-Consumer (SPSC) ring buffer \
     for 4096 f32 values using only static arrays and core::sync::atomic::AtomicUsize, \
     no alloc or Vec allowed, suitable for zero-copy DMA sensor data"),
    ("bump_alloc", "a simple bump allocator that manages a static [u8; 65536] buffer, \
     providing alloc(size, align) -> *mut u8 and reset() methods, all no_std"),
    ("bitset", "a fixed-size Bitset<4096> backed by [u64; 64] with set/clear/test/count_ones \
     methods and an iterator over set bits, all no_std"),
    ("task_queue", "a priority task queue for up to 32 tasks using a static array and \
     a binary heap, with push(priority, task_id) and pop_highest() -> Option<u32>, no_std"),
    ("crc32", "a CRC-32 implementation using a compile-time generated lookup table \
     (const fn), computing checksums over &[u8] slices, no_std"),
    ("utf8_validator", "a UTF-8 validator that takes &[u8] and returns Result<&str, usize> \
     where the error is the byte offset of the first invalid sequence, no_std, \
     without using core::str::from_utf8"),
    ("fixed_point", "a FixedPoint<16> type (Q16.16 format) with add/sub/mul/div and \
     conversion from/to i32 and f32-compatible bit patterns, all no_std"),
    ("trie", "a static trie for prefix matching over a fixed alphabet of a-z, \
     supporting insert (up to 64 keys of max 16 chars) and longest_prefix_match, no_std"),
];

/// Maximum steps per task plan.
const MAX_STEPS: usize = 5;

/// Maximum retries per step.
const MAX_STEP_RETRIES: u8 = 2;

/// Run the Planner persona: ask LLM to decompose a task into steps.
/// Returns a TaskPlan with parsed steps, or None on failure.
pub fn plan_task(task_id: &str, task_desc: &str) -> Option<TaskPlan> {
    write_str("\n[Planner] ============================================\n");
    write_str("[Planner] Planning: ");
    write_str(task_id);
    write_str("\n");

    // Build the Planner persona prompt
    let mut prompt = String::with_capacity(512);
    prompt.push_str("You are a chief software architect. Break down the following ");
    prompt.push_str("coding task into 3 to 5 implementation steps. ");
    prompt.push_str("Respond with ONLY one step per line, formatted exactly as: ");
    prompt.push_str("STEP|Short description of the step\n");
    prompt.push_str("No other text, no numbering, no blank lines.\n\n");
    prompt.push_str("Task: Write ");
    prompt.push_str(task_desc);

    let mut llm_buf = [0u8; 4096];
    let result = llm_generate(compositor::draug::PLANNER_MODEL, &prompt, &mut llm_buf)?;
    if result.status != 0 {
        write_str("[Planner] FAIL: LLM error\n");
        return None;
    }

    let raw_len = result.output_len.min(llm_buf.len());
    let raw = core::str::from_utf8(&llm_buf[..raw_len]).ok()?;

    // Parse STEP|description lines
    let mut steps = Vec::new();
    for line in raw.split('\n') {
        let trimmed = line.trim();
        if let Some(desc) = trimmed.strip_prefix("STEP|") {
            let desc = desc.trim();
            if !desc.is_empty() && steps.len() < MAX_STEPS {
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
        write_str("[Planner] FAIL: no STEP| lines parsed from LLM response\n");
        write_str("[Planner] raw: ");
        write_str(&raw[..raw.len().min(200)]);
        write_str("\n");
        return None;
    }

    write_str("[Planner] ");
    write_dec(steps.len() as u32);
    write_str(" steps planned:\n");
    for (i, step) in steps.iter().enumerate() {
        write_str("[Planner]   ");
        write_dec((i + 1) as u32);
        write_str(". ");
        write_str(&step.description[..step.description.len().min(80)]);
        write_str("\n");
    }

    // NOTE: Knowledge graph writes removed (was: entities + edges per
    // plan step). These consumed ~3KB per plan but were NEVER read back.
    // Draug's operational state lives in task_levels/task_code/task_errors
    // (persisted via draug_state.bin). Graph writes filled the 4MB Synapse
    // DB in ~4 weeks. See eviction policy discussion in CHANGELOG.md.

    Some(TaskPlan {
        task_id: String::from(task_id),
        task_desc: String::from(task_desc),
        steps,
        current_step: 0,
        completed: false,
    })
}

/// Execute the next pending step in a plan.
/// Returns true if a step was attempted (pass or fail).
pub fn execute_next_step(plan: &mut TaskPlan) -> bool {
    // Guard: don't execute steps on completed/abandoned plans
    if plan.completed { return false; }

    // Find first pending step
    let step_idx = match plan.steps.iter().position(|s| !s.done) {
        Some(i) => i,
        None => {
            plan.completed = true;
            write_str("[Executor] All steps done for ");
            write_str(&plan.task_id);
            write_str("!\n");
            return false;
        }
    };

    write_str("\n[Executor] ");
    write_str(&plan.task_id);
    write_str(" step ");
    write_dec((step_idx + 1) as u32);
    write_str("/");
    write_dec(plan.steps.len() as u32);
    write_str(": ");
    write_str(&plan.steps[step_idx].description[..plan.steps[step_idx].description.len().min(60)]);
    write_str("\n");

    // Gather code from completed prior steps (capped at 8KB to
    // prevent unbounded heap growth over many steps).
    let mut prior_code = String::with_capacity(4096);
    for prev in &plan.steps[..step_idx] {
        if let Some(ref code) = prev.code {
            if prior_code.len() + code.len() > 8192 { break; } // cap
            if !prior_code.is_empty() { prior_code.push('\n'); }
            prior_code.push_str(code);
        }
    }

    // Build the Executor prompt with context chaining
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
    prompt.push_str(&plan.steps[step_idx].description);
    prompt.push_str("\n\n");
    prompt.push_str("Write ONLY the code needed for this step. ");
    if !prior_code.is_empty() {
        prompt.push_str("Include all previous code plus your additions ");
        prompt.push_str("so the file compiles as a complete lib.rs. ");
    }
    prompt.push_str("Respond with ONLY code in a ```rust fenced block. ");
    prompt.push_str("Must compile in a fresh no_std-compatible Rust crate. ");
    prompt.push_str("No explanation.");

    // Retry loop with error feedback
    // Pre-allocate error buffer to avoid fragmentation from variable Strings
    let mut attempt = 0u8;
    let mut last_error_buf = [0u8; 1024];
    let mut last_error_len = 0usize;
    let mut last_code = String::new();

    loop {
        let effective_prompt = if attempt == 0 {
            core::mem::take(&mut prompt)
        } else {
            let err_str = core::str::from_utf8(&last_error_buf[..last_error_len]).unwrap_or("");
            let mut retry = String::with_capacity(last_code.len() + err_str.len() + 300);
            retry.push_str("Your previous code failed compilation.\n\n");
            retry.push_str("[YOUR CODE]\n```rust\n");
            retry.push_str(&last_code);
            retry.push_str("\n```\n\n[COMPILER ERROR]\n```\n");
            retry.push_str(err_str);
            retry.push_str("\n```\n\nFix the errors. Respond with the FIXED code in a ```rust block.");
            retry
        };

        if attempt > 0 {
            write_str("[Executor] retry #");
            write_dec(attempt as u32);
            write_str("\n");
        }

        // Call LLM
        let mut llm_buf = [0u8; 8192];
        let llm_result = match llm_generate(compositor::draug::EXECUTOR_MODEL, &effective_prompt, &mut llm_buf) {
            Some(s) => s,
            None => {
                // Stability: Ollama down — skip, will retry next tick
                write_str("[Executor] SKIP: LLM ipc error (will retry)\n");
                return false; // false = no attempt counted
            }
        };
        if llm_result.status != 0 {
            write_str("[Executor] SKIP: LLM status=");
            write_dec(llm_result.status);
            write_str(" (will retry)\n");
            return false;
        }

        let raw_len = llm_result.output_len.min(llm_buf.len());
        let raw = match core::str::from_utf8(&llm_buf[..raw_len]) {
            Ok(s) => s,
            Err(_) => {
                write_str("[Executor] FAIL: non-UTF8\n");
                return true;
            }
        };

        let code = extract_rust_code_block(raw);
        if code.is_empty() {
            write_str("[Executor] FAIL: empty code\n");
            return true;
        }

        write_str("[Executor] extracted ");
        write_dec(code.len() as u32);
        write_str(" bytes\n");

        // Compile via sandbox
        let mut result_buf = [0u8; 4096];
        let patch = match fbp_patch("draug_latest.rs", code.as_bytes(), &mut result_buf) {
            Some(s) => s,
            None => {
                write_str("[Executor] FAIL: patch ipc error\n");
                return true;
            }
        };

        if patch.status == 0 {
            // SUCCESS
            plan.steps[step_idx].code = Some(code);
            plan.steps[step_idx].done = true;
            plan.current_step = step_idx + 1;

            write_str("[Executor] ");
            write_str(&plan.task_id);
            write_str(" step ");
            write_dec((step_idx + 1) as u32);
            write_str(" PASS");
            if attempt > 0 {
                write_str(" (");
                write_dec(attempt as u32);
                write_str(" retries)");
            }
            write_str("\n");

            // Check if all steps are done
            if plan.steps.iter().all(|s| s.done) {
                plan.completed = true;
                write_str("[Executor] === ");
                write_str(&plan.task_id);
                write_str(" COMPLETE ===\n");

                // Phase 16: compile to WASM and deploy into the OS
                deploy_wasm(&plan.task_id);
            }

            return true;
        } else if patch.status == 1 && attempt < MAX_STEP_RETRIES {
            // BUILD FAILED — copy error to fixed buffer (no heap alloc)
            let out_len = patch.output_len.min(result_buf.len()).min(1024);
            last_error_len = out_len;
            last_error_buf[..out_len].copy_from_slice(&result_buf[..out_len]);
            last_code = code;

            write_str("[Executor] cargo check FAILED\n");
            attempt += 1;
            continue;
        } else {
            // FINAL FAIL — increment step fail counter
            plan.steps[step_idx].fail_count += 1;
            write_str("[Executor] ");
            write_str(&plan.task_id);
            write_str(" step ");
            write_dec((step_idx + 1) as u32);
            write_str(" FAIL (after ");
            write_dec(attempt as u32);
            write_str(" retries, attempt ");
            write_dec(plan.steps[step_idx].fail_count as u32);
            write_str("/3)\n");

            // After 3 total failures on the same step, abandon the task
            if plan.steps[step_idx].fail_count >= 3 {
                plan.completed = true;
                write_str("[Executor] === ");
                write_str(&plan.task_id);
                write_str(" ABANDONED (step ");
                write_dec((step_idx + 1) as u32);
                write_str(" unresolvable) ===\n");
            }
            return true;
        }
    }
}

/// Phase 16 — compile the sandbox to WASM and report the result.
///
/// After a complex task completes (all steps pass cargo test), we ask
/// the proxy to compile the final code to wasm32-unknown-unknown. If
/// successful, the .wasm bytes are available in the OS — ready for
/// the compositor's wasmi runtime to instantiate.
fn deploy_wasm(task_id: &str) {
    write_str("[Deploy] Compiling ");
    write_str(task_id);
    write_str(" to WASM...\n");

    // 128 KB buffer for the .wasm binary
    let mut wasm_buf = [0u8; 131072];
    let result = match libfolk::sys::wasm_compile(&mut wasm_buf) {
        Some(r) => r,
        None => {
            write_str("[Deploy] FAIL: wasm_compile syscall error\n");
            return;
        }
    };

    if result.status == 0 {
        let wasm_len = result.output_len;
        write_str("[Deploy] ");
        write_str(task_id);
        write_str(".wasm = ");
        write_dec(wasm_len as u32);
        write_str(" bytes\n");

        // Store in Synapse for persistence across boots
        let mut wasm_name = String::with_capacity(32);
        wasm_name.push_str("draug_");
        wasm_name.push_str(task_id);
        wasm_name.push_str(".wasm");

        let _ = libfolk::sys::synapse::write_file(
            &wasm_name,
            &wasm_buf[..wasm_len],
        );

        // ── THE MISSING STEP: actually RUN the code in the OS ────
        //
        // Try silverfir-nano JIT (Trusted backend) first for
        // near-native speed. Falls back to wasmi if JIT can't
        // handle the opcodes.
        write_str("[Deploy] Loading into OS runtime...\n");

        let config = compositor::wasm_runtime::WasmConfig {
            screen_width: 0,
            screen_height: 0,
            uptime_ms: 0,
        };

        let (result, _output) = compositor::wasm_runtime::execute_wasm_with_backend(
            &wasm_buf[..wasm_len],
            config,
            compositor::wasm_runtime::WasmBackend::Trusted,
        );

        match result {
            compositor::wasm_runtime::WasmResult::Ok => {
                write_str("[Deploy] ");
                write_str(task_id);
                write_str(" EXECUTED in OS — code is LIVE!\n");
            }
            compositor::wasm_runtime::WasmResult::Trap(ref msg) => {
                write_str("[Deploy] Execution trapped: ");
                write_str(&msg[..msg.len().min(80)]);
                write_str("\n");
            }
            compositor::wasm_runtime::WasmResult::LoadError(ref msg) => {
                write_str("[Deploy] Load failed: ");
                write_str(&msg[..msg.len().min(80)]);
                write_str(" (stored in Synapse for later)\n");
            }
            compositor::wasm_runtime::WasmResult::OutOfFuel => {
                write_str("[Deploy] Out of fuel (computation too long)\n");
            }
        }
    } else {
        write_str("[Deploy] WASM compile failed (status=");
        write_dec(result.status);
        write_str(")\n");
        // Log the error
        let err_len = result.output_len.min(wasm_buf.len()).min(200);
        if let Ok(s) = core::str::from_utf8(&wasm_buf[..err_len]) {
            write_str("[Deploy] error: ");
            write_str(s);
            write_str("\n");
        }
    }
}

fn push_dec(out: &mut String, mut v: u32) {
    if v == 0 { out.push('0'); return; }
    let mut buf = [0u8; 10];
    let mut i = 0;
    while v > 0 {
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        out.push(buf[i] as char);
    }
}
