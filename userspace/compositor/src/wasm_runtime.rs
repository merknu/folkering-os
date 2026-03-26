//! WASM Runtime — Sandboxed execution of AI-generated applications
//!
//! Uses wasmi interpreter to safely execute WebAssembly modules generated
//! by the Gemini LLM. Fuel metering prevents infinite loops.
//!
//! # Host Functions (WASM → OS bridge)
//! ## Phase 1 — Graphics + System Metrics
//! - `folk_draw_rect(x, y, w, h, color)` — filled rectangle
//! - `folk_draw_text(x, y, ptr, len, color)` — text from WASM linear memory
//! - `folk_draw_line(x1, y1, x2, y2, color)` — Bresenham line
//! - `folk_draw_circle(cx, cy, r, color)` — midpoint circle
//! - `folk_fill_screen(color)` — fill entire framebuffer
//! - `folk_get_time() -> i64` — uptime in milliseconds
//! - `folk_screen_width() -> i32` — framebuffer width
//! - `folk_screen_height() -> i32` — framebuffer height
//! - `folk_random() -> i32` — hardware random (RDRAND)
//!
//! ## Phase 2 Stub — Interactive Input (future)
//! - `folk_poll_event(event_ptr) -> i32` — stub, returns 0
//!
//! ## Phase 3 Stub — Zero-Copy Surface (future)
//! - `folk_get_surface() -> i32` — stub, returns 0
//!
//! # Safety
//! - WASM linear memory is isolated from kernel/compositor memory
//! - Fuel limit per execution (default 1M instructions)
//! - All coordinates are i32 (handles off-screen/negative values safely)
//! - Out-of-bounds traps → clean error, no kernel panic

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use wasmi::*;

/// Maximum fuel (instructions) per WASM execution tick
const FUEL_LIMIT: u64 = 1_000_000;

// ── Public Types ─────────────────────────────────────────────────────────

/// Configuration passed into WASM execution from compositor
pub struct WasmConfig {
    pub screen_width: u32,
    pub screen_height: u32,
    pub uptime_ms: u32,
}

/// Result of a WASM app execution
pub enum WasmResult {
    Ok,
    OutOfFuel,
    Trap(String),
    LoadError(String),
}

/// Filled rectangle command
#[derive(Clone)]
pub struct DrawCmd {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub color: u32,
}

/// Text rendering command
#[derive(Clone)]
pub struct TextCmd {
    pub x: u32,
    pub y: u32,
    pub text: String,
    pub color: u32,
}

/// Line drawing command (Bresenham) — i32 coords for off-screen safety
#[derive(Clone)]
pub struct LineCmd {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub color: u32,
}

/// Circle drawing command (midpoint) — i32 coords for off-screen safety
#[derive(Clone)]
pub struct CircleCmd {
    pub cx: i32,
    pub cy: i32,
    pub r: i32,
    pub color: u32,
}

/// All output produced by a WASM execution
pub struct WasmOutput {
    pub draw_commands: Vec<DrawCmd>,
    pub text_commands: Vec<TextCmd>,
    pub line_commands: Vec<LineCmd>,
    pub circle_commands: Vec<CircleCmd>,
    pub fill_screen: Option<u32>,
}

// ── Internal State ───────────────────────────────────────────────────────

/// State shared between host functions and the WASM module
struct HostState {
    draw_commands: Vec<DrawCmd>,
    text_commands: Vec<TextCmd>,
    line_commands: Vec<LineCmd>,
    circle_commands: Vec<CircleCmd>,
    fill_screen: Option<u32>,
    config: WasmConfig,
}

// ── Execution ────────────────────────────────────────────────────────────

/// Execute a WASM module with fuel-limited sandboxing.
/// Returns result + all draw commands produced by the module.
pub fn execute_wasm(
    wasm_bytes: &[u8],
    config: WasmConfig,
) -> (WasmResult, WasmOutput) {
    let engine = Engine::default();

    let module = match Module::new(&engine, wasm_bytes) {
        Ok(m) => m,
        Err(e) => {
            return (
                WasmResult::LoadError(alloc::format!("Module parse error: {:?}", e)),
                empty_output(),
            );
        }
    };

    let mut store = Store::new(&engine, HostState {
        draw_commands: Vec::new(),
        text_commands: Vec::new(),
        line_commands: Vec::new(),
        circle_commands: Vec::new(),
        fill_screen: None,
        config,
    });
    store.set_fuel(FUEL_LIMIT).unwrap_or(());

    let mut linker = Linker::<HostState>::new(&engine);

    // ── Phase 1: Graphics + System Metrics ───────────────────────────

    // Existing: filled rectangle
    let _ = linker.func_wrap("env", "folk_draw_rect",
        |mut caller: Caller<HostState>, x: i32, y: i32, w: i32, h: i32, color: i32| {
            caller.data_mut().draw_commands.push(DrawCmd {
                x: x as u32, y: y as u32, w: w as u32, h: h as u32, color: color as u32,
            });
        },
    );

    // Existing: text from WASM linear memory
    let _ = linker.func_wrap("env", "folk_draw_text",
        |mut caller: Caller<HostState>, x: i32, y: i32, ptr: i32, len: i32, color: i32| {
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return,
            };
            // ptr is i32 (WASM 32-bit address), cast to usize for memory offset
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

    // NEW: line drawing (Bresenham, rendered by compositor)
    let _ = linker.func_wrap("env", "folk_draw_line",
        |mut caller: Caller<HostState>, x1: i32, y1: i32, x2: i32, y2: i32, color: i32| {
            caller.data_mut().line_commands.push(LineCmd {
                x1, y1, x2, y2, color: color as u32,
            });
        },
    );

    // NEW: circle drawing (midpoint, rendered by compositor)
    let _ = linker.func_wrap("env", "folk_draw_circle",
        |mut caller: Caller<HostState>, cx: i32, cy: i32, r: i32, color: i32| {
            caller.data_mut().circle_commands.push(CircleCmd {
                cx, cy, r, color: color as u32,
            });
        },
    );

    // NEW: fill entire screen with solid color
    let _ = linker.func_wrap("env", "folk_fill_screen",
        |mut caller: Caller<HostState>, color: i32| {
            caller.data_mut().fill_screen = Some(color as u32);
        },
    );

    // FIXED: return actual uptime (was dummy 0). i64 for large uptimes.
    let _ = linker.func_wrap("env", "folk_get_time",
        |caller: Caller<HostState>| -> i64 {
            caller.data().config.uptime_ms as i64
        },
    );

    // NEW: screen dimensions for self-scaling UI
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

    // NEW: hardware random number (RDRAND via kernel)
    let _ = linker.func_wrap("env", "folk_random",
        |_caller: Caller<HostState>| -> i32 {
            libfolk::sys::random::random_u32() as i32
        },
    );

    // ── Phase 2 Stub: Interactive Input (future) ─────────────────────

    // Stub — returns 0 (no events). FolkEvent is 16 bytes (4 × i32):
    // { event_type: i32, x: i32, y: i32, data: i32 }
    let _ = linker.func_wrap("env", "folk_poll_event",
        |_caller: Caller<HostState>, _event_ptr: i32| -> i32 {
            0 // No events available (stub)
        },
    );

    // ── Phase 3 Stub: Zero-Copy Surface (future) ────────────────────

    // Stub — returns 0 (null pointer). Future: returns pointer to ARGB
    // pixel buffer that WASM can write directly for zero-copy rendering.
    let _ = linker.func_wrap("env", "folk_get_surface",
        |_caller: Caller<HostState>| -> i32 {
            0 // Not yet implemented (stub)
        },
    );

    // ── Instantiate and Run ──────────────────────────────────────────

    let instance = match linker.instantiate(&mut store, &module) {
        Ok(inst) => match inst.ensure_no_start(&mut store) {
            Ok(i) => i,
            Err(e) => {
                return (
                    WasmResult::Trap(alloc::format!("Start trap: {:?}", e)),
                    empty_output(),
                );
            }
        },
        Err(e) => {
            return (
                WasmResult::LoadError(alloc::format!("Instantiation error: {:?}", e)),
                empty_output(),
            );
        }
    };

    let run_fn = match instance.get_typed_func::<(), ()>(&store, "run") {
        Ok(f) => f,
        Err(_) => {
            return (
                WasmResult::LoadError(String::from("No 'run' function exported")),
                empty_output(),
            );
        }
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

fn empty_output() -> WasmOutput {
    WasmOutput {
        draw_commands: Vec::new(),
        text_commands: Vec::new(),
        line_commands: Vec::new(),
        circle_commands: Vec::new(),
        fill_screen: None,
    }
}

fn state_to_output(state: HostState) -> WasmOutput {
    WasmOutput {
        draw_commands: state.draw_commands,
        text_commands: state.text_commands,
        line_commands: state.line_commands,
        circle_commands: state.circle_commands,
        fill_screen: state.fill_screen,
    }
}
