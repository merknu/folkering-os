//! Telemetry Ring Buffer — App-level event logging for AutoDream pattern mining
//!
//! A lock-free ring buffer for recording high-level application and system events.
//! Unlike IQE (which tracks hardware-level ISR→render latencies), this module
//! captures semantic events: which apps are opened, IPC patterns, UI interactions,
//! and AI inference requests.
//!
//! # Architecture
//! - Static `[TelemetryEvent; 8192]` ring buffer (192KB)
//! - Lock-free MPSC: multiple producers (WASM apps via host function, kernel),
//!   single consumer (AutoDream drain via syscall)
//! - Overwrite-oldest semantics: when full, head advances past tail with warning
//!
//! # AutoDream Integration
//! When the system goes idle (Draug tick), AutoDream calls `drain_all()` to
//! harvest events for pattern mining. The buffer is cleared after drain.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

// ── Event Types ─────────────────────────────────────────────────────────

/// High-level telemetry action types logged by apps and kernel.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ActionType {
    /// WASM app was opened (target_id = app name hash)
    AppOpened = 0,
    /// WASM app was closed (target_id = app name hash)
    AppClosed = 1,
    /// IPC message sent between tasks (target_id = destination task ID)
    IpcMessageSent = 2,
    /// UI interaction: button press, text input, etc. (target_id = widget hash)
    UiInteraction = 3,
    /// AI inference was requested (target_id = prompt length)
    AiInferenceRequested = 4,
    /// AI inference completed (target_id = response length, duration_ms = latency)
    AiInferenceCompleted = 5,
    /// File read from Synapse VFS (target_id = file name hash)
    FileAccessed = 6,
    /// File written to Synapse VFS (target_id = file name hash)
    FileWritten = 7,
    /// Omnibar command executed (target_id = command hash)
    OmnibarCommand = 8,
    /// System metric threshold crossed (target_id = metric_id)
    MetricAlert = 9,
    /// Network event: HTTP request, DNS, etc. (target_id = 0)
    NetworkEvent = 10,
    /// Error/panic in WASM app (target_id = app name hash)
    AppError = 11,
}

impl ActionType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::AppOpened,
            1 => Self::AppClosed,
            2 => Self::IpcMessageSent,
            3 => Self::UiInteraction,
            4 => Self::AiInferenceRequested,
            5 => Self::AiInferenceCompleted,
            6 => Self::FileAccessed,
            7 => Self::FileWritten,
            8 => Self::OmnibarCommand,
            9 => Self::MetricAlert,
            10 => Self::NetworkEvent,
            11 => Self::AppError,
            _ => Self::UiInteraction, // fallback
        }
    }
}

// ── Event Structure ─────────────────────────────────────────────────────

/// A single telemetry event — 16 bytes, tightly packed.
///
/// Designed for minimal overhead: no allocation, no strings, just IDs and timestamps.
/// The `target_id` meaning depends on `action_type` (documented in ActionType).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TelemetryEvent {
    /// What happened (ActionType as u8)
    pub action_type: u8,
    /// Reserved for future flags (e.g., priority, source task)
    pub flags: u8,
    /// Source task ID (which task logged this)
    pub source_task: u16,
    /// Context-dependent target (app hash, task ID, metric ID, etc.)
    pub target_id: u32,
    /// Duration in milliseconds (0 if not applicable)
    pub duration_ms: u32,
    /// Uptime in milliseconds when event was recorded
    pub timestamp_ms: u32,
}

impl TelemetryEvent {
    pub const fn empty() -> Self {
        Self {
            action_type: 0,
            flags: 0,
            source_task: 0,
            target_id: 0,
            duration_ms: 0,
            timestamp_ms: 0,
        }
    }
}

// ── Ring Buffer ─────────────────────────────────────────────────────────

const RING_SIZE: usize = 8192; // 8K events × 16 bytes = 128KB
const RING_MASK: usize = RING_SIZE - 1;

/// Lock-free ring buffer with overwrite-oldest semantics.
///
/// Uses atomic head (producer) and tail (consumer) pointers.
/// When full: head overwrites oldest event and advances tail with it.
/// A separate overflow counter tracks how many events were lost.
struct TelemetryRing {
    events: [TelemetryEvent; RING_SIZE],
    /// Next write position (producer advances)
    head: AtomicUsize,
    /// Next read position (consumer advances during drain)
    tail: AtomicUsize,
    /// Total events ever recorded (monotonic, never reset)
    total_recorded: AtomicU32,
    /// Number of events overwritten before drain (cumulative)
    overflow_count: AtomicU32,
    /// Flag: set to 1 when overflow warning has been printed (avoids spam)
    overflow_warned: AtomicU32,
}

static mut RING: TelemetryRing = TelemetryRing {
    events: [TelemetryEvent::empty(); RING_SIZE],
    head: AtomicUsize::new(0),
    tail: AtomicUsize::new(0),
    total_recorded: AtomicU32::new(0),
    overflow_count: AtomicU32::new(0),
    overflow_warned: AtomicU32::new(0),
};

// ── Producer API (called from ISR, kernel, host functions) ──────────────

/// Record a telemetry event. Lock-free, safe from any context.
///
/// If the ring is full, the oldest event is overwritten and `overflow_count`
/// is incremented. A serial warning is printed on the FIRST overflow.
pub fn record(action: ActionType, target_id: u32, duration_ms: u32) {
    let timestamp_ms = (crate::timer::uptime_ms() & 0xFFFFFFFF) as u32;

    unsafe {
        let head = RING.head.load(Ordering::Relaxed);
        let next_head = (head + 1) & RING_MASK;
        let tail = RING.tail.load(Ordering::Acquire);

        // Check if full: next_head == tail means we'd collide
        if next_head == tail {
            // Overwrite oldest: advance tail to make room
            RING.tail.store((tail + 1) & RING_MASK, Ordering::Release);
            RING.overflow_count.fetch_add(1, Ordering::Relaxed);

            // Print warning on first overflow (avoid serial spam)
            if RING.overflow_warned.compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                crate::serial_strln!("[TELEMETRY] WARNING: Ring buffer full, overwriting oldest events. Consider increasing RING_SIZE or draining more frequently.");
            }
        }

        // Write event
        let slot = &mut RING.events[head];
        slot.action_type = action as u8;
        slot.flags = 0;
        slot.source_task = 0; // TODO: get current task ID from scheduler
        slot.target_id = target_id;
        slot.duration_ms = duration_ms;
        slot.timestamp_ms = timestamp_ms;

        // Advance head
        RING.head.store(next_head, Ordering::Release);
        RING.total_recorded.fetch_add(1, Ordering::Relaxed);
    }
}

/// Record with explicit source task ID.
pub fn record_from_task(action: ActionType, target_id: u32, duration_ms: u32, task_id: u16) {
    let timestamp_ms = (crate::timer::uptime_ms() & 0xFFFFFFFF) as u32;

    unsafe {
        let head = RING.head.load(Ordering::Relaxed);
        let next_head = (head + 1) & RING_MASK;
        let tail = RING.tail.load(Ordering::Acquire);

        if next_head == tail {
            RING.tail.store((tail + 1) & RING_MASK, Ordering::Release);
            RING.overflow_count.fetch_add(1, Ordering::Relaxed);
            if RING.overflow_warned.compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                crate::serial_strln!("[TELEMETRY] WARNING: Ring buffer full, overwriting oldest.");
            }
        }

        let slot = &mut RING.events[head];
        slot.action_type = action as u8;
        slot.flags = 0;
        slot.source_task = task_id;
        slot.target_id = target_id;
        slot.duration_ms = duration_ms;
        slot.timestamp_ms = timestamp_ms;

        RING.head.store(next_head, Ordering::Release);
        RING.total_recorded.fetch_add(1, Ordering::Relaxed);
    }
}

// ── Consumer API (called by AutoDream via syscall) ──────────────────────

/// Drain all pending events into a userspace buffer.
/// Returns the number of events copied.
///
/// This is the primary interface for AutoDream pattern mining.
/// After drain, the ring is empty (tail catches up to head).
pub fn drain_to_user(buf_vaddr: usize, max_count: usize) -> usize {
    let event_size = core::mem::size_of::<TelemetryEvent>();
    let mut count = 0usize;

    unsafe {
        loop {
            let tail = RING.tail.load(Ordering::Acquire);
            let head = RING.head.load(Ordering::Relaxed);

            if tail == head || count >= max_count {
                break;
            }

            // Copy event to userspace buffer
            let event = &RING.events[tail];
            let dst = (buf_vaddr + count * event_size) as *mut TelemetryEvent;
            core::ptr::write(dst, *event);

            // Advance tail
            RING.tail.store((tail + 1) & RING_MASK, Ordering::Release);
            count += 1;
        }

        // Reset overflow warning flag after successful drain
        if count > 0 {
            RING.overflow_warned.store(0, Ordering::Relaxed);
        }
    }

    count
}

/// Get ring buffer statistics without draining.
/// Returns (pending_count, total_recorded, overflow_count).
pub fn stats() -> (u32, u32, u32) {
    unsafe {
        let head = RING.head.load(Ordering::Relaxed);
        let tail = RING.tail.load(Ordering::Relaxed);
        let pending = if head >= tail {
            (head - tail) as u32
        } else {
            (RING_SIZE - tail + head) as u32
        };
        let total = RING.total_recorded.load(Ordering::Relaxed);
        let overflow = RING.overflow_count.load(Ordering::Relaxed);
        (pending, total, overflow)
    }
}
