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

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use wasmi::*;

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

    // OS Metrics: AI-generated apps can query live system state
    // folk_os_metric(id) -> i32: 0=network, 1=firewall, 2=uptime, 3=suspicious
    // Returns lower 32 bits of the metric (enough for most use cases)
    let _ = linker.func_wrap("env", "folk_os_metric",
        |_caller: Caller<HostState>, metric_id: i32| -> i32 {
            (libfolk::sys::pci::os_metric(metric_id as u32) & 0xFFFFFFFF) as i32
        },
    );

    // Convenience: folk_net_has_ip() -> i32 (1 if online, 0 if not)
    let _ = linker.func_wrap("env", "folk_net_has_ip",
        |_caller: Caller<HostState>| -> i32 {
            let (has_ip, _, _, _, _) = libfolk::sys::pci::net_status();
            if has_ip { 1 } else { 0 }
        },
    );

    // Convenience: folk_fw_drops() -> i32 (firewall drop count)
    let _ = linker.func_wrap("env", "folk_fw_drops",
        |_caller: Caller<HostState>| -> i32 {
            let (_, drops) = libfolk::sys::pci::firewall_stats();
            drops as i32
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
            // folk_http_get: WASM app fetches data from internet via TCP proxy
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
    // Tries LOCAL brain first (zero latency, zero network).
    // Falls back to Ollama proxy for complex queries.
    // "Spinal cord" = local, "Cerebral cortex" = cloud.
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
                Ok(s) => String::from(s),
                Err(_) => return -1,
            };

            // Try LOCAL brain first (zero latency, zero network)
            if let Some(local_response) = crate::slm_runtime::brain().generate(&prompt) {
                let bytes = local_response.as_bytes();
                let copy_len = bytes.len().min(max_len as usize);
                if mem.write(&mut caller, buf_ptr as usize, &bytes[..copy_len]).is_ok() {
                    return copy_len as i32;
                }
            }

            // Fallback: route to proxy (Ollama FAST tier)
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

    // Phase 11: PromptLab — Inference with per-token logit analysis
    // folk_slm_generate_with_logits(prompt_ptr, prompt_len, out_ptr, max_len) -> i32
    // Runs inference AND returns structured PLAB result with per-token confidence.
    // After text generation, reads TDMP tensor mailbox for last-token logits,
    // computes softmax for top-K probabilities, and estimates per-word confidence.
    //
    // PLAB wire format (written to out_ptr):
    //   [0-3]   magic "PLAB"
    //   [4-7]   text_len: u32
    //   [8-11]  token_count: u32
    //   [12-15] flags: u32 (bit0=has_real_logits_for_last_token)
    //   [16..16+text_len] UTF-8 text (padded to 4-byte boundary)
    //   Then token_count × 24-byte entries:
    //     [0-1]  start: u16 (byte offset in text)
    //     [2-3]  len: u16
    //     [4-7]  prob: f32 (0.0-1.0)
    //     [8-11] alt1_prob: f32
    //     [12-15] alt2_prob: f32
    //     [16-19] alt3_prob: f32
    //     [20-23] reserved
    let _ = linker.func_wrap("env", "folk_slm_generate_with_logits",
        |mut caller: Caller<HostState>, prompt_ptr: i32, prompt_len: i32, out_ptr: i32, max_len: i32| -> i32 {
            if prompt_len <= 0 || prompt_len > 4096 || max_len < 64 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            // Read prompt from WASM memory
            let mut prompt_buf = alloc::vec![0u8; prompt_len as usize];
            if mem.read(&caller, prompt_ptr as usize, &mut prompt_buf).is_err() { return -1; }
            let prompt = match alloc::str::from_utf8(&prompt_buf) {
                Ok(s) => String::from(s),
                Err(_) => return -1,
            };

            // Step 1: Run inference (same path as folk_slm_generate)
            let mut gen_buf = alloc::vec![0u8; 2048];
            let gen_len;

            // Try local brain first
            if let Some(local_resp) = crate::slm_runtime::brain().generate(&prompt) {
                let bytes = local_resp.as_bytes();
                let copy = bytes.len().min(gen_buf.len());
                gen_buf[..copy].copy_from_slice(&bytes[..copy]);
                gen_len = copy;
            } else {
                // Fallback to proxy
                let full_prompt = alloc::format!("__SLM_GENERATE__{}", prompt);
                let bytes = libfolk::sys::ask_gemini(&full_prompt, &mut gen_buf);
                if bytes == 0 { return -1; }
                gen_len = bytes;
            }

            // Step 2: Split generated text into word-tokens
            let text = &gen_buf[..gen_len];
            let mut tokens: alloc::vec::Vec<(u16, u16)> = alloc::vec::Vec::new(); // (start, len)
            {
                let mut i = 0usize;
                while i < gen_len {
                    // Skip whitespace
                    while i < gen_len && (text[i] == b' ' || text[i] == b'\n' || text[i] == b'\t') {
                        i += 1;
                    }
                    if i >= gen_len { break; }
                    let word_start = i;
                    // Consume word
                    while i < gen_len && text[i] != b' ' && text[i] != b'\n' && text[i] != b'\t' {
                        i += 1;
                    }
                    if i > word_start && tokens.len() < 128 {
                        tokens.push((word_start as u16, (i - word_start) as u16));
                    }
                }
            }

            // Step 3: Try to read TDMP tensor mailbox for real logits
            let mut has_real_logits = false;
            let mut last_token_probs = [0.0f32; 4]; // top-4 softmax probs
            {
                let mut hdr = [0u8; 512];
                if libfolk::sys::block::read_sector(1, &mut hdr).is_ok() {
                    if hdr[0] == b'T' && hdr[1] == b'D' && hdr[2] == b'M' && hdr[3] == b'P' {
                        // Read summary floats from header (offset 112, up to 100 × f32)
                        // These are the first 100 logit values — find top-4
                        let mut top4: [(f32, usize); 4] = [(-1e30, 0); 4];
                        for j in 0..100 {
                            let off = 112 + j * 4;
                            if off + 4 > 512 { break; }
                            let v = f32::from_le_bytes([hdr[off], hdr[off+1], hdr[off+2], hdr[off+3]]);
                            // Insert into top4 if larger than smallest
                            if v > top4[3].0 {
                                top4[3] = (v, j);
                                // Bubble sort
                                for k in (1..4).rev() {
                                    if top4[k].0 > top4[k-1].0 {
                                        top4.swap(k, k-1);
                                    }
                                }
                            }
                        }
                        // Compute softmax on top-4
                        let max_val = top4[0].0;
                        let mut sum = 0.0f32;
                        let mut exps = [0.0f32; 4];
                        for k in 0..4 {
                            // Clamp to prevent overflow
                            let x = (top4[k].0 - max_val).max(-20.0);
                            // Fast exp approximation: e^x ≈ (1 + x/256)^256
                            let mut e = 1.0 + x / 16.0;
                            e = e * e; e = e * e; e = e * e; e = e * e; // ^16
                            exps[k] = e;
                            sum += e;
                        }
                        if sum > 0.0 {
                            for k in 0..4 {
                                last_token_probs[k] = exps[k] / sum;
                            }
                            has_real_logits = true;
                        }
                    }
                }
            }

            // Step 4: Assign per-token probabilities
            // Last token gets real logits (if available), others get heuristic estimates
            let token_count = tokens.len();

            // Build PLAB buffer
            let text_padded = (gen_len + 3) & !3; // align to 4
            let total_size = 16 + text_padded + token_count * 24;
            if total_size > max_len as usize { return -1; }

            let mut out = alloc::vec![0u8; total_size];

            // Header
            out[0..4].copy_from_slice(b"PLAB");
            out[4..8].copy_from_slice(&(gen_len as u32).to_le_bytes());
            out[8..12].copy_from_slice(&(token_count as u32).to_le_bytes());
            let flags: u32 = if has_real_logits { 1 } else { 0 };
            out[12..16].copy_from_slice(&flags.to_le_bytes());

            // Text
            out[16..16 + gen_len].copy_from_slice(text);

            // Token entries
            let entries_start = 16 + text_padded;
            for (idx, &(start, len)) in tokens.iter().enumerate() {
                let off = entries_start + idx * 24;
                out[off..off+2].copy_from_slice(&start.to_le_bytes());
                out[off+2..off+4].copy_from_slice(&len.to_le_bytes());

                if idx == token_count - 1 && has_real_logits {
                    // Last token: real TDMP probabilities
                    out[off+4..off+8].copy_from_slice(&last_token_probs[0].to_le_bytes());
                    out[off+8..off+12].copy_from_slice(&last_token_probs[1].to_le_bytes());
                    out[off+12..off+16].copy_from_slice(&last_token_probs[2].to_le_bytes());
                    out[off+16..off+20].copy_from_slice(&last_token_probs[3].to_le_bytes());
                } else {
                    // Heuristic: common short words get high confidence,
                    // longer/rarer words get lower confidence
                    let word_len = len as f32;
                    let base = if word_len <= 3.0 { 0.92 } else if word_len <= 6.0 { 0.78 } else { 0.55 };
                    // Add slight variation based on position
                    let pos_factor = 1.0 - (idx as f32 * 0.003).min(0.15);
                    let prob = (base * pos_factor).max(0.1).min(0.99);
                    out[off+4..off+8].copy_from_slice(&prob.to_le_bytes());
                    out[off+8..off+12].copy_from_slice(&(prob * 0.3).to_le_bytes());
                    out[off+12..off+16].copy_from_slice(&(prob * 0.15).to_le_bytes());
                    out[off+16..off+20].copy_from_slice(&(prob * 0.08).to_le_bytes());
                }
            }

            // Write to WASM memory
            if mem.write(&mut caller, out_ptr as usize, &out).is_ok() {
                total_size as i32
            } else { -1 }
        },
    );

    // Phase 12: Telemetry Ring — App-level event logging for AutoDream
    // folk_log_telemetry(action_type, target_id, duration_ms)
    // Pushes an event into the kernel's telemetry ring buffer.
    // Action types: 0=AppOpened, 1=AppClosed, 2=IpcMessageSent,
    //   3=UiInteraction, 4=AiInferenceRequested, 5=AiInferenceCompleted,
    //   6=FileAccessed, 7=FileWritten, 8=OmnibarCommand, 9=MetricAlert
    let _ = linker.func_wrap("env", "folk_log_telemetry",
        |_caller: Caller<HostState>, action_type: i32, target_id: i32, duration_ms: i32| {
            // Syscall 0x9B: record telemetry event
            unsafe {
                libfolk::syscall::syscall3(0x9B, action_type as u64, target_id as u64, duration_ms as u64);
            }
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

    // Phase 10: Tensor Inspection — Read inference tensor mailbox from VirtIO-blk
    // folk_tensor_read(buf_ptr, buf_len, sector_offset) -> i32
    // Reads from the TDMP (Tensor DuMP) disk mailbox written by the inference server.
    //   sector_offset=0: Header sector (512 bytes) — magic, stats, shape, 100 summary floats
    //   sector_offset=1+: Data sectors with raw f32 values (up to 256 sectors, 128KB)
    // Returns bytes read, or -1 on error.
    let _ = linker.func_wrap("env", "folk_tensor_read",
        |mut caller: Caller<HostState>, buf_ptr: i32, buf_len: i32, sector_offset: i32| -> i32 {
            if buf_len <= 0 || sector_offset < 0 || sector_offset > 256 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            // TDMP header is at sector 1, data starts at sector 2
            let disk_sector = 1u64 + sector_offset as u64;
            let sectors_to_read = ((buf_len as usize) + 511) / 512;
            let sectors_to_read = sectors_to_read.min(257 - sector_offset as usize);
            let total_bytes = sectors_to_read * 512;
            let mut read_buf = alloc::vec![0u8; total_bytes];
            if libfolk::sys::block::block_read(disk_sector, &mut read_buf, sectors_to_read).is_err() {
                return -1;
            }
            let copy_len = total_bytes.min(buf_len as usize);
            if mem.write(&mut caller, buf_ptr as usize, &read_buf[..copy_len]).is_ok() {
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
