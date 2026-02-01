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
use libfolk::sys::ipc::{receive, reply, recv_async, reply_with_token, send, IpcError};
use libfolk::sys::boot_info::{get_boot_info, FramebufferConfig, BOOT_INFO_VADDR};
use libfolk::sys::map_physical::{map_framebuffer, MapFlags};
use libfolk::sys::{yield_cpu, read_mouse, read_key, uptime, task_list};
use libfolk::sys::io::{write_char, write_str};
use libfolk::sys::shell::{
    SHELL_TASK_ID, SHELL_OP_LIST_FILES, SHELL_OP_PS, SHELL_OP_UPTIME,
};
use libfolk::{entry, println};

// ============================================================================
// Debug printing helpers
// ============================================================================

/// Print a u32 as decimal
fn print_dec(n: u32) {
    if n == 0 {
        write_char(b'0');
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = 0;
    let mut val = n;
    while val > 0 {
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        write_char(buf[i]);
    }
}

/// Print a signed i32
fn print_signed(n: i32) {
    if n < 0 {
        write_char(b'-');
        print_dec((-n) as u32);
    } else {
        print_dec(n as u32);
    }
}

/// Convert a nibble (0-15) to a hex digit character
fn hex_digit(n: u8) -> u8 {
    match n & 0xF {
        0..=9 => b'0' + n,
        10..=15 => b'A' + (n - 10),
        _ => b'?',
    }
}

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
    write_str("<<<1>>>");  // DEBUG marker 1
    let boot_info = match get_boot_info() {
        Some(info) => {
            write_str("<<<2>>>");  // DEBUG marker 2 - got boot info
            info
        }
        None => {
            println!("[COMPOSITOR] ERROR: Boot info not found or invalid magic!");
            run_ipc_loop();
        }
    };
    write_str("<<<3>>>");  // DEBUG marker 3 - after match

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
    let mut cursor_bg: [u32; CURSOR_W * CURSOR_H] = [0; CURSOR_W * CURSOR_H];

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
    let mut show_results: bool = false;
    let mut omnibar_visible: bool = true;  // Start VISIBLE by default

    // Colors for omnibar
    let text_box_bg = omnibar_bg;

    write_str("[COMPOSITOR] Omnibar ready\n");

    write_str("[COMPOSITOR] Entering main loop...\n");

    let mut loop_count: u32 = 0;
    loop {
        if loop_count < 3 {
            write_str("[LOOP]");
            loop_count += 1;
        }
        // Track if we did any work this iteration
        let mut did_work = false;

        // ===== Process mouse input =====
        write_str("M");
        while let Some(event) = read_mouse() {
            write_str("!");
            did_work = true;
            // Determine cursor color based on button state
            let cursor_fill = match (event.left_button(), event.right_button()) {
                (true, true) => cursor_magenta,   // Both buttons
                (true, false) => cursor_red,      // Left only
                (false, true) => cursor_blue,     // Right only
                (false, false) => cursor_white,   // No buttons
            };

            // First mouse event: draw cursor at center, ignore accumulated delta
            if !cursor_drawn {
                fb.save_rect(cursor_x as usize, cursor_y as usize, CURSOR_W, CURSOR_H, &mut cursor_bg);
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

            // Redraw if cursor moved OR button state changed OR background is dirty
            if new_x != cursor_x || new_y != cursor_y || event.buttons != last_buttons || cursor_bg_dirty {
                // Only restore background if it's not dirty (stale)
                if !cursor_bg_dirty {
                    fb.restore_rect(cursor_x as usize, cursor_y as usize, CURSOR_W, CURSOR_H, &cursor_bg);
                }
                cursor_bg_dirty = false;

                // Update position
                cursor_x = new_x;
                cursor_y = new_y;
                last_buttons = event.buttons;

                // Save background at new position, then draw cursor
                fb.save_rect(cursor_x as usize, cursor_y as usize, CURSOR_W, CURSOR_H, &mut cursor_bg);
                fb.draw_cursor(cursor_x as usize, cursor_y as usize, cursor_fill, cursor_outline);
            }
        }

        // ===== Process keyboard input =====
        // First, collect all pending keys without redrawing
        write_str("K");
        let mut need_redraw = false;
        let mut execute_command = false;
        while let Some(key) = read_key() {
            write_str("!");
            did_work = true;

            match key {
                // Backspace - delete last character
                0x08 | 0x7F => {
                    if text_len > 0 {
                        text_len -= 1;
                        text_buffer[text_len] = 0;
                        need_redraw = true;
                        show_results = false;  // Hide results when typing
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
                        // If results showing, just hide them
                        show_results = false;
                        need_redraw = true;
                    } else if text_len > 0 {
                        // If text entered, clear it
                        text_len = 0;
                        for i in 0..MAX_TEXT_LEN {
                            text_buffer[i] = 0;
                        }
                        need_redraw = true;
                    } else {
                        // Toggle omnibar visibility
                        omnibar_visible = !omnibar_visible;
                        need_redraw = true;
                    }
                }
                // Printable ASCII - add to buffer (always, since omnibar always visible now)
                0x20..=0x7E => {
                    if text_len < MAX_TEXT_LEN - 1 {
                        text_buffer[text_len] = key;
                        text_len += 1;
                        need_redraw = true;
                        show_results = false;  // Hide results when typing
                    }
                }
                // Ignore other keys (including Windows key for now)
                _ => {}
            }
        }

        // Only redraw once after processing all keys
        write_str("R");
        if need_redraw {
            write_str("!");
            if omnibar_visible {
                // ===== Draw Omnibar =====
                // Outer glow (subtle)
                fb.fill_rect(text_box_x - 2, text_box_y - 2, text_box_w + 4, text_box_h + 4, dark_gray);
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

                // Draw cursor after text
                let cursor_x_pos = text_box_x + TEXT_PADDING + (text_len * 8);
                if cursor_x_pos < text_box_x + text_box_w - 30 {
                    fb.draw_string(cursor_x_pos, text_box_y + 12, "_", folk_accent, text_box_bg);
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
                            // List files - send to Shell via IPC
                            fb.draw_string(results_x + 12, results_y + 36, "Files in ramdisk:", white, results_bg);
                            let ipc_result = unsafe {
                                libfolk::syscall::syscall3(
                                    libfolk::syscall::SYS_IPC_SEND,
                                    SHELL_TASK_ID as u64,
                                    SHELL_OP_LIST_FILES,
                                    0
                                )
                            };
                            if ipc_result != u64::MAX {
                                let count = (ipc_result >> 32) as usize;
                                // Format count as string
                                let mut count_buf = [0u8; 16];
                                let count_str = format_usize(count, &mut count_buf);
                                fb.draw_string(results_x + 12, results_y + 56, count_str, folk_accent, results_bg);
                                fb.draw_string(results_x + 12 + count_str.len() * 8 + 8, results_y + 56, "file(s) found", gray, results_bg);
                            } else {
                                fb.draw_string(results_x + 12, results_y + 56, "Shell not responding", gray, results_bg);
                            }
                        } else if cmd_str == "ps" || cmd_str == "tasks" {
                            // Process list
                            fb.draw_string(results_x + 12, results_y + 36, "Running tasks:", white, results_bg);
                            let ipc_result = unsafe {
                                libfolk::syscall::syscall3(
                                    libfolk::syscall::SYS_IPC_SEND,
                                    SHELL_TASK_ID as u64,
                                    SHELL_OP_PS,
                                    0
                                )
                            };
                            if ipc_result != u64::MAX {
                                let count = ipc_result as usize;
                                let mut count_buf = [0u8; 16];
                                let count_str = format_usize(count, &mut count_buf);
                                fb.draw_string(results_x + 12, results_y + 56, count_str, folk_accent, results_bg);
                                fb.draw_string(results_x + 12 + count_str.len() * 8 + 8, results_y + 56, "task(s) running", gray, results_bg);
                            } else {
                                fb.draw_string(results_x + 12, results_y + 56, "Shell not responding", gray, results_bg);
                            }
                        } else if cmd_str == "uptime" {
                            // System uptime
                            fb.draw_string(results_x + 12, results_y + 36, "System uptime:", white, results_bg);
                            let ipc_result = unsafe {
                                libfolk::syscall::syscall3(
                                    libfolk::syscall::SYS_IPC_SEND,
                                    SHELL_TASK_ID as u64,
                                    SHELL_OP_UPTIME,
                                    0
                                )
                            };
                            if ipc_result != u64::MAX {
                                let ms = ipc_result;
                                let secs = ms / 1000;
                                let mins = secs / 60;
                                let mut buf = [0u8; 32];
                                let time_str = format_uptime(ms, &mut buf);
                                fb.draw_string(results_x + 12, results_y + 56, time_str, folk_accent, results_bg);
                            } else {
                                fb.draw_string(results_x + 12, results_y + 56, "Shell not responding", gray, results_bg);
                            }
                        } else if cmd_str.starts_with("calc ") {
                            // Simple calculator
                            fb.draw_string(results_x + 12, results_y + 36, "Calculator:", white, results_bg);
                            fb.draw_string(results_x + 12, results_y + 56, cmd_str, gray, results_bg);
                            fb.draw_string(results_x + 12, results_y + 80, "(math evaluation coming soon)", dark_gray, results_bg);
                        } else if cmd_str.starts_with("find ") || cmd_str.starts_with("search ") {
                            // Search query
                            fb.draw_string(results_x + 12, results_y + 36, "Searching Synapse...", white, results_bg);
                            fb.draw_string(results_x + 12, results_y + 56, "(vector search coming soon)", dark_gray, results_bg);
                        } else if cmd_str.starts_with("open ") {
                            // Open app/file
                            fb.draw_string(results_x + 12, results_y + 36, "Opening:", white, results_bg);
                            let app_name = &cmd_str[5..];
                            fb.draw_string(results_x + 12, results_y + 56, app_name, folk_accent, results_bg);
                            fb.draw_string(results_x + 12, results_y + 80, "(app launcher coming soon)", dark_gray, results_bg);
                        } else if cmd_str == "help" {
                            // Help command
                            fb.draw_string(results_x + 12, results_y + 36, "Available commands:", white, results_bg);
                            fb.draw_string(results_x + 12, results_y + 56, "ls, ps, uptime, help", folk_accent, results_bg);
                            fb.draw_string(results_x + 12, results_y + 80, "find <query>, calc <expr>, open <app>", gray, results_bg);
                        } else {
                            // Default: show help
                            fb.draw_string(results_x + 12, results_y + 36, "Unknown command:", white, results_bg);
                            fb.draw_string(results_x + 12, results_y + 56, cmd_str, gray, results_bg);
                            fb.draw_string(results_x + 12, results_y + 80, "Type 'help' for available commands", dark_gray, results_bg);
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

            // Mark cursor background as dirty - mouse handler will refresh it
            cursor_bg_dirty = true;
        }

        // ===== Process IPC messages (non-blocking) =====
        write_str("I");
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
        write_str("Y");
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
