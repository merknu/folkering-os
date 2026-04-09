//! Compositor State — Consolidated state structs for main loop
//!
//! Replaces ~100 loose `let mut` declarations in main() with typed structs.
//! State becomes explicitly passable to functions, enabling future extraction
//! of command dispatch, MCP handling, and rendering into separate modules.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;

// ── Omnibar / Text Input State ──────────────────────────────────────────

/// State for the omnibar text input and clipboard.
pub struct InputState {
    pub text_buffer: [u8; 256],
    pub text_len: usize,
    pub cursor_pos: usize,
    pub omnibar_visible: bool,
    pub show_results: bool,
    pub execute_command: bool,
    pub clipboard_buf: [u8; 256],
    pub clipboard_len: usize,
    pub caret_visible: bool,
    pub last_caret_flip_ms: u64,
    pub show_ram_graph: bool,
    pub prev_left_button: bool,
}

impl InputState {
    pub const fn new() -> Self {
        Self {
            text_buffer: [0; 256],
            text_len: 0,
            cursor_pos: 0,
            omnibar_visible: true,
            show_results: false,
            execute_command: false,
            clipboard_buf: [0; 256],
            clipboard_len: 0,
            caret_visible: true,
            last_caret_flip_ms: 0,
            show_ram_graph: false,
            prev_left_button: false,
        }
    }

    /// Get the current input as a string slice
    pub fn text(&self) -> &str {
        core::str::from_utf8(&self.text_buffer[..self.text_len]).unwrap_or("")
    }

    /// Clear the input buffer
    pub fn clear(&mut self) {
        self.text_len = 0;
        self.cursor_pos = 0;
        for b in &mut self.text_buffer { *b = 0; }
        self.show_results = false;
    }
}

// ── WASM Application State ──────────────────────────────────────────────

/// State for active WASM applications and caching.
pub struct WasmState {
    pub active_app: Option<crate::wasm_runtime::PersistentWasmApp>,
    pub active_app_key: Option<String>,
    pub app_open_since_ms: u64,
    pub fuel_fail_count: u8,
    pub last_bytes: Option<Vec<u8>>,
    pub last_interactive: bool,
    pub cache: BTreeMap<String, Vec<u8>>,
    pub state_snapshot: Option<Vec<u8>>,
    pub active_drivers: Vec<crate::driver_runtime::WasmDriver>,
    pub streaming_upstream: Option<crate::wasm_runtime::PersistentWasmApp>,
    pub streaming_downstream: Option<crate::wasm_runtime::PersistentWasmApp>,
    pub node_connections: Vec<crate::spatial::NodeConnection>,
    pub connection_drag: Option<crate::spatial::ConnectionDrag>,
    pub window_apps: BTreeMap<u32, crate::wasm_runtime::PersistentWasmApp>,
}

impl WasmState {
    pub fn new() -> Self {
        Self {
            active_app: None,
            active_app_key: None,
            app_open_since_ms: 0,
            fuel_fail_count: 0,
            last_bytes: None,
            last_interactive: false,
            cache: BTreeMap::new(),
            state_snapshot: None,
            active_drivers: Vec::new(),
            streaming_upstream: None,
            streaming_downstream: None,
            node_connections: Vec::new(),
            connection_drag: None,
            window_apps: BTreeMap::new(),
        }
    }
}

// ── MCP / Async Pipeline State ──────────────────────────────────────────

/// State for MCP communication, async WASM generation, and pending operations.
pub struct McpState {
    pub tz_sync_pending: bool,
    pub tz_synced: bool,
    pub tz_offset_minutes: i32,
    pub deferred_tool_gen: Option<(u32, String)>,
    pub async_tool_gen: Option<(u32, String)>,
    pub immune_patching: Option<String>,
    pub pending_adapter: Option<String>,
    pub pending_driver_device: Option<libfolk::sys::pci::PciDeviceInfo>,
    pub pending_shell_jit: Option<String>,
    pub shell_jit_pipeline: Option<(Vec<crate::folkshell::Command>, usize, String)>,
    pub adapter_cache: BTreeMap<String, Vec<u8>>,
    pub mcp_time_sent: bool,
}

impl McpState {
    pub fn new() -> Self {
        Self {
            tz_sync_pending: false,
            tz_synced: false,
            tz_offset_minutes: 0,
            deferred_tool_gen: None,
            async_tool_gen: None,
            immune_patching: None,
            pending_adapter: None,
            pending_driver_device: None,
            pending_shell_jit: None,
            shell_jit_pipeline: None,
            adapter_cache: BTreeMap::new(),
            mcp_time_sent: false,
        }
    }

    /// Check if any async MCP operation is pending (for poll guard)
    pub fn has_pending(&self) -> bool {
        self.tz_sync_pending
            || self.async_tool_gen.is_some()
            || self.pending_shell_jit.is_some()
    }
}

// ── Mouse / Cursor State ────────────────────────────────────────────────

/// State for mouse cursor, drag operations, and friction tracking.
pub struct CursorState {
    pub x: i32,
    pub y: i32,
    pub bg_dirty: bool,
    pub prev_left_button: bool,
    pub dragging_window_id: Option<u32>,
    pub drag_last_x: i32,
    pub drag_last_y: i32,
    // Friction sensor
    pub click_timestamps: [u64; 8],
    pub click_ts_idx: usize,
}

impl CursorState {
    pub const fn new() -> Self {
        Self {
            x: 640,
            y: 400,
            bg_dirty: true,
            prev_left_button: false,
            dragging_window_id: None,
            drag_last_x: 0,
            drag_last_y: 0,
            click_timestamps: [0; 8],
            click_ts_idx: 0,
        }
    }
}

// ── Rendering / Frame State ─────────────────────────────────────────────

/// Per-frame rendering state and timing.
pub struct RenderState {
    pub need_redraw: bool,
    pub did_work: bool,
    pub last_clock_second: u8,
    // HUD (Alt+Tab overlay)
    pub hud_title: [u8; 32],
    pub hud_title_len: usize,
    pub hud_show_until: u64,
    // App launcher
    pub open_folder: i32,
    pub hover_folder: i32,
    pub tile_clicked: i32,
}

impl RenderState {
    pub const fn new() -> Self {
        Self {
            need_redraw: false,
            did_work: false,
            last_clock_second: 255,
            hud_title: [0; 32],
            hud_title_len: 0,
            hud_show_until: 0,
            open_folder: -1,
            hover_folder: -1,
            tile_clicked: -1,
        }
    }
}

// ── Token Streaming State ───────────────────────────────────────────────

/// State for async LLM token streaming (inference ring buffer).
pub struct StreamState {
    pub ring_handle: u32,
    pub ring_read_idx: usize,
    pub win_id: u32,
    pub query_handle: u32,
    // Tag state machines
    pub tool_state: u8,
    pub tool_open_match: usize,
    pub tool_close_match: usize,
    pub tool_buf: [u8; 512],
    pub tool_buf_len: usize,
    pub tool_pending: [u8; 9],
    pub tool_pending_len: usize,
    pub think_state: u8,
    pub think_open_match: usize,
    pub think_close_match: usize,
    pub think_pending: [u8; 8],
    pub think_pending_len: usize,
    pub think_display: [u8; 512],
    pub think_display_len: usize,
    pub think_active: bool,
    pub think_fade_timer: u32,
    pub result_state: u8,
    pub result_open_match: usize,
    pub result_close_match: usize,
}

impl StreamState {
    pub const fn new() -> Self {
        Self {
            ring_handle: 0,
            ring_read_idx: 0,
            win_id: 0,
            query_handle: 0,
            tool_state: 0,
            tool_open_match: 0,
            tool_close_match: 0,
            tool_buf: [0; 512],
            tool_buf_len: 0,
            tool_pending: [0; 9],
            tool_pending_len: 0,
            think_state: 0,
            think_open_match: 0,
            think_close_match: 0,
            think_pending: [0; 8],
            think_pending_len: 0,
            think_display: [0; 512],
            think_display_len: 0,
            think_active: false,
            think_fade_timer: 0,
            result_state: 0,
            result_open_match: 0,
            result_close_match: 0,
        }
    }
}

// ── IQE Latency Tracking ────────────────────────────────────────────────

/// Interaction Quality Engine — measures input latency.
pub struct IqeState {
    pub last_kbd_tsc: u64,
    pub last_kbd_read_tsc: u64,
    pub last_mou_tsc: u64,
    pub last_mou_read_tsc: u64,
    pub ewma_kbd_us: u64,
    pub ewma_mou_us: u64,
    pub ewma_kbd_wake: u64,
    pub ewma_kbd_rend: u64,
    pub ewma_mou_wake: u64,
    pub ewma_mou_rend: u64,
    pub buf: [u8; 288],
}

impl IqeState {
    pub const fn new() -> Self {
        Self {
            last_kbd_tsc: 0,
            last_kbd_read_tsc: 0,
            last_mou_tsc: 0,
            last_mou_read_tsc: 0,
            ewma_kbd_us: 0,
            ewma_mou_us: 0,
            ewma_kbd_wake: 0,
            ewma_kbd_rend: 0,
            ewma_mou_wake: 0,
            ewma_mou_rend: 0,
            buf: [0; 288],
        }
    }
}

// ── God Mode Pipe (COM3) ───────────────────────────────────────────

pub struct Com3State {
    pub buf: [u8; 512],
    pub len: usize,
    pub queue: Vec<String>,
}

impl Com3State {
    pub fn new() -> Self {
        Self { buf: [0; 512], len: 0, queue: Vec::new() }
    }
}

// ── RAM History Graph ──────────────────────────────────────────────

pub const RAM_HISTORY_LEN: usize = 120;

pub struct RamHistory {
    pub data: [u8; RAM_HISTORY_LEN],
    pub idx: usize,
    pub count: usize,
}

impl RamHistory {
    pub const fn new() -> Self {
        Self { data: [0; RAM_HISTORY_LEN], idx: 0, count: 0 }
    }

    pub fn push(&mut self, pct: u8) {
        self.data[self.idx] = pct;
        self.idx = (self.idx + 1) % RAM_HISTORY_LEN;
        if self.count < RAM_HISTORY_LEN { self.count += 1; }
    }
}

// ── App Launcher ───────────────────────────────────────────────────

pub const MAX_CATEGORIES: usize = 6;
pub const MAX_APPS_PER_CAT: usize = 20;

pub struct AppEntry {
    pub name: [u8; 24],
    pub name_len: usize,
}

pub struct Category {
    pub label: &'static [u8],
    pub color: u32,
    pub apps: [AppEntry; MAX_APPS_PER_CAT],
    pub count: usize,
}

// ── GPU / VGA Mirror State ─────────────────────────────────────────

pub struct GpuState {
    pub use_gpu: bool,
    pub vga_mirror_ptr: *mut u8,
    pub vga_mirror_pitch: usize,
    pub vga_mirror_w: usize,
    pub vga_mirror_h: usize,
}

unsafe impl Send for GpuState {}
unsafe impl Sync for GpuState {}

impl GpuState {
    pub const fn new() -> Self {
        Self {
            use_gpu: false,
            vga_mirror_ptr: core::ptr::null_mut(),
            vga_mirror_pitch: 0,
            vga_mirror_w: 0,
            vga_mirror_h: 0,
        }
    }
}
