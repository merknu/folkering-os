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
use libfolk::sys::ipc::{receive, reply, recv_async, reply_with_token, IpcError};
use libfolk::sys::boot_info::{get_boot_info, FramebufferConfig, BOOT_INFO_VADDR};
use libfolk::sys::map_physical::{map_framebuffer, MapFlags};
use libfolk::sys::{yield_cpu, read_mouse, read_key};
use libfolk::sys::io::{write_char, write_str};
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

    // ===== FIRST LIGHT TEST =====
    // Fill screen with Folkering blue as proof of life
    let folk_blue = fb.color_from_rgb24(colors::FOLK_BLUE);
    let folk_dark = fb.color_from_rgb24(colors::FOLK_DARK);
    let white = fb.color_from_rgb24(colors::WHITE);
    let folk_accent = fb.color_from_rgb24(colors::FOLK_ACCENT);

    // Clear to dark background
    fb.clear(folk_dark);

    // Draw a centered rectangle
    let rect_w = 400;
    let rect_h = 200;
    let rect_x = (fb.width.saturating_sub(rect_w)) / 2;
    let rect_y = (fb.height.saturating_sub(rect_h)) / 2;
    fb.fill_rect(rect_x, rect_y, rect_w, rect_h, folk_blue);

    // Draw border
    fb.draw_rect(rect_x, rect_y, rect_w, rect_h, folk_accent);

    // Draw "First Light" text
    let title = "Folkering OS - First Light";
    let title_x = rect_x + (rect_w.saturating_sub(title.len() * 8)) / 2;
    let title_y = rect_y + 40;
    fb.draw_string(title_x, title_y, title, white, folk_blue);

    let subtitle = "Phase 6.2 Complete";
    let sub_x = rect_x + (rect_w.saturating_sub(subtitle.len() * 8)) / 2;
    let sub_y = rect_y + 80;
    fb.draw_string(sub_x, sub_y, subtitle, folk_accent, folk_blue);

    let info = "Semantic Mirror Active";
    let info_x = rect_x + (rect_w.saturating_sub(info.len() * 8)) / 2;
    let info_y = rect_y + 120;
    fb.draw_string(info_x, info_y, info, white, folk_blue);

    write_str("[COMPOSITOR] *** FIRST LIGHT ***\n");

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

    // ===== Phase 7.1: Keyboard input display =====
    // Text input box configuration - positioned below the First Light box
    let text_box_x: usize = rect_x;  // Same X as First Light box
    let text_box_y: usize = rect_y + rect_h + 30;  // 30px below First Light box
    let text_box_w: usize = rect_w;  // Same width as First Light box
    let text_box_h: usize = 80;
    const TEXT_PADDING: usize = 8;
    const MAX_TEXT_LEN: usize = 256;
    let chars_per_line: usize = (text_box_w - TEXT_PADDING * 2) / 8;

    // Text buffer for typed input - use static to avoid stack corruption issues
    static mut TEXT_BUFFER: [u8; 256] = [0; 256];
    static mut TEXT_LEN: usize = 0;

    // Local references for easier use
    let text_buffer = unsafe { &mut TEXT_BUFFER };
    let text_len_ptr = unsafe { &mut TEXT_LEN };

    // Draw text input box
    let text_box_bg = fb.color_from_rgb24(0x1a1a2e);  // Darker background
    let text_box_border = folk_accent;
    fb.fill_rect(text_box_x, text_box_y, text_box_w, text_box_h, text_box_bg);
    fb.draw_rect(text_box_x, text_box_y, text_box_w, text_box_h, text_box_border);

    // Draw label above box
    fb.draw_string(text_box_x, text_box_y.saturating_sub(20), "Keyboard Test - Type here:", white, folk_dark);

    // Draw cursor indicator
    fb.draw_string(text_box_x + TEXT_PADDING, text_box_y + TEXT_PADDING, "_", folk_accent, text_box_bg);

    write_str("[COMPOSITOR] Keyboard ready\n");

    loop {
        // Track if we did any work this iteration
        let mut did_work = false;

        // ===== Process mouse input =====
        while let Some(event) = read_mouse() {
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
        let mut need_redraw = false;
        while let Some(key) = read_key() {
            did_work = true;

            match key {
                // Backspace - delete last character
                0x08 | 0x7F => {
                    if *text_len_ptr > 0 {
                        *text_len_ptr -= 1;
                        text_buffer[*text_len_ptr] = 0;
                        need_redraw = true;
                    }
                }
                // Enter - clear the buffer
                b'\n' | b'\r' => {
                    *text_len_ptr = 0;
                    for i in 0..MAX_TEXT_LEN {
                        text_buffer[i] = 0;
                    }
                    need_redraw = true;
                }
                // Escape - clear buffer
                0x1B => {
                    *text_len_ptr = 0;
                    for i in 0..MAX_TEXT_LEN {
                        text_buffer[i] = 0;
                    }
                    need_redraw = true;
                }
                // Printable ASCII - add to buffer
                0x20..=0x7E => {
                    if *text_len_ptr < MAX_TEXT_LEN - 1 {
                        text_buffer[*text_len_ptr] = key;
                        *text_len_ptr += 1;
                        need_redraw = true;
                    }
                }
                // Ignore other keys
                _ => {}
            }
        }

        // Only redraw once after processing all keys
        if need_redraw {
            // Clear text area
            fb.fill_rect(
                text_box_x + 2,
                text_box_y + 2,
                text_box_w - 4,
                text_box_h - 4,
                text_box_bg
            );

            // Calculate max lines that fit in the box (16px per line)
            let max_lines = (text_box_h - TEXT_PADDING * 2) / 16;

            // Draw text with line wrapping
            if *text_len_ptr > 0 {
                let mut char_idx = 0;
                let mut line = 0;

                while char_idx < *text_len_ptr && line < max_lines {
                    // Calculate how many chars to draw on this line
                    let remaining = *text_len_ptr - char_idx;
                    let line_chars = if remaining > chars_per_line { chars_per_line } else { remaining };

                    // Draw this line
                    if let Ok(line_str) = core::str::from_utf8(&text_buffer[char_idx..char_idx + line_chars]) {
                        let line_y = text_box_y + TEXT_PADDING + (line * 16);
                        fb.draw_string(text_box_x + TEXT_PADDING, line_y, line_str, white, text_box_bg);
                    }

                    char_idx += line_chars;
                    line += 1;
                }
            }

            // Draw text cursor (blinking underscore)
            // Calculate cursor position with wrapping
            let cursor_line = *text_len_ptr / chars_per_line;
            let cursor_col = *text_len_ptr % chars_per_line;
            let max_lines = (text_box_h - TEXT_PADDING * 2) / 16;

            if cursor_line < max_lines {
                let cursor_x = text_box_x + TEXT_PADDING + (cursor_col * 8);
                let cursor_y = text_box_y + TEXT_PADDING + (cursor_line * 16);
                fb.draw_string(cursor_x, cursor_y, "_", folk_accent, text_box_bg);
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
