//! WASM Runtime — Sandboxed execution of AI-generated applications
//!
//! Two execution modes:
//! - **One-shot** (`execute_wasm`): compile + run + destroy. For tool scripts.
//! - **Persistent** (`PersistentWasmApp`): compile once, run every frame.
//!   Store/Instance/Linear Memory survive between frames. WASM `static mut`
//!   variables persist. Input via `folk_poll_event`. For interactive apps/games.
//!
//! # Host Functions (WASM → OS bridge)
//! ## Drawing
//! - `folk_draw_rect(x, y, w, h, color)` — filled rectangle
//! - `folk_draw_text(x, y, ptr, len, color)` — text from WASM linear memory
//! - `folk_draw_line(x1, y1, x2, y2, color)` — Bresenham line
//! - `folk_draw_circle(cx, cy, r, color)` — midpoint circle
//! - `folk_fill_screen(color)` — fill entire framebuffer
//! ## System
//! - `folk_get_time() -> i32` — uptime in milliseconds
//! - `folk_screen_width() -> i32` / `folk_screen_height() -> i32`
//! - `folk_random() -> i32` — hardware random (RDRAND)
//! ## Input (Phase 2)
//! - `folk_poll_event(event_ptr) -> i32` — dequeue input event (16 bytes)
//! ## Direct Pixel Access (Phase 3)
//! - `folk_get_surface() -> i32` — returns offset in WASM memory for pixel buffer
//! - `folk_surface_pitch() -> i32` — bytes per row (width * 4)
//! - `folk_surface_present()` — marks surface dirty for blit to framebuffer
//! ## Tensor Inspection (Phase 10)
//! - `folk_tensor_read(buf_ptr, buf_len, sector_offset) -> i32` — read TDMP mailbox
//! ## PromptLab (Phase 11)
//! - `folk_slm_generate_with_logits(prompt_ptr, prompt_len, out_ptr, max_len) -> i32` — inference + PLAB result
//! ## Telemetry (Phase 12)
//! - `folk_log_telemetry(action_type, target_id, duration_ms)` — push event to kernel ring buffer
//! ## Shadow Runtime (Phase 13)
//! - `execute_shadow_test(wasm_bytes, inputs) -> TestReport` — sandboxed WASM testing for AutoDream
//! ## WebSocket (Phase 14)
//! - `folk_ws_connect(url_ptr, url_len) -> i32` — open persistent connection
//! - `folk_ws_send(socket_id, data_ptr, data_len) -> i32` — send text frame
//! - `folk_ws_poll_recv(socket_id, buf_ptr, max_len) -> i32` — non-blocking receive

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use wasmi::*;

#[path = "host_api/mod.rs"]
mod host_api;

/// Maximum fuel (instructions) per WASM execution tick
/// Default fuel for WASM apps (1M instructions per frame)
const FUEL_LIMIT: u64 = 1_000_000;
/// Boosted fuel for the foreground/active app (5x more CPU time)
pub const FUEL_FOREGROUND: u64 = 5_000_000;
/// Reduced fuel for background windows (save CPU for foreground)
pub const FUEL_BACKGROUND: u64 = 200_000;

/// Maximum pending events per frame (prevent unbounded growth)
const MAX_EVENTS: usize = 64;

/// Offset in WASM linear memory where the surface pixel buffer starts (1MB)
const SURFACE_OFFSET: usize = 0x100000;

/// Minimum WASM memory pages for surface support
/// 1024*768*4 = 3MB at offset 1MB = need 4MB = 64 pages
/// But only grow if heap can afford it (check before growing)
const MIN_SURFACE_PAGES: u32 = 64;

// ── Public Types ─────────────────────────────────────────────────────────

/// Configuration passed into WASM execution from compositor
#[derive(Clone)]
pub struct WasmConfig {
    pub screen_width: u32,
    pub screen_height: u32,
    pub uptime_ms: u32,
}

/// Input event passed to WASM apps (16 bytes, 4 × i32)
#[derive(Clone)]
pub struct FolkEvent {
    pub event_type: i32,  // 1=mouse_move, 2=mouse_click, 3=key_down, 4=asset_loaded
    pub x: i32,           // mouse x / asset handle
    pub y: i32,           // mouse y / asset status (0=ok, 1=not_found)
    pub data: i32,        // buttons / keycode / bytes_loaded
}

/// Pending async file request (submitted by WASM, resolved by compositor)
#[derive(Clone)]
pub struct PendingAssetRequest {
    pub handle: u32,
    pub filename: String,
    pub dest_ptr: u32,   // Offset in WASM linear memory
    pub dest_len: u32,   // Max bytes to write
}

/// Result of a WASM app execution
pub enum WasmResult {
    Ok,
    OutOfFuel,
    Trap(String),
    LoadError(String),
}

#[derive(Clone)]
pub struct DrawCmd { pub x: u32, pub y: u32, pub w: u32, pub h: u32, pub color: u32 }
#[derive(Clone)]
pub struct PixelBlit { pub x: u32, pub y: u32, pub w: u32, pub h: u32, pub data: Vec<u8> }
#[derive(Clone)]
pub struct TextCmd { pub x: u32, pub y: u32, pub text: String, pub color: u32 }
#[derive(Clone)]
pub struct LineCmd { pub x1: i32, pub y1: i32, pub x2: i32, pub y2: i32, pub color: u32 }
#[derive(Clone)]
pub struct CircleCmd { pub cx: i32, pub cy: i32, pub r: i32, pub color: u32 }

/// All output produced by a WASM execution frame
pub struct WasmOutput {
    pub draw_commands: Vec<DrawCmd>,
    pub text_commands: Vec<TextCmd>,
    pub line_commands: Vec<LineCmd>,
    pub circle_commands: Vec<CircleCmd>,
    pub fill_screen: Option<u32>,
    pub surface_dirty: bool,
    pub asset_requests: Vec<PendingAssetRequest>,
    /// Pixel blits from folk_draw_pixels (image rendering)
    pub pixel_blits: Vec<PixelBlit>,
    /// Semantic Streams: data pushed by upstream via folk_stream_write()
    pub stream_data: Vec<u8>,
    /// Semantic Streams: upstream signals completion
    pub stream_complete: bool,
}

/// Generate a text description of what a WASM app renders.
/// Used by AutoDream Creative mode — sent to LLM instead of raw pixels.
pub fn render_summary(output: &WasmOutput) -> String {
    let mut s = String::new();
    if let Some(color) = output.fill_screen {
        s.push_str(&alloc::format!("Background: #{:06X}\n", color));
    }
    if !output.draw_commands.is_empty() {
        s.push_str(&alloc::format!("{} rectangles:\n", output.draw_commands.len()));
        for (i, cmd) in output.draw_commands.iter().take(5).enumerate() {
            s.push_str(&alloc::format!("  [{}] {}x{} at ({},{}) color=#{:06X}\n", i, cmd.w, cmd.h, cmd.x, cmd.y, cmd.color));
        }
        if output.draw_commands.len() > 5 { s.push_str("  ...\n"); }
    }
    if !output.circle_commands.is_empty() {
        s.push_str(&alloc::format!("{} circles:\n", output.circle_commands.len()));
        for (i, cmd) in output.circle_commands.iter().take(3).enumerate() {
            s.push_str(&alloc::format!("  [{}] r={} at ({},{}) color=#{:06X}\n", i, cmd.r, cmd.cx, cmd.cy, cmd.color));
        }
    }
    if !output.line_commands.is_empty() {
        s.push_str(&alloc::format!("{} lines\n", output.line_commands.len()));
    }
    if !output.text_commands.is_empty() {
        s.push_str(&alloc::format!("{} text labels:\n", output.text_commands.len()));
        for cmd in output.text_commands.iter().take(3) {
            s.push_str(&alloc::format!("  \"{}\" at ({},{}) color=#{:06X}\n", &cmd.text[..cmd.text.len().min(30)], cmd.x, cmd.y, cmd.color));
        }
    }
    if s.is_empty() { s.push_str("(empty output)"); }
    s
}

// ── Internal State ───────────────────────────────────────────────────────

/// State shared between host functions and the WASM module
struct HostState {
    draw_commands: Vec<DrawCmd>,
    text_commands: Vec<TextCmd>,
    line_commands: Vec<LineCmd>,
    circle_commands: Vec<CircleCmd>,
    fill_screen: Option<u32>,
    surface_dirty: bool,
    pending_events: Vec<FolkEvent>,
    pending_asset_requests: Vec<PendingAssetRequest>,
    next_asset_handle: u32,
    config: WasmConfig,
    // Pixel blit queue (for image rendering)
    pending_pixel_blits: Vec<PixelBlit>,
    // Semantic Streams
    stream_write_buf: Vec<u8>,  // upstream writes here via folk_stream_write
    stream_read_buf: Vec<u8>,   // downstream reads from here (set by compositor)
    stream_complete: bool,
}

// ── Host Function Registration ───────────────────────────────────────────

/// Register all host functions on a Linker. Used by both one-shot and persistent modes.
fn register_host_functions(linker: &mut Linker<HostState>) {
    host_api::graphics::register(linker);
    host_api::network::register(linker);
    host_api::ai::register(linker);
    host_api::vfs::register(linker);
    host_api::system::register(linker);
}

// ── One-Shot Execution (tools/scripts) ───────────────────────────────────

/// Execute a WASM module once. Compile + run + destroy.
/// For tool scripts (non-interactive, run-to-completion).
pub fn execute_wasm(
    wasm_bytes: &[u8],
    config: WasmConfig,
) -> (WasmResult, WasmOutput) {
    let engine = Engine::default();

    let module = match Module::new(&engine, wasm_bytes) {
        Ok(m) => m,
        Err(e) => {
            return (WasmResult::LoadError(alloc::format!("Module parse: {:?}", e)), empty_output());
        }
    };

    let mut store = Store::new(&engine, new_host_state(config));
    store.set_fuel(FUEL_LIMIT).unwrap_or(());

    let mut linker = Linker::<HostState>::new(&engine);
    register_host_functions(&mut linker);

    let instance = match linker.instantiate_and_start(&mut store, &module) {
        Ok(i) => i,
        Err(e) => return (WasmResult::LoadError(alloc::format!("Instantiation: {:?}", e)), empty_output()),
    };

    let run_fn = match instance.get_typed_func::<(), ()>(&store, "run") {
        Ok(f) => f,
        Err(_) => return (WasmResult::LoadError(String::from("No 'run' exported")), empty_output()),
    };

    match run_fn.call(&mut store, ()) {
        Ok(()) => {
            let state = store.into_data();
            (WasmResult::Ok, state_to_output(state))
        }
        Err(e) => {
            let msg = alloc::format!("{:?}", e);
            let result = if msg.contains("fuel") || msg.contains("Fuel") {
                WasmResult::OutOfFuel
            } else {
                WasmResult::Trap(msg)
            };
            let state = store.into_data();
            (result, state_to_output(state))
        }
    }
}

// ── Persistent Execution (interactive apps/games) ────────────────────────

/// Persistent WASM app — Store/Instance/Memory survive between frames.
/// WASM `static mut` variables persist. Called every frame with fresh events.
pub struct PersistentWasmApp {
    store: Store<HostState>,
    instance: Instance,
    run_fn: TypedFunc<(), ()>,
    pub active: bool,
    /// Dynamic fuel budget: foreground apps get more CPU time
    pub fuel_budget: u64,
}

impl PersistentWasmApp {
    /// Compile and instantiate a WASM module for persistent execution.
    pub fn new(wasm_bytes: &[u8], config: WasmConfig) -> Result<Self, String> {
        let engine = Engine::default();

        let module = Module::new(&engine, wasm_bytes)
            .map_err(|e| alloc::format!("Module parse: {:?}", e))?;

        let mut store = Store::new(&engine, new_host_state(config));
        store.set_fuel(FUEL_LIMIT).unwrap_or(());

        let mut linker = Linker::<HostState>::new(&engine);
        register_host_functions(&mut linker);

        let instance = linker.instantiate_and_start(&mut store, &module)
            .map_err(|e| alloc::format!("Instantiation: {:?}", e))?;

        // Try to grow WASM memory for surface buffer support.
        // If allocation fails (heap too small), surface just won't be available
        // and folk_get_surface() will return 0 (apps use DrawCmd fallback).
        if let Some(Extern::Memory(mem)) = instance.get_export(&store, "memory") {
            let current_pages = mem.size(&store) as u32;
            if current_pages < MIN_SURFACE_PAGES {
                match mem.grow(&mut store, (MIN_SURFACE_PAGES - current_pages) as u64) {
                    Ok(_) => {} // Surface buffer available
                    Err(_) => {} // Growth failed — surface won't work, but app runs fine with DrawCmd
                }
            }
        }

        let run_fn = instance.get_typed_func::<(), ()>(&store, "run")
            .map_err(|_| String::from("No 'run' exported"))?;

        Ok(Self { store, instance, run_fn, active: true, fuel_budget: FUEL_LIMIT })
    }

    /// Push an input event into the app's queue (max 64 per frame).
    pub fn push_event(&mut self, event: FolkEvent) {
        let events = &mut self.store.data_mut().pending_events;
        if events.len() < MAX_EVENTS {
            events.push(event);
        }
    }

    /// Run one frame. Clears draw commands, resets fuel, executes run().
    /// Returns the frame's draw output. Store/Memory persist for next frame.
    pub fn run_frame(&mut self, config: WasmConfig) -> (WasmResult, WasmOutput) {
        // Reset per-frame state (draw commands), keep events (consumed by folk_poll_event)
        {
            let state = self.store.data_mut();
            state.draw_commands.clear();
            state.text_commands.clear();
            state.line_commands.clear();
            state.circle_commands.clear();
            state.fill_screen = None;
            state.surface_dirty = false;
            state.config = config;
        }

        // Reset fuel — use dynamic budget if set, otherwise default
        self.store.set_fuel(self.fuel_budget).unwrap_or(());

        // Execute run()
        match self.run_fn.call(&mut self.store, ()) {
            Ok(()) => {
                let output = take_output(self.store.data_mut());
                (WasmResult::Ok, output)
            }
            Err(e) => {
                let msg = alloc::format!("{:?}", e);
                let result = if msg.contains("fuel") || msg.contains("Fuel") {
                    WasmResult::OutOfFuel
                } else {
                    WasmResult::Trap(msg)
                };
                let output = take_output(self.store.data_mut());
                (result, output)
            }
        }
    }

    /// Access WASM linear memory as a byte slice (for surface blit).
    /// Returns the full WASM linear memory including the surface buffer at SURFACE_OFFSET.
    pub fn get_memory_slice(&self) -> Option<&[u8]> {
        match self.instance.get_export(&self.store, "memory") {
            Some(Extern::Memory(mem)) => Some(mem.data(&self.store)),
            _ => None,
        }
    }

    /// Surface buffer offset constant (for bounds checking in compositor).
    pub fn surface_offset(&self) -> usize { SURFACE_OFFSET }

    /// Write data into WASM linear memory at given offset (for async asset loading).
    pub fn write_memory(&mut self, offset: usize, data: &[u8]) -> bool {
        if let Some(Extern::Memory(mem)) = self.instance.get_export(&self.store, "memory") {
            if offset + data.len() <= mem.data_size(&self.store) {
                return mem.write(&mut self.store, offset, data).is_ok();
            }
        }
        false
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn new_host_state(config: WasmConfig) -> HostState {
    HostState {
        draw_commands: Vec::new(),
        text_commands: Vec::new(),
        line_commands: Vec::new(),
        circle_commands: Vec::new(),
        fill_screen: None,
        surface_dirty: false,
        pending_events: Vec::new(),
        pending_asset_requests: Vec::new(),
        next_asset_handle: 1,
        config,
        pending_pixel_blits: Vec::new(),
        stream_write_buf: Vec::new(),
        stream_read_buf: Vec::new(),
        stream_complete: false,
    }
}

fn empty_output() -> WasmOutput {
    WasmOutput {
        draw_commands: Vec::new(),
        text_commands: Vec::new(),
        line_commands: Vec::new(),
        circle_commands: Vec::new(),
        fill_screen: None,
        surface_dirty: false,
        asset_requests: Vec::new(),
        pixel_blits: Vec::new(),
        stream_data: Vec::new(),
        stream_complete: false,
    }
}

fn state_to_output(state: HostState) -> WasmOutput {
    WasmOutput {
        draw_commands: state.draw_commands,
        text_commands: state.text_commands,
        line_commands: state.line_commands,
        circle_commands: state.circle_commands,
        fill_screen: state.fill_screen,
        surface_dirty: state.surface_dirty,
        asset_requests: state.pending_asset_requests,
        pixel_blits: state.pending_pixel_blits,
        stream_data: state.stream_write_buf,
        stream_complete: state.stream_complete,
    }
}

/// Zero-copy output extraction: moves Vecs out, replaces with empty.
fn take_output(state: &mut HostState) -> WasmOutput {
    let draws = ::core::mem::replace(&mut state.draw_commands, Vec::new());
    let texts = ::core::mem::replace(&mut state.text_commands, Vec::new());
    let lines = ::core::mem::replace(&mut state.line_commands, Vec::new());
    let circles = ::core::mem::replace(&mut state.circle_commands, Vec::new());
    let assets = ::core::mem::replace(&mut state.pending_asset_requests, Vec::new());
    let pixels = ::core::mem::replace(&mut state.pending_pixel_blits, Vec::new());
    let stream = ::core::mem::replace(&mut state.stream_write_buf, Vec::new());
    let dirty = state.surface_dirty;
    let stream_done = state.stream_complete;
    state.surface_dirty = false;
    state.stream_complete = false;
    WasmOutput {
        draw_commands: draws,
        text_commands: texts,
        line_commands: lines,
        circle_commands: circles,
        fill_screen: state.fill_screen.take(),
        surface_dirty: dirty,
        asset_requests: assets,
        pixel_blits: pixels,
        stream_data: stream,
        stream_complete: stream_done,
    }
}

/// Inject stream data into a PersistentWasmApp's read buffer (for Tick-Tock).
/// Called by compositor between upstream.run_frame() and downstream.run_frame().
impl PersistentWasmApp {
    pub fn inject_stream_data(&mut self, data: &[u8]) {
        self.store.data_mut().stream_read_buf = Vec::from(data);
    }
}

// ── View Adapter: Data Format Translation Engine ──────────────────────────

/// Execute a View Adapter WASM module to transform data.
///
/// The adapter exports `transform()` which reads input via `folk_adapter_input`
/// and writes output via `folk_adapter_output`.
///
/// Returns the transformed bytes, or None if the adapter fails.
pub fn execute_adapter(adapter_wasm: &[u8], input_data: &[u8]) -> Option<Vec<u8>> {
    let engine = Engine::default();

    // Adapter host state: input/output buffers
    struct AdapterState {
        input: Vec<u8>,
        output: Vec<u8>,
        input_read: bool,
    }

    let mut store = Store::new(&engine, AdapterState {
        input: Vec::from(input_data),
        output: Vec::new(),
        input_read: false,
    });
    store.set_fuel(500_000).unwrap_or(()); // Adapters get less fuel than apps

    let module = match Module::new(&engine, adapter_wasm) {
        Ok(m) => m,
        Err(_) => return None,
    };

    let mut linker = <Linker<AdapterState>>::new(&engine);

    // folk_adapter_input(ptr, max_len) -> i32: write input data to WASM memory
    let _ = linker.func_wrap("env", "folk_adapter_input",
        |mut caller: Caller<AdapterState>, ptr: i32, max_len: i32| -> i32 {
            if caller.data().input_read { return 0; }
            let input = &caller.data().input;
            let copy_len = input.len().min(max_len as usize);
            let data = input[..copy_len].to_vec();
            if let Some(Extern::Memory(mem)) = caller.get_export("memory") {
                if mem.write(&mut caller, ptr as usize, &data).is_ok() {
                    caller.data_mut().input_read = true;
                    return copy_len as i32;
                }
            }
            0
        },
    );

    // folk_adapter_output(ptr, len): read transformed data from WASM memory
    let _ = linker.func_wrap("env", "folk_adapter_output",
        |mut caller: Caller<AdapterState>, ptr: i32, len: i32| {
            if len <= 0 || len > 8192 { return; }
            let mut buf = alloc::vec![0u8; len as usize];
            if let Some(Extern::Memory(mem)) = caller.get_export("memory") {
                if mem.read(&caller, ptr as usize, &mut buf).is_ok() {
                    caller.data_mut().output = buf;
                }
            }
        },
    );

    // Also provide basic folk_* stubs so adapters can reuse the WASM template
    let _ = linker.func_wrap("env", "folk_get_time", |_: Caller<AdapterState>| -> i32 { 0 });
    let _ = linker.func_wrap("env", "folk_screen_width", |_: Caller<AdapterState>| -> i32 { 0 });
    let _ = linker.func_wrap("env", "folk_screen_height", |_: Caller<AdapterState>| -> i32 { 0 });

    let instance = match linker.instantiate_and_start(&mut store, &module) {
        Ok(i) => i,
        Err(_) => return None,
    };

    // Call transform() or run() — adapters may export either
    let func_name = if instance.get_func(&store, "transform").is_some() {
        "transform"
    } else {
        "run"
    };

    let func = match instance.get_typed_func::<(), ()>(&store, func_name) {
        Ok(f) => f,
        Err(_) => return None,
    };

    if func.call(&mut store, ()).is_err() {
        return None;
    }

    let output = ::core::mem::replace(&mut store.data_mut().output, Vec::new());
    if output.is_empty() { None } else { Some(output) }
}

// ═══════════════════════════════════════════════════════════════════════════
// Shadow Runtime — AutoDream Phase 3: Safe WASM testing sandbox
// ═══════════════════════════════════════════════════════════════════════════
//
// A secondary wasmi runtime with MOCKED host functions that cannot:
// - Write to real Synapse VFS (writes go to in-memory hashmap)
// - Draw to real framebuffer (draw calls are counted but discarded)
// - Access network or serial ports
//
// Used by AutoDream to test proposed WASM app modifications before
// applying them to the live system.

/// Result of a shadow test execution
pub struct TestReport {
    /// Did the app complete without crashing?
    pub completed: bool,
    /// Fuel consumed (proxy for CPU cycles)
    pub fuel_consumed: u64,
    /// Number of draw calls made
    pub draw_call_count: u32,
    /// Number of text draws made
    pub text_draw_count: u32,
    /// Number of file writes attempted
    pub file_write_count: u32,
    /// Number of AI inference calls attempted
    pub ai_call_count: u32,
    /// Total frames executed (run() calls)
    pub frames_executed: u32,
    /// Error message if crashed
    pub error: Option<String>,
    /// Virtual files written (name → size)
    pub virtual_files: Vec<(String, usize)>,
}

/// Synthetic input event for shadow testing
#[derive(Clone)]
pub struct InputEvent {
    pub event_type: i32,
    pub x: i32,
    pub y: i32,
    pub data: i32,
}

/// State for the shadow (mocked) runtime — no side effects on real system
struct ShadowState {
    config: WasmConfig,
    /// Pending synthetic input events
    pending_events: Vec<FolkEvent>,
    /// Counters
    draw_calls: u32,
    text_draws: u32,
    file_writes: u32,
    ai_calls: u32,
    /// Virtual filesystem (in-memory, not persisted)
    virtual_files: Vec<(String, Vec<u8>)>,
}

/// Shadow fuel budget — hard limit to prevent infinite loops
const SHADOW_FUEL_LIMIT: u64 = 10_000_000; // 10M instructions per frame
/// Max frames to simulate
const SHADOW_MAX_FRAMES: u32 = 5;

/// Register MOCKED host functions for the shadow runtime.
/// All drawing is no-op, all I/O goes to in-memory state.
fn register_shadow_functions(linker: &mut Linker<ShadowState>) {
    // Drawing — count but don't render
    let _ = linker.func_wrap("env", "folk_draw_rect",
        |mut caller: Caller<ShadowState>, _x: i32, _y: i32, _w: i32, _h: i32, _color: i32| {
            caller.data_mut().draw_calls += 1;
        },
    );

    let _ = linker.func_wrap("env", "folk_draw_text",
        |mut caller: Caller<ShadowState>, _x: i32, _y: i32, _ptr: i32, _len: i32, _color: i32| {
            caller.data_mut().text_draws += 1;
        },
    );

    let _ = linker.func_wrap("env", "folk_draw_line",
        |mut caller: Caller<ShadowState>, _x1: i32, _y1: i32, _x2: i32, _y2: i32, _color: i32| {
            caller.data_mut().draw_calls += 1;
        },
    );

    let _ = linker.func_wrap("env", "folk_draw_circle",
        |mut caller: Caller<ShadowState>, _cx: i32, _cy: i32, _r: i32, _color: i32| {
            caller.data_mut().draw_calls += 1;
        },
    );

    let _ = linker.func_wrap("env", "folk_fill_screen",
        |mut caller: Caller<ShadowState>, _color: i32| {
            caller.data_mut().draw_calls += 1;
        },
    );

    // System info — return config values
    let _ = linker.func_wrap("env", "folk_get_time",
        |caller: Caller<ShadowState>| -> i32 { caller.data().config.uptime_ms as i32 },
    );
    let _ = linker.func_wrap("env", "folk_screen_width",
        |caller: Caller<ShadowState>| -> i32 { caller.data().config.screen_width as i32 },
    );
    let _ = linker.func_wrap("env", "folk_screen_height",
        |caller: Caller<ShadowState>| -> i32 { caller.data().config.screen_height as i32 },
    );
    let _ = linker.func_wrap("env", "folk_random",
        |_: Caller<ShadowState>| -> i32 { 42 }, // Deterministic for reproducibility
    );
    let _ = linker.func_wrap("env", "folk_get_datetime",
        |mut caller: Caller<ShadowState>, ptr: i32| -> i32 {
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            // Fake datetime: 2026-04-09 12:00:00
            let dt: [i32; 6] = [2026, 4, 9, 12, 0, 0];
            let bytes: [u8; 24] = unsafe { core::mem::transmute(dt) };
            let _ = mem.write(&mut caller, ptr as usize, &bytes);
            0
        },
    );

    // Metrics — return safe defaults
    let _ = linker.func_wrap("env", "folk_os_metric",
        |_: Caller<ShadowState>, _id: i32| -> i32 { 0 },
    );
    let _ = linker.func_wrap("env", "folk_net_has_ip",
        |_: Caller<ShadowState>| -> i32 { 1 }, // Pretend online
    );
    let _ = linker.func_wrap("env", "folk_fw_drops",
        |_: Caller<ShadowState>| -> i32 { 0 },
    );

    // Input — drain synthetic events
    let _ = linker.func_wrap("env", "folk_poll_event",
        |mut caller: Caller<ShadowState>, event_ptr: i32| -> i32 {
            let event = match caller.data_mut().pending_events.pop() {
                Some(e) => e,
                None => return 0,
            };
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            let buf = [
                event.event_type.to_le_bytes(),
                event.x.to_le_bytes(),
                event.y.to_le_bytes(),
                event.data.to_le_bytes(),
            ].concat();
            let _ = mem.write(&mut caller, event_ptr as usize, &buf);
            1
        },
    );

    // File I/O — mock: write to in-memory hashmap
    let _ = linker.func_wrap("env", "folk_write_file",
        |mut caller: Caller<ShadowState>, path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32| -> i32 {
            if path_len <= 0 || data_len < 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut path_buf = alloc::vec![0u8; path_len as usize];
            let mut data_buf = alloc::vec![0u8; data_len as usize];
            if mem.read(&caller, path_ptr as usize, &mut path_buf).is_err() { return -1; }
            if data_len > 0 {
                if mem.read(&caller, data_ptr as usize, &mut data_buf).is_err() { return -1; }
            }
            let name = String::from(core::str::from_utf8(&path_buf).unwrap_or("?"));
            caller.data_mut().virtual_files.push((name, data_buf));
            caller.data_mut().file_writes += 1;
            0
        },
    );

    // File read — return empty (shadow has no real VFS)
    let _ = linker.func_wrap("env", "folk_list_files",
        |_: Caller<ShadowState>, _buf_ptr: i32, _max_len: i32| -> i32 { 0 },
    );
    let _ = linker.func_wrap("env", "folk_request_file",
        |_: Caller<ShadowState>, _p: i32, _pl: i32, _d: i32, _dl: i32| -> i32 { -1 },
    );
    let _ = linker.func_wrap("env", "folk_query_files",
        |_: Caller<ShadowState>, _q: i32, _ql: i32, _r: i32, _rl: i32| -> i32 { 0 },
    );

    // Network — no-op
    let _ = linker.func_wrap("env", "folk_http_get",
        |_: Caller<ShadowState>, _u: i32, _ul: i32, _b: i32, _bl: i32| -> i32 { -1 },
    );

    // AI — count but return empty (no real LLM calls in shadow)
    let _ = linker.func_wrap("env", "folk_slm_generate",
        |mut caller: Caller<ShadowState>, _p: i32, _pl: i32, _b: i32, _bl: i32| -> i32 {
            caller.data_mut().ai_calls += 1;
            0 // Return 0 bytes (empty response)
        },
    );
    let _ = linker.func_wrap("env", "folk_slm_generate_with_logits",
        |mut caller: Caller<ShadowState>, _p: i32, _pl: i32, _o: i32, _ol: i32| -> i32 {
            caller.data_mut().ai_calls += 1;
            -1
        },
    );
    let _ = linker.func_wrap("env", "folk_intent_fetch",
        |_: Caller<ShadowState>, _q: i32, _ql: i32, _b: i32, _bl: i32| -> i32 { -1 },
    );

    // Tensor — return empty
    let _ = linker.func_wrap("env", "folk_tensor_read",
        |_: Caller<ShadowState>, _b: i32, _bl: i32, _s: i32| -> i32 { -1 },
    );

    // Telemetry — silent no-op
    let _ = linker.func_wrap("env", "folk_log_telemetry",
        |_: Caller<ShadowState>, _a: i32, _t: i32, _d: i32| {},
    );

    // Streams — no-op
    let _ = linker.func_wrap("env", "folk_stream_write",
        |_: Caller<ShadowState>, _p: i32, _l: i32| {},
    );
    let _ = linker.func_wrap("env", "folk_stream_read",
        |_: Caller<ShadowState>, _p: i32, _l: i32| -> i32 { 0 },
    );
    let _ = linker.func_wrap("env", "folk_stream_done",
        |_: Caller<ShadowState>| {},
    );

    // Surface — return 0 (no surface in shadow)
    let _ = linker.func_wrap("env", "folk_get_surface",
        |_: Caller<ShadowState>| -> i32 { 0 },
    );
    let _ = linker.func_wrap("env", "folk_surface_pitch",
        |_: Caller<ShadowState>| -> i32 { 0 },
    );
    let _ = linker.func_wrap("env", "folk_surface_present",
        |_: Caller<ShadowState>| {},
    );

    // Pixel blit — no-op in shadow
    let _ = linker.func_wrap("env", "folk_draw_pixels",
        |_: Caller<ShadowState>, _x: i32, _y: i32, _w: i32, _h: i32, _p: i32, _l: i32| -> i32 { 0 },
    );
    // Large HTTP — no network in shadow
    let _ = linker.func_wrap("env", "folk_http_get_large",
        |_: Caller<ShadowState>, _u: i32, _ul: i32, _b: i32, _bl: i32| -> i32 { -1 },
    );

    // Display list — count commands in shadow (no rendering)
    let _ = linker.func_wrap("env", "folk_submit_display_list",
        |mut caller: Caller<ShadowState>, ptr: i32, len: i32| -> i32 {
            if len <= 0 { return 0; }
            // Count opcodes without rendering
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            let mut buf = alloc::vec![0u8; (len as usize).min(65536)];
            if mem.read(&caller, ptr as usize, &mut buf).is_err() { return 0; }
            let mut pos = 0usize;
            let mut count = 0i32;
            while pos < buf.len() {
                let skip = match buf[pos] {
                    0x01 => 13, 0x02 => 15, 0x03 => 13, 0x04 => 5, 0x05 => 11,
                    _ => break,
                };
                pos += skip;
                caller.data_mut().draw_calls += 1;
                count += 1;
            }
            count
        },
    );

    // Adapters — no-op in shadow
    let _ = linker.func_wrap("env", "folk_adapter_input",
        |_: Caller<ShadowState>, _p: i32, _m: i32| -> i32 { 0 },
    );
    let _ = linker.func_wrap("env", "folk_adapter_output",
        |_: Caller<ShadowState>, _p: i32, _l: i32| {},
    );

    // Sync file read — empty in shadow
    let _ = linker.func_wrap("env", "folk_read_file_sync",
        |_: Caller<ShadowState>, _p: i32, _pl: i32, _d: i32, _dl: i32| -> i32 { -1 },
    );

    // IPC stats — empty in shadow
    let _ = linker.func_wrap("env", "folk_ipc_stats",
        |_: Caller<ShadowState>, _b: i32, _m: i32| -> i32 { 0 },
    );

    // Shadow test — no nesting in shadow
    let _ = linker.func_wrap("env", "folk_shadow_test",
        |_: Caller<ShadowState>, _w: i32, _wl: i32, _r: i32, _rl: i32| -> i32 { -1 },
    );

    // Memory map + tokenizer — defaults in shadow
    let _ = linker.func_wrap("env", "folk_memory_map",
        |_: Caller<ShadowState>, _b: i32, _m: i32| -> i32 { -1 },
    );
    let _ = linker.func_wrap("env", "folk_tokenize",
        |_: Caller<ShadowState>, _t: i32, _tl: i32, _o: i32, _ol: i32| -> i32 { -1 },
    );

    // PCI/IRQ — empty in shadow
    let _ = linker.func_wrap("env", "folk_pci_list",
        |_: Caller<ShadowState>, _b: i32, _m: i32| -> i32 { 0 },
    );
    let _ = linker.func_wrap("env", "folk_irq_stats",
        |_: Caller<ShadowState>, _b: i32, _m: i32| -> i32 { 0 },
    );

    // Telemetry poll — empty in shadow
    let _ = linker.func_wrap("env", "folk_telemetry_poll",
        |_: Caller<ShadowState>, _b: i32, _m: i32| -> i32 { 0 },
    );

    // Tensor write — no-op in shadow
    let _ = linker.func_wrap("env", "folk_tensor_write",
        |_: Caller<ShadowState>, _s: i32, _b: i32, _v: i32| -> i32 { -1 },
    );

    // WebSocket — no-op in shadow (no real network)
    let _ = linker.func_wrap("env", "folk_ws_connect",
        |_: Caller<ShadowState>, _u: i32, _ul: i32| -> i32 { -1 },
    );
    let _ = linker.func_wrap("env", "folk_ws_send",
        |_: Caller<ShadowState>, _s: i32, _d: i32, _dl: i32| -> i32 { -1 },
    );
    let _ = linker.func_wrap("env", "folk_ws_poll_recv",
        |_: Caller<ShadowState>, _s: i32, _b: i32, _bl: i32| -> i32 { -1 },
    );
}

/// Execute a WASM app in the shadow sandbox.
///
/// The app runs in complete isolation: no real VFS writes, no real
/// screen draws, no real network access, no real AI calls.
/// AutoDream uses this to test proposed modifications before applying.
///
/// # Arguments
/// * `wasm_bytes` — The WASM module to test
/// * `synthetic_inputs` — Fake input events to inject (key presses, mouse clicks)
///
/// # Returns
/// `TestReport` with fuel consumed, crash status, call counts, and virtual files.
pub fn execute_shadow_test(
    wasm_bytes: &[u8],
    synthetic_inputs: &[InputEvent],
) -> TestReport {
    let engine = Engine::default();

    let module = match Module::new(&engine, wasm_bytes) {
        Ok(m) => m,
        Err(e) => return TestReport {
            completed: false,
            fuel_consumed: 0,
            draw_call_count: 0,
            text_draw_count: 0,
            file_write_count: 0,
            ai_call_count: 0,
            frames_executed: 0,
            error: Some(alloc::format!("Module parse: {:?}", e)),
            virtual_files: Vec::new(),
        },
    };

    let config = WasmConfig {
        screen_width: 1280,
        screen_height: 800,
        uptime_ms: 60_000, // Pretend 1 minute uptime
    };

    // Pre-load synthetic events (reversed so pop() gives them in order)
    let mut events: Vec<FolkEvent> = synthetic_inputs.iter().rev().map(|e| FolkEvent {
        event_type: e.event_type,
        x: e.x,
        y: e.y,
        data: e.data,
    }).collect();

    let mut state = ShadowState {
        config: config.clone(),
        pending_events: events,
        draw_calls: 0,
        text_draws: 0,
        file_writes: 0,
        ai_calls: 0,
        virtual_files: Vec::new(),
    };

    let mut store = Store::new(&engine, state);
    store.set_fuel(SHADOW_FUEL_LIMIT).unwrap_or(());

    let mut linker = Linker::<ShadowState>::new(&engine);
    register_shadow_functions(&mut linker);

    let instance = match linker.instantiate_and_start(&mut store, &module) {
        Ok(i) => i,
        Err(e) => return TestReport {
            completed: false,
            fuel_consumed: 0,
            draw_call_count: 0,
            text_draw_count: 0,
            file_write_count: 0,
            ai_call_count: 0,
            frames_executed: 0,
            error: Some(alloc::format!("Instantiation: {:?}", e)),
            virtual_files: Vec::new(),
        },
    };

    let run_fn = match instance.get_typed_func::<(), ()>(&store, "run") {
        Ok(f) => f,
        Err(_) => return TestReport {
            completed: false,
            fuel_consumed: 0,
            draw_call_count: 0,
            text_draw_count: 0,
            file_write_count: 0,
            ai_call_count: 0,
            frames_executed: 0,
            error: Some(String::from("No 'run' export")),
            virtual_files: Vec::new(),
        },
    };

    // Execute multiple frames (simulates the compositor calling run() each frame)
    let fuel_start = store.get_fuel().unwrap_or(0);
    let mut frames = 0u32;
    let mut error_msg: Option<String> = None;

    for frame in 0..SHADOW_MAX_FRAMES {
        // Refuel between frames (each frame gets its own budget)
        store.set_fuel(SHADOW_FUEL_LIMIT).unwrap_or(());

        // Advance fake time
        store.data_mut().config.uptime_ms += 16; // ~60fps

        match run_fn.call(&mut store, ()) {
            Ok(()) => {
                frames += 1;
            }
            Err(e) => {
                let msg = alloc::format!("{:?}", e);
                if msg.contains("fuel") {
                    error_msg = Some(String::from("Out of fuel (possible infinite loop)"));
                } else {
                    error_msg = Some(alloc::format!("Trap at frame {}: {}", frame, msg));
                }
                frames = frame + 1;
                break;
            }
        }
    }

    let fuel_remaining = store.get_fuel().unwrap_or(0);
    let fuel_consumed = SHADOW_FUEL_LIMIT.saturating_sub(fuel_remaining);

    let state = store.into_data();
    TestReport {
        completed: error_msg.is_none(),
        fuel_consumed: fuel_consumed + (frames.saturating_sub(1) as u64 * SHADOW_FUEL_LIMIT),
        draw_call_count: state.draw_calls,
        text_draw_count: state.text_draws,
        file_write_count: state.file_writes,
        ai_call_count: state.ai_calls,
        frames_executed: frames,
        error: error_msg,
        virtual_files: state.virtual_files.iter()
            .map(|(n, d)| (n.clone(), d.len()))
            .collect(),
    }
}

/// Build the LLM prompt for generating a View Adapter.
pub fn adapter_generation_prompt(source_mime: &str, target_format: &str, sample_data: &str) -> String {
    alloc::format!(
        "Generate a Rust no_std WASM module that transforms data.\n\n\
         Source format: {}\n\
         Target format: {}\n\n\
         The module must:\n\
         - #![no_std] #![no_main]\n\
         - Import: extern \"C\" {{ fn folk_adapter_input(ptr: *mut u8, max_len: i32) -> i32; \
           fn folk_adapter_output(ptr: *const u8, len: i32); }}\n\
         - Export: #[no_mangle] pub extern \"C\" fn transform()\n\
         - Read input via folk_adapter_input into a stack buffer\n\
         - Transform the data from {} to {}\n\
         - Write output via folk_adapter_output\n\n\
         Sample input data (first 200 bytes):\n{}\n\n\
         Return ONLY the Rust code, no explanation.",
        source_mime, target_format, source_mime, target_format,
        &sample_data[..sample_data.len().min(200)]
    )
}
