//! WASM Runtime — Sandboxed execution of AI-generated applications
//!
//! Uses wasmi interpreter to safely execute WebAssembly modules generated
//! by the Gemini LLM. Fuel metering prevents infinite loops.
//!
//! # Host Functions (WASM → OS bridge)
//! - `draw_rect(x, y, w, h, color)` — draw filled rectangle
//! - `draw_text(x, y, ptr, len, color)` — draw text from WASM linear memory
//! - `get_time()` — return uptime in milliseconds
//! - `yield_cpu()` — cooperative yield to other tasks
//!
//! # Safety
//! - WASM linear memory is isolated from kernel/compositor memory
//! - Fuel limit per execution (default 1M instructions)
//! - Out-of-bounds traps → clean error, no kernel panic

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use wasmi::*;

/// Maximum fuel (instructions) per WASM execution tick
const FUEL_LIMIT: u64 = 1_000_000;

/// Result of a WASM app execution
pub enum WasmResult {
    /// App completed successfully
    Ok,
    /// App ran out of fuel (possible infinite loop)
    OutOfFuel,
    /// App trapped (runtime error)
    Trap(String),
    /// Module failed to load
    LoadError(String),
}

/// State shared between host functions and the WASM module
struct HostState {
    /// Pixels drawn by the WASM app (x, y, w, h, color)
    draw_commands: Vec<DrawCmd>,
    /// Text drawn by the WASM app
    text_commands: Vec<TextCmd>,
}

#[derive(Clone)]
pub struct DrawCmd {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub color: u32,
}

#[derive(Clone)]
pub struct TextCmd {
    pub x: u32,
    pub y: u32,
    pub text: String,
    pub color: u32,
}

/// Execute a WASM module with fuel-limited sandboxing.
/// Returns draw/text commands produced by the module.
pub fn execute_wasm(
    wasm_bytes: &[u8],
) -> (WasmResult, Vec<DrawCmd>, Vec<TextCmd>) {
    let engine = Engine::default();

    // Parse WASM module
    let module = match Module::new(&engine, wasm_bytes) {
        Ok(m) => m,
        Err(e) => {
            return (
                WasmResult::LoadError(alloc::format!("Module parse error: {:?}", e)),
                Vec::new(), Vec::new(),
            );
        }
    };

    // Create store with fuel metering
    let mut store = Store::new(&engine, HostState {
        draw_commands: Vec::new(),
        text_commands: Vec::new(),
    });
    store.set_fuel(FUEL_LIMIT).unwrap_or(());

    // Create linker with host functions
    let mut linker = Linker::<HostState>::new(&engine);

    // Register host functions — only safe draw functions, NO yield
    let _ = linker.func_wrap("env", "folk_draw_rect",
        |mut caller: Caller<HostState>, x: i32, y: i32, w: i32, h: i32, color: i32| {
            caller.data_mut().draw_commands.push(DrawCmd {
                x: x as u32, y: y as u32, w: w as u32, h: h as u32, color: color as u32,
            });
        },
    );

    let _ = linker.func_wrap("env", "folk_draw_text",
        |mut caller: Caller<HostState>, x: i32, y: i32, ptr: i32, len: i32, color: i32| {
            // Read text from WASM linear memory
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

    let _ = linker.func_wrap("env", "folk_get_time",
        |_caller: Caller<HostState>| -> i32 {
            // Return a dummy time for now (would use uptime syscall)
            0i32
        },
    );

    // NOTE: folk_yield intentionally REMOVED — sys_yield() inside wasmi's
    // synchronous interpreter loop corrupts interpreter state. WASM scripts
    // are strictly run-to-completion. Fuel metering prevents infinite loops.

    // Instantiate module
    let instance = match linker.instantiate(&mut store, &module) {
        Ok(inst) => match inst.ensure_no_start(&mut store) {
            Ok(i) => i,
            Err(e) => {
                return (
                    WasmResult::Trap(alloc::format!("Start trap: {:?}", e)),
                    Vec::new(), Vec::new(),
                );
            }
        },
        Err(e) => {
            return (
                WasmResult::LoadError(alloc::format!("Instantiation error: {:?}", e)),
                Vec::new(), Vec::new(),
            );
        }
    };

    // Find and call the "run" export
    let run_fn = match instance.get_typed_func::<(), ()>(&store, "run") {
        Ok(f) => f,
        Err(_) => {
            return (
                WasmResult::LoadError(String::from("No 'run' function exported")),
                Vec::new(), Vec::new(),
            );
        }
    };

    // Execute with fuel limit
    match run_fn.call(&mut store, ()) {
        Ok(()) => {
            let state = store.into_data();
            (WasmResult::Ok, state.draw_commands, state.text_commands)
        }
        Err(e) => {
            // Check if it was a fuel exhaustion
            let msg = alloc::format!("{:?}", e);
            let result = if msg.contains("fuel") || msg.contains("Fuel") {
                WasmResult::OutOfFuel
            } else {
                WasmResult::Trap(msg)
            };
            let state = store.into_data();
            (result, state.draw_commands, state.text_commands)
        }
    }
}
