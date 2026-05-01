//! Knowledge Hunt (Phase 7) — Draug's one-shot Wikipedia fetch.
//!
//! When the system has been idle for ~15 seconds, `agent_logic::tick`
//! calls `run()`. This function:
//!
//!   1. Asks the kernel to fetch the configured Wikipedia article via
//!      `libfolk::sys::fbp_request` (syscall 0x5E — plain TCP to the
//!      host-side folkering-proxy on 10.0.2.2:14711, which is a live
//!      headless Chromium in Phase 6).
//!   2. Parses the returned FBP `DOM_STATE_UPDATE` using `fbp-rs`
//!      (zero-copy) — we don't need a display list here, just the
//!      text content of each visible `SemanticNode`.
//!   3. Concatenates all node text into one blob (this simulates the
//!      "AAAK-compressed summary" the user asked for — a future pass
//!      will replace this with a real LLM summarization step).
//!   4. Persists the blob to the MemPalace via Synapse IPC:
//!        - `synapse::write_file(ROOM, text_blob)` creates the row in
//!          the `files` table
//!        - `synapse::read_file_by_name(ROOM)` gets the assigned
//!          `file_id`
//!        - `synapse::write_intent(file_id, "text/folk-aaak", json)`
//!          writes the semantic intent into `file_intents`
//!   5. Flips the `knowledge_hunted` gate on `DraugDaemon` so we
//!      don't re-fetch on the next tick.
//!
//! All calls are synchronous. The tick loop blocks for the duration
//! of the fetch (~1.5 s in happy path), which is fine because Draug
//! only runs while the user is idle anyway.

use alloc::string::String;
use libfolk::sys::io::write_str;

use crate::draug::{
    DraugDaemon, KNOWLEDGE_HUNT_ROOM, KNOWLEDGE_HUNT_URL,
};

/// Static scratch buffer for the FBP payload. `[u64; 16384]` → 128 KB
/// with natural 8-byte alignment so `fbp_rs::parse_state_update` can
/// zero-copy slice-cast the `SemanticNode` array. The compositor is a
/// single-threaded task, so a single static buffer is safe. BSS-only
/// allocation, no runtime cost at module init.
const FBP_BUF_WORDS: usize = 16384;
const FBP_BUF_SIZE: usize = FBP_BUF_WORDS * 8;
static mut FBP_BUF: [u64; FBP_BUF_WORDS] = [0u64; FBP_BUF_WORDS];

#[inline]
unsafe fn fbp_buf_mut() -> &'static mut [u8] {
    let ptr = core::ptr::addr_of_mut!(FBP_BUF) as *mut u8;
    core::slice::from_raw_parts_mut(ptr, FBP_BUF_SIZE)
}

#[inline]
unsafe fn fbp_buf_ref(len: usize) -> &'static [u8] {
    let ptr = core::ptr::addr_of!(FBP_BUF) as *const u8;
    core::slice::from_raw_parts(ptr, len)
}

/// Maximum characters of concatenated text we retain after extraction.
/// Phase 8 implemented SQLite overflow pages in synapse-service, so
/// cells larger than a btree leaf page now spill into a chain of
/// overflow pages instead of corrupting neighbouring pages. With that
/// fix in place we can store the full extracted Wikipedia article —
/// capped at 64 KB to match the compositor host-fn buffer.
const SUMMARY_CHAR_CAP: usize = 64 * 1024;

/// Run one Knowledge Hunt. Returns `true` if the hunt completed
/// (successfully or with a terminal error — either way Draug marks
/// the session flag so we don't retry this boot).
pub fn run(draug: &mut DraugDaemon) -> bool {
    // ── 0. Phase 9 self-test — ALWAYS runs ─────────────────────────
    //
    // The bi-temporal graph supersession test is fully self-contained
    // (uses hardcoded entity ids + edges, touches only Synapse) so we
    // run it first, before the proxy fetch. That way a missing proxy
    // doesn't hide a genuine KG regression. The test is idempotent —
    // repeated boots just re-supersede the same edge.
    run_graph_supersession_test();

    write_str("[KHunt] waking up — fetching ");
    // Truncate the URL in the log to keep output clean.
    let url_show = &KNOWLEDGE_HUNT_URL[..KNOWLEDGE_HUNT_URL.len().min(72)];
    write_str(url_show);
    write_str("\n");

    // ── 1. fetch via kernel syscall ─────────────────────────────────
    let n = unsafe {
        libfolk::sys::fbp_request(KNOWLEDGE_HUNT_URL, fbp_buf_mut())
    };
    if n == 0 {
        write_str("[KHunt] fbp_request failed — proxy unreachable?\n");
        draug.mark_knowledge_hunted();
        return true;
    }
    write_str("[KHunt] got ");
    write_dec(n as u32);
    write_str(" bytes of FBP from proxy\n");

    // ── 2. parse the FBP payload ────────────────────────────────────
    let (node_count, viewport_w, viewport_h, text_blob) = {
        let slice = unsafe { fbp_buf_ref(n) };
        let view = match fbp_rs::parse_state_update(slice) {
            Ok(v) => v,
            Err(_) => {
                write_str("[KHunt] FBP parse failed — dropping payload\n");
                draug.mark_knowledge_hunted();
                return true;
            }
        };

        // ── 3. concatenate visible text ─────────────────────────────
        //
        // Walk every SemanticNode. For each one with non-empty text,
        // decode as UTF-8 and push into the blob. Skip nodes whose
        // text fails UTF-8 validation (rare — dom_extract.js sends
        // plain JS strings so it should all be valid).
        //
        // We stop early once we hit SUMMARY_CHAR_CAP so the blob
        // stays bounded regardless of page size. The Phase 10
        // `__url__` sentinel node (carrying window.location.href for
        // folk_browser's URL bar) is explicitly filtered out so it
        // doesn't pollute the Draug summary with a duplicate URL.
        let mut blob = String::with_capacity(8192);
        for node in view.nodes {
            if view.tag(node) == b"__url__" { continue; }
            let text = view.text(node);
            if text.is_empty() { continue; }
            let s = match core::str::from_utf8(text) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !blob.is_empty() { blob.push(' '); }
            blob.push_str(s);
            if blob.len() >= SUMMARY_CHAR_CAP {
                blob.truncate(SUMMARY_CHAR_CAP);
                break;
            }
        }

        (view.nodes.len(), view.viewport_w, view.viewport_h, blob)
    };

    write_str("[KHunt] extracted ");
    write_dec(text_blob.len() as u32);
    write_str(" chars from ");
    write_dec(node_count as u32);
    write_str(" nodes (viewport ");
    write_dec(viewport_w);
    write_str("x");
    write_dec(viewport_h);
    write_str(")\n");

    // Log the first 120 chars of the blob so we can eyeball the
    // extraction quality from the serial log.
    let preview_len = text_blob.len().min(120);
    let preview: &str = &text_blob[..preview_len];
    write_str("[KHunt] preview: \"");
    write_str(preview);
    if text_blob.len() > preview_len { write_str("..."); }
    write_str("\"\n");

    // ── 4. persist to MemPalace (Synapse SQLite) ───────────────────
    //
    // `write_file_get_rowid` returns the newly-assigned rowid so we
    // can stamp a semantic intent on the fresh row without an extra
    // `read_file_by_name` round-trip (which doesn't see freshly
    // inserted rows because the directory cache path differs).
    let file_id = match libfolk::sys::synapse::write_file_get_rowid(
        KNOWLEDGE_HUNT_ROOM,
        text_blob.as_bytes(),
    ) {
        Ok(rowid) => rowid,
        Err(_) => {
            write_str("[KHunt] synapse::write_file failed — MemPalace unreachable\n");
            draug.mark_knowledge_hunted();
            return true;
        }
    };
    write_str("[KHunt] wrote ");
    write_str(KNOWLEDGE_HUNT_ROOM);
    write_str(" to MemPalace (files table), rowid=");
    write_dec(file_id);
    write_str("\n");

    // Stamp the semantic intent. We mark it as AAAK-compressed so a
    // future lookup can distinguish Draug-fetched knowledge from
    // regular app files. The JSON is hand-built so we don't have to
    // drag serde_json into the compositor.
    let intent_json = build_intent_json(&text_blob, node_count);
    let mime = "text/folk-aaak";
    if let Err(_) = libfolk::sys::synapse::write_intent(file_id, mime, &intent_json) {
        write_str("[KHunt] synapse::write_intent failed — metadata skipped\n");
        // Not fatal — the blob is still in the files table.
    } else {
        write_str("[KHunt] wrote file_intents row (mime=");
        write_str(mime);
        write_str(")\n");
    }

    // ── 5. close the gate ──────────────────────────────────────────
    draug.mark_knowledge_hunted();
    write_str("[KHunt] hunt complete — entry persisted under room=/");
    write_str(KNOWLEDGE_HUNT_ROOM);
    write_str("\n");

    // (Phase 9 KG self-test already ran at step 0 above.)

    // ── 7. Phase 11 — Draug auto-refactor pipeline test ─────────
    //
    // Draug generates a tiny Rust source file, ships it to the
    // host-side proxy via syscall 0x61, the proxy writes it to
    // `draug-sandbox/src/draug_latest.rs` and runs `cargo check`
    // on the sandbox crate. Success = Draug wrote Rust that
    // compiled on the host while the user was idle.
    run_auto_refactor_test();

    true
}

/// Phase 13 — Overnight programming task rotation.
///
/// 20 distinct prompts Draug cycles through. Each entry is
/// (short_id, task_description). The short_id is used as the
/// knowledge-graph entity name for that task, and the description
/// is interpolated into the LLM prompt template.
pub const REFACTOR_TASKS: &[(&str, &str)] = &[
    ("fib_iter",     "an iterative Fibonacci function named `fib` that takes a `u32` and returns a `u64`"),
    ("factorial",    "a function named `factorial` that takes a `u32` and returns the factorial as a `u64`"),
    ("gcd",          "a function named `gcd` that takes two `u64` arguments and returns their greatest common divisor using Euclid's algorithm"),
    ("is_prime",     "a function named `is_prime` that takes a `u64` and returns `true` if it is prime, `false` otherwise"),
    ("reverse_u32",  "a function named `reverse_u32` that takes a `u32` and returns the bit-reversed value"),
    ("popcount",     "a function named `popcount` that takes a `u64` and returns the number of `1` bits, computed manually without the builtin"),
    ("clamp",        "a function named `clamp` that takes three `i32` arguments (value, lo, hi) and returns the value clamped to [lo, hi]"),
    ("abs_i64",      "a function named `abs_i64` that takes an `i64` and returns its absolute value as `u64` without using `.abs()`"),
    ("max_of_three", "a function named `max_of_three` that takes three `i32` and returns the largest"),
    ("is_power_of_two", "a function named `is_power_of_two` that takes a `u64` and returns `true` iff it is a power of two"),
    ("square_sum",   "a function named `square_sum` that takes a `u32` and returns `1^2 + 2^2 + ... + n^2` as a `u64`"),
    ("triangular",   "a function named `triangular` that takes a `u32` and returns the n-th triangular number as a `u64`"),
    ("is_leap",      "a function named `is_leap` that takes a `u32` year and returns `true` iff it is a Gregorian leap year"),
    ("digit_sum",    "a function named `digit_sum` that takes a `u64` and returns the sum of its base-10 digits as `u64`"),
    ("reverse_digits", "a function named `reverse_digits` that takes a `u64` and returns the number with its base-10 digits reversed"),
    ("count_trailing_zeros", "a function named `count_trailing_zeros` that takes a `u64` and returns the number of trailing zero bits, manually (no builtin)"),
    ("collatz_steps","a function named `collatz_steps` that takes a `u64` and returns the number of Collatz steps to reach 1"),
    ("min_of_array", "a function named `min_of_array` that takes `arr: &[i32]` and returns the minimum as `Option<i32>` (None for empty)"),
    ("sum_of_array", "a function named `sum_of_array` that takes `arr: &[i64]` and returns the sum as `i64`"),
    ("binary_search", "a function named `binary_search` that takes a sorted `arr: &[i32]` and a `target: i32` and returns `Option<usize>` of the index if found"),
];

/// Phase 14 — Self-Improving Loop.
///
/// State machine per iteration:
///   1. PICK: select next (task, level) from skill tree
///   2. BUILD_PROMPT: level-aware prompt with memory context
///   3. GENERATE: call Gemma4 via LLM gateway
///   4. COMPILE: ship to sandbox via fbp_patch
///   5. EVALUATE: if pass → advance level, store code
///                if fail → extract error, retry (max 2)
///
/// Levels:
///   L1: Write function (cargo check)
///   L2: Write function + #[cfg(test)] tests (cargo check)
///   L3: Optimize prior code with MemPalace context (cargo check)
pub fn run_refactor_step(draug: &mut crate::draug::DraugDaemon, now_ms: u64) {
    use libfolk::sys::{fbp_patch, llm_generate};

    // NOTE: advance_refactor() is called AFTER proxy check to avoid
    // counting skips toward REFACTOR_MAX_ITER. Without this, ~4-16
    // hours of Ollama downtime would permanently kill the loop.
    draug.last_refactor_ms = now_ms;

    // ── Stability Fix 7: proxy health check (cached 60s) ─────────────
    {
        let now = libfolk::sys::uptime();
        let needs_ping = draug.last_ping_ms == 0
            || !draug.last_ping_ok
            || now.saturating_sub(draug.last_ping_ms) > 60_000;
        if needs_ping {
            let ok = libfolk::sys::proxy_ping();
            draug.last_ping_ms = now;
            draug.last_ping_ok = ok;
            if !ok {
                write_str("[Draug] SKIP: proxy unreachable\n");
                draug.record_skip();
                return; // iter NOT incremented
            }
        }
    }

    // Increment iter only when we actually attempt work
    let iter = draug.advance_refactor(now_ms);

    // ── Phase 15: Plan-and-Solve mode ────────────────────────────────
    // Once the L1-L3 skill tree completes, Draug transitions to
    // complex multi-step tasks using the Planner/Executor architecture.
    if draug.next_task_and_level().is_none() {
        if !draug.plan_mode_active {
            draug.plan_mode_active = true;
            draug.save_state();
            write_str("\n[Draug] *** PHASE 15: Plan-and-Solve activated ***\n");
            write_str("[Draug] Skill tree L1-L3 complete. Switching to complex tasks.\n");
        }
        run_plan_step(draug, iter);
        return;
    }

    // ── 1. PICK (Skill Tree mode) ────────────────────────────────────
    let (task_idx, level) = draug.next_task_and_level().unwrap();
    let (task_id, task_desc) = REFACTOR_TASKS[task_idx];

    // Update bridge with current task for TCP shell
    {
        let mut label = alloc::string::String::with_capacity(32);
        label.push_str(task_id);
        label.push_str(" L");
        push_decimal(&mut label, level as u32);
        libfolk::sys::draug_bridge_set_task(&label);
    }

    write_str("\n[Draug] ============================================\n");
    write_str("[Draug] iter=");
    write_dec(iter);
    write_str(" task=");
    write_str(task_id);
    write_str(" L");
    write_dec(level as u32);
    write_str(" (");
    write_str(level_name(level));
    write_str(")\n");

    // ── 2. BUILD_PROMPT ──────────────────────────────────────────────
    let mut base_prompt = build_level_prompt(level, task_id, task_desc, draug.get_task_code(task_idx));

    // Fix 4: inject error memory from previous iteration attempts
    if let Some(prev_error) = draug.get_task_error(task_idx) {
        base_prompt.push_str("\n\nIMPORTANT: A previous attempt failed with:\n```\n");
        let cap = prev_error.len().min(512);
        base_prompt.push_str(&prev_error[..cap]);
        base_prompt.push_str("\n```\nAvoid this mistake.");
    }

    // Fix 2: heap monitoring every 50 iterations
    if iter % 50 == 0 {
        let (_total_mb, used_mb, pct) = libfolk::sys::memory_stats();
        write_str("[Draug] HEAP: ");
        write_dec(used_mb);
        write_str("MB (");
        write_dec(pct);
        write_str("%)\n");
        if pct > 80 {
            write_str("[Draug] WARNING: memory >80% — pausing\n");
            return;
        }
    }

    // ── 3-5. GENERATE → COMPILE → EVALUATE (with retry loop) ────────
    // Pre-allocate retry buffers to avoid fragmentation from variable
    // String allocations in the hot loop. Fixed 1KB error + reuse
    // base_prompt reference on first attempt (no clone).
    let max_retries = crate::draug::MAX_RETRIES;
    let mut attempt = 0u8;
    let mut last_error_buf = [0u8; 1024];
    let mut last_error_len = 0usize;
    let mut last_code = alloc::string::String::new();

    loop {
        let prompt = if attempt == 0 {
            // First attempt: use base_prompt directly (no clone)
            core::mem::take(&mut base_prompt)
        } else {
            let err_str = core::str::from_utf8(&last_error_buf[..last_error_len]).unwrap_or("");
            build_retry_prompt(&last_code, err_str)
        };

        if attempt > 0 {
            write_str("[Draug] retry #");
            write_dec(attempt as u32);
            write_str(" — feeding compiler error back to Gemma4\n");
            draug.refactor_retries += 1;
        }

        // ── GENERATE ─────────────────────────────────────────────────
        let mut llm_buf = [0u8; 8192];
        let llm_result = match llm_generate(crate::draug::model_for_level(level), &prompt, &mut llm_buf) {
            Some(s) => s,
            None => {
                // Stability: Ollama down or TCP error — skip this
                // iteration, don't count as task failure. Draug will
                // retry on the next tick.
                write_str("[Draug] SKIP: llm_generate ipc error (Ollama down?)\n");
                draug.record_skip();
                return;
            }
        };
        if llm_result.status != 0 {
            // Stability: LLM HTTP error (Ollama timeout/overloaded).
            // Skip without counting as task failure.
            write_str("[Draug] SKIP: LLM status=");
            write_dec(llm_result.status);
            write_str(" (will retry next tick)\n");
            draug.record_skip();
            return;
        }

        let raw_len = llm_result.output_len.min(llm_buf.len());
        let raw = match core::str::from_utf8(&llm_buf[..raw_len]) {
            Ok(s) => s,
            Err(_) => {
                write_str("[Draug] FAIL: non-UTF8 reply\n");
                draug.record_refactor_fail();
                return;
            }
        };

        let code = extract_rust_code_block(raw);
        if code.is_empty() {
            write_str("[Draug] FAIL: empty code block\n");
            draug.record_refactor_fail();
            return;
        }

        write_str("[Draug] extracted ");
        write_dec(code.len() as u32);
        write_str(" bytes\n");

        // ── COMPILE ──────────────────────────────────────────────────
        let mut result_buf = [0u8; 4096];
        let patch = match fbp_patch("draug_latest.rs", code.as_bytes(), &mut result_buf) {
            Some(s) => s,
            None => {
                write_str("[Draug] FAIL: fbp_patch ipc error\n");
                draug.record_refactor_fail();
                return;
            }
        };

        // ── EVALUATE ─────────────────────────────────────────────────
        if patch.status == 0 {
            // ── SUCCESS ──────────────────────────────────────────────
            draug.record_refactor_pass();
            draug.advance_task_level(task_idx);
            draug.reset_skips();
            draug.clear_task_error(task_idx);
            draug.save_state();

            // Store L1 code in memory for L2/L3 prompts + persist to Synapse
            if level == 1 {
                draug.store_task_code(task_idx, code);
                draug.save_task_code(task_idx);
            }

            write_str("[Draug] ");
            write_str(task_id);
            write_str(" L");
            write_dec(level as u32);
            write_str(" PASS");
            if attempt > 0 {
                write_str(" (");
                write_dec(attempt as u32);
                write_str(" retries)");
            }
            write_str("\n");

            // Log skill tree progress
            let at_l1 = draug.tasks_at_level(1);
            let at_l2 = draug.tasks_at_level(2);
            let at_l3 = draug.tasks_at_level(3);
            write_str("[Draug] Skill tree: L1=");
            write_dec(at_l1 as u32);
            write_str("/20 L2=");
            write_dec(at_l2 as u32);
            write_str("/20 L3=");
            write_dec(at_l3 as u32);
            write_str("/20\n");
            break;

        } else if patch.status == 1 && attempt < max_retries {
            // ── BUILD FAILED — retry with error feedback ─────────────
            // Copy error into fixed buffer (no heap alloc)
            let out_len = patch.output_len.min(result_buf.len()).min(1024);
            last_error_len = out_len;
            last_error_buf[..out_len].copy_from_slice(&result_buf[..out_len]);
            last_code = code;

            write_str("[Draug] cargo check FAILED — will retry with error\n");
            write_str("[Draug] error preview: ");
            if let Ok(s) = core::str::from_utf8(&last_error_buf[..last_error_len.min(120)]) {
                write_str(s);
            }
            write_str("\n");

            attempt += 1;
            continue;

        } else {
            // ── FINAL FAIL ───────────────────────────────────────────
            draug.record_refactor_fail();
            // Fix 4: store error for cross-iteration learning
            let err_len = patch.output_len.min(result_buf.len()).min(512);
            if let Ok(s) = core::str::from_utf8(&result_buf[..err_len]) {
                draug.store_task_error(task_idx, alloc::string::String::from(s));
            }

            write_str("[Draug] ");
            write_str(task_id);
            write_str(" L");
            write_dec(level as u32);
            write_str(" FAIL");
            if attempt > 0 {
                write_str(" (after ");
                write_dec(attempt as u32);
                write_str(" retries)");
            }
            write_str("\n");

            let out_len = patch.output_len.min(result_buf.len()).min(200);
            if let Ok(s) = core::str::from_utf8(&result_buf[..out_len]) {
                write_str("[Draug] final error: ");
                write_str(s);
                write_str("\n");
            }
            break;
        }
    }

    write_str("[Draug] progress: passed=");
    write_dec(draug.refactor_passed);
    write_str(" failed=");
    write_dec(draug.refactor_failed);
    write_str(" retries=");
    write_dec(draug.refactor_retries);
    write_str("\n");
}

/// Phase 15 — execute one step of the active plan, or plan a new task.
pub fn run_plan_step(draug: &mut crate::draug::DraugDaemon, iter: u32) {
    use crate::agent_planner;

    // If there's an active plan with pending steps, execute the next one
    if let Some(ref mut plan) = draug.active_plan {
        if !plan.completed {
            write_str("\n[Draug] ============================================\n");
            write_str("[Draug] iter=");
            write_dec(iter);
            write_str(" [PLAN] ");
            write_str(&plan.task_id);
            write_str("\n");

            agent_planner::execute_next_step(plan);

            write_str("[Draug] progress: passed=");
            write_dec(draug.refactor_passed);
            write_str(" failed=");
            write_dec(draug.refactor_failed);
            write_str("\n");
            return;
        }
    }

    // Active plan is done (or None). Pick the next complex task.
    if draug.complex_task_idx >= agent_planner::COMPLEX_TASKS.len() {
        write_str("[Draug] All complex tasks planned and completed!\n");
        return;
    }

    let (task_id, task_desc) = agent_planner::COMPLEX_TASKS[draug.complex_task_idx];

    write_str("\n[Draug] ============================================\n");
    write_str("[Draug] iter=");
    write_dec(iter);
    write_str(" [PLAN-NEW] ");
    write_str(task_id);
    write_str("\n");

    match agent_planner::plan_task(task_id, task_desc) {
        Some(plan) => {
            // Explicit drop: free old plan's Vec<PlanStep> + code Strings
            // before allocating the new one (reduces peak fragmentation).
            drop(draug.active_plan.take());
            draug.active_plan = Some(plan);
            draug.complex_task_idx += 1;
            // Don't execute yet — next tick will pick up the first step
        }
        None => {
            write_str("[Draug] Planning failed for ");
            write_str(task_id);
            write_str(" — skipping\n");
            draug.complex_task_idx += 1;
        }
    }
}

/// Human-readable level name for serial log.
fn level_name(level: u8) -> &'static str {
    match level {
        1 => "The Fixer",
        2 => "TDD",
        3 => "Evolution",
        4 => "OS Integration",
        5 => "Hardware",
        _ => "???",
    }
}

/// Build a level-appropriate prompt for Gemma4.
///
/// L1: "Write <function>. Only code, no explanation."
/// L2: "Write <function> + #[cfg(test)] module with 3+ test cases."
/// L3: "Here is prior code: <code>. Write optimized version + tests."
pub fn build_level_prompt(
    level: u8,
    task_id: &str,
    task_desc: &str,
    prior_code: Option<&str>,
) -> alloc::string::String {
    let mut p = alloc::string::String::with_capacity(1536);

    match level {
        1 => {
            // L1: The Fixer — write a standalone function
            p.push_str("Write ");
            p.push_str(task_desc);
            p.push_str(". Respond with ONLY the code inside a ```rust fenced block. ");
            p.push_str("No explanation, no imports, no `fn main`, no attributes. ");
            p.push_str("The function must compile standalone as part of a lib.rs ");
            p.push_str("in a fresh Rust crate.");
        }
        2 => {
            // L2: TDD — function + tests that verify correctness
            p.push_str("Write ");
            p.push_str(task_desc);
            p.push_str(". Also include a `#[cfg(test)] mod tests` block with ");
            p.push_str("at least 3 test functions that verify correctness ");
            p.push_str("using `assert_eq!`. Include edge cases (zero, ");
            p.push_str("boundary values, large inputs). ");
            p.push_str("Respond with ONLY the code inside a ```rust fenced ");
            p.push_str("block. No explanation, no `fn main`. Everything must ");
            p.push_str("compile as lib.rs in a fresh Rust crate.");
        }
        3 => {
            // L3: Evolution — optimize prior code using MemPalace context
            p.push_str("You are a Rust optimization expert. ");
            p.push_str("Here is an existing function you wrote earlier:\n```rust\n");
            if let Some(code) = prior_code {
                p.push_str(code);
            } else {
                p.push_str("// (no prior code available — write from scratch)");
            }
            p.push_str("\n```\n");
            p.push_str("Task: Write an optimized version named `");
            p.push_str(task_id);
            p.push_str("_fast` that produces identical results but is ");
            p.push_str("more efficient (fewer branches, SIMD-friendly, ");
            p.push_str("or uses a closed-form formula). Include BOTH ");
            p.push_str("the original function and the optimized one, plus ");
            p.push_str("a `#[cfg(test)] mod tests` with at least 3 tests ");
            p.push_str("that verify both functions return the same results ");
            p.push_str("for the same inputs. Respond with ONLY the code ");
            p.push_str("in a ```rust fenced block. Must compile as lib.rs.");
        }
        _ => {
            // L4/L5: placeholder
            p.push_str("Write ");
            p.push_str(task_desc);
            p.push_str(". Respond with ONLY the code in a ```rust block.");
        }
    }
    p
}

/// Build a retry prompt that includes the failed code and compiler error.
/// This is the core of Draug's error-driven learning.
fn build_retry_prompt(
    failed_code: &str,
    compiler_error: &str,
) -> alloc::string::String {
    let mut p = alloc::string::String::with_capacity(
        failed_code.len() + compiler_error.len() + 256,
    );
    p.push_str("You are an expert Rust developer. Your previous code failed ");
    p.push_str("compilation. Fix ONLY the errors — do not change the logic.\n\n");
    p.push_str("[YOUR PREVIOUS CODE]\n```rust\n");
    p.push_str(failed_code);
    p.push_str("\n```\n\n[COMPILER ERROR]\n```\n");
    // Cap error at 1KB to stay within prompt budget
    let cap = compiler_error.len().min(1024);
    p.push_str(&compiler_error[..cap]);
    p.push_str("\n```\n\nRespond with the FIXED code in a ```rust fenced block. ");
    p.push_str("No explanation.");
    p
}

/// Phase 12 — LLM-driven auto-refactor (Phase 13 wraps this into a
/// loop via `run_refactor_step`).
fn run_auto_refactor_test() {
    use libfolk::sys::{fbp_patch, llm_generate};

    write_str("[AutoRefactor] asking Gemma4 to write Rust...\n");

    // Keep the prompt small + strict — the sandbox crate is a
    // plain std lib, but we ask for no_std to make the LLM's job
    // harder and prove the compilation path is live.
    let prompt = "Write a complete, standalone Rust function named `fib` \
that takes a `u32` argument and returns the `n`th Fibonacci number as \
a `u64`. Use iteration, not recursion. Respond with ONLY the code inside \
a ```rust fenced block. Do NOT include any explanation, no imports, no \
`fn main`, no attributes — just the single `fib` function.";

    let mut llm_buf = [0u8; 8192];
    let llm_result =
        match llm_generate(crate::draug::model_for_level(1), prompt, &mut llm_buf) {
            Some(s) => s,
            None => {
                write_str("[AutoRefactor] FAIL: llm_generate syscall errored\n");
                record_outcome("llm_ipc_error");
                return;
            }
        };

    write_str("[AutoRefactor] LLM status=");
    write_dec(llm_result.status);
    write_str(" response_bytes=");
    write_dec(llm_result.output_len as u32);
    write_str("\n");

    if llm_result.status != 0 {
        write_str("[AutoRefactor] FAIL: LLM returned error status\n");
        record_outcome("llm_http_error");
        return;
    }

    let raw_len = llm_result.output_len.min(llm_buf.len());
    let raw = match core::str::from_utf8(&llm_buf[..raw_len]) {
        Ok(s) => s,
        Err(_) => {
            write_str("[AutoRefactor] FAIL: LLM response was not UTF-8\n");
            record_outcome("llm_non_utf8");
            return;
        }
    };

    // Show the first 100 bytes so we can eyeball what Gemma4 said.
    let preview_len = raw.len().min(100);
    write_str("[AutoRefactor] LLM preview: ");
    write_str(&raw[..preview_len]);
    if raw.len() > preview_len { write_str("..."); }
    write_str("\n");

    // Extract the code block between ```rust fences.
    let code = extract_rust_code_block(raw);
    write_str("[AutoRefactor] extracted ");
    write_dec(code.len() as u32);
    write_str(" bytes of code\n");

    if code.is_empty() {
        write_str("[AutoRefactor] FAIL: could not locate code in LLM response\n");
        record_outcome("llm_parse_failed");
        return;
    }

    // Show the first line of the extracted code as a sanity check.
    let first_line_end = code.find('\n').unwrap_or(code.len()).min(80);
    write_str("[AutoRefactor] code[0]: ");
    write_str(&code[..first_line_end]);
    write_str("\n");

    // Ship the LLM's code to the sandbox — fbp_patch writes it
    // into draug-sandbox/src/draug_latest.rs and runs cargo check.
    let mut result_buf = [0u8; 4096];
    let status = match fbp_patch("draug_latest.rs", code.as_bytes(), &mut result_buf) {
        Some(s) => s,
        None => {
            write_str("[AutoRefactor] FAIL: fbp_patch syscall errored\n");
            record_outcome("refactor_ipc_error");
            return;
        }
    };

    write_str("[AutoRefactor] cargo check status=");
    write_dec(status.status);
    write_str(" output_bytes=");
    write_dec(status.output_len as u32);
    write_str("\n");

    let out_len = status.output_len.min(result_buf.len()).min(200);
    let preview = match core::str::from_utf8(&result_buf[..out_len]) {
        Ok(s) => s,
        Err(_) => "<non-utf8>",
    };
    write_str("[AutoRefactor] cargo output: ");
    write_str(preview);
    if status.output_len > out_len { write_str("..."); }
    write_str("\n");

    let outcome: &str = match status.status {
        0 => "refactor_ok",
        1 => "refactor_build_failed",
        2 => "refactor_bad_filename",
        3 => "refactor_io_error",
        4 => "refactor_timeout",
        5 => "refactor_too_large",
        _ => "refactor_unknown",
    };
    record_outcome(outcome);

    if status.status == 0 {
        write_str("[AutoRefactor] PASS: Gemma4 wrote Rust, cargo check accepted it.\n");
    } else {
        write_str("[AutoRefactor] FAIL: cargo rejected Gemma4's code (see output above)\n");
    }
}

/// Persist the outcome entity + refactor lineage edges in the
/// knowledge graph so a future session can reason about Draug's
/// refactor history.
fn record_outcome(outcome: &str) {
    use libfolk::sys::synapse::{upsert_edge, upsert_entity};
    let _ = upsert_entity(outcome, outcome, "REFACTOR_OUTCOME");
    let _ = upsert_entity("draug_latest_rs", "draug_latest.rs", "SOURCE_FILE");
    let _ = upsert_entity("draug", "Draug daemon", "DAEMON");
    let _ = upsert_entity("gemma4_31b_cloud", "Gemma4 31B cloud", "LLM");
    let _ = upsert_edge("edge_draug_patches_file", "draug", "patches", "draug_latest_rs");
    let _ = upsert_edge("edge_file_outcome", "draug_latest_rs", "last_check", outcome);
    let _ = upsert_edge("edge_draug_uses_llm", "draug", "uses", "gemma4_31b_cloud");
}

/// Tiny no_std extractor that pulls a Rust code block out of an
/// LLM reply. Looks for the first ` ```rust ` fence and returns
/// everything up to the matching closing ` ``` `. Falls back to
/// the whole text if no fence is found — the LLM sometimes returns
/// pure code for terse prompts.
pub fn extract_rust_code_block(raw: &str) -> alloc::string::String {
    // Primary: find "```rust"
    if let Some(start) = raw.find("```rust") {
        // Skip to the end of the opening line
        let after_fence = &raw[start + "```rust".len()..];
        let body_start = match after_fence.find('\n') {
            Some(i) => i + 1,
            None => 0,
        };
        let body = &after_fence[body_start..];
        if let Some(end) = body.find("```") {
            return alloc::string::String::from(body[..end].trim());
        }
        // No closing fence — take the rest
        return alloc::string::String::from(body.trim());
    }
    // Secondary: plain "```"
    if let Some(start) = raw.find("```") {
        let after = &raw[start + 3..];
        let body_start = match after.find('\n') {
            Some(i) => i + 1,
            None => 0,
        };
        let body = &after[body_start..];
        if let Some(end) = body.find("```") {
            return alloc::string::String::from(body[..end].trim());
        }
        return alloc::string::String::from(body.trim());
    }
    // Fallback: no fence at all — assume pure code
    alloc::string::String::from(raw.trim())
}

pub fn push_decimal(out: &mut alloc::string::String, mut v: u32) {
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

/// Phase 9 Knowledge Graph demo — proves that temporal supersession
/// works end-to-end. If it passes, the serial log reads `[KGraph] PASS`;
/// if it fails, `[KGraph] FAIL` with context.
fn run_graph_supersession_test() {
    use libfolk::sys::synapse::{upsert_entity, upsert_edge, graph_walk, GraphHop, MAX_GRAPH_HOP_ID_LEN};

    write_str("[KGraph] test start — folk_browser uses wasmi → Silverfir supersession\n");

    // Upsert the three entities.
    for (eid, name, kind) in &[
        ("folk_browser", "Folkering Browser", "MODULE"),
        ("wasmi", "wasmi interpreter", "DEPENDENCY"),
        ("Silverfir", "Silverfir-nano engine", "DEPENDENCY"),
    ] {
        match upsert_entity(eid, name, kind) {
            Ok(rowid) => {
                write_str("[KGraph]   entity ok: ");
                write_str(eid);
                write_str(" rowid=");
                write_dec(rowid);
                write_str("\n");
            }
            Err(_) => {
                write_str("[KGraph] FAIL: upsert_entity failed for ");
                write_str(eid);
                write_str("\n");
                return;
            }
        }
    }

    // Insert the first edge: folk_browser uses wasmi
    match upsert_edge("edge_fb_wasmi", "folk_browser", "uses", "wasmi") {
        Ok(_) => write_str("[KGraph]   edge #1 folk_browser-[uses]-wasmi inserted\n"),
        Err(_) => { write_str("[KGraph] FAIL: upsert_edge #1 failed\n"); return; }
    }

    // Insert the second edge: folk_browser uses Silverfir
    // This MUST trigger temporal supersession of edge #1 so wasmi's
    // valid_to flips from 0 (active) to a non-zero timestamp.
    match upsert_edge("edge_fb_silverfir", "folk_browser", "uses", "Silverfir") {
        Ok(_) => write_str("[KGraph]   edge #2 folk_browser-[uses]-Silverfir inserted (should have expired #1)\n"),
        Err(_) => { write_str("[KGraph] FAIL: upsert_edge #2 failed\n"); return; }
    }

    // Walk the graph from folk_browser.
    let mut hops = [GraphHop {
        depth: 0,
        id_len: 0,
        id_bytes: [0u8; MAX_GRAPH_HOP_ID_LEN],
    }; 8];
    let count = match graph_walk("folk_browser", 3, &mut hops) {
        Ok(n) => n,
        Err(_) => { write_str("[KGraph] FAIL: graph_walk errored\n"); return; }
    };

    write_str("[KGraph]   graph_walk returned ");
    write_dec(count as u32);
    write_str(" hop(s)\n");

    let mut saw_silverfir = false;
    let mut saw_wasmi = false;
    for i in 0..count {
        let eid = hops[i].as_str();
        write_str("[KGraph]     -> ");
        write_str(eid);
        write_str(" (depth=");
        write_dec(hops[i].depth as u32);
        write_str(")\n");
        if eid == "Silverfir" { saw_silverfir = true; }
        if eid == "wasmi" { saw_wasmi = true; }
    }

    if saw_silverfir && !saw_wasmi {
        write_str("[KGraph] PASS: only Silverfir is reachable from folk_browser; wasmi was superseded\n");
    } else if saw_wasmi {
        write_str("[KGraph] FAIL: wasmi is still reachable — temporal supersession did NOT fire\n");
    } else {
        write_str("[KGraph] FAIL: Silverfir not reached — graph_walk didn't find the new active edge\n");
    }

    // ── MVFS smoke test ───────────────────────────────────────────
    //
    // Phase 1 check: basic write/read/delete round-trip within a
    // single boot session.
    //
    // Phase 2 check: "boot_counter" survives across reboots. First
    // boot on a fresh disk writes 1; every subsequent boot reads the
    // prior value and increments. This proves the load-from-disk +
    // flush-on-write path works end-to-end.
    {
        use libfolk::sys::fs::{mvfs_write, mvfs_read, mvfs_delete};

        // ── Round-trip test (Phase 1 — still green).
        let name = "mvfs_smoke";
        let payload: &[u8] = b"round-trip";
        if !mvfs_write(name, payload) {
            write_str("[MVFS] FAIL: write rejected\n");
        } else {
            let mut buf = [0u8; 32];
            match mvfs_read(name, &mut buf) {
                Some(n) if &buf[..n] == payload => {
                    write_str("[MVFS] PASS: write/read round-trip matches\n");
                }
                Some(n) => {
                    write_str("[MVFS] FAIL: read returned ");
                    write_dec(n as u32);
                    write_str(" bytes, content mismatch\n");
                }
                None => write_str("[MVFS] FAIL: read returned None on just-written entry\n"),
            }
            if !mvfs_delete(name) {
                write_str("[MVFS] FAIL: delete reported not-found on known entry\n");
            }
            if mvfs_read(name, &mut buf).is_some() {
                write_str("[MVFS] FAIL: entry survived delete\n");
            }
        }

        // ── Persistence test (Phase 2).
        let mut buf = [0u8; 8];
        let prev = match mvfs_read("boot_counter", &mut buf) {
            Some(n) if n >= 1 => buf[0],
            _ => 0,
        };
        let next = prev.saturating_add(1);
        if mvfs_write("boot_counter", &[next]) {
            write_str("[MVFS] boot_counter = ");
            write_dec(next as u32);
            write_str("\n");
        } else {
            write_str("[MVFS] FAIL: boot_counter write rejected\n");
        }
    }
}

/// Build the intent JSON payload that lands in `file_intents.intent_json`.
fn build_intent_json(blob: &str, node_count: usize) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("{\"source_url\":\"");
    out.push_str(KNOWLEDGE_HUNT_URL);
    out.push_str("\",\"room\":\"/");
    out.push_str(KNOWLEDGE_HUNT_ROOM);
    out.push_str("\",\"compression\":\"AAAK\",\"fetcher\":\"draug_v1\",\"nodes\":");
    push_u32(&mut out, node_count as u32);
    out.push_str(",\"chars\":");
    push_u32(&mut out, blob.len() as u32);
    out.push('}');
    out
}

/// Append a decimal `u32` to a `String`. `alloc::format!` works too
/// but pulls in the full fmt machinery for a single number — tiny
/// hand-rolled version keeps the compiled compositor binary a hair
/// smaller.
fn push_u32(out: &mut String, mut v: u32) {
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

pub fn write_dec(v: u32) {
    let mut buf = [0u8; 10];
    let mut tmp = v;
    let mut i = 0;
    if tmp == 0 {
        write_str("0");
        return;
    }
    while tmp > 0 {
        buf[i] = b'0' + (tmp % 10) as u8;
        tmp /= 10;
        i += 1;
    }
    let mut out = [0u8; 10];
    for k in 0..i {
        out[k] = buf[i - 1 - k];
    }
    if let Ok(s) = core::str::from_utf8(&out[..i]) {
        write_str(s);
    }
}
