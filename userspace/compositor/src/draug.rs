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
pub const TICK_INTERVAL_MS: u64 = 30_000; // 30 seconds

/// Maximum observation log entries before forced consolidation
pub const MAX_LOG_ENTRIES: usize = 20;

/// Maximum entries to send to LLM per analysis cycle
pub const ANALYSIS_BATCH: usize = 8;

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
            && now_ms.saturating_sub(self.last_user_input_ms) > 60_000
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
}

/// Simple JSON field extractor (reuse from agent.rs pattern)
fn extract_field(json: &str, key: &str) -> Option<String> {
    let search = format!("\"{}\"", key);
    let pos = json.find(&search)?;
    let after = &json[pos + search.len()..];
    let after = after.trim_start();
    if !after.starts_with(':') { return None; }
    let val = after[1..].trim_start();
    if !val.starts_with('"') { return None; }
    let content = &val[1..];
    let end = content.find('"')?;
    Some(String::from(&content[..end]))
}
