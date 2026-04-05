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
    // Semantic Streams
    stream_write_buf: Vec<u8>,  // upstream writes here via folk_stream_write
    stream_read_buf: Vec<u8>,   // downstream reads from here (set by compositor)
    stream_complete: bool,
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
            // Bounds check: prevent integer overflow and out-of-bounds read
            if len <= 0 || len > 4096 { return; }
            let ptr_u = ptr as u32;
            let len_u = len as u32;
            let end = match ptr_u.checked_add(len_u) {
                Some(e) => e,
                None => return, // Integer overflow
            };
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return,
            };
            if end as usize > mem.data_size(&caller) { return; }
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

    // Real-Time Clock: write 6 × i32 (year, month, day, hour, minute, second) to WASM memory
    let _ = linker.func_wrap("env", "folk_get_datetime",
        |mut caller: Caller<HostState>, ptr: i32| -> i32 {
            let p = ptr as u32;
            let end = match p.checked_add(24) { Some(e) => e, None => return 0 };
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            if end as usize > mem.data_size(&caller) { return 0; }
            let dt = libfolk::sys::get_rtc();
            let mut buf = [0u8; 24];
            buf[0..4].copy_from_slice(&(dt.year as i32).to_le_bytes());
            buf[4..8].copy_from_slice(&(dt.month as i32).to_le_bytes());
            buf[8..12].copy_from_slice(&(dt.day as i32).to_le_bytes());
            buf[12..16].copy_from_slice(&(dt.hour as i32).to_le_bytes());
            buf[16..20].copy_from_slice(&(dt.minute as i32).to_le_bytes());
            buf[20..24].copy_from_slice(&(dt.second as i32).to_le_bytes());
            if mem.write(&mut caller, ptr as usize, &buf).is_ok() { 1 } else { 0 }
        },
    );

    // Phase 2: Input polling — dequeue from pending_events
    let _ = linker.func_wrap("env", "folk_poll_event",
        |mut caller: Caller<HostState>, event_ptr: i32| -> i32 {
            let event = match caller.data_mut().pending_events.pop() {
                Some(e) => e,
                None => return 0,
            };
            // Bounds check: event_ptr + 16 must fit in WASM memory
            let ptr_u = event_ptr as u32;
            let end = match ptr_u.checked_add(16) {
                Some(e) => e,
                None => return 0,
            };
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            if end as usize > mem.data_size(&caller) { return 0; }
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

    // Phase 4: Async file loading — request file, get handle, poll for completion
    let _ = linker.func_wrap("env", "folk_request_file",
        |mut caller: Caller<HostState>, path_ptr: i32, path_len: i32, dest_ptr: i32, dest_len: i32| -> i32 {
            // Bounds check path pointer
            if path_len <= 0 || path_len > 256 { return 0; }
            let p = path_ptr as u32;
            let end = match p.checked_add(path_len as u32) {
                Some(e) => e,
                None => return 0,
            };
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            if end as usize > mem.data_size(&caller) { return 0; }

            // Read filename from WASM memory
            let mut name_buf = alloc::vec![0u8; path_len as usize];
            if mem.read(&caller, path_ptr as usize, &mut name_buf).is_err() { return 0; }
            let filename = match alloc::str::from_utf8(&name_buf) {
                Ok(s) => String::from(s),
                Err(_) => return 0,
            };

            // Bounds check dest pointer
            if dest_len <= 0 { return 0; }
            let d = dest_ptr as u32;
            let dend = match d.checked_add(dest_len as u32) {
                Some(e) => e,
                None => return 0,
            };
            if dend as usize > mem.data_size(&caller) { return 0; }

            // Assign handle and queue request
            let handle = caller.data_mut().next_asset_handle;
            caller.data_mut().next_asset_handle += 1;
            caller.data_mut().pending_asset_requests.push(PendingAssetRequest {
                handle,
                filename,
                dest_ptr: dest_ptr as u32,
                dest_len: dest_len as u32,
            });

            handle as i32
        },
    );

    // Phase 5: Semantic file query — search files by concept/purpose
    // folk_query_files(query_ptr, query_len, result_ptr, result_max_len) -> i32
    // Writes the first matching filename to result_ptr.
    // Returns filename length on success, 0 on not found, -1 on error.
    let _ = linker.func_wrap("env", "folk_query_files",
        |mut caller: Caller<HostState>, query_ptr: i32, query_len: i32, result_ptr: i32, result_max_len: i32| -> i32 {
            if query_len <= 0 || query_len > 256 || result_max_len <= 0 { return -1; }

            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            // Read query string from WASM memory
            let mut query_buf = alloc::vec![0u8; query_len as usize];
            if mem.read(&caller, query_ptr as usize, &mut query_buf).is_err() { return -1; }
            let query = match alloc::str::from_utf8(&query_buf) {
                Ok(s) => String::from(s),
                Err(_) => return -1,
            };

            // Call Synapse semantic query
            match libfolk::sys::synapse::query_intent(&query) {
                Ok(info) => {
                    // Construct result filename from query
                    let result_name = alloc::format!("{}.wasm", query);
                    let result_bytes = result_name.as_bytes();
                    let copy_len = result_bytes.len().min(result_max_len as usize);
                    if mem.write(&mut caller, result_ptr as usize, &result_bytes[..copy_len]).is_ok() {
                        copy_len as i32
                    } else { -1 }
                }
                Err(_) => 0, // Not found
            }
        },
    );

    // Phase 6: VFS write + list — apps can save data and browse files
    // folk_list_files(buf_ptr, max_len) -> i32
    // Writes "name1\nname2\n..." to buf. Returns total bytes written.
    let _ = linker.func_wrap("env", "folk_list_files",
        |mut caller: Caller<HostState>, buf_ptr: i32, max_len: i32| -> i32 {
            if max_len <= 0 { return 0; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            // Read directory entries from ramdisk (kernel syscall)
            let mut entries: [libfolk::sys::fs::DirEntry; 32] = unsafe { ::core::mem::zeroed() };
            let count = libfolk::sys::fs::read_dir(&mut entries);
            // Build newline-separated file list with size info
            let mut result = String::new();
            for i in 0..count {
                let e = &entries[i];
                let name = e.name_str();
                result.push_str(name);
                result.push('\t');
                // Append size
                let mut nbuf = [0u8; 12];
                let mut n = e.size as usize;
                let mut pos = nbuf.len();
                if n == 0 { pos -= 1; nbuf[pos] = b'0'; }
                while n > 0 && pos > 0 { pos -= 1; nbuf[pos] = b'0' + (n % 10) as u8; n /= 10; }
                if let Ok(s) = ::core::str::from_utf8(&nbuf[pos..]) { result.push_str(s); }
                result.push('\n');
            }
            let bytes = result.as_bytes();
            let copy_len = bytes.len().min(max_len as usize);
            if mem.write(&mut caller, buf_ptr as usize, &bytes[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // folk_write_file(path_ptr, path_len, data_ptr, data_len) -> i32
    // Saves data to Synapse VFS. Returns 0 on success, -1 on error.
    let _ = linker.func_wrap("env", "folk_write_file",
        |mut caller: Caller<HostState>, path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32| -> i32 {
            if path_len <= 0 || path_len > 256 || data_len < 0 || data_len > 4096 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut name_buf = alloc::vec![0u8; path_len as usize];
            if mem.read(&caller, path_ptr as usize, &mut name_buf).is_err() { return -1; }
            let name = match alloc::str::from_utf8(&name_buf) {
                Ok(s) => s,
                Err(_) => return -1,
            };
            let mut data_buf = alloc::vec![0u8; data_len as usize];
            if data_len > 0 {
                if mem.read(&caller, data_ptr as usize, &mut data_buf).is_err() { return -1; }
            }
            match libfolk::sys::synapse::write_file(name, &data_buf) {
                Ok(_) => 0,
                Err(_) => -1,
            }
        },
    );

    // Phase 7: Intent-IP — Semantic Network Requests
    // folk_http_get(url_ptr, url_len, buf_ptr, max_len) -> i32
    // Makes an HTTP GET request via kernel network stack.
    // Returns bytes written to buf, or -1 on error.
    let _ = linker.func_wrap("env", "folk_http_get",
        |mut caller: Caller<HostState>, url_ptr: i32, url_len: i32, buf_ptr: i32, max_len: i32| -> i32 {
            if url_len <= 0 || url_len > 512 || max_len <= 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            // Read URL from WASM memory
            let mut url_buf = alloc::vec![0u8; url_len as usize];
            if mem.read(&caller, url_ptr as usize, &mut url_buf).is_err() { return -1; }
            let url = match alloc::str::from_utf8(&url_buf) {
                Ok(s) => s,
                Err(_) => return -1,
            };
            // Use kernel ask_gemini syscall with __HTTP_GET__ prefix
            // The proxy intercepts this and performs actual HTTP GET
            let prompt = alloc::format!("__HTTP_GET__{}", url);
            let mut response = alloc::vec![0u8; max_len as usize];
            let bytes = libfolk::sys::ask_gemini(&prompt, &mut response);
            if bytes == 0 { return -1; }
            let copy_len = bytes.min(max_len as usize);
            if mem.write(&mut caller, buf_ptr as usize, &response[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // Phase 8: On-Device SLM — Local AI inference for WASM apps
    // folk_slm_generate(prompt_ptr, prompt_len, buf_ptr, max_len) -> i32
    // Routes to local Ollama (FAST tier) for instant AI responses.
    // This is the "spinal cord" — fast local inference for UI decisions,
    // JIT synthesis, and simple reasoning. Cloud models (Gemini) are the
    // "cerebral cortex" — only used for complex tasks like AutoDream.
    let _ = linker.func_wrap("env", "folk_slm_generate",
        |mut caller: Caller<HostState>, prompt_ptr: i32, prompt_len: i32, buf_ptr: i32, max_len: i32| -> i32 {
            if prompt_len <= 0 || prompt_len > 2048 || max_len <= 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut prompt_buf = alloc::vec![0u8; prompt_len as usize];
            if mem.read(&caller, prompt_ptr as usize, &mut prompt_buf).is_err() { return -1; }
            let prompt = match alloc::str::from_utf8(&prompt_buf) {
                Ok(s) => s,
                Err(_) => return -1,
            };
            // Route to local Ollama via __SLM_GENERATE__ prefix
            // Proxy routes this to FAST tier (local, free, instant)
            let full_prompt = alloc::format!("__SLM_GENERATE__{}", prompt);
            let mut response = alloc::vec![0u8; max_len as usize];
            let bytes = libfolk::sys::ask_gemini(&full_prompt, &mut response);
            if bytes == 0 { return -1; }
            let copy_len = bytes.min(max_len as usize);
            if mem.write(&mut caller, buf_ptr as usize, &response[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // folk_intent_fetch(query_ptr, query_len, buf_ptr, max_len) -> i32
    // Semantic network request: "Get weather in Oslo" → OS translates to API call.
    // The LLM proxy interprets the intent, calls the appropriate API, and returns
    // structured data. The app never needs to know HTTP headers or JSON parsing.
    let _ = linker.func_wrap("env", "folk_intent_fetch",
        |mut caller: Caller<HostState>, query_ptr: i32, query_len: i32, buf_ptr: i32, max_len: i32| -> i32 {
            if query_len <= 0 || query_len > 512 || max_len <= 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut query_buf = alloc::vec![0u8; query_len as usize];
            if mem.read(&caller, query_ptr as usize, &mut query_buf).is_err() { return -1; }
            let query = match alloc::str::from_utf8(&query_buf) {
                Ok(s) => s,
                Err(_) => return -1,
            };
            // Send as MCP ChatRequest with __INTENT_FETCH__ prefix
            // Proxy will: interpret intent → call API → return structured result
            let prompt = alloc::format!("__INTENT_FETCH__{}", query);
            let mut response = alloc::vec![0u8; max_len as usize];
            let bytes = libfolk::sys::ask_gemini(&prompt, &mut response);
            if bytes == 0 { return -1; }
            let copy_len = bytes.min(max_len as usize);
            if mem.write(&mut caller, buf_ptr as usize, &response[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // Phase 9: Semantic Streams — Tick-Tock Co-Scheduling
    // folk_stream_write(ptr, len) — upstream pushes data to stream buffer
    let _ = linker.func_wrap("env", "folk_stream_write",
        |mut caller: Caller<HostState>, ptr: i32, len: i32| {
            if len <= 0 || len > 4096 { return; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return,
            };
            let mut buf = alloc::vec![0u8; len as usize];
            if mem.read(&caller, ptr as usize, &mut buf).is_ok() {
                caller.data_mut().stream_write_buf.extend_from_slice(&buf);
            }
        },
    );

    // folk_stream_read(ptr, max_len) -> i32 — downstream pulls data from stream
    let _ = linker.func_wrap("env", "folk_stream_read",
        |mut caller: Caller<HostState>, ptr: i32, max_len: i32| -> i32 {
            if max_len <= 0 { return 0; }
            let data = caller.data().stream_read_buf.clone();
            if data.is_empty() { return 0; }
            let copy_len = data.len().min(max_len as usize);
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            if mem.write(&mut caller, ptr as usize, &data[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // folk_stream_done() — signal that streaming is complete
    let _ = linker.func_wrap("env", "folk_stream_done",
        |mut caller: Caller<HostState>| {
            caller.data_mut().stream_complete = true;
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

        // Try to grow WASM memory for surface buffer support.
        // If allocation fails (heap too small), surface just won't be available
        // and folk_get_surface() will return 0 (apps use DrawCmd fallback).
        if let Some(Extern::Memory(mem)) = instance.get_export(&store, "memory") {
            let current_pages = mem.size(&store);
            if current_pages < MIN_SURFACE_PAGES {
                match mem.grow(&mut store, MIN_SURFACE_PAGES - current_pages) {
                    Ok(_) => {} // Surface buffer available
                    Err(_) => {} // Growth failed — surface won't work, but app runs fine with DrawCmd
                }
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

    let instance = match linker.instantiate(&mut store, &module) {
        Ok(i) => match i.ensure_no_start(&mut store) {
            Ok(i) => i,
            Err(_) => return None,
        },
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
