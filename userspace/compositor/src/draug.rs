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

extern crate alloc;
use alloc::string::String;
use alloc::format;

/// Interval between Draug ticks (in milliseconds)
pub const TICK_INTERVAL_MS: u64 = 10_000; // 10 seconds

/// Maximum observation log entries before forced consolidation
pub const MAX_LOG_ENTRIES: usize = 20;

/// Minimum entries before analysis can trigger
pub const ANALYSIS_BATCH: usize = 3;

/// AutoDream: idle threshold before dreaming starts (15 minutes)
pub const DREAM_IDLE_MS: u64 = 900_000;

/// AutoDream: cooldown between dreams (10 minutes)
pub const DREAM_COOLDOWN_MS: u64 = 600_000;

/// AutoDream: max dreams per session
pub const DREAM_MAX_PER_SESSION: u32 = 5;

/// AutoDream: max refactoring failures before marking as "perfected"
pub const DREAM_STRIKE_LIMIT: u8 = 3;

/// Dream modes — three hemispheres
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DreamMode {
    /// Left brain: CPU cycle reduction — headless benchmark V1 vs V2
    Refactor,
    /// Right brain: GUI vision — LLM sees render output + adds features
    Creative,
    /// Immune system: fuzzing — inject extreme inputs to find crashes
    Nightmare,
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

/// Maximum command history entries for prediction
pub const MAX_CMD_HISTORY: usize = 16;

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

    /// Command history for prediction (Pillar 4)
    cmd_history: [Option<alloc::string::String>; MAX_CMD_HISTORY],
    cmd_head: usize,
    cmd_count: usize,

    /// AutoDream state
    dream_count: u32,
    last_dream_ms: u64,
    dreaming: bool,
    dream_target: Option<alloc::string::String>,
    dream_mode: DreamMode,

    /// Strike tracker: cache_key_hash → failure count
    /// Apps with 3 strikes are "perfected" and skipped
    strikes: [Option<(u32, u8)>; 8], // (key_hash, strike_count)
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
            cmd_history: [const { None }; MAX_CMD_HISTORY],
            cmd_head: 0,
            cmd_count: 0,
            dream_count: 0,
            last_dream_ms: 0,
            dreaming: false,
            dream_target: None,
            dream_mode: DreamMode::Refactor,
            strikes: [const { None }; 8],
        }
    }

    /// Record a user command for prediction history.
    pub fn record_command(&mut self, cmd: &str) {
        self.cmd_history[self.cmd_head] = Some(String::from(cmd));
        self.cmd_head = (self.cmd_head + 1) % MAX_CMD_HISTORY;
        if self.cmd_count < MAX_CMD_HISTORY { self.cmd_count += 1; }
    }

    /// Predict what the user might ask next based on command history.
    /// Simple frequency analysis: returns the most common command that
    /// followed the last command, if pattern is strong enough (>50% match).
    pub fn predict_next(&self) -> Option<&str> {
        if self.cmd_count < 4 { return None; } // Need enough history

        // Get the last command
        let last_idx = (self.cmd_head + MAX_CMD_HISTORY - 1) % MAX_CMD_HISTORY;
        let last_cmd = self.cmd_history[last_idx].as_deref()?;

        // Count what follows `last_cmd` in history
        let mut best: Option<&str> = None;
        let mut best_count = 0u32;
        let mut total_follows = 0u32;

        for i in 0..self.cmd_count.saturating_sub(1) {
            let idx = (self.cmd_head + MAX_CMD_HISTORY - self.cmd_count + i) % MAX_CMD_HISTORY;
            let next_idx = (idx + 1) % MAX_CMD_HISTORY;
            if let (Some(cmd), Some(next)) = (&self.cmd_history[idx], &self.cmd_history[next_idx]) {
                if cmd.as_str() == last_cmd {
                    total_follows += 1;
                    // Count this "next" command
                    let mut count = 0u32;
                    for j in 0..self.cmd_count.saturating_sub(1) {
                        let ji = (self.cmd_head + MAX_CMD_HISTORY - self.cmd_count + j) % MAX_CMD_HISTORY;
                        let jn = (ji + 1) % MAX_CMD_HISTORY;
                        if let (Some(jc), Some(jnc)) = (&self.cmd_history[ji], &self.cmd_history[jn]) {
                            if jc.as_str() == last_cmd && jnc.as_str() == next.as_str() {
                                count += 1;
                            }
                        }
                    }
                    if count > best_count {
                        best_count = count;
                        best = Some(next.as_str());
                    }
                }
            }
        }

        // Only predict if >50% confidence
        if total_follows >= 2 && best_count * 2 > total_follows {
            best
        } else {
            None
        }
    }

    /// Record user input activity (resets idle timer)
    pub fn on_user_input(&mut self, now_ms: u64) {
        self.last_user_input_ms = now_ms;
    }

    /// Check if it's time for a tick. Call from main loop.
    pub fn should_tick(&self, now_ms: u64) -> bool {
        self.active && !self.waiting_for_llm
            && now_ms.saturating_sub(self.last_tick_ms) >= TICK_INTERVAL_MS
    }

    /// Execute a tick: collect telemetry and log it.
    pub fn tick(&mut self, now_ms: u64) {
        self.last_tick_ms = now_ms;

        let (total_mb, used_mb, mem_pct) = libfolk::sys::memory_stats();
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
    /// Only runs when user is idle (>60s since last input).
    pub fn should_analyze(&self, now_ms: u64) -> bool {
        self.active
            && !self.waiting_for_llm
            && self.log_count >= ANALYSIS_BATCH
            && now_ms.saturating_sub(self.last_user_input_ms) > 30_000 // 30s idle
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

    /// Start an analysis cycle (send prompt to LLM via MCP)
    pub fn start_analysis(&mut self) -> bool {
        let prompt = self.build_analysis_prompt();
        if libfolk::mcp::client::send_chat(&prompt).is_some() {
            self.waiting_for_llm = true;
            self.analysis_count += 1;
            true
        } else {
            false
        }
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

    /// Check if the system should enter dream mode.
    pub fn should_dream(&self, now_ms: u64) -> bool {
        self.active
            && !self.dreaming
            && !self.waiting_for_llm
            && self.dream_count < DREAM_MAX_PER_SESSION
            && now_ms.saturating_sub(self.last_user_input_ms) > DREAM_IDLE_MS
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
        let h = Self::key_hash(key);
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
        let h = Self::key_hash(key);
        for slot in &mut self.strikes {
            if let Some((k, _)) = slot {
                if *k == h { *slot = None; return; }
            }
        }
    }

    /// Pick a cached WASM app and choose dream mode.
    /// Skips "perfected" apps for Refactor mode.
    /// Alternates between Refactor and Creative.
    pub fn start_dream(&mut self, cache_keys: &[&str]) -> Option<(String, DreamMode)> {
        if cache_keys.is_empty() { return None; }

        // Rotate modes: 0=Refactor, 1=Creative, 2=Nightmare
        let mode = match self.dream_count % 3 {
            0 => DreamMode::Refactor,
            1 => DreamMode::Creative,
            _ => DreamMode::Nightmare,
        };

        // Find a suitable target
        for i in 0..cache_keys.len() {
            let idx = ((self.dream_count as usize) + i) % cache_keys.len();
            let key = cache_keys[idx];
            // Skip perfected apps for Refactor mode
            if mode == DreamMode::Refactor && self.is_perfected(key) {
                continue;
            }
            let target = String::from(key);
            self.dream_target = Some(target.clone());
            self.dream_mode = mode;
            self.dreaming = true;
            return Some((target, mode));
        }

        // All apps perfected for Refactor? Try Creative instead
        if mode == DreamMode::Refactor && !cache_keys.is_empty() {
            let idx = (self.dream_count as usize) % cache_keys.len();
            let target = String::from(cache_keys[idx]);
            self.dream_target = Some(target.clone());
            self.dream_mode = DreamMode::Creative;
            self.dreaming = true;
            return Some((target, DreamMode::Creative));
        }
        None
    }

    /// Build the dream prompt based on current mode.
    pub fn build_dream_prompt(&self, source_code: &str, app_name: &str, render_summary: &str) -> String {
        match self.dream_mode {
            DreamMode::Refactor => format!(
                "You are Draug, optimizing WASM apps for Folkering OS.\n\
                 The system is idle. Refactor this app for FEWER CPU CYCLES.\n\n\
                 App: '{}'\n```rust\n{}\n```\n\n\
                 Rules:\n\
                 - Remove unnecessary calculations\n\
                 - Use simpler math where possible\n\
                 - Do NOT add new features\n\
                 - Return ONLY the improved Rust code",
                app_name, source_code
            ),
            DreamMode::Creative => format!(
                "You are Draug, the creative daemon of Folkering OS.\n\
                 The system is idle. Improve the VISUAL QUALITY of this app.\n\n\
                 App: '{}'\n```rust\n{}\n```\n\n\
                 Current render output:\n{}\n\n\
                 Rules:\n\
                 - Add ONE meaningful visual improvement (better colors, animation, layout)\n\
                 - Keep the core functionality the same\n\
                 - Return ONLY the improved Rust code",
                app_name, source_code, render_summary
            ),
            DreamMode::Nightmare => format!(
                "You are Draug in Nightmare mode — the immune system of Folkering OS.\n\
                 Your job is to find and FIX vulnerabilities in this WASM app.\n\n\
                 App: '{}'\n```rust\n{}\n```\n\n\
                 Think like a fuzzer:\n\
                 - What happens with screen_width=0? Division by zero?\n\
                 - What if folk_random() returns i32::MIN or i32::MAX?\n\
                 - What if folk_poll_event returns 1000 events per frame?\n\
                 - Are there array index overflows with extreme coordinates?\n\n\
                 Add defensive checks:\n\
                 - Clamp values to safe ranges\n\
                 - Avoid division by zero\n\
                 - Bounds-check array indices\n\
                 - Handle edge cases gracefully\n\n\
                 Return ONLY the hardened Rust code. No explanation.",
                app_name, source_code
            ),
        }
    }

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
}

/// Extract a string value from JSON — delegates to shared libfolk::json parser.
fn extract_field(json: &str, key: &str) -> Option<String> {
    libfolk::json::extract(json, key).map(String::from)
}
