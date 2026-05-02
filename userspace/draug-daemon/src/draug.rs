//! Draug Daemon — Proactive Background AI for Folkering OS
//!
//! Named after the Norse undead that never sleeps, Draug watches over
//! the system as a background state machine in the compositor's main loop. It:
//!
//! 1. **Observes**: Logs system events (uptime, memory, task changes)
//! 2. **Reasons**: Periodically sends observations to LLM for analysis
//! 3. **Acts**: Executes suggested optimizations or alerts
//!
//! Draug is non-intrusive — it only acts during idle periods and
//! never interrupts active user interaction.
//!
//! # Architecture
//!
//! ```text
//! Timer tick (every 30s) → Draug::tick()
//!   → Collect system telemetry
//!   → Append to observation log
//!   → If idle && log has unprocessed entries:
//!       → Send observations to LLM via MCP
//!       → Parse suggestions
//!       → Execute safe actions (alerts, cleanup)
//! ```

// `extern crate alloc;` lives in the daemon's lib.rs — no need to
// repeat it inside this module.
use alloc::string::String;
use alloc::format;

/// Interval between Draug ticks (in milliseconds)
pub const TICK_INTERVAL_MS: u64 = 10_000; // 10 seconds

/// Maximum observation log entries before forced consolidation
pub const MAX_LOG_ENTRIES: usize = 20;

/// Minimum entries before analysis can trigger
pub const ANALYSIS_BATCH: usize = 3;

/// Circadian rhythm: daytime idle threshold (09:00-23:00)
pub const DREAM_IDLE_DAY_MS: u64 = 2_700_000; // 45 minutes

/// Circadian rhythm: nighttime idle threshold (23:00-06:00)
pub const DREAM_IDLE_NIGHT_MS: u64 = 300_000; // 5 minutes

/// AutoDream: cooldown between dreams (10 minutes)
pub const DREAM_COOLDOWN_MS: u64 = 600_000;

/// Pattern-Mining: minimum idle before mining starts (5 minutes)
pub const PATTERN_MINE_IDLE_MS: u64 = 300_000;

/// Pattern-Mining: cooldown between mining runs (30 minutes)
pub const PATTERN_MINE_COOLDOWN_MS: u64 = 1_800_000;

/// Pattern-Mining: max telemetry events to include in LLM prompt
pub const PATTERN_MINE_MAX_EVENTS: usize = 500;

/// Pattern-Mining: max chars per LLM chunk (avoid context overflow)
pub const PATTERN_MINE_CHUNK_SIZE: usize = 1800;

/// AutoDream: max dreams per session
pub const DREAM_MAX_PER_SESSION: u32 = 10;

/// AutoDream: max refactoring failures before marking as "perfected"
pub const DREAM_STRIKE_LIMIT: u8 = 3;

// ── Async TCP State Machine ────────────────────────────────────────

/// Non-blocking phase of a Draug iteration.
/// Each phase takes <1ms. Compositor renders between phases.
#[derive(Clone, PartialEq)]
pub enum AsyncPhase {
    /// Ready for next iteration (gate check).
    Idle,
    /// TCP connect in progress (EAGAIN polling).
    Connecting,
    /// Sending request bytes (EAGAIN polling).
    Sending,
    /// Reading response (EAGAIN polling).
    Reading,
    /// Response complete — process result.
    Processing,
}

/// What operation the async TCP is serving.
#[derive(Clone, PartialEq)]
pub enum AsyncOp {
    None,
    /// Skill tree: LLM call for code generation
    LlmGenerate,
    /// Skill tree / Phase 15: cargo test via PATCH
    FbpPatch,
    /// Phase 16: WASM compilation
    WasmCompile,
    /// Health check
    ProxyPing,
    /// Phase 15: LLM call for planning (STEP| format)
    PlannerLlm,
    /// Phase 15: LLM call for step execution
    ExecutorLlm,
    /// Phase 17 (autonomous refactor loop): LLM call to refactor an
    /// existing function. Body of the request includes the source +
    /// goal + (optionally, gated by `--cg-policy by-model`) the
    /// caller list from CodeGraph.
    RefactorLlm,
    /// Phase 17: ship Draug's refactored source to the proxy's
    /// CARGO_CHECK endpoint, get back a verdict on whether the
    /// patch compiles + keeps callers compiling.
    CargoCheck,
    /// Phase A.5 (Path A): self-analysis cycle — Draug summarises
    /// its own observation log to the LLM and parses the action
    /// suggestion. Used to live on MCP/COM2 and reach the LLM via
    /// `libfolk::mcp::client::send_chat`; moved to direct TCP via
    /// the proxy so it can run in the daemon process without
    /// touching compositor's COM2 frame queue.
    AnalysisLlm,
    /// Phase C: ask the LLM for a multi-file Rust project (lib.rs +
    /// tests.rs + Cargo.toml etc.) split by `// === FILE: <path>`
    /// markers, parse the response into separate files, and write
    /// each into Synapse via `Project::write` under a
    /// `proj/<project>/` namespace. First demo of Draug authoring
    /// beyond a single function.
    PhaseCMultiFile,
}

/// Which kind of work Draug is doing right now. Lets `tick_idle`
/// dispatch to one of three coexisting flows (Skill Tree / Phase 15
/// / Phase 17 refactor) instead of cramming all three into a single
/// linear path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// Skill Tree L1-L3: write fresh code from a static task list.
    SkillTree,
    /// Phase 15 Plan-and-Solve: planner + executor loop on
    /// `agent_planner::COMPLEX_TASKS`.
    PlanAndSolve,
    /// Phase 17 autonomous refactor: read an existing fn from the
    /// tree, ship to LLM with CodeGraph caller context (per the
    /// by-model policy), CARGO_CHECK the result. Picks the next
    /// `Pending` entry from `task_store::load()`.
    Refactor,
}

// ── Knowledge Hunt (Phase 7) ────────────────────────────────────────
//
// When the system idles, Draug fires a one-shot "Knowledge Hunt" that
// asks the host-side Cloud DOM Proxy to fetch a configured URL and
// persists the extracted text to the MemPalace (files.db) under a
// symbolic "room" like `knowledge/rust_wiki.txt`. Intended for demo
// + proof-of-concept; a future session will promote this into a
// general reading queue driven by pending user intents.

/// Idle threshold before a Knowledge Hunt fires (15 seconds). Kept
/// intentionally short so the demo triggers without a 45-minute wait.
pub const KNOWLEDGE_HUNT_IDLE_MS: u64 = 15_000;

/// Cooldown between successive Knowledge Hunts (5 minutes). Currently
/// the hunt is one-shot per boot, but this constant is used by the
/// scheduler so future sessions can convert the flag into a queue.
pub const KNOWLEDGE_HUNT_COOLDOWN_MS: u64 = 300_000;

/// URL the first Knowledge Hunt fetches. Hardcoded for Phase 7; a
/// later pass will pull this from an intent queue fed by the user.
pub const KNOWLEDGE_HUNT_URL: &str = "https://en.wikipedia.org/wiki/Rust_(programming_language)";

/// MemPalace "room" the hunt stores its result under. This ends up
/// as the `name` column in the Synapse `files` table.
pub const KNOWLEDGE_HUNT_ROOM: &str = "knowledge/rust_wiki.txt";

// ── Overnight auto-refactor loop (Phase 13) ──────────────────────────
//
// After the one-shot Knowledge Hunt + Graph supersession test fire,
// Draug enters an overnight loop where it rotates through a list of
// programming tasks, prompting Gemma4 for each and sandboxing the
// result. Designed to run for hours without supervision — the gates
// below cap wall-clock work and rate-limit the Ollama calls.

/// Minimum wall-clock interval between refactor iterations.
/// 60 seconds → on cloud-backed Gemma4 (~3-5s per call) + cargo
/// check (~0.2-1s) we spend about 10% of each interval actually
/// working. Keeps the local Ollama endpoint happy.
pub const REFACTOR_INTERVAL_MS: u64 = 60_000;

/// Hard safety cap on total iterations per boot. With the Phase 14
/// skill tree (20 tasks × 3 levels + retries), 1000 gives ~10 full
/// retry budgets per task-level. At 60s/iter that's ~16 hours.
pub const REFACTOR_MAX_ITER: u32 = 1000;

/// Phase 15 — number of complex tasks in agent_planner::COMPLEX_TASKS.
/// Kept as a constant here so the lib crate doesn't reference the bin.
pub const COMPLEX_TASK_COUNT: usize = 8;

/// Phase 14 — Draug Autonomy Curriculum.
///
/// Five cognitive training levels:
///   L1: The Fixer — write function, retry with compiler feedback
///   L2: TDD — write function + tests, verify both compile
///   L3: Evolution — optimize prior code using MemPalace context
///   L4: OS Integration — use libfolk syscalls (future)
///   L5: Hardware — lock-free structures for DAQ (future)
///
/// Only L1-L3 are active in the current build. L4-L5 are defined
/// in the curriculum but skipped until the sandbox has libfolk stubs.
///
/// Stability: model selection per level — L1 uses a fast 7B coder
/// model to reduce GPU pressure by ~75%. L2+ uses the full 31B.

/// Select LLM model based on skill level.
pub fn model_for_level(level: u8) -> &'static str {
    match level {
        0 | 1 => "qwen2.5-coder:7b",  // fast, good for simple functions
        _ => "gemma4:31b-cloud",        // full reasoning for TDD/optimization
    }
}

/// Model for Phase 15 Planner persona (needs strong reasoning).
pub const PLANNER_MODEL: &str = "gemma4:31b-cloud";
/// Model for Phase 15 Executor persona (complex multi-step).
pub const EXECUTOR_MODEL: &str = "gemma4:31b-cloud";
/// Model for Phase 17 autonomous refactor LLM calls. Uses the small
/// model to keep cloud costs bounded — the eval-runner trial 002
/// showed gemma4:31b doesn't measurably outperform 7b on the fixture
/// task set, while costing ~10× per call.
pub const REFACTOR_MODEL: &str = "qwen2.5-coder:7b";
/// Model for the Draug self-analysis cycle. Cheap and frequent —
/// fires up to 5× per session, summarises ~3 observations into a
/// JSON action suggestion. The 7B model is plenty for the JSON
/// pattern; matches what MCP's default backend would have served.
pub const ANALYSIS_MODEL: &str = "qwen2.5-coder:7b";
pub const ACTIVE_SKILL_LEVELS: u8 = 3;
pub const MAX_SKILL_LEVELS: u8 = 5;
/// Number of tasks in REFACTOR_TASKS.
pub const TASK_COUNT: usize = 20;
/// Max retries per attempt when cargo check fails.
pub const MAX_RETRIES: u8 = 2;

/// Wait this long after boot before the first refactor attempt —
/// gives the Knowledge Hunt + Graph test room to finish without
/// competing for the proxy socket. 30 seconds after last user input.
pub const REFACTOR_INITIAL_IDLE_MS: u64 = 30_000;

/// Compress source code for LLM prompts: strip comments, blank lines, trailing spaces.
/// Saves 30-50% of tokens without losing meaning.
fn compress_source(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let trimmed = line.trim();
        // Skip blank lines
        if trimmed.is_empty() { continue; }
        // Skip full-line comments (but keep doc comments)
        if trimmed.starts_with("//") && !trimmed.starts_with("///") { continue; }
        // Strip inline comments (simple heuristic: // after code)
        let code = if let Some(pos) = trimmed.find("//") {
            // Don't strip if inside a string literal (check for odd quotes before //)
            let before = &trimmed[..pos];
            let quote_count = before.chars().filter(|&c| c == '"').count();
            if quote_count % 2 == 0 {
                before.trim_end()
            } else {
                trimmed // Inside string, keep as-is
            }
        } else {
            trimmed
        };
        if code.is_empty() { continue; }
        out.push_str(code);
        out.push('\n');
    }
    out
}

/// Dream modes — five hemispheres (3 app + 2 driver)
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DreamMode {
    /// Left brain: CPU cycle reduction — headless benchmark V1 vs V2
    Refactor,
    /// Right brain: GUI vision — LLM sees render output + adds features
    Creative,
    /// Immune system: fuzzing — inject extreme inputs to find crashes
    Nightmare,
    /// Driver optimization: reduce fuel, preserve IRQ loop structure
    DriverRefactor,
    /// Driver hardening: handle edge cases (SFI violations, IRQ storms)
    DriverNightmare,
}

/// System observation snapshot
pub struct Observation {
    pub timestamp_ms: u64,
    pub uptime_s: u64,
    pub mem_used_mb: u32,
    pub mem_total_mb: u32,
    pub mem_pct: u32,
    pub task_count: u32,
    pub event: ObservationEvent,
}

/// What triggered this observation
#[derive(Clone)]
pub enum ObservationEvent {
    /// Regular periodic tick
    Tick,
    /// Memory usage crossed a threshold
    MemoryWarning { pct: u32 },
    /// User was idle for extended period
    IdleDetected { idle_ms: u64 },
    /// System just booted
    BootComplete,
}

// ── Friction Sensor: Frustration-Driven Evolution ──────────────────────

/// Signal weights for friction tracking
pub const FRICTION_RAGE_CLICK: u16 = 10;
pub const FRICTION_QUICK_CLOSE: u16 = 20;
pub const FRICTION_BACKSPACE_SPAM: u16 = 5;

/// Threshold above which an app is considered "frustrating"
pub const FRICTION_THRESHOLD: u16 = 15;

/// Decay interval: reduce all scores by 1 every 60s
pub const FRICTION_DECAY_MS: u64 = 60_000;

/// Tracks per-app frustration signals to prioritize dream targets
pub struct FrictionTracker {
    /// (cache_key_hash, score) — up to 8 tracked apps
    scores: [(u32, u16); 8],
    last_decay_ms: u64,
}

impl FrictionTracker {
    pub const fn new() -> Self {
        Self {
            scores: [(0, 0); 8],
            last_decay_ms: 0,
        }
    }

    /// Record a friction signal for an app
    pub fn record_signal(&mut self, key_hash: u32, weight: u16) {
        // Find existing slot
        for (h, score) in &mut self.scores {
            if *h == key_hash && *score > 0 {
                *score = score.saturating_add(weight);
                return;
            }
        }
        // Find empty slot (score == 0)
        for (h, score) in &mut self.scores {
            if *score == 0 {
                *h = key_hash;
                *score = weight;
                return;
            }
        }
        // Full — overwrite lowest score
        let mut min_idx = 0;
        let mut min_score = u16::MAX;
        for (i, (_, s)) in self.scores.iter().enumerate() {
            if *s < min_score { min_score = *s; min_idx = i; }
        }
        self.scores[min_idx] = (key_hash, weight);
    }

    /// Returns the hash of the most frustrated app, if any exceeds threshold
    pub fn most_frustrated(&self) -> Option<u32> {
        let mut best: Option<(u32, u16)> = None;
        for (h, score) in &self.scores {
            if *score >= FRICTION_THRESHOLD {
                if best.map_or(true, |(_, s)| *score > s) {
                    best = Some((*h, *score));
                }
            }
        }
        best.map(|(h, _)| h)
    }

    /// Decay all scores by 1 every FRICTION_DECAY_MS
    pub fn decay(&mut self, now_ms: u64) {
        if now_ms.saturating_sub(self.last_decay_ms) < FRICTION_DECAY_MS {
            return;
        }
        self.last_decay_ms = now_ms;
        for (_, score) in &mut self.scores {
            *score = score.saturating_sub(1);
        }
    }

    /// Get friction score for an app (for logging)
    pub fn score_for(&self, key_hash: u32) -> u16 {
        for (h, score) in &self.scores {
            if *h == key_hash { return *score; }
        }
        0
    }
}

/// Draug daemon state
pub struct DraugDaemon {
    /// Observation log (circular, append-only)
    log: [Option<ObservationSummary>; MAX_LOG_ENTRIES],
    log_head: usize,
    log_count: usize,

    /// Timing
    last_tick_ms: u64,
    last_user_input_ms: u64,

    /// State
    active: bool,
    waiting_for_llm: bool,
    analysis_count: u32,
    last_analysis_ms: u64,

    /// AutoDream state
    dream_count: u32,
    last_dream_ms: u64,
    dreaming: bool,
    dream_target: Option<alloc::string::String>,
    dream_mode: DreamMode,

    /// Strike tracker: cache_key_hash → failure count
    /// Apps with 3 strikes are "perfected" and skipped
    strikes: [Option<(u32, u8)>; 8],

    /// Dream journal: tracks which app was dreamt about most recently.
    dream_journal: [Option<(u32, u32)>; 16],

    /// Friction Sensor: tracks user frustration per app
    pub friction: FrictionTracker,

    /// Crash tracker: apps that hit fuel limit repeatedly (for Nightmare priority)
    crash_hashes: [(u32, u8); 8],

    /// Pattern-Mining state (Phase 1 of new AutoDream cycle)
    last_pattern_mine_ms: u64,
    pattern_mine_count: u32,
    /// Last insight stored — avoid duplicates
    last_insight_hash: u32,

    /// Knowledge Hunt (Phase 7): one-shot flag that flips true after
    /// the first successful hunt of this boot. A future session will
    /// promote this into an `Option<String>` reading queue so Draug
    /// can chew through multiple URLs.
    knowledge_hunted: bool,

    /// Phase 13 — Overnight auto-refactor loop state. Draug picks a
    /// new programming task every REFACTOR_INTERVAL_MS, runs it
    /// through the LLM gateway, ships the result to the sandbox,
    /// records the outcome, then sleeps until the next tick.
    /// Bounded by REFACTOR_MAX_ITER to avoid runaway execution.
    pub refactor_iter: u32,
    pub last_refactor_ms: u64,
    pub refactor_passed: u32,
    pub refactor_failed: u32,

    /// Phase 14 — Skill tree state.
    ///
    /// `task_levels[i]` = highest completed level for task i (0-3).
    /// `task_code[i]` = L1 code for task i, fed into L2/L3 prompts.
    /// `refactor_retries` = lifetime count of error-driven retries.
    pub task_levels: [u8; TASK_COUNT],
    pub task_code: [Option<alloc::string::String>; TASK_COUNT],
    pub refactor_retries: u32,

    /// Phase 15 — Agentic Plan-and-Solve state.
    pub active_plan: Option<TaskPlan>,
    pub complex_task_idx: usize,
    pub plan_mode_active: bool,

    // ── Stability fields ─────────────────────────────────────────
    /// Fix 4: Error memory — last compiler error per task.
    pub task_errors: [Option<alloc::string::String>; TASK_COUNT],
    /// Fix 5: Consecutive LLM skips (Ollama down). For backoff.
    pub consecutive_skips: u32,
    /// Fix 8: Hibernation mode — set after 30 consecutive skips.
    pub refactor_hibernating: bool,
    /// Per-task consecutive parse-failure count, reset on PASS or
    /// when the daemon switches to a different task. Used to force-
    /// advance a task that keeps failing at LLM-parse stage (proxy
    /// returned empty bytes, model dropped offline mid-stream, etc.)
    /// instead of looping on it forever until the global 30-skip
    /// hibernation kicks in. See `process_skill_llm` for the wiring.
    pub task_parse_fails: [u32; TASK_COUNT],
    /// Cumulative count of force-advance events (parse-fail SKIPs +
    /// cargo-fail SKIPs). Reported in the Skill: line so an operator
    /// can tell at a glance how much of `tasks_at_level(N)` is real
    /// PASS vs a level the daemon gave up on. Doesn't persist across
    /// boots — diagnostic only.
    pub force_advance_count: u32,
    /// Phase C: index of the next multi-file project Draug should
    /// attempt from `phase_c::MULTI_FILE_PROJECTS`. Once this hits
    /// the list length, the autonomous loop stops trying new
    /// projects (one-shot per project, success or fail). In-memory
    /// only — Phase C resets each boot, which is fine for the
    /// initial demo since proj/<id>/ persistence is up to Synapse.
    pub phase_c_idx: u8,
    /// Cached proxy ping result (avoid 2s TCP per iteration).
    pub last_ping_ms: u64,
    pub last_ping_ok: bool,

    // ── Async TCP state machine ──────────────────────────────────
    /// Current async phase (non-blocking Draug iteration).
    pub async_phase: AsyncPhase,
    /// TCP slot ID for the current async connection (0xFFFF = none).
    pub async_tcp_slot: u64,
    /// Buffer for accumulating async TCP response.
    pub async_response: alloc::vec::Vec<u8>,
    /// What we're waiting for (LLM or PATCH).
    pub async_operation: AsyncOp,
    /// The prompt/request bytes to send.
    pub async_request: alloc::vec::Vec<u8>,
    /// Bytes sent so far.
    pub async_sent: usize,
    /// Task context preserved across async calls.
    pub async_task_idx: usize,
    pub async_level: u8,
    pub async_attempt: u8,
    /// Uptime (ms) when current async phase started — for timeout.
    /// If now - async_phase_started_ms > ASYNC_TIMEOUT_MS, force abort.
    pub async_phase_started_ms: u64,

    // ── Phase 17 — autonomous refactor loop ─────────────────────────
    /// Persisted refactor task queue. Loaded from Synapse VFS at
    /// startup (or seeded from `mcp_handler::refactor_loop::REFACTOR_FIXTURES`
    /// on cold-boot). `None` until the loader runs at boot —
    /// `tick_idle` treats `None` as "Phase 17 unavailable".
    pub refactor_tasks: Option<alloc::vec::Vec<crate::refactor_types::RefactorTask>>,
    /// Index into `refactor_tasks` for the iteration currently in
    /// flight. `usize::MAX` = nothing in flight. Outlives the
    /// LlmGenerate→CargoCheck transition so process_cargo_check_result
    /// can find the task it should record_attempt against.
    pub current_refactor_idx: usize,
    /// Repo-relative path of the file the in-flight refactor is
    /// targeting. Cached so `process_refactor_llm` can pass it to
    /// `build_cargo_check_request` without re-reading the task.
    pub current_refactor_target: alloc::string::String,
    /// Cap on how many refactor iterations we run per boot — keeps
    /// cloud costs bounded and means Draug eventually idles instead
    /// of looping forever.
    pub refactor_iterations_done: u32,
}

/// Timeout for any single async TCP phase (connect/send/read).
/// 90 seconds wall clock — enough for cold Ollama + cargo test,
/// but prevents permanent hang if proxy stops responding.
pub const ASYNC_TIMEOUT_MS: u64 = 90_000;

/// Cap on autonomous refactor iterations per boot. cargo check on
/// real OS code is expensive (cold workspace ≈ 45 s) and each
/// iteration also burns a cloud-routed LLM call. 16 iterations is
/// enough to cycle every fixture task at the 3-attempt cap a few
/// times before idling — see `mcp_handler::refactor_loop::pick_next_refactor_task`.
pub const MAX_REFACTOR_ITERATIONS_PER_BOOT: u32 = 16;

// ── Phase 15 types (must live in lib crate so draug.rs can own them) ──

/// A multi-step task plan generated by the Planner persona.
#[derive(Clone)]
pub struct TaskPlan {
    pub task_id: alloc::string::String,
    pub task_desc: alloc::string::String,
    pub steps: alloc::vec::Vec<PlanStep>,
    pub current_step: usize,
    pub completed: bool,
}

/// A single step within a task plan.
#[derive(Clone)]
pub struct PlanStep {
    pub description: alloc::string::String,
    pub code: Option<alloc::string::String>,
    pub done: bool,
    /// How many times this step has FINAL-failed (after retries).
    /// After 3, the entire task is abandoned.
    pub fail_count: u8,
}

/// Compact observation summary for the log
pub struct ObservationSummary {
    pub timestamp_s: u32,
    pub mem_pct: u8,
    pub task_count: u8,
    pub event_tag: u8, // 0=tick, 1=mem_warn, 2=idle, 3=boot
}

impl DraugDaemon {
    pub const fn new() -> Self {
        Self {
            log: [const { None }; MAX_LOG_ENTRIES],
            log_head: 0,
            log_count: 0,
            last_tick_ms: 0,
            last_user_input_ms: 0,
            active: true,
            waiting_for_llm: false,
            analysis_count: 0,
            last_analysis_ms: 0,
            dream_count: 0,
            last_dream_ms: 0,
            dreaming: false,
            dream_target: None,
            dream_mode: DreamMode::Refactor,
            strikes: [const { None }; 8],
            dream_journal: [const { None }; 16],
            friction: FrictionTracker::new(),
            crash_hashes: [(0, 0); 8],
            last_pattern_mine_ms: 0,
            pattern_mine_count: 0,
            last_insight_hash: 0,
            knowledge_hunted: false,
            refactor_iter: 0,
            last_refactor_ms: 0,
            refactor_passed: 0,
            refactor_failed: 0,
            task_levels: [0u8; TASK_COUNT],
            task_code: [const { None }; TASK_COUNT],
            refactor_retries: 0,
            active_plan: None,
            complex_task_idx: 0,
            plan_mode_active: false,
            task_errors: [const { None }; TASK_COUNT],
            consecutive_skips: 0,
            refactor_hibernating: false,
            task_parse_fails: [0u32; TASK_COUNT],
            force_advance_count: 0,
            phase_c_idx: 0,
            last_ping_ms: 0,
            last_ping_ok: false,
            async_phase: AsyncPhase::Idle,
            async_tcp_slot: 0xFFFF,
            async_response: alloc::vec::Vec::new(),
            async_operation: AsyncOp::None,
            async_request: alloc::vec::Vec::new(),
            async_sent: 0,
            async_task_idx: 0,
            async_level: 0,
            async_attempt: 0,
            async_phase_started_ms: 0,
            refactor_tasks: None,
            current_refactor_idx: usize::MAX,
            current_refactor_target: alloc::string::String::new(),
            refactor_iterations_done: 0,
        }
    }

    // ── Phase 13 — Overnight refactor loop gate ──────────────────
    //
    // Returns true at most once per REFACTOR_INTERVAL_MS wall-clock
    // interval, and never after REFACTOR_MAX_ITER steps.
    //
    // Phase 13.4: decoupled from KHunt. The refactor loop only
    // needs `llm_generate` + `fbp_patch`, not the one-shot
    // Wikipedia fetch. If KHunt is flaky (virtio-net RX stalls
    // under whpx with large responses), Phase 13 should still
    // proceed overnight.

    pub fn should_run_refactor_step(&mut self, now_ms: u64) -> bool {
        if !self.active { return false; }
        if self.dreaming || self.waiting_for_llm { return false; }
        if self.refactor_iter >= REFACTOR_MAX_ITER { return false; }
        // Heap pressure guard: skip if physical RAM is tight (>90% used)
        let (_total, _used, pct) = libfolk::sys::memory_stats();
        if pct > 90 {
            return false;
        }
        // Fix 8: hibernation — stop until proxy comes back.
        // Auto-wake: try proxy ping every 60s while hibernating.
        // Issue #58: try UDP ping first (independent of wedged TCP state),
        // fall through to TCP. Either succeeding wakes Draug.
        if self.refactor_hibernating {
            if now_ms.saturating_sub(self.last_refactor_ms) >= 60_000 {
                self.last_refactor_ms = now_ms;
                libfolk::sys::io::write_str("[Draug-hib] 60s elapsed → trying proxy_ping_udp first\n");
                let mut ok = libfolk::sys::proxy_ping_udp();
                if ok {
                    libfolk::sys::io::write_str("[Draug-hib] UDP ping OK\n");
                } else {
                    libfolk::sys::io::write_str("[Draug-hib] UDP ping failed → falling back to TCP ping\n");
                    ok = libfolk::sys::proxy_ping();
                }
                if ok {
                    libfolk::sys::io::write_str("[Draug-hib] proxy reachable → resetting skips, resuming\n");
                    self.consecutive_skips = 0;
                    self.refactor_hibernating = false;
                    // Fall through to normal scheduling
                } else {
                    libfolk::sys::io::write_str("[Draug-hib] both pings failed → still hibernating, retry in 60s\n");
                    return false;
                }
            } else {
                return false;
            }
        }

        let has_skill_work = self.next_task_and_level().is_some();
        let has_plan_work = self.plan_mode_active && self.has_plan_work();
        let needs_plan_transition = !has_skill_work
            && !self.plan_mode_active
            && self.complex_task_idx < COMPLEX_TASK_COUNT;

        if !has_skill_work && !has_plan_work && !needs_plan_transition { return false; }

        if now_ms.saturating_sub(self.last_user_input_ms) < REFACTOR_INITIAL_IDLE_MS {
            return false;
        }
        if self.last_refactor_ms == 0 {
            return true;
        }
        // Adaptive interval: L1 with fast 7b model needs less wait,
        // L2+ with 31b needs full 60s. Backoff on skips.
        let base_interval = if self.consecutive_skips > 5 {
            let multiplier = 1u64 << ((self.consecutive_skips.saturating_sub(5)).min(3) as u64);
            (REFACTOR_INTERVAL_MS * multiplier).min(300_000)
        } else if !self.plan_mode_active {
            // Skill tree mode: check current level
            match self.next_task_and_level() {
                Some((_, 1)) => 15_000,  // L1: 15s (7b model responds in ~3s)
                _ => REFACTOR_INTERVAL_MS, // L2+: 60s
            }
        } else {
            REFACTOR_INTERVAL_MS // Plan mode: 60s (31b model)
        };
        now_ms.saturating_sub(self.last_refactor_ms) >= base_interval
    }

    /// Advance the refactor counter. Called BEFORE the actual work so
    /// the iteration number logged to serial matches the counter
    /// after the increment.
    pub fn advance_refactor(&mut self, now_ms: u64) -> u32 {
        self.refactor_iter += 1;
        self.last_refactor_ms = now_ms;
        self.refactor_iter
    }

    pub fn record_refactor_pass(&mut self) { self.refactor_passed += 1; }
    pub fn record_refactor_fail(&mut self) { self.refactor_failed += 1; }

    // ── Phase 14 — Skill Tree ───────────────────────────────────────

    /// Returns `(task_index, target_level)` for the next task to
    /// attempt. Breadth-first: all L1s first, then all L2s, then L3s.
    /// Returns `None` when every task has reached ACTIVE_SKILL_LEVELS.
    pub fn next_task_and_level(&self) -> Option<(usize, u8)> {
        for level in 1..=ACTIVE_SKILL_LEVELS {
            for i in 0..TASK_COUNT {
                if self.task_levels[i] < level {
                    return Some((i, level));
                }
            }
        }
        None
    }

    /// Phase 17 — find the next refactor task eligible for an
    /// attempt. Mirrors the eval-runner's pick logic: first
    /// `Pending`, then any failed entry under the per-task retry
    /// cap (3). Returns `None` when every task has settled — caller
    /// should fall through to `start_phase15` then.
    pub fn pick_next_refactor(&self) -> Option<usize> {
        const MAX_ATTEMPTS_PER_TASK: u32 = 3;
        let tasks = self.refactor_tasks.as_ref()?;
        for (idx, t) in tasks.iter().enumerate() {
            use crate::refactor_types::TaskStatus;
            match t.last_status {
                TaskStatus::Pending => return Some(idx),
                TaskStatus::Pass | TaskStatus::Skip => continue,
                TaskStatus::FailCompile | TaskStatus::FailCallerCompat => {
                    if t.attempts < MAX_ATTEMPTS_PER_TASK {
                        return Some(idx);
                    }
                }
            }
        }
        None
    }

    /// Phase 17 — caller-side guard for the per-boot iteration cap.
    /// `tick_idle` calls this before picking a refactor task to
    /// short-circuit when we've already done MAX_REFACTOR_ITERATIONS_PER_BOOT.
    pub fn refactor_budget_remaining(&self) -> bool {
        self.refactor_iterations_done < MAX_REFACTOR_ITERATIONS_PER_BOOT
    }

    /// Phase 17 — install the refactor task queue. Caller is the
    /// boot path in `main.rs`, which loads from Synapse VFS via
    /// `task_store::load()` (and seeds from REFACTOR_FIXTURES on
    /// cold boot). Stored as `Some(_)` so `tick_idle` can tell
    /// "queue available" from "queue not yet loaded".
    pub fn install_refactor_tasks(
        &mut self,
        tasks: alloc::vec::Vec<crate::refactor_types::RefactorTask>,
    ) {
        self.refactor_tasks = Some(tasks);
    }

    /// Record that task `idx` passed its current level.
    pub fn advance_task_level(&mut self, idx: usize) {
        if idx < TASK_COUNT && self.task_levels[idx] < MAX_SKILL_LEVELS {
            self.task_levels[idx] += 1;
        }
    }

    /// Store L1 code so L2/L3 prompts can reference it.
    pub fn store_task_code(&mut self, idx: usize, code: alloc::string::String) {
        if idx < TASK_COUNT {
            self.task_code[idx] = Some(code);
        }
    }

    /// Retrieve stored code for a task (used by L2/L3 prompts).
    pub fn get_task_code(&self, idx: usize) -> Option<&str> {
        if idx < TASK_COUNT {
            self.task_code[idx].as_deref()
        } else {
            None
        }
    }

    /// How many tasks have completed a given level?
    pub fn tasks_at_level(&self, level: u8) -> usize {
        self.task_levels.iter().filter(|&&l| l >= level).count()
    }

    /// Phase 15: is there remaining plan work?
    pub fn has_plan_work(&self) -> bool {
        self.complex_task_idx < COMPLEX_TASK_COUNT
            || self.active_plan.as_ref().map_or(false, |p| !p.completed)
    }

    // ── Stability methods ───────────────────────────────────────────

    /// Fix 4: Store error for cross-iteration learning.
    pub fn store_task_error(&mut self, idx: usize, error: alloc::string::String) {
        if idx < TASK_COUNT { self.task_errors[idx] = Some(error); }
    }
    pub fn get_task_error(&self, idx: usize) -> Option<&str> {
        if idx < TASK_COUNT { self.task_errors[idx].as_deref() } else { None }
    }
    pub fn clear_task_error(&mut self, idx: usize) {
        if idx < TASK_COUNT { self.task_errors[idx] = None; }
    }

    /// Check if Draug is active (not paused).
    pub fn is_active(&self) -> bool { self.active }
    /// Set Draug active state (for remote pause/resume).
    pub fn set_active(&mut self, v: bool) { self.active = v; }

    /// Fix 5: Record a skip (Ollama down).
    pub fn record_skip(&mut self) {
        self.consecutive_skips = self.consecutive_skips.saturating_add(1);
        // Issue #58 instrumentation: log every skip so we can correlate
        // with serial-side TIMEOUT events and see if hibernation
        // triggers. Use the existing 10-byte `write_dec` helper rather
        // than an inline 4-byte buffer — the bespoke formatter would
        // overflow once `consecutive_skips` reached 10_000+.
        libfolk::sys::io::write_str("[Draug-skip] consecutive=");
        write_dec(self.consecutive_skips);
        libfolk::sys::io::write_str("/30\n");
        // Fix 8: hibernate after 30 consecutive skips
        if self.consecutive_skips >= 30 && !self.refactor_hibernating {
            self.refactor_hibernating = true;
            libfolk::sys::io::write_str("[Draug-skip] >>> HIBERNATE (waiting for proxy_ping every 60s)\n");
        }
    }
    /// Fix 5: Reset skips on success.
    pub fn reset_skips(&mut self) {
        if self.refactor_hibernating {
            libfolk::sys::io::write_str("[Draug-skip] <<< WAKE (proxy back, skips reset)\n");
        } else if self.consecutive_skips > 0 {
            libfolk::sys::io::write_str("[Draug-skip] reset to 0 (PASS)\n");
        }
        self.consecutive_skips = 0;
        self.refactor_hibernating = false;
    }

    /// Save critical state to Synapse.
    pub fn save_state(&self) {
        let mut buf = [0u8; 26];
        buf[0..20].copy_from_slice(&self.task_levels);
        buf[20..24].copy_from_slice(&self.refactor_iter.to_le_bytes());
        buf[24] = self.complex_task_idx as u8;
        buf[25] = if self.plan_mode_active { 1 } else { 0 };
        let _ = libfolk::sys::synapse::write_file("draug_state.bin", &buf);
    }

    /// Save L1 code for a specific task (called after L1 PASS).
    /// Stored separately from the 26-byte state to keep save_state fast.
    pub fn save_task_code(&self, idx: usize) {
        if idx >= TASK_COUNT { return; }
        if let Some(ref code) = self.task_code[idx] {
            let mut name = alloc::string::String::with_capacity(24);
            name.push_str("draug_code_");
            // Simple decimal index
            if idx >= 10 { name.push((b'0' + (idx / 10) as u8) as char); }
            name.push((b'0' + (idx % 10) as u8) as char);
            name.push_str(".rs");
            let _ = libfolk::sys::synapse::write_file(&name, code.as_bytes());
        }
    }

    /// Fix 1: Restore state from Synapse on boot.
    pub fn restore_state(&mut self) -> bool {
        let resp = match libfolk::sys::synapse::read_file_shmem("draug_state.bin") {
            Ok(r) if r.size >= 26 => r,
            _ => return false,
        };
        const VADDR: usize = 0x30003000;
        if libfolk::sys::shmem_map(resp.shmem_handle, VADDR).is_err() {
            let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
            return false;
        }
        // Validate shmem response has expected size before reading
        if resp.size < 26 {
            let _ = libfolk::sys::shmem_unmap(resp.shmem_handle, VADDR);
            let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
            return false;
        }
        let data = unsafe { core::slice::from_raw_parts(VADDR as *const u8, 26) };
        self.task_levels.copy_from_slice(&data[0..20]);
        self.refactor_iter = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
        let idx = data[24] as usize;
        self.complex_task_idx = if idx <= COMPLEX_TASK_COUNT { idx } else { 0 };
        self.plan_mode_active = data[25] != 0;
        let _ = libfolk::sys::shmem_unmap(resp.shmem_handle, VADDR);
        let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);

        // Skip the one-shot KHunt + KGraph test (saves 30-60s boot time)
        self.knowledge_hunted = true;

        // Restore L1 code for L3 prompts
        for i in 0..TASK_COUNT {
            if self.task_levels[i] >= 1 {
                let mut name = alloc::string::String::with_capacity(24);
                name.push_str("draug_code_");
                if i >= 10 { name.push((b'0' + (i / 10) as u8) as char); }
                name.push((b'0' + (i % 10) as u8) as char);
                name.push_str(".rs");
                if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(&name) {
                    let sz = resp.size as usize;
                    if sz > 0 && sz < 4096 {
                        const CODE_VADDR: usize = 0x30004000;
                        if libfolk::sys::shmem_map(resp.shmem_handle, CODE_VADDR).is_ok() {
                            let bytes = unsafe {
                                core::slice::from_raw_parts(CODE_VADDR as *const u8, sz)
                            };
                            if let Ok(s) = core::str::from_utf8(bytes) {
                                self.task_code[i] = Some(alloc::string::String::from(s));
                            }
                            let _ = libfolk::sys::shmem_unmap(resp.shmem_handle, CODE_VADDR);
                        }
                        let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
                    }
                }
            }
        }

        true
    }

    // ── Knowledge Hunt gate ──────────────────────────────────────────
    //
    // Draug fires one Knowledge Hunt per boot when the system has been
    // idle long enough. The actual fetch + SQLite write is driven from
    // `mcp_handler::knowledge_hunt::run`, which calls
    // `mark_knowledge_hunted()` on success so the gate stays closed
    // afterward.

    /// Is it time to fire a Knowledge Hunt?
    pub fn should_hunt_knowledge(&self, now_ms: u64) -> bool {
        if self.knowledge_hunted { return false; }
        if !self.active { return false; }
        if self.dreaming || self.waiting_for_llm { return false; }
        now_ms.saturating_sub(self.last_user_input_ms) >= KNOWLEDGE_HUNT_IDLE_MS
    }

    /// Mark the hunt as done (called on both success and terminal
    /// failure so we don't spam the proxy with retries).
    pub fn mark_knowledge_hunted(&mut self) {
        self.knowledge_hunted = true;
    }



    /// Record user input activity (resets idle timer)
    pub fn on_user_input(&mut self, now_ms: u64) {
        self.last_user_input_ms = now_ms;
    }

    /// Check if it's time for a tick. Call from main loop.
    pub fn should_tick(&self, now_ms: u64) -> bool {
        // Ticks ALWAYS run (collect telemetry only, no LLM calls)
        // Don't gate on waiting_for_llm — that blocks tick counting during analysis
        self.active && now_ms.saturating_sub(self.last_tick_ms) >= TICK_INTERVAL_MS
    }

    /// Execute a tick: collect telemetry and log it.
    pub fn tick(&mut self, now_ms: u64) {
        self.last_tick_ms = now_ms;

        // Decay friction scores over time
        self.friction.decay(now_ms);

        let (_total_mb, _used_mb, mem_pct) = libfolk::sys::memory_stats();
        let uptime_s = (now_ms / 1000) as u32;

        // Determine event type
        let event_tag = if mem_pct > 85 { 1 } // Memory warning
        else if now_ms.saturating_sub(self.last_user_input_ms) > 120_000 { 2 } // Idle >2min
        else { 0 }; // Regular tick

        let summary = ObservationSummary {
            timestamp_s: uptime_s,
            mem_pct: mem_pct.min(100) as u8,
            task_count: 0, // TODO: get from task list
            event_tag,
        };

        // Append to circular log
        self.log[self.log_head] = Some(summary);
        self.log_head = (self.log_head + 1) % MAX_LOG_ENTRIES;
        if self.log_count < MAX_LOG_ENTRIES {
            self.log_count += 1;
        }
    }

    /// Check if Draug should run an analysis cycle.
    /// Hard limits: max 5 analyses per session, 5 minute cooldown between each.
    pub fn should_analyze(&self, now_ms: u64) -> bool {
        self.active
            && !self.waiting_for_llm
            && self.log_count >= ANALYSIS_BATCH
            && self.analysis_count < 5  // HARD LIMIT: max 5 analyses ever
            && now_ms.saturating_sub(self.last_user_input_ms) > 30_000
            && now_ms.saturating_sub(self.last_analysis_ms) > 300_000 // 5 min cooldown
    }

    /// Build an analysis prompt from recent observations.
    pub fn build_analysis_prompt(&self) -> String {
        let mut prompt = String::from(
            "You are Draug, the ever-watchful background daemon of Folkering OS.\n\
             Analyze these system observations and suggest ONE action if needed.\n\
             Respond with JSON: {\"action\": \"alert\", \"message\": \"...\"} or {\"action\": \"none\"}\n\n\
             Recent observations:\n"
        );

        let start = if self.log_count >= ANALYSIS_BATCH {
            (self.log_head + MAX_LOG_ENTRIES - ANALYSIS_BATCH) % MAX_LOG_ENTRIES
        } else { 0 };

        for i in 0..self.log_count.min(ANALYSIS_BATCH) {
            let idx = (start + i) % MAX_LOG_ENTRIES;
            if let Some(obs) = &self.log[idx] {
                let event = match obs.event_tag {
                    1 => "MEM_WARNING",
                    2 => "IDLE",
                    3 => "BOOT",
                    _ => "TICK",
                };
                prompt.push_str(&format!(
                    "  T+{}s: RAM={}% event={}\n",
                    obs.timestamp_s, obs.mem_pct, event
                ));
            }
        }

        prompt
    }

    /// Start an analysis cycle (send prompt to LLM via MCP).
    /// Records timestamp for cooldown enforcement.
    ///
    /// Phase A.5 (Path A): the MCP path is now legacy. New callers
    /// in the daemon use `begin_analysis_cycle` + the async TCP
    /// path in `draug_async::start_analysis_via_tcp`. This method
    /// stays for compositor's local DraugDaemon (still on MCP)
    /// until the local instance is dropped in a later step.
    pub fn start_analysis(&mut self, now_ms: u64) -> bool {
        let prompt = self.build_analysis_prompt();
        if libfolk::mcp::client::send_chat(&prompt).is_some() {
            self.waiting_for_llm = true;
            self.analysis_count += 1;
            self.last_analysis_ms = now_ms;
            true
        } else {
            false
        }
    }

    /// Phase A.5 (Path A): begin an analysis cycle for the direct-
    /// TCP path. Sets the same bookkeeping fields `start_analysis`
    /// would have set (`waiting_for_llm`, `analysis_count`,
    /// `last_analysis_ms`) and returns the prompt — the actual TCP
    /// hand-off lives in `draug_async::start_analysis_via_tcp` so
    /// it shares the slot pool with the rest of Draug's LLM calls.
    pub fn begin_analysis_cycle(&mut self, now_ms: u64) -> String {
        let prompt = self.build_analysis_prompt();
        self.waiting_for_llm = true;
        self.analysis_count = self.analysis_count.saturating_add(1);
        self.last_analysis_ms = now_ms;
        prompt
    }

    /// Phase A.5 (Path A): clear the waiting-for-LLM flag once the
    /// async TCP processor has consumed an analysis response (or a
    /// non-UTF-8 / parse-failure surrogate). Mirrors what
    /// `on_analysis_response` does internally on the happy path —
    /// kept as a separate method so the async machinery in
    /// `draug_async` doesn't need direct field access.
    pub fn finish_analysis_cycle(&mut self) {
        self.waiting_for_llm = false;
    }

    /// Check if Draug has been waiting too long for LLM and should give up.
    /// Returns true if timed out (caller should log and continue).
    pub fn check_waiting_timeout(&mut self, now_ms: u64) -> bool {
        if self.waiting_for_llm && self.last_analysis_ms > 0
            && now_ms.saturating_sub(self.last_analysis_ms) > 60_000 // 60s timeout
        {
            self.waiting_for_llm = false;
            return true;
        }
        // Also timeout dreams
        if self.dreaming && self.last_dream_ms > 0
            && now_ms.saturating_sub(self.last_dream_ms) > 120_000 // 2 min for dreams
        {
            self.dreaming = false;
            self.dream_target = None;
            return true;
        }
        false
    }

    /// Handle LLM response to analysis
    pub fn on_analysis_response(&mut self, response: &str) -> Option<String> {
        self.waiting_for_llm = false;

        // Parse {"action": "alert", "message": "..."} or {"action": "none"}
        if let Some(action) = extract_field(response, "action") {
            if action == "alert" {
                if let Some(msg) = extract_field(response, "message") {
                    return Some(format!("[Draug] {}", msg));
                }
            }
        }
        None
    }

    /// Get number of observations logged
    pub fn observation_count(&self) -> usize {
        self.log_count
    }

    // ═══════ AutoDream: Two-Hemisphere Self-Improving Software ════════
    //
    // Mode 1 (Refactor): CPU cycle reduction — headless benchmark V1 vs V2
    // Mode 2 (Creative): GUI vision — LLM sees render output + adds features
    //
    // Three Strikes Rule: after 3 failed refactor attempts, app is "perfected"

    /// Check if it's nighttime (23:00 - 06:00) based on RTC.
    pub fn is_nighttime() -> bool {
        let rtc = libfolk::sys::get_rtc();
        rtc.hour >= 23 || rtc.hour < 6
    }

    /// Get current idle threshold based on circadian rhythm.
    fn idle_threshold() -> u64 {
        if Self::is_nighttime() {
            DREAM_IDLE_NIGHT_MS // 5 min at night
        } else {
            DREAM_IDLE_DAY_MS // 45 min during day
        }
    }

    /// Check if the system should enter dream mode.
    /// Uses circadian rhythm: 5 min idle at night, 45 min during day.
    /// Nightmare mode blocked during daytime (too CPU-heavy).
    pub fn should_dream(&self, now_ms: u64) -> bool {
        self.active
            && !self.dreaming
            && !self.waiting_for_llm
            && self.dream_count < DREAM_MAX_PER_SESSION
            && now_ms.saturating_sub(self.last_user_input_ms) > Self::idle_threshold()
            && now_ms.saturating_sub(self.last_dream_ms) > DREAM_COOLDOWN_MS
    }

    /// Simple hash for strike tracking
    fn key_hash(key: &str) -> u32 {
        let mut h: u32 = 5381;
        for b in key.bytes() { h = h.wrapping_mul(33).wrapping_add(b as u32); }
        h
    }

    /// Check if an app has been "perfected" (3 failed refactors)
    pub fn is_perfected(&self, key: &str) -> bool {
        let h = Self::key_hash(key);
        self.strikes.iter().any(|s| matches!(s, Some((k, c)) if *k == h && *c >= DREAM_STRIKE_LIMIT))
    }

    /// Record a refactoring failure (strike)
    pub fn add_strike(&mut self, key: &str) {
        self.add_strike_by_hash(Self::key_hash(key));
    }

    /// Same as `add_strike` but takes the precomputed hash directly.
    /// Used by the IPC handler so we don't have to ship the key string
    /// over the wire.
    pub fn add_strike_by_hash(&mut self, h: u32) {
        // Find existing entry or empty slot
        for slot in &mut self.strikes {
            if let Some((k, c)) = slot {
                if *k == h { *c += 1; return; }
            }
        }
        // Insert new
        for slot in &mut self.strikes {
            if slot.is_none() { *slot = Some((h, 1)); return; }
        }
    }

    /// Reset strikes for an app (e.g., after user tweaks it)
    pub fn reset_strikes(&mut self, key: &str) {
        self.reset_strikes_by_hash(Self::key_hash(key));
    }

    /// Same as `reset_strikes` but takes the precomputed hash directly.
    pub fn reset_strikes_by_hash(&mut self, h: u32) {
        for slot in &mut self.strikes {
            if let Some((k, _)) = slot {
                if *k == h { *slot = None; return; }
            }
        }
    }

    /// Get the dream journal entry for a key — returns when it was last dreamt about.
    fn last_dreamt_about(&self, key: &str) -> u32 {
        let h = Self::key_hash(key);
        for entry in &self.dream_journal {
            if let Some((k, when)) = entry {
                if *k == h { return *when; }
            }
        }
        0 // Never dreamt about → highest priority
    }

    /// Record that we dreamt about this key.
    fn journal_record(&mut self, key: &str) {
        let h = Self::key_hash(key);
        // Update existing entry or find empty slot
        for entry in &mut self.dream_journal {
            if let Some((k, when)) = entry {
                if *k == h { *when = self.dream_count; return; }
            }
        }
        for entry in &mut self.dream_journal {
            if entry.is_none() {
                *entry = Some((h, self.dream_count));
                return;
            }
        }
        // Full — overwrite oldest
        let mut oldest_idx = 0;
        let mut oldest_when = u32::MAX;
        for (i, entry) in self.dream_journal.iter().enumerate() {
            if let Some((_, when)) = entry {
                if *when < oldest_when { oldest_when = *when; oldest_idx = i; }
            }
        }
        self.dream_journal[oldest_idx] = Some((h, self.dream_count));
    }

    /// Check if an app needs a dream (has friction, crashes, or is new).
    fn app_needs_dream(&self, key: &str) -> bool {
        let h = Self::key_hash(key);
        // 1. Friction score > 0 → needs Creative
        if self.friction.score_for(h) > 0 { return true; }
        // 2. Crash record → needs Nightmare
        for (ch, count) in &self.crash_hashes {
            if *ch == h && *count > 0 { return true; }
        }
        // 3. Never dreamt about → needs baseline Refactor
        if self.last_dreamt_about(key) == 0 { return true; }
        // 4. Not perfected for Refactor → still room to optimize
        if !self.is_perfected(key) { return true; }
        false
    }

    /// Pick a dream target using Digital Homeostasis.
    ///
    /// Instead of always forcing a dream, the engine checks if any app
    /// actually NEEDS improvement. If all apps are stable (perfected,
    /// zero friction, no crashes), it returns None and conserves budget.
    ///
    /// Priority: Friction → Crashes → New apps → Unperfected → Sleep
    pub fn start_dream(&mut self, cache_keys: &[&str], now_ms: u64) -> Option<(String, DreamMode)> {
        if cache_keys.is_empty() { return None; }

        // ── Digital Homeostasis: check if ANY app needs a dream ──
        let any_needs_dream = cache_keys.iter().any(|k| self.app_needs_dream(k));
        if !any_needs_dream {
            // All systems stable — conserve budget
            return None;
        }

        // ── Priority 1: Friction Override ──
        if let Some(frustrated_hash) = self.friction.most_frustrated() {
            for key in cache_keys {
                if Self::key_hash(key) == frustrated_hash {
                    let target = String::from(*key);
                    self.journal_record(&target);
                    self.dream_target = Some(target.clone());
                    self.dream_mode = DreamMode::Creative;
                    self.dreaming = true;
                    self.last_dream_ms = now_ms;
                    return Some((target, DreamMode::Creative));
                }
            }
        }

        // ── Priority 2: Crashed apps → Nightmare ──
        for key in cache_keys {
            let h = Self::key_hash(key);
            for (ch, count) in &self.crash_hashes {
                if *ch == h && *count > 0 {
                    let target = String::from(*key);
                    self.journal_record(&target);
                    self.dream_target = Some(target.clone());
                    self.dream_mode = DreamMode::Nightmare;
                    self.dreaming = true;
                    self.last_dream_ms = now_ms;
                    return Some((target, DreamMode::Nightmare));
                }
            }
        }

        // ── Priority 3: New apps (never dreamt) → Refactor baseline ──
        for key in cache_keys {
            if self.last_dreamt_about(key) == 0 && !self.is_perfected(key) {
                let target = String::from(*key);
                self.journal_record(&target);
                self.dream_target = Some(target.clone());
                self.dream_mode = DreamMode::Refactor;
                self.dreaming = true;
                self.last_dream_ms = now_ms;
                return Some((target, DreamMode::Refactor));
            }
        }

        // ── Priority 4: Normal rotation for unperfected apps ──
        let mut mode = match self.dream_count % 3 {
            0 => DreamMode::Refactor,
            1 => DreamMode::Creative,
            _ => DreamMode::Nightmare,
        };

        if mode == DreamMode::Nightmare && !Self::is_nighttime() {
            mode = DreamMode::Creative;
        }

        let mut best_key: Option<&str> = None;
        let mut best_when: u32 = u32::MAX;

        for key in cache_keys {
            if !self.app_needs_dream(key) { continue; }
            if mode == DreamMode::Refactor && self.is_perfected(key) { continue; }
            let when = self.last_dreamt_about(key);
            if when < best_when {
                best_when = when;
                best_key = Some(key);
            }
        }

        if let Some(key) = best_key {
            let target = String::from(key);
            self.journal_record(&target);
            self.dream_target = Some(target.clone());
            self.dream_mode = mode;
            self.dreaming = true;
            self.last_dream_ms = now_ms;
            Some((target, mode))
        } else {
            None // All filtered out — homeostasis achieved
        }
    }

    /// Build the dream prompt based on current mode.
    /// `app_name` is the cache key (original user prompt, e.g., "bouncing ball").
    /// `render_summary` is a text description of what the app currently draws.
    /// If the app has high friction score, adds frustration-aware guidance.
    pub fn build_dream_prompt(&self, source_code: &str, app_name: &str, render_summary: &str) -> String {
        // Context compression: strip comments and blank lines to save LLM tokens
        let compressed = compress_source(source_code);
        let source_code = &compressed;

        // The app_name IS the description — it's the original "gemini generate X" prompt
        let context = format!(
            "APP: '{}'\n\
             PURPOSE: This app was created by the command 'gemini generate {}'. \
             It should continue to fulfill this purpose after your modifications.\n",
            app_name, app_name
        );

        // Check if this app is frustrating the user
        let frustration_suffix = {
            let h = Self::key_hash(app_name);
            if self.friction.score_for(h) >= FRICTION_THRESHOLD {
                "\n\nUSER FRUSTRATION DETECTED: The user is frustrated with this app. \
                 Focus on usability: clearer layout, better visual feedback, \
                 more intuitive interaction, smoother animations."
            } else {
                ""
            }
        };

        match self.dream_mode {
            DreamMode::Refactor => format!(
                "You are Draug, optimizing WASM apps for Folkering OS.\n\n\
                 {}\n\
                 Current code:\n```rust\n{}\n```\n\n\
                 REFACTOR RULES:\n\
                 - ONLY reduce CPU cycles. Do NOT add features.\n\
                 - Do NOT change the visual output — it must look identical.\n\
                 - Pre-compute constants, use integer math, remove redundancy.\n\
                 - Do NOT remove safety checks.\n\
                 - Return ONLY the improved Rust code.",
                context, source_code
            ),
            DreamMode::Creative => format!(
                "You are Draug, the creative daemon of Folkering OS.\n\n\
                 {}\n\
                 Current code:\n```rust\n{}\n```\n\n\
                 Current visual output:\n{}\n\n\
                 CREATIVE RULES:\n\
                 - Add ONE meaningful visual improvement.\n\
                 - Good: smoother animation, better colors, text labels, layout polish.\n\
                 - Bad: changing the app's purpose, removing functionality.\n\
                 - Use Folkering palette: bg=0x001a1a2e, blue=0x003498db, green=0x0044FF44.\n\
                 - Keep it under 2KB compiled WASM.\n\
                 - Return ONLY the improved Rust code.{}",
                context, source_code, render_summary, frustration_suffix
            ),
            DreamMode::Nightmare => format!(
                "You are Draug in Nightmare mode — the immune system of Folkering OS.\n\n\
                 {}\n\
                 Current code:\n```rust\n{}\n```\n\n\
                 NIGHTMARE RULES:\n\
                 - HARDEN the code. Do NOT change behavior.\n\
                 - What if screen_width=0 or screen_height=0? Add .max(1) before division.\n\
                 - What if folk_random() returns i32::MIN? Use .wrapping_abs() or .clamp().\n\
                 - What if coordinates overflow? Use .clamp(0, width) for bounds.\n\
                 - Use saturating_add/sub instead of +/- where overflow is possible.\n\
                 - Return ONLY the hardened Rust code.",
                context, source_code
            ),
            DreamMode::DriverRefactor => format!(
                "You are Draug, optimizing a WASM device driver for Folkering OS.\n\n\
                 DRIVER: {}\n\
                 Current code:\n```rust\n{}\n```\n\n\
                 DRIVER REFACTOR RULES:\n\
                 - ONLY reduce fuel consumption. Do NOT change functionality.\n\
                 - The IRQ wait loop structure (folk_wait_irq/folk_ack_irq) MUST be preserved.\n\
                 - Device initialization sequence MUST be identical.\n\
                 - Pre-compute register offsets as constants.\n\
                 - Combine redundant MMIO reads.\n\
                 - Use bitwise ops instead of branches where possible.\n\
                 - #![no_std] #![no_main] #![allow(unused)] — no allocation.\n\
                 - Return ONLY the improved Rust code.",
                app_name, source_code
            ),
            DreamMode::DriverNightmare => format!(
                "You are Draug in Driver Nightmare mode — hardening a WASM device driver.\n\n\
                 DRIVER: {}\n\
                 Current code:\n```rust\n{}\n```\n\n\
                 DRIVER NIGHTMARE RULES:\n\
                 - HARDEN the driver. Do NOT change its purpose.\n\
                 - What if folk_mmio_read_u32 returns -1 (SFI violation)? Check for it.\n\
                 - What if IRQs fire faster than the handler can process? Add overflow guards.\n\
                 - What if folk_dma_alloc returns -1? Handle allocation failure.\n\
                 - Use saturating_add for all counters.\n\
                 - Add folk_log debug output for unexpected register values.\n\
                 - #![no_std] #![no_main] #![allow(unused)] — no allocation.\n\
                 - Return ONLY the hardened Rust code.",
                app_name, source_code
            ),
        }
    }

    /// Record a crash (fuel exhaustion) for priority Nightmare dreaming.
    pub fn record_crash(&mut self, key: &str) {
        let h = Self::key_hash(key);
        for (ch, count) in &mut self.crash_hashes {
            if *ch == h && *count > 0 { *count = count.saturating_add(1); return; }
        }
        for (ch, count) in &mut self.crash_hashes {
            if *count == 0 { *ch = h; *count = 1; return; }
        }
    }

    /// Pick a driver to dream about based on stability metrics.
    /// Returns (vendor_id, device_id, mode) or None if all drivers are stable.
    pub fn pick_driver_dream(
        &self,
        drivers: &[(u16, u16, u16, u16, u32)] // (vid, did, version, stability, fault_count)
    ) -> Option<(u16, u16, DreamMode)> {
        let mut worst: Option<(u16, u16, u16)> = None; // (vid, did, stability)
        for &(vid, did, _ver, stability, faults) in drivers {
            // Dream about drivers with faults or low stability
            if faults > 0 || stability < 500 {
                if worst.map_or(true, |(_, _, s)| stability < s) {
                    worst = Some((vid, did, stability));
                }
            }
        }
        worst.map(|(vid, did, stability)| {
            let mode = if stability < 200 {
                DreamMode::DriverNightmare
            } else {
                DreamMode::DriverRefactor
            };
            (vid, did, mode)
        })
    }

    /// Public key hash for main.rs friction signal recording.
    pub fn key_hash_pub(key: &str) -> u32 { Self::key_hash(key) }

    /// Record dream completion.
    pub fn on_dream_complete(&mut self, now_ms: u64) {
        self.dreaming = false;
        self.dream_count += 1;
        self.last_dream_ms = now_ms;
        self.dream_target = None;
    }

    /// Cancel dreaming (user woke up).
    pub fn wake_up(&mut self) {
        if self.dreaming {
            self.dreaming = false;
            self.dream_target = None;
        }
    }

    pub fn dream_target(&self) -> Option<&str> { self.dream_target.as_deref() }
    pub fn is_dreaming(&self) -> bool { self.dreaming }
    pub fn dream_count(&self) -> u32 { self.dream_count }
    pub fn current_dream_mode(&self) -> DreamMode { self.dream_mode }
    pub fn last_input_ms(&self) -> u64 { self.last_user_input_ms }

    // ── Synapse GC: Garbage Collection of old WASM versions ─────────

    /// Identify cache keys that should be garbage collected.
    /// Returns keys that are "perfected" (3 strikes) AND older than the threshold.
    /// The compositor removes these from wasm_cache to free RAM.
    pub fn gc_candidates<'a>(&self, cache_keys: &[&'a str]) -> alloc::vec::Vec<&'a str> {
        let mut candidates = alloc::vec::Vec::new();
        for &key in cache_keys {
            // Only GC apps that are perfected (fully optimized, no more dreams)
            if self.is_perfected(key) {
                // Check if we've dreamt about this more than 5 times (well-tested)
                let h = Self::key_hash(key);
                let dreams = self.dream_journal.iter()
                    .filter_map(|e| *e)
                    .filter(|(k, _)| *k == h)
                    .map(|(_, count)| count)
                    .next()
                    .unwrap_or(0);
                if dreams >= 5 {
                    candidates.push(key);
                }
            }
        }
        candidates
    }

    /// Count total strikes across all tracked apps
    pub fn total_strikes(&self) -> u32 {
        self.strikes.iter().filter_map(|s| *s).map(|(_, c)| c as u32).sum()
    }

    /// Count perfected apps
    pub fn perfected_count(&self, cache_keys: &[&str]) -> usize {
        cache_keys.iter().filter(|k| self.is_perfected(k)).count()
    }
    pub fn is_waiting(&self) -> bool { self.waiting_for_llm }
    pub fn analysis_count(&self) -> u32 { self.analysis_count }

    // Morning Briefing (`pending_creative` queue) was extracted into
    // `compositor::briefing::BriefingState` in Phase A.5 step 3 — it
    // was always compositor UI state, never agent state.

    // ═══════ Token Scheduler: Attention-Based LLM Priority ══════════
    //
    // The most precious resource isn't CPU time — it's LLM tokens.
    // Draug yields to user-facing tasks, only consuming tokens during idle.

    /// Check if Draug should suppress LLM calls to preserve tokens for the user.
    /// Returns true if Draug should stay silent.
    pub fn should_yield_tokens(&self, active_agent: bool, now_ms: u64) -> bool {
        // Always yield if user has an active agent session
        if active_agent { return true; }
        // Yield if user was active in last 30s
        if now_ms.saturating_sub(self.last_user_input_ms) < 30_000 { return true; }
        false
    }

    // ═══════ Pattern-Mining: Phase 1 of AutoDream Cycle ══════════════════
    //
    // Drains the kernel telemetry ring buffer, formats events as a compact
    // text log, sends to LLM for analysis, and saves insights to Synapse VFS.
    // Runs BEFORE app dreams — provides strategic context for optimization.

    /// Check if Pattern-Mining should run.
    /// Requires: 5 min idle, cooldown elapsed, not dreaming, telemetry available.
    pub fn should_mine_patterns(&self, now_ms: u64) -> bool {
        self.active
            && !self.dreaming
            && !self.waiting_for_llm
            && now_ms.saturating_sub(self.last_user_input_ms) > PATTERN_MINE_IDLE_MS
            && now_ms.saturating_sub(self.last_pattern_mine_ms) > PATTERN_MINE_COOLDOWN_MS
    }

    /// Execute Pattern-Mining: drain telemetry → format → LLM analyze → save insight.
    /// Returns Some(insight_text) on success, None on failure or no data.
    pub fn mine_patterns(&mut self, now_ms: u64) -> Option<String> {
        // Step 1: Drain telemetry ring buffer via syscall 0x9C
        const EVENT_SIZE: usize = 16; // sizeof TelemetryEvent
        let max_events = PATTERN_MINE_MAX_EVENTS;
        let buf_size = max_events * EVENT_SIZE;
        let mut buf = alloc::vec![0u8; buf_size];
        let drained = libfolk::sys::telemetry_drain(&mut buf, max_events);

        if drained == 0 {
            // No telemetry data — nothing to mine
            self.last_pattern_mine_ms = now_ms;
            return None;
        }

        libfolk::sys::io::write_str("[Draug] Pattern-Mining: draining ");
        write_dec(drained as u32);
        libfolk::sys::io::write_str(" telemetry events\n");

        // Step 2: Format events as compact text log
        let log_text = format_telemetry_log(&buf, drained);

        if log_text.is_empty() {
            self.last_pattern_mine_ms = now_ms;
            return None;
        }

        // Step 3: Chunk if necessary (avoid LLM context overflow)
        let analysis_input = if log_text.len() > PATTERN_MINE_CHUNK_SIZE {
            // Take the most recent chunk (end of log)
            let start = log_text.len() - PATTERN_MINE_CHUNK_SIZE;
            // Find a newline boundary
            let boundary = log_text[start..].find('\n').unwrap_or(0) + start + 1;
            &log_text[boundary..]
        } else {
            &log_text
        };

        // Step 4: Send to LLM for analysis
        let prompt = format!(
            "Analyze this telemetry log from Folkering OS. \
             Identify repeating patterns, high friction (apps that crash or are used inefficiently together), \
             and suggest ONE concrete architectural improvement or IPC shortcut.\n\
             Be concise (max 3 sentences).\n\n\
             TELEMETRY LOG ({} events):\n{}",
            drained, analysis_input
        );

        let mut response = alloc::vec![0u8; 512];
        let resp_len = libfolk::sys::ask_gemini(&prompt, &mut response);

        if resp_len == 0 {
            libfolk::sys::io::write_str("[Draug] Pattern-Mining: LLM returned empty response\n");
            self.last_pattern_mine_ms = now_ms;
            return None;
        }

        let insight = match core::str::from_utf8(&response[..resp_len]) {
            Ok(s) => String::from(s.trim()),
            Err(_) => {
                self.last_pattern_mine_ms = now_ms;
                return None;
            }
        };

        // Step 5: Deduplicate — skip if same insight as last time
        let insight_hash = Self::key_hash(&insight);
        if insight_hash == self.last_insight_hash {
            libfolk::sys::io::write_str("[Draug] Pattern-Mining: duplicate insight, skipping save\n");
            self.last_pattern_mine_ms = now_ms;
            return Some(insight);
        }

        // Step 6: Save to Synapse VFS
        let rtc = libfolk::sys::get_rtc();
        let filename = format!(
            "autodream/insights/{:04}-{:02}-{:02}_{:02}{:02}.txt",
            rtc.year, rtc.month, rtc.day, rtc.hour, rtc.minute
        );

        let file_content = format!(
            "#AutoDreamInsight\n\
             # Pattern-Mining Phase 1 — {}\n\
             # Events analyzed: {} | Uptime: {}s\n\n\
             {}\n",
            filename, drained, now_ms / 1000, insight
        );

        let _ = libfolk::sys::synapse::write_file(&filename, file_content.as_bytes());

        libfolk::sys::io::write_str("[Draug] Pattern-Mining: insight saved to ");
        libfolk::sys::io::write_str(&filename);
        libfolk::sys::io::write_str("\n");

        // Update state
        self.last_pattern_mine_ms = now_ms;
        self.last_insight_hash = insight_hash;
        self.pattern_mine_count += 1;

        Some(insight)
    }

    /// Get pattern mining statistics.
    pub fn pattern_mine_count(&self) -> u32 { self.pattern_mine_count }
}

// ═══════ Pattern-Mining Helpers ══════════════════════════════════════════

/// Action type names for telemetry formatting
const ACTION_NAMES: [&str; 12] = [
    "AppOpened", "AppClosed", "IpcSent", "UiInteract",
    "AiReq", "AiDone", "FileRead", "FileWrite",
    "Omnibar", "Alert", "NetEvt", "AppErr",
];

/// Format telemetry events as a compact text log for LLM consumption.
/// Each line: "T+{seconds} {action} target={id} dur={ms}"
fn format_telemetry_log(buf: &[u8], count: usize) -> String {
    let mut out = String::with_capacity(count * 40);
    let event_size = 16; // sizeof TelemetryEvent

    for i in 0..count {
        let off = i * event_size;
        if off + event_size > buf.len() { break; }

        let action_type = buf[off] as usize;
        let _flags = buf[off + 1];
        let source_task = u16::from_le_bytes([buf[off + 2], buf[off + 3]]);
        let target_id = u32::from_le_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
        let duration_ms = u32::from_le_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]);
        let timestamp_ms = u32::from_le_bytes([buf[off + 12], buf[off + 13], buf[off + 14], buf[off + 15]]);

        let action_name = if action_type < ACTION_NAMES.len() {
            ACTION_NAMES[action_type]
        } else {
            "Unknown"
        };

        // Compact format: "T+123s AppOpened t3 id=0x1234 dur=50ms"
        use core::fmt::Write;
        let _ = write!(out, "T+{}s {} t{} id={:#x}",
            timestamp_ms / 1000, action_name, source_task, target_id);
        if duration_ms > 0 {
            let _ = write!(out, " dur={}ms", duration_ms);
        }
        out.push('\n');
    }
    out
}

/// Simple decimal writer (for serial output without format!)
fn write_dec(val: u32) {
    if val == 0 {
        libfolk::sys::io::write_char(b'0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut n = val;
    let mut i = 0;
    while n > 0 { buf[i] = b'0' + (n % 10) as u8; n /= 10; i += 1; }
    while i > 0 { i -= 1; libfolk::sys::io::write_char(buf[i]); }
}

/// Extract a string value from JSON — delegates to shared libfolk::json parser.
fn extract_field(json: &str, key: &str) -> Option<String> {
    libfolk::json::extract(json, key).map(String::from)
}
