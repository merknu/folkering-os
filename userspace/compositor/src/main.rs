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

use compositor::Compositor;
use compositor::framebuffer::{FramebufferView, colors};
use compositor::window_manager::{WindowManager, HitZone};
use libfolk::sys::ipc::{receive, reply, recv_async, reply_with_token, send, IpcError};
use libfolk::sys::boot_info::{get_boot_info, FramebufferConfig, BOOT_INFO_VADDR};
use libfolk::sys::map_physical::{map_framebuffer, MapFlags};
use libfolk::sys::{yield_cpu, read_mouse, read_key, uptime, shmem_create, shmem_map, shmem_unmap, shmem_destroy, shmem_grant};
use libfolk::sys::io::{write_char, write_str};
use libfolk::sys::shell::{
    SHELL_TASK_ID, SHELL_OP_LIST_FILES, SHELL_OP_CAT_FILE, SHELL_OP_SEARCH,
    SHELL_OP_PS, SHELL_OP_UPTIME,
    SHELL_STATUS_NOT_FOUND, hash_name as shell_hash_name,
};
use libfolk::{entry, println};

/// Virtual address for mapping shared memory received from shell
const COMPOSITOR_SHMEM_VADDR: usize = 0x30000000;

/// Format a usize as a decimal string into buffer, return slice
fn format_usize(n: usize, buf: &mut [u8; 16]) -> &str {
    if n == 0 {
        buf[0] = b'0';
        return unsafe { core::str::from_utf8_unchecked(&buf[..1]) };
    }
    let mut val = n;
    let mut i = 0;
    // Write digits in reverse
    while val > 0 && i < 16 {
        buf[15 - i] = b'0' + (val % 10) as u8;
        val /= 10;
        i += 1;
    }
    // Copy to start
    for j in 0..i {
        buf[j] = buf[16 - i + j];
    }
    unsafe { core::str::from_utf8_unchecked(&buf[..i]) }
}

/// Format uptime in ms as "Xm Ys" or "Xs" string
fn format_uptime(ms: u64, buf: &mut [u8; 32]) -> &str {
    let secs = ms / 1000;
    let mins = secs / 60;
    let remaining_secs = secs % 60;

    let mut i = 0;

    if mins > 0 {
        // Format minutes
        let mut m = mins;
        let mut digits = [0u8; 10];
        let mut d = 0;
        while m > 0 && d < 10 {
            digits[9 - d] = b'0' + (m % 10) as u8;
            m /= 10;
            d += 1;
        }
        for j in (10 - d)..10 {
            buf[i] = digits[j];
            i += 1;
        }
        buf[i] = b'm';
        i += 1;
        buf[i] = b' ';
        i += 1;
    }

    // Format seconds
    let mut s = remaining_secs;
    let mut digits = [0u8; 10];
    let mut d = 0;
    if s == 0 {
        buf[i] = b'0';
        i += 1;
    } else {
        while s > 0 && d < 10 {
            digits[9 - d] = b'0' + (s % 10) as u8;
            s /= 10;
            d += 1;
        }
        for j in (10 - d)..10 {
            buf[i] = digits[j];
            i += 1;
        }
    }
    buf[i] = b's';
    i += 1;

    unsafe { core::str::from_utf8_unchecked(&buf[..i]) }
}

// ============================================================================
// Simple Bump Allocator for userspace
// ============================================================================

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

/// Simple bump allocator for userspace tasks.
/// Allocates from a fixed-size heap, never deallocates (sufficient for Phase 6).
struct BumpAllocator {
    heap: UnsafeCell<[u8; HEAP_SIZE]>,
    next: UnsafeCell<usize>,
}

const HEAP_SIZE: usize = 64 * 1024; // 64KB heap

unsafe impl Sync for BumpAllocator {}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let next = &mut *self.next.get();
        let heap = &mut *self.heap.get();

        // Align up
        let align = layout.align();
        let aligned_next = (*next + align - 1) & !(align - 1);

        let new_next = aligned_next + layout.size();
        if new_next > HEAP_SIZE {
            core::ptr::null_mut() // Out of memory
        } else {
            *next = new_next;
            heap.as_mut_ptr().add(aligned_next)
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator doesn't deallocate
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    heap: UnsafeCell::new([0; HEAP_SIZE]),
    next: UnsafeCell::new(0),
};

// IPC message types
const MSG_CREATE_WINDOW: u64 = 0x01;
const MSG_UPDATE: u64 = 0x02;
const MSG_CLOSE: u64 = 0x03;
const MSG_QUERY_NAME: u64 = 0x10;
const MSG_QUERY_FOCUS: u64 = 0x11;

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

    // Step 5: Create FramebufferView
    let mut fb = unsafe {
        FramebufferView::new(FRAMEBUFFER_VADDR as *mut u8, fb_config)
    };

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

    // Draw the omnibar immediately (visible by default)
    fb.fill_rect(omnibar_x.saturating_sub(2), omnibar_y.saturating_sub(2), omnibar_w + 4, omnibar_h + 4, dark_gray);
    fb.fill_rect(omnibar_x, omnibar_y, omnibar_w, omnibar_h, omnibar_bg);
    fb.draw_rect(omnibar_x, omnibar_y, omnibar_w, omnibar_h, omnibar_border);
    fb.draw_string(omnibar_x + 12, omnibar_y + 12, "Type here...", gray, omnibar_bg);
    fb.draw_string(omnibar_x + omnibar_w - 24, omnibar_y + 12, ">", folk_accent, omnibar_bg);

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

    // ===== Continue with normal operation =====
    let mut compositor = Compositor::new();

    // ===== Phase 7: Mouse cursor tracking =====
    // Initialize cursor at center of screen
    let mut cursor_x: i32 = (fb.width / 2) as i32;
    let mut cursor_y: i32 = (fb.height / 2) as i32;

    // Cursor colors - changes based on button state
    let cursor_white = fb.color_from_rgb24(colors::WHITE);   // No buttons
    let cursor_red = fb.color_from_rgb24(colors::RED);       // Left button
    let cursor_blue = fb.color_from_rgb24(colors::BLUE);     // Right button
    let cursor_magenta = fb.color_from_rgb24(colors::MAGENTA); // Both buttons
    let cursor_outline = fb.color_from_rgb24(colors::BLACK);

    // Cursor size (must match draw_cursor dimensions)
    const CURSOR_W: usize = 12;
    const CURSOR_H: usize = 16;

    // Buffer to save pixels behind cursor (12x16 = 192 pixels)
    // Using repr(align(16)) to ensure proper alignment for potential SSE operations
    #[repr(C, align(16))]
    struct AlignedCursorBuffer([u32; 192]);
    let mut cursor_bg = AlignedCursorBuffer([0; 192]);

    // Track if cursor has been drawn yet (don't draw until first mouse event)
    let mut cursor_drawn = false;
    let mut last_buttons: u8 = 0;
    let mut cursor_bg_dirty = false;  // Set when screen content changes under cursor

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
    let mut text_buffer: [u8; 256] = [0; 256];
    let mut text_len: usize = 0;
    let mut cursor_pos: usize = 0;  // Cursor position within text (0..=text_len)
    let mut show_results: bool = false;
    let mut omnibar_visible: bool = true;  // Start VISIBLE by default

    // Blinking caret state (toggles every ~500ms using uptime syscall)
    let mut caret_visible: bool = true;
    let mut last_caret_flip_ms: u64 = 0;
    const CARET_BLINK_MS: u64 = 500;

    // Mouse click tracking (detect left-button press edge)
    let mut prev_left_button: bool = false;

    // ===== Window Manager (Milestone 2.1) =====
    let mut wm = WindowManager::new();
    // Track which window (if any) is being dragged
    let mut dragging_window_id: Option<u32> = None;
    let mut drag_last_x: i32 = 0;
    let mut drag_last_y: i32 = 0;

    // Colors for omnibar
    let text_box_bg = omnibar_bg;

    write_str("[COMPOSITOR] Omnibar ready\n");

    write_str("[COMPOSITOR] Entering main loop...\n");

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
        write_str("[WM] Boot test window drawn\n");
        // Pixel probe: verify compositor actually painted non-black pixels
        let probe = fb.get_pixel(300, 155); // center of test window
        if probe != 0 {
            write_str("[FB_PROBE] PASS: compositor drew non-black pixels\n");
        } else {
            write_str("[FB_PROBE] FAIL: pixel at (300,155) is black - compositing broken\n");
        }
    }

    loop {
        // Track if we did any work this iteration
        let mut did_work = false;
        // Consolidated redraw flag — any subsystem can set this
        let mut need_redraw = false;

        // ===== Process mouse input =====
        while let Some(event) = read_mouse() {
            did_work = true;

            // Sanity check cursor position (can get corrupted by kernel register save/restore bugs)
            if cursor_x < 0 || cursor_x >= fb.width as i32 || cursor_y < 0 || cursor_y >= fb.height as i32 {
                cursor_x = (fb.width / 2) as i32;
                cursor_y = (fb.height / 2) as i32;
                cursor_bg_dirty = true;
                cursor_drawn = false;
            }

            // Determine cursor color based on button state
            let cursor_fill = match (event.left_button(), event.right_button()) {
                (true, true) => cursor_magenta,   // Both buttons
                (true, false) => cursor_red,      // Left only
                (false, true) => cursor_blue,     // Right only
                (false, false) => cursor_white,   // No buttons
            };

            // First mouse event: draw cursor at center, ignore accumulated delta
            if !cursor_drawn {
                fb.save_rect(cursor_x as usize, cursor_y as usize, CURSOR_W, CURSOR_H, &mut cursor_bg.0);
                fb.draw_cursor(cursor_x as usize, cursor_y as usize, cursor_fill, cursor_outline);
                cursor_drawn = true;
                last_buttons = event.buttons;
                continue;
            }

            // Calculate new position from delta
            let new_x = cursor_x.saturating_add(event.dx as i32);
            let new_y = cursor_y.saturating_sub(event.dy as i32);

            // Clamp to screen bounds
            let new_x = if new_x < 0 { 0 } else if new_x >= fb.width as i32 { fb.width as i32 - 1 } else { new_x };
            let new_y = if new_y < 0 { 0 } else if new_y >= fb.height as i32 { fb.height as i32 - 1 } else { new_y };

            // ===== Milestone 1.4 + 2.2: Mouse Click Hit-Testing + Window Dragging =====
            let left_now = event.left_button();
            let left_pressed = left_now && !prev_left_button;  // rising edge
            let left_released = !left_now && prev_left_button; // falling edge
            prev_left_button = left_now;

            // Window drag: continue drag if in progress
            if left_now {
                if let Some(drag_id) = dragging_window_id {
                    let dx = new_x - drag_last_x;
                    let dy = new_y - drag_last_y;
                    drag_last_x = new_x;
                    drag_last_y = new_y;
                    if dx != 0 || dy != 0 {
                        if let Some(win) = wm.get_window_mut(drag_id) {
                            win.x = win.x.saturating_add(dx);
                            win.y = win.y.saturating_add(dy);
                            // Clamp to screen
                            if win.x < 0 { win.x = 0; }
                            if win.y < 0 { win.y = 0; }
                        }
                        need_redraw = true;
                        cursor_bg_dirty = true;
                    }
                }
            }

            // Release drag
            if left_released {
                dragging_window_id = None;
            }

            if left_pressed {
                let cx = new_x;
                let cy = new_y;

                // Hit-test windows first (topmost)
                let mut handled = false;
                if let Some((win_id, zone)) = wm.hit_test(cx, cy) {
                    match zone {
                        HitZone::CloseButton => {
                            wm.close_window(win_id);
                            need_redraw = true;
                            cursor_bg_dirty = true;
                            handled = true;
                        }
                        HitZone::TitleBar => {
                            wm.focus(win_id);
                            dragging_window_id = Some(win_id);
                            drag_last_x = new_x;
                            drag_last_y = new_y;
                            need_redraw = true;
                            handled = true;
                        }
                        HitZone::Content => {
                            wm.focus(win_id);
                            need_redraw = true;
                            handled = true;
                        }
                    }
                }

                if !handled {
                    let cx = cx as usize;
                    let cy = cy as usize;

                    // Hit-test: click inside the omnibar
                    if cx >= text_box_x && cx < text_box_x + text_box_w
                        && cy >= text_box_y && cy < text_box_y + text_box_h
                    {
                        if show_results {
                            show_results = false;
                            need_redraw = true;
                        }
                    }

                    // Hit-test: click in results panel items
                    if show_results
                        && cx >= results_x && cx < results_x + results_w
                        && cy >= results_y && cy < results_y + results_h
                    {
                        show_results = false;
                        need_redraw = true;
                    }
                }
            }

            // Redraw if cursor moved OR button state changed OR background is dirty
            if new_x != cursor_x || new_y != cursor_y || event.buttons != last_buttons || cursor_bg_dirty {
                // Only restore background if it's not dirty (stale)
                if !cursor_bg_dirty {
                    fb.restore_rect(cursor_x as usize, cursor_y as usize, CURSOR_W, CURSOR_H, &cursor_bg.0);
                }
                cursor_bg_dirty = false;

                // Update position
                cursor_x = new_x;
                cursor_y = new_y;
                last_buttons = event.buttons;

                // Save background at new position, then draw cursor
                fb.save_rect(cursor_x as usize, cursor_y as usize, CURSOR_W, CURSOR_H, &mut cursor_bg.0);
                fb.draw_cursor(cursor_x as usize, cursor_y as usize, cursor_fill, cursor_outline);
            }
        }


        // ===== Blink caret =====
        // Toggle caret every CARET_BLINK_MS; force redraw when it flips
        {
            let now = uptime();
            if now.saturating_sub(last_caret_flip_ms) >= CARET_BLINK_MS {
                caret_visible = !caret_visible;
                last_caret_flip_ms = now;
                if omnibar_visible { need_redraw = true; }
            }
        }


        // ===== Process keyboard input =====
        // First, collect all pending keys without redrawing
        let mut execute_command = false;
        let mut win_execute_command: Option<u32> = None; // window id to execute from
        while let Some(key) = read_key() {
            did_work = true;

            // Arrow key codes from kernel keyboard driver
            const KEY_ARROW_LEFT: u8 = 0x82;
            const KEY_ARROW_RIGHT: u8 = 0x83;
            const KEY_HOME: u8 = 0x84;
            const KEY_END: u8 = 0x85;
            const KEY_DELETE: u8 = 0x86;

            // Route keys to focused interactive window when omnibar is hidden
            if !omnibar_visible {
                if let Some(focused_id) = wm.focused_id {
                    if let Some(win) = wm.get_window_mut(focused_id) {
                        if win.interactive {
                            match key {
                                0x08 | 0x7F => {
                                    if win.input_cursor > 0 {
                                        let mut i = win.input_cursor - 1;
                                        while i < win.input_len - 1 {
                                            win.input_buf[i] = win.input_buf[i + 1];
                                            i += 1;
                                        }
                                        win.input_len -= 1;
                                        win.input_buf[win.input_len] = 0;
                                        win.input_cursor -= 1;
                                        need_redraw = true;
                                    }
                                }
                                b'\n' | b'\r' => {
                                    if win.input_len > 0 {
                                        win_execute_command = Some(focused_id);
                                        need_redraw = true;
                                    }
                                }
                                0x1B => {
                                    // Escape: toggle omnibar back
                                    omnibar_visible = true;
                                    need_redraw = true;
                                }
                                0x20..=0x7E => {
                                    if win.input_len < 126 {
                                        let mut i = win.input_len;
                                        while i > win.input_cursor {
                                            win.input_buf[i] = win.input_buf[i - 1];
                                            i -= 1;
                                        }
                                        win.input_buf[win.input_cursor] = key;
                                        win.input_len += 1;
                                        win.input_cursor += 1;
                                        need_redraw = true;
                                    }
                                }
                                _ => {}
                            }
                            continue; // Key consumed by window
                        }
                    }
                }
                // No interactive window focused — Escape reopens omnibar
                if key == 0x1B {
                    omnibar_visible = true;
                    need_redraw = true;
                    continue;
                }
            }

            match key {
                // Backspace - delete character before cursor
                0x08 | 0x7F => {
                    if cursor_pos > 0 {
                        // Shift characters left to fill gap
                        let mut i = cursor_pos - 1;
                        while i < text_len - 1 {
                            text_buffer[i] = text_buffer[i + 1];
                            i += 1;
                        }
                        text_len -= 1;
                        text_buffer[text_len] = 0;
                        cursor_pos -= 1;
                        need_redraw = true;
                        show_results = false;
                    }
                }
                // Delete key - delete character at cursor
                KEY_DELETE => {
                    if cursor_pos < text_len {
                        let mut i = cursor_pos;
                        while i < text_len - 1 {
                            text_buffer[i] = text_buffer[i + 1];
                            i += 1;
                        }
                        text_len -= 1;
                        text_buffer[text_len] = 0;
                        need_redraw = true;
                        show_results = false;
                    }
                }
                // Arrow keys - move cursor
                KEY_ARROW_LEFT => {
                    if cursor_pos > 0 {
                        cursor_pos -= 1;
                        need_redraw = true;
                    }
                }
                KEY_ARROW_RIGHT => {
                    if cursor_pos < text_len {
                        cursor_pos += 1;
                        need_redraw = true;
                    }
                }
                KEY_HOME => {
                    if cursor_pos != 0 {
                        cursor_pos = 0;
                        need_redraw = true;
                    }
                }
                KEY_END => {
                    if cursor_pos != text_len {
                        cursor_pos = text_len;
                        need_redraw = true;
                    }
                }
                // Enter - execute command/search
                b'\n' | b'\r' => {
                    if text_len > 0 {
                        execute_command = true;
                        show_results = true;
                        need_redraw = true;
                    }
                }
                // Escape - toggle omnibar visibility / clear buffer
                0x1B => {
                    if show_results {
                        show_results = false;
                        need_redraw = true;
                    } else if text_len > 0 {
                        text_len = 0;
                        cursor_pos = 0;
                        for i in 0..MAX_TEXT_LEN {
                            text_buffer[i] = 0;
                        }
                        need_redraw = true;
                    } else {
                        omnibar_visible = !omnibar_visible;
                        need_redraw = true;
                    }
                }
                // Printable ASCII - insert at cursor position
                0x20..=0x7E => {
                    if text_len < MAX_TEXT_LEN - 1 {
                        // Shift characters right to make room
                        let mut i = text_len;
                        while i > cursor_pos {
                            text_buffer[i] = text_buffer[i - 1];
                            i -= 1;
                        }
                        text_buffer[cursor_pos] = key;
                        text_len += 1;
                        cursor_pos += 1;
                        need_redraw = true;
                        show_results = false;
                    }
                }
                // Ignore other keys (arrow up/down, windows key, etc.)
                _ => {}
            }
        }

        // ===== Milestone 2.3: Create terminal window on Enter =====
        if execute_command && text_len > 0 {
            if let Ok(cmd_str) = core::str::from_utf8(&text_buffer[..text_len]) {
                write_str("[WM] Creating window for: ");
                write_str(cmd_str);
                write_str("\n");

                // Spawn a terminal window at a cascade position
                let win_count = wm.windows.len() as i32;
                let wx = 80 + win_count * 24;
                let wy = 60 + win_count * 24;
                let win_id = wm.create_terminal(cmd_str, wx, wy, 480, 200);

                if let Some(win) = wm.get_window_mut(win_id) {
                    // Execute the command and populate the window
                    win.push_line("> ");  // we'll append cmd below
                    // Title is already the command, show it as first line too
                    let mut title_line = [0u8; 130];
                    title_line[0] = b'>';
                    title_line[1] = b' ';
                    let tlen = cmd_str.len().min(126);
                    title_line[2..2+tlen].copy_from_slice(&cmd_str.as_bytes()[..tlen]);
                    if let Ok(s) = core::str::from_utf8(&title_line[..2+tlen]) {
                        win.push_line(s);
                    }

                    // Built-in commands — routed through Intent Service (microkernel IPC)
                    if cmd_str == "ls" || cmd_str == "files" {
                        win.push_line("Files in ramdisk:");
                        let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                            | (SHELL_OP_LIST_FILES << 8);
                        let ipc_result = unsafe {
                            libfolk::syscall::syscall3(
                                libfolk::syscall::SYS_IPC_SEND,
                                libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                            )
                        };
                        if ipc_result != u64::MAX {
                            let count = (ipc_result >> 32) as usize;
                            let shmem_handle = (ipc_result & 0xFFFFFFFF) as u32;

                            if shmem_handle != 0 && count > 0 {
                                // Map shmem from shell
                                if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                    let buf = unsafe {
                                        core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, count * 32)
                                    };
                                    for i in 0..count {
                                        let offset = i * 32;
                                        // name: [u8; 24]
                                        let name_end = buf[offset..offset+24].iter()
                                            .position(|&b| b == 0).unwrap_or(24);
                                        let name = unsafe {
                                            core::str::from_utf8_unchecked(&buf[offset..offset+name_end])
                                        };
                                        // size: u32 at offset+24
                                        let size = u32::from_le_bytes([
                                            buf[offset+24], buf[offset+25],
                                            buf[offset+26], buf[offset+27]
                                        ]) as usize;
                                        // type: u32 at offset+28 (0=ELF, 1=data)
                                        let kind = u32::from_le_bytes([
                                            buf[offset+28], buf[offset+29],
                                            buf[offset+30], buf[offset+31]
                                        ]);
                                        let kind_str = if kind == 0 { "ELF " } else { "DATA" };

                                        // Format: "  ELF   12345 filename"
                                        let mut line = [0u8; 64];
                                        line[0] = b' '; line[1] = b' ';
                                        line[2..6].copy_from_slice(kind_str.as_bytes());
                                        line[6] = b' ';
                                        let mut size_buf = [0u8; 16];
                                        let size_str = format_usize(size, &mut size_buf);
                                        let slen = size_str.len();
                                        let pad = 8usize.saturating_sub(slen);
                                        for j in 0..pad { line[7 + j] = b' '; }
                                        line[7+pad..7+pad+slen].copy_from_slice(size_str.as_bytes());
                                        line[7+pad+slen] = b' ';
                                        let nlen = name.len().min(40);
                                        line[8+pad+slen..8+pad+slen+nlen].copy_from_slice(&name.as_bytes()[..nlen]);
                                        let total = 8 + pad + slen + nlen;
                                        if let Ok(s) = core::str::from_utf8(&line[..total]) {
                                            win.push_line(s);
                                        }
                                    }
                                    let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
                                    let _ = shmem_destroy(shmem_handle);
                                }
                            }
                            let mut count_buf = [0u8; 16];
                            let count_str = format_usize(count, &mut count_buf);
                            let suffix = b" file(s)";
                            let mut line_buf = [0u8; 32];
                            let clen = count_str.len().min(16);
                            let slen2 = suffix.len();
                            line_buf[..clen].copy_from_slice(&count_str.as_bytes()[..clen]);
                            line_buf[clen..clen+slen2].copy_from_slice(suffix);
                            if let Ok(s) = core::str::from_utf8(&line_buf[..clen+slen2]) {
                                win.push_line(s);
                            }
                        } else {
                            win.push_line("Shell not responding");
                        }
                    } else if cmd_str == "ps" || cmd_str == "tasks" {
                        win.push_line("Running tasks:");
                        // Route through Intent Service (4-hop IPC test)
                        let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                            | (SHELL_OP_PS << 8);
                        let ipc_result = unsafe {
                            libfolk::syscall::syscall3(
                                libfolk::syscall::SYS_IPC_SEND,
                                libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                            )
                        };
                        if ipc_result != u64::MAX {
                            let count = (ipc_result >> 32) as usize;
                            let shmem_handle = (ipc_result & 0xFFFFFFFF) as u32;

                            if shmem_handle != 0 && count > 0 {
                                if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                    let buf = unsafe {
                                        core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, count * 32)
                                    };
                                    for i in 0..count {
                                        let offset = i * 32;
                                        let tid = u32::from_le_bytes([
                                            buf[offset], buf[offset+1],
                                            buf[offset+2], buf[offset+3]
                                        ]);
                                        let state = u32::from_le_bytes([
                                            buf[offset+4], buf[offset+5],
                                            buf[offset+6], buf[offset+7]
                                        ]);
                                        let name_end = buf[offset+8..offset+24].iter()
                                            .position(|&b| b == 0).unwrap_or(16);
                                        let name = unsafe {
                                            core::str::from_utf8_unchecked(&buf[offset+8..offset+8+name_end])
                                        };
                                        let state_str = match state {
                                            0 => "Runnable",
                                            1 => "Running",
                                            2 => "Blocked",
                                            3 => "Blocked",
                                            4 => "Waiting",
                                            5 => "Exited",
                                            _ => "Unknown",
                                        };
                                        // Format: "  Task 2: synapse (Blocked)"
                                        let mut line = [0u8; 64];
                                        let mut pos = 0usize;
                                        let prefix = b"  Task ";
                                        line[..prefix.len()].copy_from_slice(prefix);
                                        pos += prefix.len();
                                        let mut tid_buf2 = [0u8; 16];
                                        let tid_str = format_usize(tid as usize, &mut tid_buf2);
                                        let tlen = tid_str.len();
                                        line[pos..pos+tlen].copy_from_slice(tid_str.as_bytes());
                                        pos += tlen;
                                        line[pos] = b':'; pos += 1;
                                        line[pos] = b' '; pos += 1;
                                        let nlen = name.len().min(15);
                                        if nlen > 0 {
                                            line[pos..pos+nlen].copy_from_slice(&name.as_bytes()[..nlen]);
                                            pos += nlen;
                                        } else {
                                            let unk = b"<unnamed>";
                                            line[pos..pos+unk.len()].copy_from_slice(unk);
                                            pos += unk.len();
                                        }
                                        line[pos] = b' '; pos += 1;
                                        line[pos] = b'('; pos += 1;
                                        let slen = state_str.len();
                                        line[pos..pos+slen].copy_from_slice(state_str.as_bytes());
                                        pos += slen;
                                        line[pos] = b')'; pos += 1;
                                        if let Ok(s) = core::str::from_utf8(&line[..pos]) {
                                            win.push_line(s);
                                        }
                                    }
                                    let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
                                    let _ = shmem_destroy(shmem_handle);
                                }
                            } else {
                                // Fallback: count only
                                let mut count_buf = [0u8; 16];
                                let count_str = format_usize(count, &mut count_buf);
                                win.push_line(count_str);
                                win.push_line("task(s) — no details available");
                            }
                        } else {
                            win.push_line("Shell not responding");
                        }
                    } else if cmd_str.starts_with("cat ") {
                        // cat <filename> — route through Intent Service → Shell → Synapse
                        let filename = cmd_str[4..].trim();
                        if filename.is_empty() {
                            win.push_line("usage: cat <filename>");
                        } else {
                            // Hash filename and route through Intent Service
                            let name_hash = shell_hash_name(filename) as u64;
                            let shell_payload = SHELL_OP_CAT_FILE | (name_hash << 8);
                            let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                                | (shell_payload << 8);
                            let ipc_result = unsafe {
                                libfolk::syscall::syscall3(
                                    libfolk::syscall::SYS_IPC_SEND,
                                    libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                                )
                            };
                            if ipc_result != u64::MAX && ipc_result != SHELL_STATUS_NOT_FOUND {
                                let size = (ipc_result >> 32) as usize;
                                let shmem_handle = (ipc_result & 0xFFFFFFFF) as u32;

                                if shmem_handle != 0 && size > 0 {
                                    if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                        let buf = unsafe {
                                            core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, size)
                                        };
                                        // Display file contents line by line
                                        let mut line_start = 0;
                                        for pos in 0..size {
                                            if buf[pos] == b'\n' || buf[pos] == 0 {
                                                if pos > line_start {
                                                    if let Ok(line) = core::str::from_utf8(&buf[line_start..pos]) {
                                                        win.push_line(line);
                                                    }
                                                }
                                                line_start = pos + 1;
                                                if buf[pos] == 0 { break; }
                                            }
                                        }
                                        // Handle last line without newline
                                        if line_start < size {
                                            let end = buf[line_start..size]
                                                .iter().position(|&b| b == 0)
                                                .map(|p| line_start + p)
                                                .unwrap_or(size);
                                            if end > line_start {
                                                if let Ok(line) = core::str::from_utf8(&buf[line_start..end]) {
                                                    win.push_line(line);
                                                }
                                            }
                                        }
                                        let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
                                        let _ = shmem_destroy(shmem_handle);
                                    }
                                } else {
                                    win.push_line("File is empty");
                                }
                            } else {
                                win.push_line("File not found");
                            }
                        }
                    } else if cmd_str == "uptime" {
                        let ms = uptime();
                        let mut buf = [0u8; 32];
                        let time_str = format_uptime(ms, &mut buf);
                        win.push_line(time_str);
                    } else if cmd_str == "help" {
                        win.push_line("Commands: ls, cat, ps, uptime");
                        win.push_line("find <q>, calc <e>, open <a>");
                        win.push_line("Windows: drag title, click X");
                    } else if cmd_str.starts_with("find ") || cmd_str.starts_with("search ") {
                        let query = if cmd_str.starts_with("find ") {
                            cmd_str[5..].trim()
                        } else {
                            cmd_str[7..].trim()
                        };
                        if query.is_empty() {
                            win.push_line("usage: find <query>");
                        } else {
                            win.push_line("Searching Synapse...");
                            // Create shmem with query string
                            let query_bytes = query.as_bytes();
                            let query_len = query_bytes.len().min(63);
                            if let Ok(query_handle) = shmem_create(64) {
                                // Grant broadly
                                for tid in 2..=8 {
                                    let _ = shmem_grant(query_handle, tid);
                                }
                                if shmem_map(query_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                    let buf = unsafe {
                                        core::slice::from_raw_parts_mut(COMPOSITOR_SHMEM_VADDR as *mut u8, 64)
                                    };
                                    buf[..query_len].copy_from_slice(&query_bytes[..query_len]);
                                    buf[query_len] = 0; // null terminate
                                    let _ = shmem_unmap(query_handle, COMPOSITOR_SHMEM_VADDR);
                                }

                                // Send to Shell: SHELL_OP_SEARCH | (query_handle << 8) | (query_len << 40)
                                let shell_req = SHELL_OP_SEARCH
                                    | ((query_handle as u64) << 8)
                                    | ((query_len as u64) << 40);
                                let ipc_result = unsafe {
                                    libfolk::syscall::syscall3(
                                        libfolk::syscall::SYS_IPC_SEND,
                                        SHELL_TASK_ID as u64, shell_req, 0
                                    )
                                };

                                // Cleanup query shmem
                                let _ = shmem_destroy(query_handle);

                                if ipc_result != u64::MAX && ipc_result != 0 {
                                    let count = (ipc_result >> 32) as usize;
                                    let results_handle = (ipc_result & 0xFFFFFFFF) as u32;

                                    if results_handle != 0 && count > 0 {
                                        win.push_line("");
                                        let mut match_buf = [0u8; 40];
                                        let prefix = b"Matches: ";
                                        match_buf[..prefix.len()].copy_from_slice(prefix);
                                        let mut num_buf = [0u8; 16];
                                        let num_str = format_usize(count, &mut num_buf);
                                        let nlen = num_str.len();
                                        match_buf[prefix.len()..prefix.len()+nlen]
                                            .copy_from_slice(num_str.as_bytes());
                                        if let Ok(s) = core::str::from_utf8(&match_buf[..prefix.len()+nlen]) {
                                            win.push_line(s);
                                        }

                                        // Read results from shmem
                                        if shmem_map(results_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                            let buf = unsafe {
                                                core::slice::from_raw_parts(
                                                    COMPOSITOR_SHMEM_VADDR as *const u8, count * 32
                                                )
                                            };
                                            for i in 0..count.min(10) {
                                                let offset = i * 32;
                                                let name_end = buf[offset..offset+24].iter()
                                                    .position(|&b| b == 0).unwrap_or(24);
                                                let name = unsafe {
                                                    core::str::from_utf8_unchecked(
                                                        &buf[offset..offset+name_end]
                                                    )
                                                };
                                                let size = u32::from_le_bytes([
                                                    buf[offset+24], buf[offset+25],
                                                    buf[offset+26], buf[offset+27]
                                                ]) as usize;
                                                // Format: "  synapse (30774 bytes)"
                                                let mut line = [0u8; 64];
                                                line[0] = b' '; line[1] = b' ';
                                                let nlen2 = name.len().min(30);
                                                line[2..2+nlen2].copy_from_slice(&name.as_bytes()[..nlen2]);
                                                let mut size_buf2 = [0u8; 16];
                                                let size_str = format_usize(size, &mut size_buf2);
                                                let slen = size_str.len();
                                                line[2+nlen2] = b' ';
                                                line[3+nlen2] = b'(';
                                                line[4+nlen2..4+nlen2+slen]
                                                    .copy_from_slice(size_str.as_bytes());
                                                let suffix = b" bytes)";
                                                line[4+nlen2+slen..4+nlen2+slen+suffix.len()]
                                                    .copy_from_slice(suffix);
                                                let total = 4 + nlen2 + slen + suffix.len();
                                                if let Ok(s) = core::str::from_utf8(&line[..total]) {
                                                    win.push_line(s);
                                                }
                                            }
                                            let _ = shmem_unmap(results_handle, COMPOSITOR_SHMEM_VADDR);
                                            let _ = shmem_destroy(results_handle);
                                        }
                                    } else {
                                        win.push_line("No matches found");
                                    }
                                } else {
                                    win.push_line("No matches found");
                                }
                            }
                        }
                    } else if cmd_str == "term" || cmd_str == "terminal" {
                        // Open interactive terminal — make this window interactive
                        win.interactive = true;
                        win.push_line("Folkering OS Terminal");
                        win.push_line("Type commands, Enter to run, Esc for omnibar");
                    } else if cmd_str.starts_with("calc ") {
                        win.push_line("Calculator: coming soon");
                    } else {
                        win.push_line("Sent to shell...");
                    }
                    if !win.interactive {
                        win.push_line("---");
                    }
                }

                // Clear the omnibar input after executing
                text_len = 0;
                cursor_pos = 0;
                for i in 0..MAX_TEXT_LEN { text_buffer[i] = 0; }
                show_results = false;
                cursor_bg_dirty = true;
            }
        }

        // ===== Execute command from interactive terminal window =====
        if let Some(win_id) = win_execute_command {
            if let Some(win) = wm.get_window_mut(win_id) {
                let cmd_len = win.input_len;
                let mut cmd_buf = [0u8; 128];
                cmd_buf[..cmd_len].copy_from_slice(&win.input_buf[..cmd_len]);
                win.clear_input();

                if let Ok(cmd_str) = core::str::from_utf8(&cmd_buf[..cmd_len]) {
                    // Echo the command
                    let mut echo = [0u8; 132];
                    echo[0] = b'f'; echo[1] = b'o'; echo[2] = b'l';
                    echo[3] = b'k'; echo[4] = b'>'; echo[5] = b' ';
                    let elen = cmd_len.min(125);
                    echo[6..6+elen].copy_from_slice(&cmd_buf[..elen]);
                    if let Ok(s) = core::str::from_utf8(&echo[..6+elen]) {
                        win.push_line(s);
                    }

                    // Execute built-in commands (same as omnibar but output to THIS window)
                    if cmd_str == "ls" || cmd_str == "files" {
                        let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                            | (SHELL_OP_LIST_FILES << 8);
                        let ipc_result = unsafe {
                            libfolk::syscall::syscall3(
                                libfolk::syscall::SYS_IPC_SEND,
                                libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                            )
                        };
                        if ipc_result != u64::MAX {
                            let count = (ipc_result >> 32) as usize;
                            let shmem_handle = (ipc_result & 0xFFFFFFFF) as u32;
                            if shmem_handle != 0 && count > 0 {
                                if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                    let buf = unsafe {
                                        core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, count * 32)
                                    };
                                    for i in 0..count {
                                        let offset = i * 32;
                                        let name_end = buf[offset..offset+24].iter()
                                            .position(|&b| b == 0).unwrap_or(24);
                                        let name = unsafe { core::str::from_utf8_unchecked(&buf[offset..offset+name_end]) };
                                        let size = u32::from_le_bytes([buf[offset+24], buf[offset+25], buf[offset+26], buf[offset+27]]);
                                        let mut line = [0u8; 48];
                                        line[0] = b' '; line[1] = b' ';
                                        let nlen = name.len().min(30);
                                        line[2..2+nlen].copy_from_slice(&name.as_bytes()[..nlen]);
                                        let mut sb = [0u8; 16];
                                        let ss = format_usize(size as usize, &mut sb);
                                        let sl = ss.len();
                                        line[3+nlen..3+nlen+sl].copy_from_slice(ss.as_bytes());
                                        if let Ok(s) = core::str::from_utf8(&line[..3+nlen+sl]) {
                                            win.push_line(s);
                                        }
                                    }
                                    let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
                                    let _ = shmem_destroy(shmem_handle);
                                }
                            }
                        }
                    } else if cmd_str == "ps" || cmd_str == "tasks" {
                        let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT | (SHELL_OP_PS << 8);
                        let ipc_result = unsafe {
                            libfolk::syscall::syscall3(
                                libfolk::syscall::SYS_IPC_SEND,
                                libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                            )
                        };
                        if ipc_result != u64::MAX {
                            let count = (ipc_result >> 32) as usize;
                            let handle = (ipc_result & 0xFFFFFFFF) as u32;
                            if handle != 0 && count > 0 {
                                if shmem_map(handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                    let buf = unsafe {
                                        core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, count * 32)
                                    };
                                    for i in 0..count {
                                        let off = i * 32;
                                        let tid = u32::from_le_bytes([buf[off], buf[off+1], buf[off+2], buf[off+3]]);
                                        let state = u32::from_le_bytes([buf[off+4], buf[off+5], buf[off+6], buf[off+7]]);
                                        let ne = buf[off+8..off+24].iter().position(|&b| b == 0).unwrap_or(16);
                                        let name = unsafe { core::str::from_utf8_unchecked(&buf[off+8..off+8+ne]) };
                                        let ss = match state { 0=>"Run", 1=>"Run", 2=>"Blk", 3=>"Blk", 4=>"Wait", 5=>"Exit", _=>"?" };
                                        let mut line = [0u8; 48];
                                        let mut p = 0;
                                        let mut tb = [0u8; 16];
                                        let ts = format_usize(tid as usize, &mut tb);
                                        line[p..p+ts.len()].copy_from_slice(ts.as_bytes()); p += ts.len();
                                        line[p] = b' '; p += 1;
                                        let nl = name.len().min(15);
                                        line[p..p+nl].copy_from_slice(&name.as_bytes()[..nl]); p += nl;
                                        line[p] = b' '; p += 1;
                                        line[p..p+ss.len()].copy_from_slice(ss.as_bytes()); p += ss.len();
                                        if let Ok(s) = core::str::from_utf8(&line[..p]) { win.push_line(s); }
                                    }
                                    let _ = shmem_unmap(handle, COMPOSITOR_SHMEM_VADDR);
                                    let _ = shmem_destroy(handle);
                                }
                            }
                        }
                    } else if cmd_str == "uptime" {
                        let ms = uptime();
                        let mut buf = [0u8; 32];
                        let time_str = format_uptime(ms, &mut buf);
                        win.push_line(time_str);
                    } else if cmd_str.starts_with("cat ") {
                        let filename = cmd_str[4..].trim();
                        if filename.is_empty() {
                            win.push_line("usage: cat <filename>");
                        } else {
                            let name_hash = shell_hash_name(filename) as u64;
                            let shell_payload = SHELL_OP_CAT_FILE | (name_hash << 8);
                            let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                                | (shell_payload << 8);
                            let ipc_result = unsafe {
                                libfolk::syscall::syscall3(
                                    libfolk::syscall::SYS_IPC_SEND,
                                    libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                                )
                            };
                            if ipc_result != u64::MAX && ipc_result != SHELL_STATUS_NOT_FOUND {
                                let size = (ipc_result >> 32) as usize;
                                let handle = (ipc_result & 0xFFFFFFFF) as u32;
                                if handle != 0 && size > 0 {
                                    if shmem_map(handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                        let buf = unsafe {
                                            core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, size)
                                        };
                                        let mut start = 0;
                                        for pos in 0..size {
                                            if buf[pos] == b'\n' || buf[pos] == 0 {
                                                if pos > start {
                                                    if let Ok(line) = core::str::from_utf8(&buf[start..pos]) {
                                                        win.push_line(line);
                                                    }
                                                }
                                                start = pos + 1;
                                                if buf[pos] == 0 { break; }
                                            }
                                        }
                                        if start < size {
                                            let end = buf[start..size].iter().position(|&b| b == 0)
                                                .map(|p| start + p).unwrap_or(size);
                                            if end > start {
                                                if let Ok(line) = core::str::from_utf8(&buf[start..end]) {
                                                    win.push_line(line);
                                                }
                                            }
                                        }
                                        let _ = shmem_unmap(handle, COMPOSITOR_SHMEM_VADDR);
                                        let _ = shmem_destroy(handle);
                                    }
                                } else {
                                    win.push_line("File is empty");
                                }
                            } else {
                                win.push_line("File not found");
                            }
                        }
                    } else if cmd_str == "help" {
                        win.push_line("ls ps cat find uptime help term");
                    } else {
                        win.push_line("Unknown command. Try: help");
                    }
                }
            }
        }

        // Only redraw once after processing all keys
        if need_redraw {
            if omnibar_visible {
                // ===== Draw Omnibar =====
                // Outer glow (subtle)
                fb.fill_rect(text_box_x.saturating_sub(2), text_box_y.saturating_sub(2), text_box_w + 4, text_box_h + 4, dark_gray);
                // Main box
                fb.fill_rect(text_box_x, text_box_y, text_box_w, text_box_h, text_box_bg);
                fb.draw_rect(text_box_x, text_box_y, text_box_w, text_box_h, omnibar_border);

                // Draw user input text (single line for omnibar)
                if text_len > 0 {
                    if let Ok(_input_str) = core::str::from_utf8(&text_buffer[..text_len]) {
                        // Truncate if too long
                        let display_len = if text_len > chars_per_line { chars_per_line } else { text_len };
                        if let Ok(display_str) = core::str::from_utf8(&text_buffer[..display_len]) {
                            fb.draw_string(text_box_x + TEXT_PADDING, text_box_y + 12, display_str, white, text_box_bg);
                        }
                    }
                } else {
                    // Show placeholder when empty
                    fb.draw_string(text_box_x + TEXT_PADDING, text_box_y + 12, "Ask anything...", gray, text_box_bg);
                }

                // Draw blinking text caret at cursor position
                let caret_x_pos = text_box_x + TEXT_PADDING + (cursor_pos.min(chars_per_line) * 8);
                if caret_x_pos < text_box_x + text_box_w - 30 {
                    let caret_char = if caret_visible { "|" } else { " " };
                    fb.draw_string(caret_x_pos, text_box_y + 10, caret_char, folk_accent, text_box_bg);
                }

                // Draw ">" icon on right
                fb.draw_string(text_box_x + text_box_w - 24, text_box_y + 12, ">", folk_accent, text_box_bg);

                // Context hints below omnibar
                let hint = "find <query> | calc <expr> | open <app>";
                let hint_x = (fb.width.saturating_sub(hint.len() * 8)) / 2;
                fb.draw_string(hint_x, text_box_y + text_box_h + 16, hint, dark_gray, folk_dark);

                // ===== Results Panel =====
                if show_results && text_len > 0 {
                    // Draw results box above omnibar
                    let results_bg = fb.color_from_rgb24(0x252540);
                    fb.fill_rect(results_x, results_y, results_w, results_h, results_bg);
                    fb.draw_rect(results_x, results_y, results_w, results_h, folk_accent);

                    // Parse command and show appropriate results
                    if let Ok(cmd_str) = core::str::from_utf8(&text_buffer[..text_len]) {
                        // Header
                        fb.draw_string(results_x + 12, results_y + 12, "Results:", folk_accent, results_bg);

                        if cmd_str == "ls" || cmd_str == "files" {
                            // Preview: no IPC — results shown in window on Enter
                            fb.draw_string(results_x + 12, results_y + 36, "List files in ramdisk", white, results_bg);
                            fb.draw_string(results_x + 12, results_y + 56, "Press Enter to run", gray, results_bg);
                        } else if cmd_str == "ps" || cmd_str == "tasks" {
                            // Preview: no IPC — results shown in window on Enter
                            fb.draw_string(results_x + 12, results_y + 36, "Show running tasks", white, results_bg);
                            fb.draw_string(results_x + 12, results_y + 56, "Press Enter to run", gray, results_bg);
                        } else if cmd_str == "uptime" {
                            // Preview: no IPC — results shown in window on Enter
                            fb.draw_string(results_x + 12, results_y + 36, "System uptime", white, results_bg);
                            fb.draw_string(results_x + 12, results_y + 56, "Press Enter to run", gray, results_bg);
                        } else if cmd_str.starts_with("calc ") {
                            // Simple calculator
                            fb.draw_string(results_x + 12, results_y + 36, "Calculator:", white, results_bg);
                            fb.draw_string(results_x + 12, results_y + 56, cmd_str, gray, results_bg);
                            fb.draw_string(results_x + 12, results_y + 80, "(math evaluation coming soon)", dark_gray, results_bg);
                        } else if cmd_str.starts_with("find ") || cmd_str.starts_with("search ") {
                            // Search query preview
                            fb.draw_string(results_x + 12, results_y + 36, "Search Synapse", white, results_bg);
                            fb.draw_string(results_x + 12, results_y + 56, "Press Enter to search", gray, results_bg);
                        } else if cmd_str.starts_with("open ") {
                            // Open app/file
                            fb.draw_string(results_x + 12, results_y + 36, "Opening:", white, results_bg);
                            let app_name = &cmd_str[5..];
                            fb.draw_string(results_x + 12, results_y + 56, app_name, folk_accent, results_bg);
                            fb.draw_string(results_x + 12, results_y + 80, "(app launcher coming soon)", dark_gray, results_bg);
                        } else if cmd_str == "help" {
                            // Help command
                            fb.draw_string(results_x + 12, results_y + 36, "Available commands:", white, results_bg);
                            fb.draw_string(results_x + 12, results_y + 56, "ls, cat <f>, ps, uptime, help", folk_accent, results_bg);
                            fb.draw_string(results_x + 12, results_y + 80, "find <query>, calc <expr>, open <app>", gray, results_bg);
                        } else {
                            // Unknown command — preview only (no IPC from results panel)
                            fb.draw_string(results_x + 12, results_y + 36, "Command:", white, results_bg);
                            fb.draw_string(results_x + 12, results_y + 56, cmd_str, folk_accent, results_bg);
                            fb.draw_string(results_x + 12, results_y + 80, "Press Enter to run", dark_gray, results_bg);
                        }
                    }
                } else {
                    // Clear results area when no results to show
                    fb.fill_rect(results_x, results_y, results_w, results_h, folk_dark);
                }
            } else {
                // ===== Omnibar hidden - clear the area =====
                // Clear omnibar area
                fb.fill_rect(text_box_x - 2, text_box_y - 2, text_box_w + 4, text_box_h + 4, folk_dark);
                // Clear results area
                fb.fill_rect(results_x, results_y, results_w, results_h, folk_dark);
                // Clear hint area below omnibar position
                fb.fill_rect(0, text_box_y + text_box_h + 8, fb.width, 24, folk_dark);

                // Show hint to open omnibar
                let hint = "Press Windows/Super key to open Omnibar";
                let hint_x = (fb.width.saturating_sub(hint.len() * 8)) / 2;
                fb.draw_string(hint_x, fb.height - 50, hint, dark_gray, folk_dark);
            }

            // ===== Composite Windows (Milestone 2.1) =====
            // Draw all managed windows on top of the desktop/omnibar
            if wm.has_visible() {
                wm.composite(&mut fb);
            }

            // Mark cursor background as dirty - mouse handler will refresh it
            cursor_bg_dirty = true;
        }

        // ===== Process IPC messages (non-blocking) =====
        match recv_async() {
            Ok(msg) => {
                did_work = true;
                let response = handle_message(&mut compositor, msg.payload0);
                let _ = reply_with_token(msg.token, response, 0);
            }
            Err(IpcError::WouldBlock) => {}
            Err(_) => {}
        }

        // Only yield CPU if we did no work this iteration
        if !did_work {
            yield_cpu();
        }
    }
}

/// Handle an incoming IPC message.
///
/// # Protocol (Phase 6.1 - Single Payload)
///
/// All data is packed into payload0 since recv_async() only provides payload0:
///
/// - MSG_CREATE_WINDOW (0x01): opcode only, returns window_id
/// - MSG_UPDATE (0x02): [opcode:8][window:4][node:16][role:8][hash:24]
/// - MSG_CLOSE (0x03): [opcode:8][window:4]
/// - MSG_QUERY_NAME (0x10): [opcode:8][hash:24]
/// - MSG_QUERY_FOCUS (0x11): opcode only
///
/// Returns the response payload.
fn handle_message(compositor: &mut Compositor, payload0: u64) -> u64 {
    // Extract opcode from low 8 bits
    let opcode = payload0 & 0xFF;

    match opcode {
        MSG_CREATE_WINDOW => {
            let window_id = compositor.create_window();
            println!("[COMPOSITOR] Created window {}", window_id);
            window_id
        }

        MSG_UPDATE => {
            // Decode: [opcode:8][window:4][node:16][role:8][hash:24]
            let window_id = (payload0 >> 8) & 0xF;
            let node_id = (payload0 >> 12) & 0xFFFF;
            let role = ((payload0 >> 28) & 0xFF) as u8;
            let name_hash = ((payload0 >> 36) & 0xFF_FFFF) as u32;

            // Convert role byte to Role enum
            let role_enum = role_from_u8(role);

            // Create node with name that will hash to the same value
            let node = libaccesskit_folk::Node::new(role_enum)
                .with_name(format_hash_name(name_hash));

            // Create TreeUpdate with single node
            let update = libaccesskit_folk::TreeUpdate::new(node_id)
                .with_node(node_id, node);

            // Process update
            if compositor.handle_update(window_id, update).is_ok() {
                println!("[COMPOSITOR] Updated win {} node {} (role={}, hash={:#x})",
                         window_id, node_id, role, name_hash);
                0
            } else {
                println!("[COMPOSITOR] Update failed for window {}", window_id);
                u64::MAX
            }
        }

        MSG_CLOSE => {
            let window_id = (payload0 >> 8) & 0xF;
            compositor.handle_close(window_id);
            println!("[COMPOSITOR] Closed window {}", window_id);
            0
        }

        MSG_QUERY_NAME => {
            // Decode: [opcode:8][hash:24]
            let name_hash = ((payload0 >> 8) & 0xFF_FFFF) as u32;

            match compositor.world.find_by_name_hash(name_hash) {
                Some((window_id, node_id, _node)) => {
                    println!("[COMPOSITOR] Query: found node {} in window {} (hash={:#x})",
                             node_id, window_id, name_hash);
                    // Pack: window_id in upper 32 bits, node_id in lower 32 bits
                    ((window_id as u64) << 32) | (node_id & 0xFFFF_FFFF)
                }
                None => {
                    println!("[COMPOSITOR] Query: not found (hash={:#x})", name_hash);
                    u64::MAX
                }
            }
        }

        MSG_QUERY_FOCUS => {
            match compositor.world.get_focus() {
                Some((window_id, node_id, _node)) => {
                    ((window_id as u64) << 32) | (node_id & 0xFFFF_FFFF)
                }
                None => u64::MAX
            }
        }

        _ => {
            println!("[COMPOSITOR] Unknown opcode: {:#x}", opcode);
            u64::MAX
        }
    }
}

/// Convert role byte to Role enum
fn role_from_u8(role: u8) -> libaccesskit_folk::Role {
    match role {
        0 => libaccesskit_folk::Role::Unknown,
        1 => libaccesskit_folk::Role::Window,
        2 => libaccesskit_folk::Role::Group,
        3 => libaccesskit_folk::Role::ScrollView,
        4 => libaccesskit_folk::Role::TabPanel,
        5 => libaccesskit_folk::Role::Dialog,
        6 => libaccesskit_folk::Role::Alert,
        10 => libaccesskit_folk::Role::Button,
        11 => libaccesskit_folk::Role::Checkbox,
        12 => libaccesskit_folk::Role::RadioButton,
        13 => libaccesskit_folk::Role::ComboBox,
        14 => libaccesskit_folk::Role::MenuItem,
        15 => libaccesskit_folk::Role::Link,
        16 => libaccesskit_folk::Role::Slider,
        17 => libaccesskit_folk::Role::Tab,
        20 => libaccesskit_folk::Role::StaticText,
        21 => libaccesskit_folk::Role::TextInput,
        22 => libaccesskit_folk::Role::TextArea,
        23 => libaccesskit_folk::Role::Label,
        24 => libaccesskit_folk::Role::Heading,
        30 => libaccesskit_folk::Role::Image,
        31 => libaccesskit_folk::Role::ProgressBar,
        32 => libaccesskit_folk::Role::Separator,
        40 => libaccesskit_folk::Role::List,
        41 => libaccesskit_folk::Role::ListItem,
        42 => libaccesskit_folk::Role::Table,
        43 => libaccesskit_folk::Role::TableRow,
        44 => libaccesskit_folk::Role::TableCell,
        45 => libaccesskit_folk::Role::Tree,
        46 => libaccesskit_folk::Role::TreeItem,
        _ => libaccesskit_folk::Role::Unknown,
    }
}

/// Format a hash as a name string.
/// Phase 6.1 workaround: we store the hash directly as a hex string
/// so that find_by_name_hash can match it.
/// Note: We use 6 hex digits (24 bits) to match the IPC encoding.
fn format_hash_name(hash: u32) -> alloc::string::String {
    use alloc::string::String;
    use core::fmt::Write;

    let mut s = String::new();
    // Use 6 hex digits to match the 24-bit truncated hash from IPC
    let _ = write!(s, "__hash_{:06x}", hash & 0xFF_FFFF);
    s
}
