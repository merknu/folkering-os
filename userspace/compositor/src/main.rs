//! Folkering OS Compositor Service
//!
//! This is the main entry point for the compositor service that runs
//! as a userspace task. It receives TreeUpdate messages from applications
//! and maintains the WorldTree for AI agent queries.
//!
//! # IPC Protocol
//!
//! Applications communicate with the compositor using the following messages:
//!
//! - `COMPOSITOR_CREATE_WINDOW` (0x01): Create a new window, returns window_id
//! - `COMPOSITOR_UPDATE` (0x02): Send TreeUpdate via shared memory
//! - `COMPOSITOR_CLOSE` (0x03): Close a window
//! - `COMPOSITOR_QUERY_NAME` (0x10): Find node by name (for AI)
//! - `COMPOSITOR_QUERY_FOCUS` (0x11): Get current focus (for AI)

#![no_std]
#![no_main]

extern crate alloc;

mod allocator;
mod util;
mod ui_dump;
mod ipc_helpers;
mod iqe;
mod god_mode;
mod input_mouse;
mod input_keyboard;
mod command_dispatch;
mod mcp_handler;
mod rendering;

use util::*;
use ui_dump::*;
use ipc_helpers::*;

use compositor::Compositor;
use compositor::framebuffer::{FramebufferView, colors};
use compositor::window_manager::{WindowManager, HitZone, BORDER_W, TITLE_BAR_H, UiWidget, WindowKind};
use libfolk::sys::ipc::{receive, reply, recv_async, reply_with_token, send, IpcError};
use libfolk::sys::boot_info::{get_boot_info, FramebufferConfig, BOOT_INFO_VADDR};
use libfolk::sys::map_physical::{map_framebuffer, MapFlags};
use libfolk::sys::{yield_cpu, read_mouse, read_key, uptime, shmem_create, shmem_map, shmem_unmap, shmem_destroy, shmem_grant};
use libfolk::sys::io::{write_char, write_str};
use libfolk::sys::shell::{
    SHELL_TASK_ID, SHELL_OP_LIST_FILES, SHELL_OP_CAT_FILE, SHELL_OP_SEARCH,
    SHELL_OP_PS, SHELL_OP_UPTIME, SHELL_OP_EXEC, SHELL_OP_OPEN_APP,
    SHELL_OP_INJECT_STATE,
    SHELL_STATUS_NOT_FOUND, hash_name as shell_hash_name,
};
use libfolk::{entry, println};
// write_file is used inline as libfolk::sys::synapse::write_file

/// Virtual address for mapping shared memory received from shell
const COMPOSITOR_SHMEM_VADDR: usize = 0x30000000;

/// Virtual address for mapping TokenRing shmem (ULTRA 43: isolated from ask shmem)
const RING_VADDR: usize = 0x32000000;

/// Virtual address for query shmem to inference (ULTRA 43)
const ASK_QUERY_VADDR: usize = 0x30000000;

/// TokenRing header — must match inference-server's TokenRing layout (ULTRA 37, 40)
/// [write_idx: AtomicU32, status: AtomicU32, _pad: [u32;2], data: [u8; 16368]]
/// Total: 16384 bytes = 4 pages
const RING_HEADER_SIZE: usize = 16;
const RING_DATA_MAX: usize = 16368;

// Utility functions moved to util.rs

// UI dump functions moved to ui_dump.rs

// Allocator moved to allocator.rs

// IPC constants moved to ipc_helpers.rs

entry!(main);

/// Run the IPC loop without graphics (fallback mode)
fn run_ipc_loop() -> ! {
    let mut compositor = Compositor::new();
    println!("[COMPOSITOR] Running in blind mode (no graphics)");
    println!("[COMPOSITOR] Ready. Waiting for IPC messages...");

    loop {
        match recv_async() {
            Ok(msg) => {
                let response = handle_message(&mut compositor, msg.payload0);
                let _ = reply_with_token(msg.token, response, 0);
            }
            Err(IpcError::WouldBlock) => {
                match receive() {
                    Ok(msg) => {
                        let response = handle_message(&mut compositor, msg.payload0);
                        let _ = reply(response, 0);
                    }
                    Err(IpcError::WouldBlock) => {
                        yield_cpu();
                    }
                    Err(_) => {}
                }
            }
            Err(_) => {}
        }
    }
}

/// Framebuffer virtual address for mapping (4GB mark in userspace)
const FRAMEBUFFER_VADDR: u64 = 0x0000_0001_0000_0000;


fn main() -> ! {
    println!("[COMPOSITOR] Starting Semantic Mirror compositor service...");

    // ===== Phase 6.2: First Light - Graphics Initialization =====

    // Step 1: Read boot info from fixed address
    let boot_info = match get_boot_info() {
        Some(info) => {
            info
        }
        None => {
            println!("[COMPOSITOR] ERROR: Boot info not found or invalid magic!");
            run_ipc_loop();
        }
    };

    // Step 2: Print framebuffer info using simple hex output
    write_str("W:");
    // Print width as decimal digits
    let w = boot_info.framebuffer.width;
    if w >= 1000 { write_char(b'0' + ((w / 1000) % 10) as u8); }
    if w >= 100 { write_char(b'0' + ((w / 100) % 10) as u8); }
    if w >= 10 { write_char(b'0' + ((w / 10) % 10) as u8); }
    write_char(b'0' + (w % 10) as u8);

    write_str(" H:");
    let h = boot_info.framebuffer.height;
    if h >= 100 { write_char(b'0' + ((h / 100) % 10) as u8); }
    if h >= 10 { write_char(b'0' + ((h / 10) % 10) as u8); }
    write_char(b'0' + (h % 10) as u8);
    write_str("\n");

    let fb_config = &boot_info.framebuffer;

    if fb_config.physical_address == 0 {
        println!("[COMPOSITOR] No framebuffer available, running blind");
        run_ipc_loop();
    }

    // Use simple output to avoid stack-heavy formatting
    write_str("[COMPOSITOR] FB info OK\n");

    // Debug: Print shift values to diagnose color issues
    write_str("[COMPOSITOR] Shifts: R=");
    write_char(b'0' + fb_config.red_mask_shift / 10);
    write_char(b'0' + fb_config.red_mask_shift % 10);
    write_str(" G=");
    write_char(b'0' + fb_config.green_mask_shift / 10);
    write_char(b'0' + fb_config.green_mask_shift % 10);
    write_str(" B=");
    write_char(b'0' + fb_config.blue_mask_shift / 10);
    write_char(b'0' + fb_config.blue_mask_shift % 10);
    write_str("\n");

    // Step 3: Calculate framebuffer size
    let fb_size = (fb_config.pitch as u64) * (fb_config.height as u64);
    write_str("[COMPOSITOR] Mapping FB...");

    // Step 4: Map framebuffer with Write-Combining
    match map_framebuffer(fb_config.physical_address, FRAMEBUFFER_VADDR, fb_size) {
        Ok(()) => {
            write_str(" OK\n");
        }
        Err(_e) => {
            write_str(" FAIL\n");
            // Continue without graphics
            run_ipc_loop();
        }
    }

    // Step 5: Try VirtIO-GPU first, fall back to Limine framebuffer
    const GPU_FB_VADDR: usize = 0x0000_0002_0000_0000; // 8GB mark for GPU FB
    let mut use_gpu = false;

    let mut gpu_w_saved: u32 = 0;
    let mut gpu_h_saved: u32 = 0;
    if let Some((gw, gh)) = libfolk::sys::gpu_info(GPU_FB_VADDR) {
        gpu_w_saved = gw;
        gpu_h_saved = gh;
        write_str("[COMPOSITOR] VirtIO-GPU: ");
        if gw >= 1000 { write_char(b'0' + ((gw / 1000) % 10) as u8); }
        if gw >= 100 { write_char(b'0' + ((gw / 100) % 10) as u8); }
        write_char(b'0' + ((gw / 10) % 10) as u8);
        write_char(b'0' + (gw % 10) as u8);
        write_str("x");
        if gh >= 100 { write_char(b'0' + ((gh / 100) % 10) as u8); }
        write_char(b'0' + ((gh / 10) % 10) as u8);
        write_char(b'0' + (gh % 10) as u8);
        write_str(" mapped at GPU_FB_VADDR\n");
        use_gpu = true;
    }

    // ── VGA Mirror: dual-output for VNC/screendump compatibility ──
    // When using VirtIO-GPU, QMP screendump captures the Bochs VGA linear FB
    // (not the VirtIO scanout). Mirror the shadow buffer to BOTH outputs so
    // VNC/screendump always shows the current frame.
    let vga_mirror_ptr: *mut u8 = if use_gpu { FRAMEBUFFER_VADDR as *mut u8 } else { core::ptr::null_mut() };
    let vga_mirror_pitch = fb_config.pitch as usize;
    let vga_mirror_w = fb_config.width as usize;
    let vga_mirror_h = fb_config.height as usize;

    // Use VirtIO-GPU framebuffer when available — gpu_flush() sends THIS memory
    // to the display via TRANSFER_TO_HOST_2D + RESOURCE_FLUSH (instant update).
    // VGA mirror ensures VNC/screendump also gets the pixels.
    let mut fb = if use_gpu && gpu_w_saved > 0 {
        let gpu_config = libfolk::sys::boot_info::FramebufferConfig {
            physical_address: 0,
            width: gpu_w_saved,
            height: gpu_h_saved,
            pitch: gpu_w_saved * 4,  // 32bpp, tightly packed
            bpp: 32,
            memory_model: 1, // RGB
            red_mask_size: fb_config.red_mask_size,
            red_mask_shift: fb_config.red_mask_shift,
            green_mask_size: fb_config.green_mask_size,
            green_mask_shift: fb_config.green_mask_shift,
            blue_mask_size: fb_config.blue_mask_size,
            blue_mask_shift: fb_config.blue_mask_shift,
            _reserved: [0; 3],
        };
        write_str("[COMPOSITOR] Rendering to VirtIO-GPU FB (instant flush)\n");
        unsafe { FramebufferView::new(GPU_FB_VADDR as *mut u8, &gpu_config) }
    } else {
        write_str("[COMPOSITOR] No VirtIO-GPU, using Limine FB\n");
        use_gpu = false;
        unsafe { FramebufferView::new(FRAMEBUFFER_VADDR as *mut u8, fb_config) }
    };

    // Initialize damage tracker for dirty rectangle optimization
    let mut damage = compositor::damage::DamageTracker::new(fb.width as u32, fb.height as u32);
    // First frame: mark everything dirty
    damage.damage_full();

    // God Mode Pipe (COM3) — direct command injection buffer
    let mut com3_buf = [0u8; 512];
    let mut com3_len = 0usize;
    let mut com3_queue: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();

    // WASM JIT Toolsmithing — async generation (non-blocking)
    // Phase 1: "gemini generate X" → send [GENERATE_TOOL] via async COM2
    // Phase 2: poll COM2 each frame until response arrives
    // Phase 3: decode WASM, execute, render results
    let mut mcp = compositor::state::McpState::new();
    // deferred_tool_gen, async_tool_gen now in mcp

    // ===== WASM State (consolidated) =====
    let mut wasm = compositor::state::WasmState::new();

    // ===== RAM History Graph =====
    const RAM_HISTORY_LEN: usize = 120; // 2 minutes at 1 sample/sec
    let mut ram_history = compositor::state::RamHistory::new();
    // input.show_ram_graph in input.input.show_ram_graph

    // ===== IQE Latency Tracking =====
    let mut iqe = compositor::state::IqeState::new();
    let tsc_per_us: u64 = 3400;

    // ===== App Launcher: Android-style folders + app grid =====
    // MAX_CATEGORIES and MAX_APPS_PER_CAT from compositor::state
    const FOLDER_W: usize = 100;
    const FOLDER_H: usize = 100;
    const FOLDER_GAP: usize = 20;
    const APP_TILE_W: usize = 72;
    const APP_TILE_H: usize = 72;
    const APP_TILE_GAP: usize = 12;
    const APP_TILE_COLS: usize = 5;

    // Category types from state.rs
    use compositor::state::{AppEntry, Category, MAX_CATEGORIES, MAX_APPS_PER_CAT};

    let mut categories: [Category; MAX_CATEGORIES] = [
        Category { label: b"System",   color: 0x003388FF, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
        Category { label: b"Games",    color: 0x00FF4466, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
        Category { label: b"Creative", color: 0x00FF8800, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
        Category { label: b"Tools",    color: 0x0044CC44, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
        Category { label: b"Demos",    color: 0x00AA44FF, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
        Category { label: b"Other",    color: 0x00888888, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
    ];

    // -1 = home (show folders), 0-5 = inside a specific folder
    let mut render = compositor::state::RenderState::new();

    // ===== NEURAL DESKTOP =====
    // AI-native interface with Omnibar at center
    let folk_blue = fb.color_from_rgb24(colors::FOLK_BLUE);
    let folk_dark = fb.color_from_rgb24(colors::FOLK_DARK);
    let white = fb.color_from_rgb24(colors::WHITE);
    let folk_accent = fb.color_from_rgb24(colors::FOLK_ACCENT);
    let gray = fb.color_from_rgb24(0x666666);
    let dark_gray = fb.color_from_rgb24(0x333333);

    // Clear to dark background
    fb.clear(folk_dark);

    // ===== Title at top =====
    let title = "FOLKERING OS";
    let title_x = (fb.width.saturating_sub(title.len() * 8)) / 2;
    fb.draw_string(title_x, 40, title, folk_accent, folk_dark);

    let subtitle = "Neural Desktop";
    let sub_x = (fb.width.saturating_sub(subtitle.len() * 8)) / 2;
    fb.draw_string(sub_x, 60, subtitle, gray, folk_dark);

    // ===== Omnibar (centered, near bottom) =====
    let omnibar_w: usize = 500;
    let omnibar_h: usize = 40;
    let omnibar_x = (fb.width.saturating_sub(omnibar_w)) / 2;
    let omnibar_y = fb.height - 120;

    // Omnibar colors
    let omnibar_bg = fb.color_from_rgb24(0x1a1a2e);
    let omnibar_border = folk_accent;

    // Draw the glass omnibar immediately (visible by default)
    let omnibar_alpha: u8 = 180;
    fb.fill_rect_alpha(omnibar_x.saturating_sub(2), omnibar_y.saturating_sub(2), omnibar_w + 4, omnibar_h + 4, 0x333333, omnibar_alpha / 2);
    fb.fill_rect_alpha(omnibar_x, omnibar_y, omnibar_w, omnibar_h, omnibar_bg, omnibar_alpha);
    fb.draw_rect(omnibar_x, omnibar_y, omnibar_w, omnibar_h, omnibar_border);
    fb.draw_string_alpha(omnibar_x + 12, omnibar_y + 12, "Type here...", gray, 0x1a1a2e, omnibar_alpha);
    fb.draw_string_alpha(omnibar_x + omnibar_w - 24, omnibar_y + 12, ">", folk_accent, 0x1a1a2e, omnibar_alpha);

    // Hint text below omnibar
    let hint = "Type and press Enter | ESC to clear";
    let hint_x = (fb.width.saturating_sub(hint.len() * 8)) / 2;
    fb.draw_string(hint_x, omnibar_y + omnibar_h + 16, hint, dark_gray, folk_dark);

    // ===== Results area (above omnibar, initially hidden) =====
    // We'll draw this when there are search results
    let results_w: usize = omnibar_w;
    let results_h: usize = 160;
    let results_x = omnibar_x;
    let results_y = omnibar_y - results_h - 10;

    write_str("[COMPOSITOR] *** NEURAL DESKTOP ***\n");

    // ===== Silverfir-nano JIT self-test =====
    compositor::wasm_runtime::test_silverfir_jit();

    // ===== Continue with normal operation =====
    let mut compositor = Compositor::new();

    // ===== Phase 7: Mouse cursor tracking =====
    // Initialize cursor at center of screen
    let mut cursor = compositor::state::CursorState::new();
    cursor.x = (fb.width / 2) as i32;
    cursor.y = (fb.height / 2) as i32;

    // Cursor colors - changes based on button state
    let cursor_white = fb.color_from_rgb24(colors::WHITE);   // No buttons
    let cursor_red = fb.color_from_rgb24(colors::RED);       // Left button
    let cursor_blue = fb.color_from_rgb24(colors::BLUE);     // Right button
    let cursor_magenta = fb.color_from_rgb24(colors::MAGENTA); // Both buttons
    let cursor_outline = fb.color_from_rgb24(colors::BLACK);

    // Cursor dimensions (from framebuffer::FramebufferView)
    use compositor::framebuffer::FramebufferView;
    const CURSOR_W: usize = FramebufferView::CURSOR_W;
    const CURSOR_H: usize = FramebufferView::CURSOR_H;

    // Buffer to save pixels behind cursor (16x24 = 384 pixels)
    #[repr(C, align(16))]
    struct AlignedCursorBuffer([u32; CURSOR_W * CURSOR_H]);
    let mut cursor_bg = AlignedCursorBuffer([0; CURSOR_W * CURSOR_H]);

    // Track if cursor has been drawn yet (don't draw until first mouse event)
    let mut cursor_drawn = false;
    let mut last_buttons: u8 = 0;
    // bg_dirty moved to cursor.bg_dirty (CursorState)

    write_str("[COMPOSITOR] Mouse+IPC ready\n");

    // ===== Omnibar Input Configuration =====
    // Use omnibar dimensions for text input
    let text_box_x: usize = omnibar_x;
    let text_box_y: usize = omnibar_y;
    let text_box_w: usize = omnibar_w;
    let text_box_h: usize = omnibar_h;
    const TEXT_PADDING: usize = 12;
    const MAX_TEXT_LEN: usize = 256;
    let chars_per_line: usize = (text_box_w - TEXT_PADDING * 2 - 24) / 8;  // -24 for the ">" icon

    // Text buffer for typed input - use local stack variables
    // (Previous static mut caused undefined behavior)
    // State struct types defined in compositor::state (see state.rs).
    // Variables below will be migrated into these structs incrementally.
    // For now, structs serve as the architectural blueprint.

    let mut input = compositor::state::InputState::new();

    // Alt+Tab HUD state — now in render (RenderState)

    // ===== Clipboard buffer (Milestone 20) =====
    // clipboard in input.input.clipboard_buf/input.clipboard_len

    // ===== Async Inference / Token Streaming State =====
    let mut stream = compositor::state::StreamState::new();

    const TOOL_OPEN: &[u8] = b"<|tool|>";    // 8 bytes
    const TOOL_CLOSE: &[u8] = b"<|/tool|>";  // 9 bytes
    const THINK_BUF_SIZE: usize = 1024;
    const THINK_OPEN: &[u8] = b"<think>";    // 7 bytes
    const THINK_CLOSE: &[u8] = b"</think>";  // 8 bytes
    const RESULT_OPEN: &[u8] = b"<|tool_result|>";   // 15 bytes
    const RESULT_CLOSE: &[u8] = b"<|/tool_result|>"; // 16 bytes

    // Blinking caret state (toggles every ~500ms using uptime syscall)
    // caret in input.input.caret_visible/input.last_caret_flip_ms
    const CARET_BLINK_MS: u64 = 500;

    // Mouse click tracking: input.prev_left_button moved to cursor.prev_left_button (CursorState)
    // Friction Sensor: click_timestamps/click_ts_idx moved to cursor (CursorState)
    // wasm.app_open_since_ms, wasm.fuel_fail_count, wasm.state_snapshot now in wasm (WasmState)
    // immune_patching now in mcp
    // wasm.active_drivers now in wasm (WasmState)
    // pending_driver_device now in mcp
    // pending_shell_jit, shell_jit_pipeline now in mcp
    // wasm.streaming_upstream, wasm.streaming_downstream now in wasm (WasmState)
    // wasm.node_connections, wasm.connection_drag, wasm.window_apps now in wasm (WasmState)

    // ===== Window Manager (Milestone 2.1) =====
    let mut wm = WindowManager::new();
    // Window drag state: dragging_window_id/drag_last_x/drag_last_y moved to cursor (CursorState)

    // Colors for omnibar
    let text_box_bg = omnibar_bg;

    write_str("[COMPOSITOR] Omnibar ready\n");

    // Initialize MCP session with random session_id
    libfolk::mcp::client::init_session();
    write_str("[COMPOSITOR] MCP session: 0x");
    {
        let sid = libfolk::mcp::client::session_id();
        let hex_chars = b"0123456789abcdef";
        for i in (0..8).rev() {
            write_char(hex_chars[((sid >> (i * 4)) & 0xF) as usize]);
        }
    }
    write_str("\n");
    write_str("[COMPOSITOR] Entering main loop v3 (Layer4 transport)...\n");

    // ===== BOOT-TIME TEST WINDOW =====
    // Diagnose compositing pipeline: if this window appears, compositing works.
    // If it does NOT appear, the draw_window path has a bug.
    {
        let test_id = wm.create_terminal("Boot Test", 100, 80, 400, 150);
        if let Some(win) = wm.get_window_mut(test_id) {
            win.push_line("Folkering OS v0.8");
            win.push_line("Window Manager: OK");
            win.push_line("Type commands in omnibar");
        }
        // Draw immediately (before first event)
        wm.composite(&mut fb);
        fb.present_full(); // Copy shadow→FB for initial display
        // Flush to VirtIO-GPU so VNC shows the initial frame immediately
        if use_gpu {
            libfolk::sys::gpu_flush(0, 0, fb.width as u32, fb.height as u32);
            // VGA Mirror: also copy initial frame to Limine VGA FB for screendump
            if !vga_mirror_ptr.is_null() {
                let shadow = fb.shadow_ptr_raw();
                if !shadow.is_null() {
                    let copy_w = (fb.width).min(vga_mirror_w);
                    let copy_h = (fb.height).min(vga_mirror_h);
                    for row in 0..copy_h {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                shadow.add(row * fb.pitch),
                                vga_mirror_ptr.add(row * vga_mirror_pitch),
                                copy_w * 4,
                            );
                        }
                    }
                    write_str("[VGA_MIRROR] Initial frame copied to Limine FB\n");
                }
            }
        }
        write_str("[WM] Boot test window drawn\n");
        // Pixel probe: verify compositor actually painted non-black pixels
        let probe = fb.get_pixel(300, 155); // center of test window
        if probe != 0 {
            write_str("[FB_PROBE] PASS: compositor drew non-black pixels\n");
        } else {
            write_str("[FB_PROBE] FAIL: pixel at (300,155) is black - compositing broken\n");
        }
        // Close boot test window — it served its diagnostic purpose
        wm.close_window(test_id);
    }

    // Bootstrap driver seeding is deferred to first `generate driver` command
    // (Synapse needs time to load SQLite from VirtIO disk at boot).
    let mut drivers_seeded = false;

    // ===== M12: Restore saved app states from previous session =====
    const RESTORE_FKUI_VADDR: usize = COMPOSITOR_SHMEM_VADDR + 0x1000;
    const RESTORE_INJECT_VADDR: usize = COMPOSITOR_SHMEM_VADDR + 0x2000;
    {
        if let Ok(resp) = libfolk::sys::synapse::read_file_shmem("app_states.dat") {
            if shmem_map(resp.shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                let state_buf = unsafe {
                    core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, resp.size as usize)
                };
                let count = state_buf[0] as usize;
                if count > 0 && count <= 8 {
                    write_str("[WM] Restoring ");
                    let mut nbuf = [0u8; 16];
                    write_str(format_usize(count, &mut nbuf));
                    write_str(" app(s) from previous session\n");

                    // Copy state data to local buffer before unmapping
                    let mut local_states = [0u8; 1 + 8 * 22];
                    let copy_len = (1 + count * 22).min(local_states.len());
                    local_states[..copy_len].copy_from_slice(&state_buf[..copy_len]);

                    let _ = shmem_unmap(resp.shmem_handle, COMPOSITOR_SHMEM_VADDR);
                    let _ = shmem_destroy(resp.shmem_handle);

                    for i in 0..count {
                        let off = 1 + i * 22;
                        let mut entry_bytes = [0u8; 22];
                        entry_bytes.copy_from_slice(&local_states[off..off + 22]);

                        // Step 1: Load calc.fkui from VFS
                        if let Ok(fkui_resp) = libfolk::sys::synapse::read_file_shmem("calc.fkui") {
                            if shmem_map(fkui_resp.shmem_handle, RESTORE_FKUI_VADDR).is_ok() {
                                let fkui_buf = unsafe {
                                    core::slice::from_raw_parts(RESTORE_FKUI_VADDR as *const u8, 4096)
                                };
                                if let Some(header) = libfolk::ui::parse_header(fkui_buf) {
                                    let wc = wm.windows.len() as i32;
                                    let app_id = wm.create_terminal(
                                        header.title,
                                        120 + wc * 30, 100 + wc * 30,
                                        header.width as u32, header.height as u32,
                                    );

                                    // Step 2: Overwrite entry with NEW win_id
                                    entry_bytes[0..4].copy_from_slice(&(app_id as u32).to_le_bytes());

                                    // Step 3: Create shmem for inject payload and send to Shell
                                    if let Ok(inject_handle) = shmem_create(4096) {
                                        let _ = shmem_grant(inject_handle, SHELL_TASK_ID);
                                        if shmem_map(inject_handle, RESTORE_INJECT_VADDR).is_ok() {
                                            let dst = unsafe {
                                                core::slice::from_raw_parts_mut(
                                                    RESTORE_INJECT_VADDR as *mut u8, 22
                                                )
                                            };
                                            dst.copy_from_slice(&entry_bytes);
                                            let _ = shmem_unmap(inject_handle, RESTORE_INJECT_VADDR);
                                        }

                                        // Send INJECT_STATE to Shell via Intent Service
                                        let shell_payload = SHELL_OP_INJECT_STATE
                                            | ((inject_handle as u64) << 16);
                                        let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                                            | (shell_payload << 8);
                                        let ipc_result = unsafe {
                                            libfolk::syscall::syscall3(
                                                libfolk::syscall::SYS_IPC_SEND,
                                                libfolk::sys::intent::INTENT_TASK_ID as u64,
                                                intent_req, 0
                                            )
                                        };

                                        // Step 4: Shell returns FKUI shmem with correct display
                                        let magic = (ipc_result >> 48) as u16;
                                        if magic == 0x5549 {
                                            let ui_handle = (ipc_result & 0xFFFFFFFF) as u32;
                                            if shmem_map(ui_handle, RESTORE_INJECT_VADDR).is_ok() {
                                                let ui_buf = unsafe {
                                                    core::slice::from_raw_parts(
                                                        RESTORE_INJECT_VADDR as *const u8, 4096
                                                    )
                                                };
                                                if let Some(ui_hdr) = libfolk::ui::parse_header(ui_buf) {
                                                    if let Some(app_win) = wm.get_window_mut(app_id) {
                                                        app_win.kind = compositor::window_manager::WindowKind::App;
                                                        app_win.owner_task = SHELL_TASK_ID;
                                                        app_win.widgets.clear();
                                                        let (root, _) = parse_widget_tree(ui_hdr.widget_data);
                                                        if let Some(widget) = root {
                                                            app_win.widgets.push(widget);
                                                        }
                                                    }
                                                }
                                                let _ = shmem_unmap(ui_handle, RESTORE_INJECT_VADDR);
                                            }
                                            let _ = shmem_destroy(ui_handle);
                                        }
                                        let _ = shmem_destroy(inject_handle);
                                    }

                                    write_str("[WM] Restored app window\n");
                                }
                                let _ = shmem_unmap(fkui_resp.shmem_handle, RESTORE_FKUI_VADDR);
                            }
                            let _ = shmem_destroy(fkui_resp.shmem_handle);
                        }
                    }

                    // Force redraw after restore
                    wm.composite(&mut fb);
                    write_str("[WM] App state restore complete\n");
                } else {
                    let _ = shmem_unmap(resp.shmem_handle, COMPOSITOR_SHMEM_VADDR);
                    let _ = shmem_destroy(resp.shmem_handle);
                }
            } else {
                let _ = shmem_destroy(resp.shmem_handle);
            }
        }
        // No saved state or empty — normal boot, no error
    }

    // last_clock_second now in render (RenderState)
    // tz_offset_minutes, tz_synced, tz_sync_pending now in mcp
    let mut active_agent: Option<compositor::agent::AgentSession> = None; // ReAct agentic loop
    let mut draug = compositor::draug::DraugDaemon::new();

    // Phase A.5 step 2.3: read draug state from the daemon's status
    // shmem instead of the local DraugDaemon for the fields the
    // daemon owns now (refactor counts, task_levels, last_input_ms,
    // hibernation flags). Compositor's local DraugDaemon stays
    // allocated until step 2.4 because the analysis-cycle path and
    // a few HUD callsites still read from it.
    //
    // `attach_status()` returns `Err` if draug-daemon hasn't booted
    // yet — that's expected during cold boot since compositor
    // currently spawns before draug-daemon (Phase A.6 fixes the
    // ordering). Treat as `None` and fall back to local reads.
    let draug_status: Option<&'static libfolk::sys::draug::DraugStatus> =
        libfolk::sys::draug::attach_status().ok();
    if draug_status.is_some() {
        libfolk::sys::io::write_str("[Draug] status shmem attached — HUD reads from daemon\n");
    } else {
        libfolk::sys::io::write_str("[Draug] status shmem not yet ready (daemon booting?) — HUD falls back to local\n");
    }

    // Stability Fix 1: restore state from previous session. Daemon
    // also restores from the same Synapse file at its own boot — no
    // contention because save_state writes only fire from the
    // daemon's tick path now (compositor's local DraugDaemon is
    // dormant on those code paths).
    if draug.restore_state() {
        libfolk::sys::io::write_str("[Draug] Restored state: iter=");
        let mut nb = [0u8; 16];
        libfolk::sys::io::write_str(crate::util::format_usize(draug.refactor_iter as usize, &mut nb));
        libfolk::sys::io::write_str(" levels=[");
        for i in 0..20 {
            if i > 0 { libfolk::sys::io::write_str(","); }
            libfolk::sys::io::write_str(crate::util::format_usize(draug.task_levels[i] as usize, &mut nb));
        }
        libfolk::sys::io::write_str("]\n");
        // The kernel-bridge push that used to live here is now done
        // by draug-daemon on every tick — the boot-time push was
        // double-writing into the bridge atomics for no benefit.
    }

    // Phase 17 — seed the autonomous refactor task queue if it's
    // missing. Daemon picks up the same Synapse VFS file on its
    // own boot, so we only need to make sure the file exists with
    // the merged fixture set.
    //
    // Phase A.5 step 2.4: dropped the `draug.install_refactor_tasks`
    // call that used to install the merged list into compositor's
    // local DraugDaemon — that instance no longer drives the refactor
    // loop (daemon does), so the install was dead code. Compositor
    // keeps the seed-and-save so the on-disk file is up to date for
    // the daemon's next boot.
    {
        let existing = mcp_handler::task_store::load().unwrap_or_default();
        let merged = mcp_handler::task_store::seed_or_merge(
            &existing,
            mcp_handler::refactor_loop::REFACTOR_FIXTURES,
        );
        let _ = mcp_handler::task_store::save(&merged);
        libfolk::sys::io::write_str("[Draug] Refactor queue: ");
        let mut nb = [0u8; 16];
        libfolk::sys::io::write_str(crate::util::format_usize(merged.len(), &mut nb));
        libfolk::sys::io::write_str(" tasks persisted for daemon\n");
    }

    // Pillar 4: WASM warm cache — pre-compiled modules for instant response
    // wasm.cache initialized by WasmState::new()
    const MAX_CACHE_ENTRIES: usize = 4;

    // adapter_cache, pending_adapter now in mcp
    const MAX_ADAPTER_ENTRIES: usize = 8;
    // Adapter input/output buffers for executing adapters
    static mut ADAPTER_INPUT: [u8; 4096] = [0u8; 4096];
    static mut ADAPTER_INPUT_LEN: usize = 0;
    static mut ADAPTER_OUTPUT: [u8; 4096] = [0u8; 4096];
    static mut ADAPTER_OUTPUT_LEN: usize = 0;

    // Timing instrumentation — find the freeze
    #[inline(always)]
    fn rdtsc() -> u64 {
        let lo: u32; let hi: u32;
        unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)); }
        ((hi as u64) << 32) | lo as u64
    }
    let tsc_per_us = libfolk::sys::iqe_tsc_freq().max(1);
    let mut timing_samples: u32 = 0;
    let mut heartbeat_tsc: u64 = 0;
    let mut heartbeat_count: u32 = 0;

    loop {
        // Track if we did any work this iteration
        let mut did_work = false;
        let t_loop_start: u64 = rdtsc();

        // One-shot: confirm loop is alive
        if heartbeat_count == 0 {
            heartbeat_count = 1;
            write_str("[LOOP ALIVE]\n");
        }

        // IQE: Poll telemetry events (moved to iqe.rs)
        iqe::poll_telemetry(&mut iqe, tsc_per_us);

        // Consolidated redraw flag — any subsystem can set this
        // WASM apps need continuous redraws for animation (60fps game loop)
        let mut need_redraw = wasm.active_app.as_ref().map_or(false, |a| a.active);

        // Clock tick: targeted status bar redraw (NO full desktop redraw!)
        // Renders only the 20px status bar directly to shadow buffer.
        // This costs ~50µs instead of 150ms+ for a full desktop redraw.
        let current_second = (libfolk::sys::get_rtc_packed() & 0x3F) as u8;
        if current_second != render.last_clock_second {
            render.last_clock_second = current_second;
            // NOT did_work — clock tick is passive, not user input
            // Status bar damage is added below and gpu_flush handles it

            // Sample RAM usage for history graph
            let (_, _, mem_pct) = libfolk::sys::memory_stats();
            ram_history.push(mem_pct.min(100) as u8);

            // === TARGETED STATUS BAR RENDER (inline, no need_redraw) ===
            // Only overwrite text positions — NO fill_rect for the entire bar.
            // fill_rect(1280×20) takes 125ms under WHPX due to per-pixel emulation.
            {
                let bar_bg = fb.color_from_rgb24(0x0a0a0a);

                // Clock (center) — clear only the clock text area (8chars × 8px = 64px wide, 16px tall)
                let time_x = (fb.width.saturating_sub(8 * 8)) / 2;
                fb.fill_rect(time_x, 0, 68, 18, bar_bg);
                // Date (left) — clear only date area
                fb.fill_rect(4, 0, 84, 18, bar_bg);
                // RAM (right) — clear only RAM area
                let ram_clear_x = fb.width.saturating_sub(70);
                fb.fill_rect(ram_clear_x, 0, 70, 18, bar_bg);

                let dt = libfolk::sys::get_rtc();
                let mut total_minutes = dt.hour as i32 * 60 + dt.minute as i32 + mcp.tz_offset_minutes;
                let mut day = dt.day as i32;
                let mut month = dt.month;
                let mut year = dt.year;
                if total_minutes >= 24 * 60 {
                    total_minutes -= 24 * 60; day += 1;
                    let dim = match month { 2 => 28, 4|6|9|11 => 30, _ => 31 };
                    if day > dim { day = 1; month += 1; if month > 12 { month = 1; year += 1; } }
                } else if total_minutes < 0 {
                    total_minutes += 24 * 60; day -= 1;
                    if day < 1 { month -= 1; if month < 1 { month = 12; year -= 1; } day = 28; }
                }
                let lh = (total_minutes / 60) as u8;
                let lm = (total_minutes % 60) as u8;
                let ls = dt.second;
                let mut t = [0u8; 8];
                t[0] = b'0' + lh / 10; t[1] = b'0' + lh % 10;
                t[2] = b':';
                t[3] = b'0' + lm / 10; t[4] = b'0' + lm % 10;
                t[5] = b':';
                t[6] = b'0' + ls / 10; t[7] = b'0' + ls % 10;
                let time_str = unsafe { core::str::from_utf8_unchecked(&t) };
                let time_x = (fb.width.saturating_sub(8 * 8)) / 2;
                fb.draw_string(time_x, 2, time_str, white, bar_bg);

                // Date (left)
                let mut d = [0u8; 10];
                d[0] = b'0' + ((year/1000)%10) as u8; d[1] = b'0' + ((year/100)%10) as u8;
                d[2] = b'0' + ((year/10)%10) as u8; d[3] = b'0' + (year%10) as u8;
                d[4] = b'-'; d[5] = b'0' + month/10; d[6] = b'0' + month%10;
                d[7] = b'-'; d[8] = b'0' + day as u8/10; d[9] = b'0' + day as u8%10;
                let date_str = unsafe { core::str::from_utf8_unchecked(&d) };
                fb.draw_string(8, 2, date_str, gray, bar_bg);

                // RAM% (right)
                let mut rbuf = [0u8; 8];
                let mut ri = 0usize;
                rbuf[ri] = b'R'; ri += 1; rbuf[ri] = b'A'; ri += 1; rbuf[ri] = b'M'; ri += 1; rbuf[ri] = b' '; ri += 1;
                if mem_pct >= 100 { rbuf[ri] = b'1'; ri += 1; rbuf[ri] = b'0'; ri += 1; rbuf[ri] = b'0'; ri += 1; }
                else { if mem_pct >= 10 { rbuf[ri] = b'0' + (mem_pct / 10) as u8; ri += 1; }
                    rbuf[ri] = b'0' + (mem_pct % 10) as u8; ri += 1; }
                rbuf[ri] = b'%'; ri += 1;
                let ram_str = unsafe { core::str::from_utf8_unchecked(&rbuf[..ri]) };
                let ram_col = if mem_pct > 80 { fb.color_from_rgb24(0xFF4444) }
                    else if mem_pct > 50 { fb.color_from_rgb24(0xFFAA00) }
                    else { fb.color_from_rgb24(0x44FF44) };
                let ram_x = fb.width.saturating_sub(ri * 8 + 8);
                fb.draw_string(ram_x, 2, ram_str, ram_col, bar_bg);

                // IQE latency (between date and clock)
                if iqe.ewma_kbd_us > 0 || iqe.ewma_mou_us > 0 {
                    let mut lbuf = [0u8; 48];
                    let mut li = 0usize;
                    lbuf[li]=b'K'; li+=1; lbuf[li]=b':'; li+=1;
                    li += fmt_u64_into(&mut lbuf[li..], iqe.ewma_kbd_us);
                    if iqe.ewma_kbd_wake > 0 {
                        lbuf[li]=b'('; li+=1;
                        li += fmt_u64_into(&mut lbuf[li..], iqe.ewma_kbd_wake);
                        lbuf[li]=b'+'; li+=1;
                        li += fmt_u64_into(&mut lbuf[li..], iqe.ewma_kbd_rend);
                        lbuf[li]=b')'; li+=1;
                    }
                    if li < 44 { lbuf[li]=b' '; li+=1; lbuf[li]=b'M'; li+=1; lbuf[li]=b':'; li+=1;
                        li += fmt_u64_into(&mut lbuf[li..], iqe.ewma_mou_us);
                    }
                    let s = unsafe { core::str::from_utf8_unchecked(&lbuf[..li.min(48)]) };
                    fb.draw_string(90, 2, s, fb.color_from_rgb24(0x88AACC), bar_bg);

                    let worst = iqe.ewma_kbd_us.max(iqe.ewma_mou_us);
                    let dot = if worst < 5000 { 0x44FF44 } else if worst < 16000 { 0xFFAA00 } else { 0xFF4444 };
                    fb.fill_rect(ram_x.saturating_sub(14), 5, 8, 8, fb.color_from_rgb24(dot));
                }

                // Damage only the text areas (3 small rects instead of full width)
                damage.add_damage(compositor::damage::Rect::new(4, 0, 84, 20));         // date
                damage.add_damage(compositor::damage::Rect::new(time_x as u32, 0, 68, 20)); // clock
                damage.add_damage(compositor::damage::Rect::new(ram_clear_x as u32, 0, 70, 20)); // RAM
            }

            // Try direct kernel NTP first (uses UDP syscall), fall back to MCP proxy
            if !mcp.tz_synced && !mcp.tz_sync_pending {
                // Try NTP via kernel: pool.ntp.org → 162.159.200.123 (Cloudflare time)
                let ntp_ip = [162, 159, 200, 123];
                let ntp_time = libfolk::sys::ntp_query(ntp_ip);
                if ntp_time > 0 {
                    // Got NTP time! Set as authoritative, no need for proxy.
                    mcp.tz_synced = true;
                    write_str("[NTP] Sync OK via kernel UDP\n");
                    // Compute timezone offset later when we know local time vs UTC
                } else if libfolk::mcp::client::send_time_sync() {
                    mcp.tz_sync_pending = true;
                    write_str("[MCP] TimeSyncRequest sent (NTP failed, fallback)\n");
                }
            }
        }

        // ===== AI Systems + MCP Polling (moved to mcp_handler.rs) =====
        {
            let ai = mcp_handler::tick_ai_systems(
                &mut mcp, &mut wasm, &mut wm, &mut stream,
                &mut draug, &mut fb, &mut damage,
                &mut active_agent, &mut drivers_seeded,
                tsc_per_us,
            );
            if ai.did_work { did_work = true; }
            if ai.need_redraw { need_redraw = true; }
        }

        // GOD MODE: Poll COM3 for injected commands (moved to god_mode.rs)
        if god_mode::poll_com3(&mut com3_buf, &mut com3_len, &mut com3_queue) {
            did_work = true;
        }
        // COM3 God Mode: if a command is pending and WASM is fullscreen,
        // force-close the WASM app so the command can be processed by the omnibar.
        if !com3_queue.is_empty() && wasm.active_app.is_some() {
            wasm.active_app = None;
            wasm.active_app_key = None;
            wasm.fuel_fail_count = 0;
            fb.clear(folk_dark);
            need_redraw = true;
            damage.damage_full();
            write_str("[COM3] Closed fullscreen WASM to process command\n");
        }

        // Check if Alt+Tab HUD has expired — clear HUD area and trigger redraw
        if render.hud_show_until > 0 && uptime() >= render.hud_show_until {
            // Clear the HUD area before resetting state
            let old_hud_w = render.hud_title_len * 8 + 24;
            let old_hud_x = (fb.width.saturating_sub(old_hud_w)) / 2;
            let old_hud_y = fb.height.saturating_sub(40);
            fb.fill_rect(old_hud_x, old_hud_y, old_hud_w, 24, folk_dark);
            render.hud_show_until = 0;
            render.hud_title_len = 0;
            need_redraw = true;
        }

        // ===== Process mouse input (moved to input_mouse.rs) =====
        let mut had_mouse_events = false;
        {
            let mouse_layout = input_mouse::MouseLayout {
                folk_dark, cursor_white, cursor_red, cursor_blue, cursor_magenta, cursor_outline,
                text_box_x, text_box_y, text_box_w, text_box_h,
                results_x, results_y, results_w, results_h,
                max_categories: compositor::state::MAX_CATEGORIES,
                folder_w: FOLDER_W, folder_h: FOLDER_H, folder_gap: FOLDER_GAP,
                app_tile_w: APP_TILE_W, app_tile_h: APP_TILE_H,
                app_tile_gap: APP_TILE_GAP, app_tile_cols: APP_TILE_COLS,
                cursor_w: CURSOR_W, cursor_h: CURSOR_H,
            };
            let mr = input_mouse::process_mouse(
                &mut cursor, &mut wm, &mut wasm, &mut input, &mut render,
                &mut stream, &mut draug, &mut fb, &mut damage,
                &mut cursor_drawn, &mut last_buttons, &mut cursor_bg.0,
                tsc_per_us, &categories[..], &mouse_layout,
            );
            if mr.did_work { did_work = true; }
            if mr.need_redraw { need_redraw = true; }
            had_mouse_events = mr.had_events;
        }
        // had_mouse_events is used later for idle detection


        // ===== Blink caret =====
        // Freeze caret when idle >10s — prevents infinite 150ms redraw loop
        {
            let caret_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { uptime() };
            // Phase A.5 step 2.3: prefer the daemon's last_input_ms
            // from shmem (cross-process source of truth). Fall back
            // to compositor's local DraugDaemon if shmem isn't
            // attached (boot-order race with draug-daemon).
            let last_input_ms = draug_status
                .map(|s| s.last_input_ms.load(core::sync::atomic::Ordering::Acquire))
                .unwrap_or_else(|| draug.last_input_ms());
            let idle_secs = caret_ms.saturating_sub(last_input_ms) / 1000;
            if idle_secs < 10 && caret_ms.saturating_sub(input.last_caret_flip_ms) >= CARET_BLINK_MS {
                input.caret_visible = !input.caret_visible;
                input.last_caret_flip_ms = caret_ms;
                if input.omnibar_visible {
                    let caret_x_pos = text_box_x + TEXT_PADDING + (input.cursor_pos.min(chars_per_line) * 8);
                    if caret_x_pos < text_box_x + text_box_w - 30 {
                        fb.fill_rect(caret_x_pos, text_box_y + 8, 8, 20, fb.color_from_rgb24(0x1a1a2e));
                        if input.caret_visible {
                            fb.draw_string(caret_x_pos, text_box_y + 10, "|", folk_accent, fb.color_from_rgb24(0x1a1a2e));
                        }
                        damage.add_damage(compositor::damage::Rect::new(
                            caret_x_pos as u32, text_box_y as u32 + 8, 10, 22));
                        // NOT did_work — caret blink is cosmetic, not user input
                    }
                }
            }
        }


        // ===== Handle app tile click → launch saved app =====
        if render.tile_clicked >= 0 && render.open_folder >= 0 {
            let cat_idx = render.open_folder as usize;
            let app_idx = render.tile_clicked as usize;
            if cat_idx < MAX_CATEGORIES && app_idx < categories[cat_idx].count {
                let entry = &categories[cat_idx].apps[app_idx];
                let name_len = entry.name_len;
                let name_buf = entry.name;
                let app_name = unsafe { core::str::from_utf8_unchecked(&name_buf[..name_len]) };
                let filename = alloc::format!("{}.wasm", app_name);
                write_str("[DESKTOP] Launching: ");
                write_str(app_name);
                write_str("\n");

                // Load from VFS
                const VFS_TILE_VADDR: usize = 0x50040000;
                if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(&filename) {
                    if shmem_map(resp.shmem_handle, VFS_TILE_VADDR).is_ok() {
                        let data = unsafe {
                            core::slice::from_raw_parts(VFS_TILE_VADDR as *const u8, resp.size as usize)
                        };
                        let wasm_bytes = alloc::vec::Vec::from(data);
                        let _ = shmem_unmap(resp.shmem_handle, VFS_TILE_VADDR);
                        let _ = shmem_destroy(resp.shmem_handle);

                        let config = compositor::wasm_runtime::WasmConfig {
                            screen_width: fb.width as u32,
                            screen_height: fb.height as u32,
                            uptime_ms: libfolk::sys::uptime() as u32,
                        };
                        match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                            Ok(app) => {
                                wasm.active_app = Some(app);
                                wasm.active_app_key = Some(alloc::string::String::from(app_name));
                                wasm.app_open_since_ms = libfolk::sys::uptime();
                                wasm.fuel_fail_count = 0;
                                wasm.last_bytes = Some(wasm_bytes);
                            }
                            Err(_) => {}
                        }
                    } else {
                        let _ = shmem_destroy(resp.shmem_handle);
                    }
                }
            }
            render.tile_clicked = -1;
        }

        // ===== Process keyboard input (moved to input_keyboard.rs) =====
        let kr = {
            let kb_layout = input_keyboard::KeyboardLayout {
                folk_dark, folk_accent, gray,
                max_text_len: 256,
                compositor_shmem_vaddr: COMPOSITOR_SHMEM_VADDR,
            };
            input_keyboard::process_keyboard(
                &mut input, &mut wasm, &mut wm, &mut render,
                &mut fb, &mut damage, &mut draug,
                tsc_per_us, &kb_layout,
            )
        };
        let mut execute_command = kr.execute_command;
        let mut win_execute_command = kr.win_execute_command;
        if kr.did_work { did_work = true; }
        if kr.need_redraw { need_redraw = true; }

        // ===== Command dispatch (moved to command_dispatch/) =====
        // Phase C1: 13 parameters → 1 DispatchContext.
        let dr = {
            let mut ctx = command_dispatch::DispatchContext {
                input: &mut input,
                wasm: &mut wasm,
                wm: &mut wm,
                mcp: &mut mcp,
                stream: &mut stream,
                draug: &mut draug,
                fb: &mut fb,
                damage: &mut damage,
                com3_queue: &mut com3_queue,
                active_agent: &mut active_agent,
                drivers_seeded: &mut drivers_seeded,
                cursor: &mut cursor,
            };
            command_dispatch::dispatch_omnibar(&mut ctx, execute_command)
        };
        let mut deferred_app_handle = dr.deferred_app_handle;
        if dr.did_work { did_work = true; }
        if dr.need_redraw { need_redraw = true; }

        // ===== Deferred UI window creation from Shell IPC =====
        if let Some(wid) = win_execute_command {
            let should_create = if let Some(w) = wm.get_window_mut(wid) {
                w.input_buf[4] == 0xAA // marker from app command
            } else {
                false
            };
            if should_create {
                let ui_handle = if let Some(w) = wm.get_window_mut(wid) {
                    w.input_buf[4] = 0; // clear marker
                    u32::from_le_bytes([w.input_buf[0], w.input_buf[1], w.input_buf[2], w.input_buf[3]])
                } else { 0 };

                if ui_handle != 0 {
                    if shmem_map(ui_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                        let buf = unsafe {
                            core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, 4096)
                        };
                        if let Some(header) = libfolk::ui::parse_header(buf) {
                            let wc = wm.windows.len() as i32;
                            let app_id = wm.create_terminal(
                                header.title,
                                120 + wc * 30, 100 + wc * 30,
                                header.width as u32, header.height as u32,
                            );
                            if let Some(app_win) = wm.get_window_mut(app_id) {
                                app_win.kind = compositor::window_manager::WindowKind::App;
                                app_win.owner_task = libfolk::sys::shell::SHELL_TASK_ID;
                                let (root, _) = parse_widget_tree(header.widget_data);
                                if let Some(widget) = root {
                                    app_win.widgets.push(widget);
                                }
                            }
                            write_str("[WM] Created UI window: ");
                            write_str(header.title);
                            write_str("\n");
                            need_redraw = true;
                        }
                        let _ = shmem_unmap(ui_handle, COMPOSITOR_SHMEM_VADDR);
                    }
                    let _ = shmem_destroy(ui_handle);
                }
            }
        }

        // ===== Rendering + Present (moved to rendering.rs) =====
        if need_redraw {
            let rl = rendering::RenderLayout {
                folk_dark, folk_accent, white, gray, dark_gray, omnibar_border,
                text_box_x, text_box_y, text_box_w, text_box_h, chars_per_line,
                results_x, results_y, results_w, results_h,
                folder_w: FOLDER_W, folder_h: FOLDER_H, folder_gap: FOLDER_GAP,
                app_tile_w: APP_TILE_W, app_tile_h: APP_TILE_H, app_tile_gap: APP_TILE_GAP, app_tile_cols: APP_TILE_COLS,
                cursor_w: CURSOR_W, cursor_h: CURSOR_H,
            };
            // Phase C2: 17 parameters → 1 RenderContext.
            let rr = {
                let mut rctx = rendering::RenderContext {
                    fb: &mut fb,
                    wm: &mut wm,
                    wasm: &mut wasm,
                    input: &input,
                    render: &mut render,
                    mcp: &mut mcp,
                    iqe: &iqe,
                    damage: &mut damage,
                    draug: &mut draug,
                    categories: &categories[..],
                    layout: &rl,
                    cursor: &cursor,
                    cursor_drawn,
                    cursor_bg: &mut cursor_bg.0,
                    ram_history: &ram_history.data,
                    ram_history_idx: ram_history.idx,
                    ram_history_count: ram_history.count,
                };
                rendering::render_frame(&mut rctx)
            };
            if rr.did_work { did_work = true; }
        }

        // Present + VGA mirror
        let t_before_present: u64 = rdtsc();
        {
            let cc = rendering::CursorColors {
                white: cursor_white, red: cursor_red, blue: cursor_blue,
                magenta: cursor_magenta, outline: cursor_outline,
            };
            rendering::present_and_flush(
                &mut fb, &mut damage,
                cursor.x, cursor.y, cursor_drawn, last_buttons,
                &cc, need_redraw, had_mouse_events,
                use_gpu, vga_mirror_ptr, vga_mirror_pitch, vga_mirror_w, vga_mirror_h,
            );
        }

        // Timing report: print if any frame took > 1ms (= potential freeze source)
        let t_end: u64 = rdtsc();
        let frame_us = (t_end - t_loop_start) / tsc_per_us;
        if frame_us > 1000 && timing_samples < 30 && need_redraw {
            // Log that need_redraw was set (helps find the trigger)
            write_str("[SLOW REDRAW]\n");
        }
        if frame_us > 1000 && timing_samples < 30 {
            timing_samples += 1;
            // Format: TIMING,<total_us>,<render_us>,<present_us>
            let render_us = (t_before_present - t_loop_start) / tsc_per_us;
            let present_us = (t_end - t_before_present) / tsc_per_us;
            let mut tbuf = [0u8; 64];
            let mut ti = 0usize;
            // "TIMING,"
            for &b in b"TIMING," { tbuf[ti] = b; ti += 1; }
            ti += fmt_u64_into(&mut tbuf[ti..], frame_us);
            tbuf[ti] = b','; ti += 1;
            ti += fmt_u64_into(&mut tbuf[ti..], render_us);
            tbuf[ti] = b','; ti += 1;
            ti += fmt_u64_into(&mut tbuf[ti..], present_us);
            tbuf[ti] = b'\n'; ti += 1;
            libfolk::sys::com3_write(&tbuf[..ti]);
            // Also to serial
            write_str("[");
            if let Ok(s) = core::str::from_utf8(&tbuf[..ti-1]) { write_str(s); }
            write_str("]\n");
        }

        if !did_work {
            // Brief spin then HLT: spin handles polled I/O (COM3, async COM2),
            // HLT handles interrupt-driven I/O (keyboard, mouse, timer).
            for _ in 0..5_000 { core::hint::spin_loop(); }
        }
    }
}


// IPC helpers moved to ipc_helpers.rs
