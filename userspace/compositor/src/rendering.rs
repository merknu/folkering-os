//! Rendering — desktop, WASM fullscreen, overlays, present/flush
//!
//! Extracted from main.rs to reduce the monolithic main loop.
//! Contains two public entry points:
//!
//! - `render_frame`: Semantic Streams, WASM fullscreen, desktop mode
//!   (omnibar, results, folder grid, app grid, windows, spatial pipelining,
//!   HUD overlay, system tray clock, RAM graph, damage tracking)
//! - `present_and_flush`: present shadow->FB, cursor redraw, GPU flush,
//!   VGA mirror, damage clear

extern crate alloc;

use compositor::damage::DamageTracker;
use compositor::draug::DraugDaemon;
use compositor::framebuffer::FramebufferView;
use compositor::state::{
    Category, CursorState, IqeState, InputState, McpState, RenderState,
    StreamState, WasmState, MAX_CATEGORIES,
};
use compositor::window_manager::WindowManager;
use libfolk::sys::io::write_str;
use libfolk::sys::{shmem_map, shmem_unmap, shmem_destroy};

use crate::util::*;
use crate::ipc_helpers::fmt_u64_into;

// ── Layout / Color Constants ──────────────────────────────────────────────

/// Layout/color constants needed for rendering.
/// Computed once in main() and passed in to avoid recomputation.
pub struct RenderLayout {
    // Colors (pre-resolved through fb.color_from_rgb24)
    pub folk_dark: u32,
    pub folk_accent: u32,
    pub white: u32,
    pub gray: u32,
    pub dark_gray: u32,
    pub omnibar_border: u32,
    // Omnibar geometry
    pub text_box_x: usize,
    pub text_box_y: usize,
    pub text_box_w: usize,
    pub text_box_h: usize,
    pub chars_per_line: usize,
    // Results panel geometry
    pub results_x: usize,
    pub results_y: usize,
    pub results_w: usize,
    pub results_h: usize,
    // App launcher constants
    pub folder_w: usize,
    pub folder_h: usize,
    pub folder_gap: usize,
    pub app_tile_w: usize,
    pub app_tile_h: usize,
    pub app_tile_gap: usize,
    pub app_tile_cols: usize,
    // Cursor constants
    pub cursor_w: usize,
    pub cursor_h: usize,
}

const TEXT_PADDING: usize = 12;

/// Inline rdtsc -- duplicated from main.rs since it's a bare #[inline] fn
#[inline(always)]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

// ── Public Result Types ──────────────────────────────────────────────────

pub struct RenderResult {
    pub did_work: bool,
    pub shadow_modified: bool,
}

// ── render_frame ─────────────────────────────────────────────────────────

/// Render one frame: Semantic Streams tick-tock, WASM fullscreen mode,
/// desktop mode (omnibar, results panel, folder grid, app grid,
/// window compositing, spatial pipelining, HUD, Alt+Tab overlay,
/// system tray clock), and damage tracking.
///
/// Returns whether any work was done and whether the shadow buffer was modified.
pub fn render_frame(
    fb: &mut FramebufferView,
    wm: &mut WindowManager,
    wasm: &mut WasmState,
    input: &InputState,
    render: &mut RenderState,
    mcp: &mut McpState,
    iqe: &IqeState,
    damage: &mut DamageTracker,
    draug: &mut DraugDaemon,
    categories: &[Category],
    layout: &RenderLayout,
    cursor: &CursorState,
    cursor_drawn: bool,
    cursor_bg: &mut [u32],
    ram_history: &[u8],
    ram_history_idx: usize,
    ram_history_count: usize,
) -> RenderResult {
    let mut did_work = false;

    // ===== SEMANTIC STREAMS: Tick-Tock Co-Scheduling =====
    let is_streaming = wasm.streaming_upstream.is_some() && wasm.streaming_downstream.is_some();
    if is_streaming {
        let config = compositor::wasm_runtime::WasmConfig {
            screen_width: fb.width as u32,
            screen_height: fb.height as u32,
            uptime_ms: libfolk::sys::uptime() as u32,
        };

        // TICK: Run upstream -- it produces stream data
        let stream_data = if let Some(up) = &mut wasm.streaming_upstream {
            let (_, up_output) = up.run_frame(config.clone());
            up_output.stream_data
        } else {
            alloc::vec::Vec::new()
        };

        // Inject stream data into downstream's read buffer
        if let Some(down) = &mut wasm.streaming_downstream {
            down.inject_stream_data(&stream_data);

            // TOCK: Run downstream -- it reads data and draws
            let (result, output) = down.run_frame(config);

            // Render downstream's visual output to framebuffer
            if let Some(color) = output.fill_screen {
                fb.clear(fb.color_from_rgb24(color));
            }
            for cmd in &output.draw_commands {
                fb.fill_rect(
                    cmd.x as usize,
                    cmd.y as usize,
                    cmd.w as usize,
                    cmd.h as usize,
                    fb.color_from_rgb24(cmd.color),
                );
            }
            for cmd in &output.text_commands {
                fb.draw_string(
                    cmd.x as usize,
                    cmd.y as usize,
                    &cmd.text,
                    fb.color_from_rgb24(cmd.color),
                    fb.color_from_rgb24(0),
                );
            }
            for cmd in &output.circle_commands {
                let c = fb.color_from_rgb24(cmd.color);
                compositor::graphics::draw_circle(&mut *fb, cmd.cx, cmd.cy, cmd.r, c);
            }
            for cmd in &output.line_commands {
                let c = fb.color_from_rgb24(cmd.color);
                compositor::graphics::draw_line(&mut *fb, cmd.x1, cmd.y1, cmd.x2, cmd.y2, c);
            }
        }

        damage.damage_full();
        did_work = true;
    }

    // Skip desktop UI when WASM app owns the screen
    let wasm_fullscreen =
        wasm.active_app.as_ref().map_or(false, |a| a.active) || is_streaming;

    // ===== WASM FULLSCREEN MODE =====
    // When a WASM app is active, it owns the entire framebuffer.
    // Skip ALL desktop rendering (omnibar, folders, windows) to prevent
    // tearing artifacts in the single-buffered framebuffer.
    if wasm_fullscreen {
        if let Some(app) = &mut wasm.active_app {
            if app.active {
                // Dynamic fuel: fullscreen app gets maximum CPU time
                app.fuel_budget = compositor::wasm_runtime::FUEL_FOREGROUND;
                let config = compositor::wasm_runtime::WasmConfig {
                    screen_width: fb.width as u32,
                    screen_height: fb.height as u32,
                    uptime_ms: libfolk::sys::uptime() as u32,
                };
                let (result, output) = app.run_frame(config);

                match &result {
                    compositor::wasm_runtime::WasmResult::OutOfFuel => {
                        wasm.fuel_fail_count = wasm.fuel_fail_count.saturating_add(1);
                        if wasm.fuel_fail_count >= 3 && mcp.immune_patching.is_none() {
                            // Live Patching: 3 consecutive fuel failures -> request fix
                            app.active = false;
                            write_str("[IMMUNE] App fuel-limited 3x — requesting live patch\n");
                            if let Some(ref k) = wasm.active_app_key {
                                let desc = alloc::format!(
                                    "This WASM app '{}' hits fuel limit every frame. \
                                     It has run() called per frame with 1M instruction budget. \
                                     Find the infinite loop or expensive computation and fix it. \
                                     Return ONLY the fixed Rust source code.",
                                    k
                                );
                                if libfolk::mcp::client::send_wasm_gen(&desc) {
                                    mcp.immune_patching = Some(k.clone());
                                    write_str("[IMMUNE] Patch request sent via MCP\n");
                                } else {
                                    write_str("[IMMUNE] Failed to send patch request\n");
                                }
                                // Record for Nightmare dream priority
                                draug.record_crash(k);
                            }
                        } else if wasm.fuel_fail_count < 3 {
                            write_str("[WASM APP] Fuel exhausted (");
                            write_str(match wasm.fuel_fail_count {
                                1 => "1/3",
                                2 => "2/3",
                                _ => "?",
                            });
                            write_str(")\n");
                        }
                    }
                    compositor::wasm_runtime::WasmResult::Trap(msg) => {
                        app.active = false;
                        write_str("[WASM APP] Trap: ");
                        write_str(&msg[..msg.len().min(80)]);
                        write_str("\n");
                        // Record for Nightmare dream priority
                        if let Some(ref k) = wasm.active_app_key {
                            draug.record_crash(k);
                        }
                    }
                    _ => {
                        // Reset fail counter on successful frame
                        wasm.fuel_fail_count = 0;
                    }
                }

                if let Some(color) = output.fill_screen {
                    fb.clear(fb.color_from_rgb24(color));
                }
                for cmd in &output.draw_commands {
                    fb.fill_rect(
                        cmd.x as usize,
                        cmd.y as usize,
                        cmd.w as usize,
                        cmd.h as usize,
                        fb.color_from_rgb24(cmd.color),
                    );
                }
                for cmd in &output.line_commands {
                    let c = fb.color_from_rgb24(cmd.color);
                    compositor::graphics::draw_line(&mut *fb, cmd.x1, cmd.y1, cmd.x2, cmd.y2, c);
                }
                for cmd in &output.circle_commands {
                    let c = fb.color_from_rgb24(cmd.color);
                    compositor::graphics::draw_circle(&mut *fb, cmd.cx, cmd.cy, cmd.r, c);
                }
                for cmd in &output.text_commands {
                    fb.draw_string(
                        cmd.x as usize,
                        cmd.y as usize,
                        &cmd.text,
                        fb.color_from_rgb24(cmd.color),
                        fb.color_from_rgb24(0),
                    );
                }

                // Phase 24: Pixel blits (images from folk_draw_pixels)
                for blit in &output.pixel_blits {
                    let bw = blit.w as usize;
                    let bh = blit.h as usize;
                    let bx = blit.x as usize;
                    let by = blit.y as usize;
                    if blit.data.len() >= bw * bh * 4 {
                        for row in 0..bh {
                            let py = by + row;
                            if py >= fb.height {
                                break;
                            }
                            for col in 0..bw {
                                let px = bx + col;
                                if px >= fb.width {
                                    break;
                                }
                                let off = (row * bw + col) * 4;
                                let r = blit.data[off] as u32;
                                let g = blit.data[off + 1] as u32;
                                let b = blit.data[off + 2] as u32;
                                // RGBA -> 0x00RRGGBB
                                let color = (r << 16) | (g << 8) | b;
                                fb.set_pixel(px, py, color);
                            }
                        }
                    }
                }

                // Phase 3: Surface blit
                if output.surface_dirty {
                    if let Some(mem_data) = app.get_memory_slice() {
                        let surface_offset = app.surface_offset();
                        let fb_size = fb.width * fb.height * 4;
                        if surface_offset + fb_size <= mem_data.len() {
                            let surface =
                                &mem_data[surface_offset..surface_offset + fb_size];
                            if fb.pitch == fb.width * 4 {
                                unsafe {
                                    core::ptr::copy_nonoverlapping(
                                        surface.as_ptr(),
                                        fb.pixel_ptr(0, 0) as *mut u8,
                                        fb_size,
                                    );
                                }
                            } else {
                                for y in 0..fb.height {
                                    let src_off = y * fb.width * 4;
                                    unsafe {
                                        core::ptr::copy_nonoverlapping(
                                            surface[src_off..].as_ptr(),
                                            fb.pixel_ptr(0, y) as *mut u8,
                                            fb.width * 4,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                // Phase 4: Async asset loading + View Adapter pipeline
                if !output.asset_requests.is_empty() {
                    for req in &output.asset_requests {
                        const VFS_ASSET_VADDR: usize = 0x50060000;

                        // Semantic VFS: check for query://, adapt://, or mime:// prefixes
                        let actual_filename = if req.filename.starts_with("query://") {
                            // query://calculator -> semantic search by concept
                            let query = &req.filename[8..];
                            match libfolk::sys::synapse::query_intent(query) {
                                Ok(info) => {
                                    // Resolved! Read the file by shmem using file_id
                                    write_str("[Synapse] query:// '");
                                    write_str(&query[..query.len().min(30)]);
                                    write_str("' → file_id=");
                                    let mut nb3 = [0u8; 16];
                                    write_str(format_usize(info.file_id as usize, &mut nb3));
                                    write_str("\n");
                                    // We need the filename to read via shmem
                                    // Use file_id to look up name via read_file_by_name won't work
                                    // Instead, construct filename from query
                                    alloc::format!("{}.wasm", query)
                                }
                                Err(_) => {
                                    write_str("[Synapse] query:// '");
                                    write_str(&query[..query.len().min(30)]);
                                    write_str("' → not found\n");
                                    req.filename.clone() // Fallback to literal
                                }
                            }
                        } else if req.filename.starts_with("mime://") {
                            // mime://application/wasm -> find first file with this MIME type
                            let mime = &req.filename[7..];
                            let mime_hash = libfolk::sys::synapse::hash_name(mime);
                            // Use QUERY_MIME IPC (simple hash lookup)
                            let request = libfolk::sys::synapse::SYN_OP_QUERY_MIME
                                | ((mime_hash as u64) << 32);
                            let ret = unsafe {
                                libfolk::syscall::syscall3(
                                    libfolk::syscall::SYS_IPC_SEND,
                                    libfolk::sys::synapse::SYNAPSE_TASK_ID as u64,
                                    request,
                                    0,
                                )
                            };
                            if ret != libfolk::sys::synapse::SYN_STATUS_NOT_FOUND
                                && ret != u64::MAX
                            {
                                let file_id = (ret & 0xFFFF) as u16;
                                write_str("[Synapse] mime:// → file_id=");
                                let mut nb3 = [0u8; 16];
                                write_str(format_usize(file_id as usize, &mut nb3));
                                write_str("\n");
                            }
                            // Fallback -- mime:// can't easily resolve to a filename yet
                            req.filename.clone()
                        } else if req.filename.starts_with("adapt://") {
                            // adapt://source_mime/target_format/filename
                            let parts: alloc::vec::Vec<&str> =
                                req.filename[8..].splitn(3, '/').collect();
                            if parts.len() == 3 {
                                let adapter_key =
                                    alloc::format!("{}|{}", parts[0], parts[1]);
                                if !mcp.adapter_cache.contains_key(&adapter_key)
                                    && mcp.pending_adapter.is_none()
                                {
                                    let prompt =
                                        compositor::wasm_runtime::adapter_generation_prompt(
                                            parts[0], parts[1], "",
                                        );
                                    if libfolk::mcp::client::send_wasm_gen(&prompt) {
                                        mcp.pending_adapter = Some(adapter_key);
                                        write_str("[ViewAdapter] Generating adapter: ");
                                        write_str(parts[0]);
                                        write_str(" → ");
                                        write_str(parts[1]);
                                        write_str("\n");
                                    }
                                }
                                alloc::string::String::from(parts[2])
                            } else {
                                req.filename.clone()
                            }
                        } else {
                            req.filename.clone()
                        };

                        match libfolk::sys::synapse::read_file_shmem(&actual_filename) {
                            Ok(resp) => {
                                if shmem_map(resp.shmem_handle, VFS_ASSET_VADDR).is_ok() {
                                    let file_data = unsafe {
                                        core::slice::from_raw_parts(
                                            VFS_ASSET_VADDR as *const u8,
                                            resp.size as usize,
                                        )
                                    };

                                    // View Adapter: if adapt:// was used, try transform
                                    let transformed =
                                        if req.filename.starts_with("adapt://") {
                                            let parts: alloc::vec::Vec<&str> =
                                                req.filename[8..].splitn(3, '/').collect();
                                            if parts.len() == 3 {
                                                let adapter_key = alloc::format!(
                                                    "{}|{}",
                                                    parts[0],
                                                    parts[1]
                                                );
                                                if let Some(adapter_wasm) =
                                                    mcp.adapter_cache.get(&adapter_key)
                                                {
                                                    compositor::wasm_runtime::execute_adapter(
                                                        adapter_wasm,
                                                        &file_data[..resp.size as usize],
                                                    )
                                                } else {
                                                    None
                                                }
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        };

                                    let final_data = transformed
                                        .as_deref()
                                        .unwrap_or(&file_data[..resp.size as usize]);
                                    let copy_len =
                                        final_data.len().min(req.dest_len as usize);
                                    app.write_memory(
                                        req.dest_ptr as usize,
                                        &final_data[..copy_len],
                                    );
                                    let _ =
                                        shmem_unmap(resp.shmem_handle, VFS_ASSET_VADDR);
                                    let _ = shmem_destroy(resp.shmem_handle);
                                    app.push_event(
                                        compositor::wasm_runtime::FolkEvent {
                                            event_type: 4,
                                            x: req.handle as i32,
                                            y: 0,
                                            data: copy_len as i32,
                                        },
                                    );
                                } else {
                                    let _ = shmem_destroy(resp.shmem_handle);
                                    app.push_event(
                                        compositor::wasm_runtime::FolkEvent {
                                            event_type: 4,
                                            x: req.handle as i32,
                                            y: 2,
                                            data: 0,
                                        },
                                    );
                                }
                            }
                            Err(_) => {
                                app.push_event(compositor::wasm_runtime::FolkEvent {
                                    event_type: 4,
                                    x: req.handle as i32,
                                    y: 1,
                                    data: 0,
                                });
                            }
                        }
                    }
                }

                did_work = true;
                // WASM owns fullscreen -- damage entire screen
                damage.damage_full();
            }
        }
    }

    // ===== DESKTOP MODE: omnibar, folders, windows =====
    // Only render desktop elements when NO WASM app is fullscreen.
    // Entire block is skipped when WASM owns the screen.

    if !wasm_fullscreen && input.omnibar_visible {
        // ===== Draw Glass Omnibar (alpha-blended) =====
        let omnibar_alpha: u8 = 180; // 70% opaque -- scene bleeds through

        // Outer glow (subtle, semi-transparent)
        fb.fill_rect_alpha(
            layout.text_box_x.saturating_sub(2),
            layout.text_box_y.saturating_sub(2),
            layout.text_box_w + 4,
            layout.text_box_h + 4,
            0x333333,
            omnibar_alpha / 2,
        );
        // Main glass box
        fb.fill_rect_alpha(
            layout.text_box_x,
            layout.text_box_y,
            layout.text_box_w,
            layout.text_box_h,
            0x1a1a2e,
            omnibar_alpha,
        );
        fb.draw_rect(
            layout.text_box_x,
            layout.text_box_y,
            layout.text_box_w,
            layout.text_box_h,
            layout.omnibar_border,
        );

        // Draw user input text (single line for omnibar)
        // Text foreground is opaque, background is transparent (alpha-blended)
        if input.text_len > 0 {
            if let Ok(_input_str) = core::str::from_utf8(&input.text_buffer[..input.text_len]) {
                // Truncate if too long
                let display_len = if input.text_len > layout.chars_per_line {
                    layout.chars_per_line
                } else {
                    input.text_len
                };
                if let Ok(display_str) =
                    core::str::from_utf8(&input.text_buffer[..display_len])
                {
                    fb.draw_string_alpha(
                        layout.text_box_x + TEXT_PADDING,
                        layout.text_box_y + 12,
                        display_str,
                        layout.white,
                        0x1a1a2e,
                        omnibar_alpha,
                    );
                }
            }
        } else {
            // Show placeholder when empty
            fb.draw_string_alpha(
                layout.text_box_x + TEXT_PADDING,
                layout.text_box_y + 12,
                "Ask anything...",
                layout.gray,
                0x1a1a2e,
                omnibar_alpha,
            );
        }

        // Draw blinking text caret at cursor position
        let caret_x_pos = layout.text_box_x
            + TEXT_PADDING
            + (input.cursor_pos.min(layout.chars_per_line) * 8);
        if caret_x_pos < layout.text_box_x + layout.text_box_w - 30 {
            let caret_char = if input.caret_visible { "|" } else { " " };
            fb.draw_string_alpha(
                caret_x_pos,
                layout.text_box_y + 10,
                caret_char,
                layout.folk_accent,
                0x1a1a2e,
                omnibar_alpha,
            );
        }

        // Draw ">" icon on right
        fb.draw_string_alpha(
            layout.text_box_x + layout.text_box_w - 24,
            layout.text_box_y + 12,
            ">",
            layout.folk_accent,
            0x1a1a2e,
            omnibar_alpha,
        );

        // Context hints below omnibar
        let hint = "Type <query> | open calc | gemini <prompt> | help";
        let hint_x = (fb.width.saturating_sub(hint.len() * 8)) / 2;
        fb.draw_string(
            hint_x,
            layout.text_box_y + layout.text_box_h + 16,
            hint,
            layout.dark_gray,
            layout.folk_dark,
        );

        // ===== Results Panel =====
        if input.show_results && input.text_len > 0 {
            // Draw results box above omnibar
            let results_bg = fb.color_from_rgb24(0x252540);
            fb.fill_rect(
                layout.results_x,
                layout.results_y,
                layout.results_w,
                layout.results_h,
                results_bg,
            );
            fb.draw_rect(
                layout.results_x,
                layout.results_y,
                layout.results_w,
                layout.results_h,
                layout.folk_accent,
            );

            // Parse command and show appropriate results
            if let Ok(cmd_str) = core::str::from_utf8(&input.text_buffer[..input.text_len]) {
                // Header
                fb.draw_string(
                    layout.results_x + 12,
                    layout.results_y + 12,
                    "Results:",
                    layout.folk_accent,
                    results_bg,
                );

                if cmd_str == "ls" || cmd_str == "files" {
                    // Preview: no IPC -- results shown in window on Enter
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 36,
                        "List files in ramdisk",
                        layout.white,
                        results_bg,
                    );
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 56,
                        "Press Enter to run",
                        layout.gray,
                        results_bg,
                    );
                } else if cmd_str == "ps" || cmd_str == "tasks" {
                    // Preview: no IPC -- results shown in window on Enter
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 36,
                        "Show running tasks",
                        layout.white,
                        results_bg,
                    );
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 56,
                        "Press Enter to run",
                        layout.gray,
                        results_bg,
                    );
                } else if cmd_str == "uptime" {
                    // Preview: no IPC -- results shown in window on Enter
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 36,
                        "System uptime",
                        layout.white,
                        results_bg,
                    );
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 56,
                        "Press Enter to run",
                        layout.gray,
                        results_bg,
                    );
                } else if cmd_str.starts_with("calc ") {
                    // Simple calculator
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 36,
                        "Calculator:",
                        layout.white,
                        results_bg,
                    );
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 56,
                        cmd_str,
                        layout.gray,
                        results_bg,
                    );
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 80,
                        "(math evaluation coming soon)",
                        layout.dark_gray,
                        results_bg,
                    );
                } else if cmd_str.starts_with("find ") || cmd_str.starts_with("search ") {
                    // Search query preview
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 36,
                        "Search Synapse",
                        layout.white,
                        results_bg,
                    );
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 56,
                        "Press Enter to search",
                        layout.gray,
                        results_bg,
                    );
                } else if cmd_str.starts_with("open ") {
                    // Open app/file
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 36,
                        "Open app:",
                        layout.white,
                        results_bg,
                    );
                    let app_name = &cmd_str[5..];
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 56,
                        app_name,
                        layout.folk_accent,
                        results_bg,
                    );
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 80,
                        "Press Enter to launch",
                        layout.dark_gray,
                        results_bg,
                    );
                } else if cmd_str == "help" {
                    // Help command
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 36,
                        "Available commands:",
                        layout.white,
                        results_bg,
                    );
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 56,
                        "ls, cat <f>, ps, uptime, help",
                        layout.folk_accent,
                        results_bg,
                    );
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 80,
                        "find <query>, calc <expr>, open <app>",
                        layout.gray,
                        results_bg,
                    );
                } else {
                    // Unknown command -- preview only (no IPC from results panel)
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 36,
                        "Command:",
                        layout.white,
                        results_bg,
                    );
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 56,
                        cmd_str,
                        layout.folk_accent,
                        results_bg,
                    );
                    fb.draw_string(
                        layout.results_x + 12,
                        layout.results_y + 80,
                        "Press Enter to run",
                        layout.dark_gray,
                        results_bg,
                    );
                }
            }
        } else {
            // Clear results area when no results to show
            fb.fill_rect(
                layout.results_x,
                layout.results_y,
                layout.results_w,
                layout.results_h,
                layout.folk_dark,
            );
        }
    } else if !wasm_fullscreen {
        // ===== Omnibar hidden - clear the area (only in desktop mode) =====
        // Clear omnibar area
        fb.fill_rect(
            layout.text_box_x - 2,
            layout.text_box_y - 2,
            layout.text_box_w + 4,
            layout.text_box_h + 4,
            layout.folk_dark,
        );
        // Clear results area
        fb.fill_rect(
            layout.results_x,
            layout.results_y,
            layout.results_w,
            layout.results_h,
            layout.folk_dark,
        );
        // Clear hint area below omnibar position
        fb.fill_rect(
            0,
            layout.text_box_y + layout.text_box_h + 8,
            fb.width,
            24,
            layout.folk_dark,
        );

        // Show hint to open omnibar
        let hint = "Press Windows/Super key to open Omnibar";
        let hint_x = (fb.width.saturating_sub(hint.len() * 8)) / 2;
        fb.draw_string(hint_x, fb.height - 50, hint, layout.dark_gray, layout.folk_dark);
    }

    // (System Tray Clock moved to always-on-top section below)

    // ===== App Launcher: Folder grid or app grid =====
    if !wasm_fullscreen {
        let tile_text = fb.color_from_rgb24(0xDDDDDD);
        let tile_bg = fb.color_from_rgb24(0x222244);
        let tile_border = fb.color_from_rgb24(0x444477);

        if render.open_folder < 0 {
            // HOME VIEW: show category folders
            // Only show folders that have apps
            let mut visible: [(usize, usize); MAX_CATEGORIES] = [(0, 0); MAX_CATEGORIES];
            let mut vis_count = 0;
            for i in 0..MAX_CATEGORIES {
                if categories[i].count > 0 {
                    visible[vis_count] = (i, vis_count);
                    vis_count += 1;
                }
            }

            if vis_count > 0 {
                let cols = vis_count.min(3);
                let grid_w =
                    cols * (layout.folder_w + layout.folder_gap) - layout.folder_gap;
                let grid_x = (fb.width.saturating_sub(grid_w)) / 2;
                let grid_y = 120;

                for v in 0..vis_count {
                    let (cat_idx, _) = visible[v];
                    let col = v % 3;
                    let row = v / 3;
                    let fx =
                        grid_x + col * (layout.folder_w + layout.folder_gap);
                    let fy =
                        grid_y + row * (layout.folder_h + layout.folder_gap);

                    let cat = &categories[cat_idx];
                    let c = fb.color_from_rgb24(cat.color);

                    // Folder tile
                    fb.fill_rect(fx, fy, layout.folder_w, layout.folder_h, tile_bg);
                    fb.draw_rect(fx, fy, layout.folder_w, layout.folder_h, c);
                    fb.draw_rect(
                        fx + 1,
                        fy + 1,
                        layout.folder_w - 2,
                        layout.folder_h - 2,
                        tile_border,
                    );

                    // Mini app preview squares (2x2 grid inside folder)
                    let preview_count = cat.count.min(4);
                    for p in 0..preview_count {
                        let px = fx + 15 + (p % 2) * 35;
                        let py = fy + 10 + (p / 2) * 25;
                        fb.fill_rect(px, py, 28, 20, c);
                    }

                    // Folder label
                    let label = unsafe { core::str::from_utf8_unchecked(cat.label) };
                    let lbl_len = label.trim_end_matches('\0').len();
                    let lbl_trimmed = &label[..lbl_len];
                    let lx = fx + (layout.folder_w.saturating_sub(lbl_len * 8)) / 2;
                    fb.draw_string(
                        lx,
                        fy + layout.folder_h - 20,
                        lbl_trimmed,
                        tile_text,
                        tile_bg,
                    );

                    // App count badge
                    let mut nbuf = [0u8; 16];
                    let ns = format_usize(cat.count, &mut nbuf);
                    fb.draw_string(
                        fx + layout.folder_w - 16,
                        fy + 4,
                        ns,
                        c,
                        tile_bg,
                    );

                    // Hover preview: show app list below the folder
                    if render.hover_folder == cat_idx as i32 {
                        let hover_bg = fb.color_from_rgb24(0x2a2a5a);
                        let prev_x = fx;
                        let prev_y = fy + layout.folder_h + 4;
                        let prev_w = layout.folder_w + 60;
                        let prev_h = 20 + cat.count.min(5) * 18;
                        fb.fill_rect(prev_x, prev_y, prev_w, prev_h, hover_bg);
                        fb.draw_rect(prev_x, prev_y, prev_w, prev_h, c);
                        for ai in 0..cat.count.min(5) {
                            let entry = &cat.apps[ai];
                            if entry.name_len > 0 {
                                let name = unsafe {
                                    core::str::from_utf8_unchecked(
                                        &entry.name[..entry.name_len],
                                    )
                                };
                                fb.draw_string(
                                    prev_x + 8,
                                    prev_y + 4 + ai * 18,
                                    &name[..name.len().min(16)],
                                    tile_text,
                                    hover_bg,
                                );
                            }
                        }
                        if cat.count > 5 {
                            fb.draw_string(
                                prev_x + 8,
                                prev_y + 4 + 5 * 18,
                                "...",
                                tile_text,
                                hover_bg,
                            );
                        }
                    }
                }
            }
        } else {
            // FOLDER VIEW: show apps inside the selected category
            let cat_idx = render.open_folder as usize;
            if cat_idx < MAX_CATEGORIES {
                let cat = &categories[cat_idx];
                let label = unsafe { core::str::from_utf8_unchecked(cat.label) };
                let c = fb.color_from_rgb24(cat.color);

                // Folder header
                let header_y = 90;
                fb.fill_rect(0, header_y, fb.width, 30, fb.color_from_rgb24(0x1a1a3a));
                let back_str = "< Back";
                fb.draw_string(
                    16,
                    header_y + 7,
                    back_str,
                    tile_text,
                    fb.color_from_rgb24(0x1a1a3a),
                );
                let title_x = (fb.width
                    .saturating_sub(label.trim_end_matches('\0').len() * 8))
                    / 2;
                fb.draw_string(
                    title_x,
                    header_y + 7,
                    label.trim_end_matches('\0'),
                    c,
                    fb.color_from_rgb24(0x1a1a3a),
                );

                // App grid
                let grid_w = layout.app_tile_cols
                    * (layout.app_tile_w + layout.app_tile_gap)
                    - layout.app_tile_gap;
                let grid_x = (fb.width.saturating_sub(grid_w)) / 2;
                let grid_y = 130;

                for i in 0..cat.count {
                    let col = i % layout.app_tile_cols;
                    let row = i / layout.app_tile_cols;
                    let ax = grid_x
                        + col * (layout.app_tile_w + layout.app_tile_gap);
                    let ay = grid_y
                        + row * (layout.app_tile_h + layout.app_tile_gap);

                    fb.fill_rect(ax, ay, layout.app_tile_w, layout.app_tile_h, tile_bg);
                    fb.draw_rect(ax, ay, layout.app_tile_w, layout.app_tile_h, tile_border);

                    // Icon (colored square)
                    fb.fill_rect(ax + 16, ay + 8, 40, 36, c);

                    // App name
                    let entry = &cat.apps[i];
                    if entry.name_len > 0 {
                        let name = unsafe {
                            core::str::from_utf8_unchecked(
                                &entry.name[..entry.name_len],
                            )
                        };
                        let nx = ax
                            + (layout
                                .app_tile_w
                                .saturating_sub(entry.name_len.min(9) * 8))
                                / 2;
                        fb.draw_string(
                            nx,
                            ay + layout.app_tile_h - 20,
                            &name[..name.len().min(9)],
                            tile_text,
                            tile_bg,
                        );
                    }
                }
            }
        }
    }

    // ===== Composite Windows (Milestone 2.1) =====
    // Only show windows in desktop mode (not when WASM app is fullscreen)
    if !wasm_fullscreen && wm.has_visible() {
        wm.composite(&mut *fb);

        // ===== Spatial Pipelining: In-Window Tick-Tock =====
        // For each connection: run upstream app, pipe stream data, run downstream app
        // Both render INSIDE their respective windows (not fullscreen)
        for conn_idx in 0..wasm.node_connections.len() {
            let src_id = wasm.node_connections[conn_idx].source_win_id;
            let dst_id = wasm.node_connections[conn_idx].dest_win_id;
            let config = compositor::wasm_runtime::WasmConfig {
                screen_width: 400,
                screen_height: 300,
                uptime_ms: libfolk::sys::uptime() as u32,
            };

            // TICK: run upstream app -> collect stream_data
            let stream_data = if let Some(up_app) = wasm.window_apps.get_mut(&src_id) {
                let (_, output) = up_app.run_frame(config.clone());
                // Render upstream output inside its window
                if let Some(w) = wm.get_window(src_id) {
                    let cx = w.x as usize + 2 + 6; // BORDER_W + padding
                    let cy = w.y as usize + 2 + 26 + 4; // BORDER + TITLE + pad
                    if let Some(color) = output.fill_screen {
                        fb.fill_rect(
                            cx,
                            cy,
                            w.width as usize - 12,
                            w.height as usize - 8,
                            fb.color_from_rgb24(color),
                        );
                    }
                    for cmd in &output.draw_commands {
                        let rx = cx + cmd.x as usize;
                        let ry = cy + cmd.y as usize;
                        fb.fill_rect(
                            rx,
                            ry,
                            cmd.w as usize,
                            cmd.h as usize,
                            fb.color_from_rgb24(cmd.color),
                        );
                    }
                    for tc in &output.text_commands {
                        let tx = cx + tc.x as usize;
                        let ty = cy + tc.y as usize;
                        fb.draw_string(
                            tx,
                            ty,
                            &tc.text,
                            fb.color_from_rgb24(tc.color),
                            fb.color_from_rgb24(0),
                        );
                    }
                }
                output.stream_data
            } else {
                alloc::vec::Vec::new()
            };

            // TOCK: inject stream data into downstream + run
            if let Some(down_app) = wasm.window_apps.get_mut(&dst_id) {
                down_app.inject_stream_data(&stream_data);
                let (_, output) = down_app.run_frame(config);
                // Render downstream output inside its window
                if let Some(w) = wm.get_window(dst_id) {
                    let cx = w.x as usize + 2 + 6;
                    let cy = w.y as usize + 2 + 26 + 4;
                    if let Some(color) = output.fill_screen {
                        fb.fill_rect(
                            cx,
                            cy,
                            w.width as usize - 12,
                            w.height as usize - 8,
                            fb.color_from_rgb24(color),
                        );
                    }
                    for cmd in &output.draw_commands {
                        let rx = cx + cmd.x as usize;
                        let ry = cy + cmd.y as usize;
                        fb.fill_rect(
                            rx,
                            ry,
                            cmd.w as usize,
                            cmd.h as usize,
                            fb.color_from_rgb24(cmd.color),
                        );
                    }
                    for tc in &output.text_commands {
                        let tx = cx + tc.x as usize;
                        let ty = cy + tc.y as usize;
                        fb.draw_string(
                            tx,
                            ty,
                            &tc.text,
                            fb.color_from_rgb24(tc.color),
                            fb.color_from_rgb24(0),
                        );
                    }
                    for cc in &output.circle_commands {
                        let c = fb.color_from_rgb24(cc.color);
                        compositor::graphics::draw_circle(
                            &mut *fb,
                            cx as i32 + cc.cx,
                            cy as i32 + cc.cy,
                            cc.r,
                            c,
                        );
                    }
                    for lc in &output.line_commands {
                        let c = fb.color_from_rgb24(lc.color);
                        compositor::graphics::draw_line(
                            &mut *fb,
                            cx as i32 + lc.x1,
                            cy as i32 + lc.y1,
                            cx as i32 + lc.x2,
                            cy as i32 + lc.y2,
                            c,
                        );
                    }
                }
            }

            did_work = true;
        }

        // ===== Spatial Pipelining: render ports + connections =====
        // Draw I/O port circles on windows that have ports enabled
        for win in &wm.windows {
            if !win.visible {
                continue;
            }
            let mid_y = win.y + win.total_h() as i32 / 2;
            if win.output_port {
                let px = win.x + win.total_w() as i32;
                let raw = if compositor::spatial::is_source(
                    &wasm.node_connections,
                    win.id,
                ) {
                    compositor::spatial::PORT_COLOR_CONNECTED
                } else {
                    compositor::spatial::PORT_COLOR_IDLE
                };
                let c = fb.color_from_rgb24(raw);
                compositor::graphics::draw_circle(
                    &mut *fb,
                    px,
                    mid_y,
                    compositor::spatial::PORT_RADIUS,
                    c,
                );
            }
            if win.input_port {
                let px = win.x;
                let raw = if compositor::spatial::is_dest(
                    &wasm.node_connections,
                    win.id,
                ) {
                    compositor::spatial::PORT_COLOR_CONNECTED
                } else {
                    compositor::spatial::PORT_COLOR_IDLE
                };
                let c = fb.color_from_rgb24(raw);
                compositor::graphics::draw_circle(
                    &mut *fb,
                    px,
                    mid_y,
                    compositor::spatial::PORT_RADIUS,
                    c,
                );
            }
        }
        // Draw connection lines between connected windows
        for conn in &wasm.node_connections {
            let (sx, sy) = if let Some(w) = wm.get_window(conn.source_win_id) {
                (w.x + w.total_w() as i32, w.y + w.total_h() as i32 / 2)
            } else {
                continue;
            };
            let (dx, dy) = if let Some(w) = wm.get_window(conn.dest_win_id) {
                (w.x, w.y + w.total_h() as i32 / 2)
            } else {
                continue;
            };
            let c = fb.color_from_rgb24(compositor::spatial::CONNECTION_COLOR);
            compositor::graphics::draw_line(&mut *fb, sx, sy, dx, dy, c);
        }
        // Draw active drag cable
        if let Some(ref drag) = wasm.connection_drag {
            if let Some(w) = wm.get_window(drag.source_win_id) {
                let sx = w.x + w.total_w() as i32;
                let sy = w.y + w.total_h() as i32 / 2;
                let c = fb.color_from_rgb24(compositor::spatial::PORT_COLOR_DRAG);
                compositor::graphics::draw_line(
                    &mut *fb,
                    sx,
                    sy,
                    drag.current_x,
                    drag.current_y,
                    c,
                );
            }
        }
    }

    // ===== Alt+Tab HUD overlay =====
    if render.hud_show_until > 0 && render.hud_title_len > 0 {
        let hud_text = unsafe {
            core::str::from_utf8_unchecked(&render.hud_title[..render.hud_title_len])
        };
        let hud_w = render.hud_title_len * 8 + 24;
        let hud_x = (fb.width.saturating_sub(hud_w)) / 2;
        let hud_y = fb.height.saturating_sub(40);
        fb.fill_rect_alpha(hud_x, hud_y, hud_w, 24, 0x1a1a2e, 200);
        fb.draw_rect(hud_x, hud_y, hud_w, 24, layout.folk_accent);
        fb.draw_string(hud_x + 12, hud_y + 8, hud_text, layout.white, layout.folk_dark);
    }

    // ===== System Tray Clock -- ALWAYS ON TOP =====
    // Rendered after windows, WASM apps, HUD -- only cursor is above
    {
        let dt = libfolk::sys::get_rtc();
        let mut total_minutes =
            dt.hour as i32 * 60 + dt.minute as i32 + mcp.tz_offset_minutes;
        let mut day = dt.day as i32;
        let mut month = dt.month;
        let mut year = dt.year;
        if total_minutes >= 24 * 60 {
            total_minutes -= 24 * 60;
            day += 1;
            let dim = match month {
                2 => 28,
                4 | 6 | 9 | 11 => 30,
                _ => 31,
            };
            if day > dim {
                day = 1;
                month += 1;
                if month > 12 {
                    month = 1;
                    year += 1;
                }
            }
        } else if total_minutes < 0 {
            total_minutes += 24 * 60;
            day -= 1;
            if day < 1 {
                month -= 1;
                if month < 1 {
                    month = 12;
                    year -= 1;
                }
                day = 28;
            }
        }
        let lh = (total_minutes / 60) as u8;
        let lm = (total_minutes % 60) as u8;
        let ls = dt.second;
        // Format: "14:30:05"  (compact, like a phone status bar)
        let mut t = [0u8; 8];
        t[0] = b'0' + lh / 10;
        t[1] = b'0' + lh % 10;
        t[2] = b':';
        t[3] = b'0' + lm / 10;
        t[4] = b'0' + lm % 10;
        t[5] = b':';
        t[6] = b'0' + ls / 10;
        t[7] = b'0' + ls % 10;
        let time_str = unsafe { core::str::from_utf8_unchecked(&t) };

        // Status bar background (semi-transparent strip at top)
        let bar_h = 20usize;
        fb.fill_rect_alpha(0, 0, fb.width, bar_h, 0x000000, 140);

        // Clock centered at top
        let time_x = (fb.width.saturating_sub(8 * 8)) / 2;
        fb.draw_string(
            time_x,
            2,
            time_str,
            layout.white,
            fb.color_from_rgb24(0x0a0a0a),
        );

        // Date on the left
        let mut d = [0u8; 10];
        d[0] = b'0' + ((year / 1000) % 10) as u8;
        d[1] = b'0' + ((year / 100) % 10) as u8;
        d[2] = b'0' + ((year / 10) % 10) as u8;
        d[3] = b'0' + (year % 10) as u8;
        d[4] = b'-';
        d[5] = b'0' + month / 10;
        d[6] = b'0' + month % 10;
        d[7] = b'-';
        d[8] = b'0' + day as u8 / 10;
        d[9] = b'0' + day as u8 % 10;
        let date_str = unsafe { core::str::from_utf8_unchecked(&d) };
        fb.draw_string(
            8,
            2,
            date_str,
            layout.gray,
            fb.color_from_rgb24(0x0a0a0a),
        );

        // RAM usage on the right side of status bar
        let (_total_mb, _used_mb, mem_pct) = libfolk::sys::memory_stats();
        let mut rbuf = [0u8; 8];
        let mut ri = 0usize;
        // "RAM XX%"
        rbuf[ri] = b'R';
        ri += 1;
        rbuf[ri] = b'A';
        ri += 1;
        rbuf[ri] = b'M';
        ri += 1;
        rbuf[ri] = b' ';
        ri += 1;
        if mem_pct >= 100 {
            rbuf[ri] = b'1';
            ri += 1;
            rbuf[ri] = b'0';
            ri += 1;
            rbuf[ri] = b'0';
            ri += 1;
        } else {
            if mem_pct >= 10 {
                rbuf[ri] = b'0' + (mem_pct / 10) as u8;
                ri += 1;
            }
            rbuf[ri] = b'0' + (mem_pct % 10) as u8;
            ri += 1;
        }
        rbuf[ri] = b'%';
        ri += 1;
        let ram_str = unsafe { core::str::from_utf8_unchecked(&rbuf[..ri]) };
        let ram_col = if mem_pct > 80 {
            fb.color_from_rgb24(0xFF4444)
        } else if mem_pct > 50 {
            fb.color_from_rgb24(0xFFAA00)
        } else {
            fb.color_from_rgb24(0x44FF44)
        };
        let ram_x = fb.width.saturating_sub(ri * 8 + 8);
        fb.draw_string(
            ram_x,
            2,
            ram_str,
            ram_col,
            fb.color_from_rgb24(0x0a0a0a),
        );

        // IQE latency display + colored dot
        if iqe.ewma_kbd_us > 0 || iqe.ewma_mou_us > 0 {
            let mut lbuf = [0u8; 48];
            let mut li = 0usize;
            // K:total(w+r) | M:total
            lbuf[li] = b'K';
            li += 1;
            lbuf[li] = b':';
            li += 1;
            li += fmt_u64_into(&mut lbuf[li..], iqe.ewma_kbd_us);
            if iqe.ewma_kbd_wake > 0 {
                lbuf[li] = b'(';
                li += 1;
                li += fmt_u64_into(&mut lbuf[li..], iqe.ewma_kbd_wake);
                lbuf[li] = b'+';
                li += 1;
                li += fmt_u64_into(&mut lbuf[li..], iqe.ewma_kbd_rend);
                lbuf[li] = b')';
                li += 1;
            }
            if li < 44 {
                lbuf[li] = b' ';
                li += 1;
                lbuf[li] = b'M';
                li += 1;
                lbuf[li] = b':';
                li += 1;
                li += fmt_u64_into(&mut lbuf[li..], iqe.ewma_mou_us);
            }
            let s = unsafe { core::str::from_utf8_unchecked(&lbuf[..li.min(48)]) };
            fb.draw_string(
                90,
                2,
                s,
                fb.color_from_rgb24(0x88AACC),
                fb.color_from_rgb24(0x0a0a0a),
            );

            let worst = iqe.ewma_kbd_us.max(iqe.ewma_mou_us);
            let dot = if worst < 5000 {
                0x44FF44
            } else if worst < 16000 {
                0xFFAA00
            } else {
                0xFF4444
            };
            fb.fill_rect(
                ram_x.saturating_sub(14),
                5,
                8,
                8,
                fb.color_from_rgb24(dot),
            );
        }

        // RAM history graph (popup when clicked)
        if input.show_ram_graph && ram_history_count > 1 {
            let graph_w: usize = 240;
            let graph_h: usize = 100;
            let graph_x = fb.width.saturating_sub(graph_w + 8);
            let graph_y: usize = 24;
            let graph_bg = fb.color_from_rgb24(0x0a0a1e);
            let graph_border = fb.color_from_rgb24(0x334466);
            let graph_grid = fb.color_from_rgb24(0x1a1a3a);

            // Background
            fb.fill_rect(graph_x, graph_y, graph_w, graph_h, graph_bg);
            fb.draw_rect(graph_x, graph_y, graph_w, graph_h, graph_border);

            // Grid lines at 25%, 50%, 75%
            for pct in [25usize, 50, 75] {
                let gy = graph_y + graph_h - (pct * graph_h / 100);
                for gx in (graph_x + 1..graph_x + graph_w - 1).step_by(4) {
                    fb.set_pixel(gx, gy, graph_grid);
                }
            }

            // Title
            fb.draw_string(
                graph_x + 4,
                graph_y + 2,
                "RAM % (2min)",
                fb.color_from_rgb24(0x6688AA),
                graph_bg,
            );

            // Scale labels
            fb.draw_string(
                graph_x + graph_w - 28,
                graph_y + graph_h - 14,
                "0%",
                fb.color_from_rgb24(0x445566),
                graph_bg,
            );
            fb.draw_string(
                graph_x + graph_w - 36,
                graph_y + 16,
                "100%",
                fb.color_from_rgb24(0x445566),
                graph_bg,
            );

            // Plot data points as filled columns
            let ram_hist_len = ram_history.len();
            let samples = ram_history_count.min(graph_w - 4);
            let bar_w = 1usize.max((graph_w - 4) / samples.max(1));

            for i in 0..samples {
                // Read from oldest to newest
                let hist_idx = if ram_history_count >= ram_hist_len {
                    (ram_history_idx + ram_hist_len - samples + i) % ram_hist_len
                } else {
                    i
                };
                let pct_val = ram_history[hist_idx] as usize;
                let bar_height = pct_val * (graph_h - 20) / 100;
                let bx = graph_x + 2 + i * bar_w;
                let by = graph_y + graph_h - 2 - bar_height;

                let bar_color = if pct_val > 80 {
                    fb.color_from_rgb24(0xFF4444)
                } else if pct_val > 50 {
                    fb.color_from_rgb24(0xFFAA00)
                } else {
                    fb.color_from_rgb24(0x44FF44)
                };

                if bx + bar_w < graph_x + graph_w - 1 {
                    fb.fill_rect(bx, by, bar_w, bar_height, bar_color);
                }
            }
        }
    }

    // Targeted damage per UI element (coalesced into minimal rects)
    if !wasm_fullscreen {
        damage.add_damage(compositor::damage::Rect::new(0, 0, fb.width as u32, 22));
        if input.omnibar_visible {
            damage.add_damage(compositor::damage::Rect::new(
                layout.text_box_x.saturating_sub(4) as u32,
                layout.text_box_y.saturating_sub(4) as u32,
                (layout.text_box_w + 8) as u32,
                (layout.text_box_h + 60) as u32,
            ));
        }
        for w in wm.windows.iter() {
            damage.add_damage(compositor::damage::Rect::new(
                w.x.max(0) as u32,
                w.y.max(0) as u32,
                (w.width + 20) as u32,
                (w.height + 40) as u32,
            ));
        }
    } else {
        damage.damage_full();
    }

    // After full redraw: save fresh scene under cursor and mark cursor bg dirty.
    // Cursor itself is drawn AFTER present_region (below), so it's on top of FB.
    if cursor_drawn {
        fb.save_rect(
            cursor.x as usize,
            cursor.y as usize,
            layout.cursor_w,
            layout.cursor_h,
            cursor_bg,
        );
        // NOTE: caller must set cursor.bg_dirty = false after this returns
        damage.add_damage(compositor::damage::Rect::new(
            cursor.x.max(0) as u32,
            cursor.y.max(0) as u32,
            layout.cursor_w as u32 + 2,
            layout.cursor_h as u32 + 2,
        ));
    }

    RenderResult {
        did_work,
        shadow_modified: true,
    }
}

// ── present_and_flush ────────────────────────────────────────────────────

/// Present shadow buffer to framebuffer, redraw cursor, GPU flush,
/// VGA mirror dirty regions, and clear damage tracker.
pub fn present_and_flush(
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    cursor_x: i32,
    cursor_y: i32,
    cursor_drawn: bool,
    last_buttons: u8,
    cursor_fill_colors: &CursorColors,
    need_redraw: bool,
    had_mouse_events: bool,
    use_gpu: bool,
    vga_mirror_ptr: *mut u8,
    vga_mirror_pitch: usize,
    vga_mirror_w: usize,
    vga_mirror_h: usize,
) {
    // Present: copy shadow->FB for dirty regions that were rendered to shadow.
    // Cursor-only movement writes directly to FB (set_pixel_overlay), so we
    // track whether shadow was modified separately.
    if damage.has_damage() {
        // Present shadow->FB for all damage EXCEPT pure cursor damage.
        // When need_redraw or clock tick happened, shadow was written and needs copying.
        // For cursor-only frames, FB was already written directly.
        if need_redraw {
            // Full redraw: present everything then redraw cursor on top
            for r in damage.regions() {
                fb.present_region(r.x, r.y, r.w, r.h);
            }
            if cursor_drawn {
                let cursor_fill = match (last_buttons & 1 != 0, last_buttons & 2 != 0) {
                    (true, true) => cursor_fill_colors.magenta,
                    (true, false) => cursor_fill_colors.red,
                    (false, true) => cursor_fill_colors.blue,
                    _ => cursor_fill_colors.white,
                };
                fb.draw_cursor(
                    cursor_x as usize,
                    cursor_y as usize,
                    cursor_fill,
                    cursor_fill_colors.outline,
                );
            }
        } else if !had_mouse_events {
            // Non-mouse damage (clock tick, Draug, etc.): present shadow->FB
            for r in damage.regions() {
                fb.present_region(r.x, r.y, r.w, r.h);
            }
            // Redraw cursor if it overlaps the presented region
            if cursor_drawn && cursor_y < 22 {
                let cursor_fill = match (last_buttons & 1 != 0, last_buttons & 2 != 0) {
                    (true, true) => cursor_fill_colors.magenta,
                    (true, false) => cursor_fill_colors.red,
                    (false, true) => cursor_fill_colors.blue,
                    _ => cursor_fill_colors.white,
                };
                fb.draw_cursor(
                    cursor_x as usize,
                    cursor_y as usize,
                    cursor_fill,
                    cursor_fill_colors.outline,
                );
            }
        }
        // else: cursor-only movement -- FB already has correct pixels
    }

    if use_gpu && damage.has_damage() {
        let regions = damage.regions();
        if regions.len() == 1 {
            let r = &regions[0];
            libfolk::sys::gpu_flush(r.x, r.y, r.w, r.h);
        } else {
            let mut batch = [[0u32; 4]; 4];
            let n = regions.len().min(4);
            for i in 0..n {
                batch[i] = [regions[i].x, regions[i].y, regions[i].w, regions[i].h];
            }
            libfolk::sys::gpu_flush_batch(&batch[..n]);
        }

        // -- VGA Mirror: copy dirty regions from shadow -> Limine VGA FB --
        // This makes QMP screendump and VNC show the current frame even when
        // the primary output is VirtIO-GPU (whose scanout QMP can't capture on TCG).
        if !vga_mirror_ptr.is_null() {
            let shadow_ptr = fb.shadow_ptr_raw();
            if !shadow_ptr.is_null() {
                let gpu_pitch = fb.pitch;
                for r in regions {
                    let rx = r.x as usize;
                    let ry = r.y as usize;
                    let rw = (r.w as usize).min(vga_mirror_w.saturating_sub(rx));
                    let rh = (r.h as usize).min(vga_mirror_h.saturating_sub(ry));
                    if rw == 0 || rh == 0 {
                        continue;
                    }
                    let bytes_per_row = rw * 4; // 32bpp
                    for row in ry..ry + rh {
                        let src_off = row * gpu_pitch + rx * 4;
                        let dst_off = row * vga_mirror_pitch + rx * 4;
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                shadow_ptr.add(src_off),
                                vga_mirror_ptr.add(dst_off),
                                bytes_per_row,
                            );
                        }
                    }
                }
            }
        }

        damage.clear();
    } else {
        damage.clear();
    }
}

/// Cursor color palette, pre-resolved through fb.color_from_rgb24.
pub struct CursorColors {
    pub white: u32,
    pub red: u32,
    pub blue: u32,
    pub magenta: u32,
    pub outline: u32,
}
