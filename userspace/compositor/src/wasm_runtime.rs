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

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use wasmi::*;

/// Maximum fuel (instructions) per WASM execution tick
const FUEL_LIMIT: u64 = 1_000_000;

/// Maximum pending events per frame (prevent unbounded growth)
const MAX_EVENTS: usize = 64;

/// Offset in WASM linear memory where the surface pixel buffer starts (1MB)
const SURFACE_OFFSET: usize = 0x100000;

/// Minimum WASM memory pages for surface support (64 pages = 4MB)
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
    pub event_type: i32,  // 1=mouse_move, 2=mouse_click, 3=key_down
    pub x: i32,
    pub y: i32,
    pub data: i32,
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
    config: WasmConfig,
}

// ── Host Function Registration ───────────────────────────────────────────

/// Register all host functions on a Linker. Used by both one-shot and persistent modes.
fn register_host_functions(linker: &mut Linker<HostState>) {
    // Drawing
    let _ = linker.func_wrap("env", "folk_draw_rect",
        |mut caller: Caller<HostState>, x: i32, y: i32, w: i32, h: i32, color: i32| {
            caller.data_mut().draw_commands.push(DrawCmd {
                x: x as u32, y: y as u32, w: w as u32, h: h as u32, color: color as u32,
            });
        },
    );

    let _ = linker.func_wrap("env", "folk_draw_text",
        |mut caller: Caller<HostState>, x: i32, y: i32, ptr: i32, len: i32, color: i32| {
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return,
            };
            let mut buf = alloc::vec![0u8; len as usize];
            if mem.read(&caller, ptr as usize, &mut buf).is_ok() {
                if let Ok(text) = alloc::str::from_utf8(&buf) {
                    caller.data_mut().text_commands.push(TextCmd {
                        x: x as u32, y: y as u32,
                        text: String::from(text),
                        color: color as u32,
                    });
                }
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_draw_line",
        |mut caller: Caller<HostState>, x1: i32, y1: i32, x2: i32, y2: i32, color: i32| {
            caller.data_mut().line_commands.push(LineCmd {
                x1, y1, x2, y2, color: color as u32,
            });
        },
    );

    let _ = linker.func_wrap("env", "folk_draw_circle",
        |mut caller: Caller<HostState>, cx: i32, cy: i32, r: i32, color: i32| {
            caller.data_mut().circle_commands.push(CircleCmd {
                cx, cy, r, color: color as u32,
            });
        },
    );

    let _ = linker.func_wrap("env", "folk_fill_screen",
        |mut caller: Caller<HostState>, color: i32| {
            caller.data_mut().fill_screen = Some(color as u32);
        },
    );

    // System metrics
    let _ = linker.func_wrap("env", "folk_get_time",
        |caller: Caller<HostState>| -> i32 {
            caller.data().config.uptime_ms as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_screen_width",
        |caller: Caller<HostState>| -> i32 {
            caller.data().config.screen_width as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_screen_height",
        |caller: Caller<HostState>| -> i32 {
            caller.data().config.screen_height as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_random",
        |_caller: Caller<HostState>| -> i32 {
            libfolk::sys::random::random_u32() as i32
        },
    );

    // Phase 2: Input polling — dequeue from pending_events
    let _ = linker.func_wrap("env", "folk_poll_event",
        |mut caller: Caller<HostState>, event_ptr: i32| -> i32 {
            let event = match caller.data_mut().pending_events.pop() {
                Some(e) => e,
                None => return 0,
            };
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            // Serialize FolkEvent as 16 bytes (4 × i32 little-endian)
            let mut buf = [0u8; 16];
            buf[0..4].copy_from_slice(&event.event_type.to_le_bytes());
            buf[4..8].copy_from_slice(&event.x.to_le_bytes());
            buf[8..12].copy_from_slice(&event.y.to_le_bytes());
            buf[12..16].copy_from_slice(&event.data.to_le_bytes());
            if mem.write(&mut caller, event_ptr as usize, &buf).is_ok() {
                event.event_type
            } else {
                0
            }
        },
    );

    // Phase 3: Direct pixel access — returns offset in WASM linear memory
    let _ = linker.func_wrap("env", "folk_get_surface",
        |caller: Caller<HostState>| -> i32 {
            // Return surface offset (only if memory is large enough)
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            let mem_size = mem.data_size(&caller);
            let fb_size = (caller.data().config.screen_width as usize)
                * (caller.data().config.screen_height as usize) * 4;
            if SURFACE_OFFSET + fb_size <= mem_size {
                SURFACE_OFFSET as i32
            } else {
                0 // Memory too small
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_surface_pitch",
        |caller: Caller<HostState>| -> i32 {
            (caller.data().config.screen_width * 4) as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_surface_present",
        |mut caller: Caller<HostState>| {
            caller.data_mut().surface_dirty = true;
        },
    );
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

    let instance = match linker.instantiate(&mut store, &module) {
        Ok(inst) => match inst.ensure_no_start(&mut store) {
            Ok(i) => i,
            Err(e) => return (WasmResult::Trap(alloc::format!("Start trap: {:?}", e)), empty_output()),
        },
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

        let instance = linker.instantiate(&mut store, &module)
            .map_err(|e| alloc::format!("Instantiation: {:?}", e))?
            .ensure_no_start(&mut store)
            .map_err(|e| alloc::format!("Start trap: {:?}", e))?;

        // Grow WASM memory to 4MB for surface buffer support
        if let Some(Extern::Memory(mem)) = instance.get_export(&store, "memory") {
            let current_pages = mem.size(&store);
            if current_pages < MIN_SURFACE_PAGES {
                let _ = mem.grow(&mut store, MIN_SURFACE_PAGES - current_pages);
            }
        }

        let run_fn = instance.get_typed_func::<(), ()>(&store, "run")
            .map_err(|_| String::from("No 'run' exported"))?;

        Ok(Self { store, instance, run_fn, active: true })
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

        // Reset fuel for this frame
        self.store.set_fuel(FUEL_LIMIT).unwrap_or(());

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
        config,
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
    }
}

/// Zero-copy output extraction: moves Vecs out, replaces with empty.
fn take_output(state: &mut HostState) -> WasmOutput {
    let draws = ::core::mem::replace(&mut state.draw_commands, Vec::new());
    let texts = ::core::mem::replace(&mut state.text_commands, Vec::new());
    let lines = ::core::mem::replace(&mut state.line_commands, Vec::new());
    let circles = ::core::mem::replace(&mut state.circle_commands, Vec::new());
    let dirty = state.surface_dirty;
    state.surface_dirty = false;
    WasmOutput {
        draw_commands: draws,
        text_commands: texts,
        line_commands: lines,
        circle_commands: circles,
        fill_screen: state.fill_screen.take(),
        surface_dirty: dirty,
    }
}
