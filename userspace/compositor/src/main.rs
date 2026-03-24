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

/// ASCII case-insensitive prefix match: does `haystack` start with `needle`?
fn format_arena_line<'a>(buf: &'a mut [u8; 32], kb: usize) -> &'a str {
    let prefix = b"Arena: ";
    let suffix = b"KB";
    buf[..7].copy_from_slice(prefix);
    let mut num_buf = [0u8; 16];
    let num_str = format_usize(kb, &mut num_buf);
    let num_bytes = num_str.as_bytes();
    buf[7..7 + num_bytes.len()].copy_from_slice(num_bytes);
    let end = 7 + num_bytes.len();
    buf[end..end + 2].copy_from_slice(suffix);
    unsafe { core::str::from_utf8_unchecked(&buf[..end + 2]) }
}

fn starts_with_ci(haystack: &str, needle: &str) -> bool {
    if haystack.len() < needle.len() { return false; }
    for (a, b) in haystack.bytes().zip(needle.bytes()) {
        let la = if a >= b'A' && a <= b'Z' { a + 32 } else { a };
        let lb = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
        if la != lb { return false; }
    }
    true
}

struct IntentEntry {
    app: &'static str,
    keywords: &'static [&'static str],
}

const INTENT_MAP: &[IntentEntry] = &[
    IntentEntry {
        app: "calc",
        keywords: &[
            "calc", "calculator", "kalkulator", "math", "matte",
            "regn", "beregn", "compute", "tax", "skatt",
            "add", "subtract", "multiply", "divide", "sum",
            "budget", "prosent", "percent",
        ],
    },
    IntentEntry {
        app: "greet",
        keywords: &[
            "greet", "greeter", "hello", "hei", "hilsen", "name",
        ],
    },
    IntentEntry {
        app: "folkpad",
        keywords: &[
            "note", "folkpad", "pad", "notat", "skriv", "memo",
        ],
    },
];

/// Sjekk om input matcher en apps intent-keywords.
/// Returnerer Some(app_name) ved match, None ellers.
///
/// Scoring:
/// - Eksakt match (case-insensitive): +10 poeng
/// - Prefix-match (word starts with kw, eller omvendt, kw.len()>=3): +kw.len() poeng
/// - Terskel: score >= 4 for å matche
fn try_intent_match(input: &str) -> Option<&'static str> {
    let mut best_app: Option<&'static str> = None;
    let mut best_score: usize = 0;

    for entry in INTENT_MAP {
        let mut score = 0usize;
        for word in input.split(|c: char| !c.is_ascii_alphanumeric()) {
            let w = word.trim();
            if w.is_empty() { continue; }
            for kw in entry.keywords {
                if w.len() == kw.len() && starts_with_ci(w, kw) {
                    // Eksakt match (case-insensitive)
                    score += 10;
                } else if kw.len() >= 3 && (starts_with_ci(w, kw) || starts_with_ci(kw, w)) {
                    // Prefix-match
                    score += kw.len();
                }
            }
        }
        if score > best_score {
            best_score = score;
            best_app = Some(entry.app);
        }
    }

    if best_score >= 4 { best_app } else { None }
}

/// Emit UI state dump to serial as minified JSON between markers.
/// Format: @@UI_DUMP@@{json}@@END_UI_DUMP@@
/// All on one line to avoid kernel log interleaving breaking the JSON.
fn emit_ui_dump(wm: &WindowManager, omnibar_visible: bool, text_buffer: &[u8], text_len: usize, cursor_pos: usize) {
    // Use a stack-allocated buffer for the JSON string (4KB should be plenty)
    let mut buf = [0u8; 4096];
    let mut pos = 0;

    buf_write(&mut buf, &mut pos, "{\"omnibar\":{\"visible\":");
    if omnibar_visible { buf_write(&mut buf, &mut pos, "true"); } else { buf_write(&mut buf, &mut pos, "false"); }
    if omnibar_visible && text_len > 0 {
        buf_write(&mut buf, &mut pos, ",\"text\":\"");
        buf_write_escaped(&mut buf, &mut pos, &text_buffer[..text_len]);
        buf_write(&mut buf, &mut pos, "\",\"cursor\":");
        buf_write_num(&mut buf, &mut pos, cursor_pos as u32);
    }
    buf_write(&mut buf, &mut pos, "},\"windows\":[");

    let mut first_win = true;
    for window in &wm.windows {
        if !window.visible { continue; }
        if !first_win { buf_write(&mut buf, &mut pos, ","); }
        first_win = false;

        buf_write(&mut buf, &mut pos, "{\"id\":");
        buf_write_num(&mut buf, &mut pos, window.id);
        buf_write(&mut buf, &mut pos, ",\"title\":\"");
        if window.title_len > 0 {
            buf_write_escaped(&mut buf, &mut pos, &window.title[..window.title_len]);
        }
        buf_write(&mut buf, &mut pos, "\"");

        // focused?
        if wm.focused_id == Some(window.id) {
            buf_write(&mut buf, &mut pos, ",\"focused\":true");
        }

        // kind
        buf_write(&mut buf, &mut pos, ",\"kind\":\"");
        match window.kind {
            WindowKind::Terminal => buf_write(&mut buf, &mut pos, "terminal"),
            WindowKind::App => buf_write(&mut buf, &mut pos, "app"),
        }
        buf_write(&mut buf, &mut pos, "\"");

        // Interactive terminal input
        if window.interactive && window.input_len > 0 {
            buf_write(&mut buf, &mut pos, ",\"input\":\"");
            buf_write_escaped(&mut buf, &mut pos, &window.input_buf[..window.input_len]);
            buf_write(&mut buf, &mut pos, "\"");
        }

        // Widgets
        if !window.widgets.is_empty() {
            buf_write(&mut buf, &mut pos, ",\"widgets\":[");
            let mut first_w = true;
            emit_widgets(&window.widgets, &mut buf, &mut pos, &mut first_w, window.focused_widget);
            buf_write(&mut buf, &mut pos, "]");
        }

        // Terminal lines (last few)
        if !window.lines.is_empty() {
            buf_write(&mut buf, &mut pos, ",\"lines\":[");
            // Only include last 5 lines to save space
            let start = if window.lines.len() > 5 { window.lines.len() - 5 } else { 0 };
            for (i, line) in window.lines[start..].iter().enumerate() {
                if i > 0 { buf_write(&mut buf, &mut pos, ","); }
                buf_write(&mut buf, &mut pos, "\"");
                let line_len = line.len.min(line.buf.len());
                buf_write_escaped(&mut buf, &mut pos, &line.buf[..line_len]);
                buf_write(&mut buf, &mut pos, "\"");
            }
            buf_write(&mut buf, &mut pos, "]");
        }

        buf_write(&mut buf, &mut pos, "}");
    }

    buf_write(&mut buf, &mut pos, "],\"focused_id\":");
    if let Some(fid) = wm.focused_id {
        buf_write_num(&mut buf, &mut pos, fid);
    } else {
        buf_write(&mut buf, &mut pos, "null");
    }
    buf_write(&mut buf, &mut pos, "}");

    // Write the complete dump atomically (as much as possible via write_str)
    write_str("@@UI_DUMP@@");
    // Write the JSON portion from buf
    if let Ok(json_str) = core::str::from_utf8(&buf[..pos]) {
        write_str(json_str);
    }
    write_str("@@END_UI_DUMP@@\n");
}

/// Write a string into a buffer at the given position, advancing pos
fn buf_write(buf: &mut [u8], pos: &mut usize, s: &str) {
    let bytes = s.as_bytes();
    let end = (*pos + bytes.len()).min(buf.len());
    let copy_len = end - *pos;
    buf[*pos..*pos + copy_len].copy_from_slice(&bytes[..copy_len]);
    *pos += copy_len;
}

/// Write a u32 as decimal into buffer
fn buf_write_num(buf: &mut [u8], pos: &mut usize, n: u32) {
    if n == 0 {
        if *pos < buf.len() { buf[*pos] = b'0'; *pos += 1; }
        return;
    }
    let mut digits = [0u8; 10];
    let mut d = 0usize;
    let mut val = n;
    while val > 0 && d < 10 {
        digits[9 - d] = b'0' + (val % 10) as u8;
        val /= 10;
        d += 1;
    }
    for j in (10 - d)..10 {
        if *pos < buf.len() { buf[*pos] = digits[j]; *pos += 1; }
    }
}

/// Write a byte slice as JSON-escaped string content into buffer
fn buf_write_escaped(buf: &mut [u8], pos: &mut usize, data: &[u8]) {
    for &b in data {
        if b == b'"' || b == b'\\' {
            if *pos < buf.len() { buf[*pos] = b'\\'; *pos += 1; }
        }
        if *pos < buf.len() { buf[*pos] = b; *pos += 1; }
    }
}

/// Recursively emit widget JSON into buffer
fn emit_widgets(widgets: &[UiWidget], buf: &mut [u8], pos: &mut usize, first: &mut bool, focused_idx: Option<usize>) {
    for widget in widgets {
        if !*first { buf_write(buf, pos, ","); }
        *first = false;

        match widget {
            UiWidget::Label { text, text_len, .. } => {
                buf_write(buf, pos, "{\"type\":\"label\",\"text\":\"");
                let len = (*text_len).min(text.len());
                buf_write_escaped(buf, pos, &text[..len]);
                buf_write(buf, pos, "\"}");
            }
            UiWidget::Button { label, label_len, action_id, .. } => {
                buf_write(buf, pos, "{\"type\":\"button\",\"label\":\"");
                let len = (*label_len).min(label.len());
                buf_write_escaped(buf, pos, &label[..len]);
                buf_write(buf, pos, "\",\"action_id\":");
                buf_write_num(buf, pos, *action_id);
                buf_write(buf, pos, "}");
            }
            UiWidget::TextInput { placeholder, placeholder_len, value, value_len, cursor_pos, action_id, .. } => {
                buf_write(buf, pos, "{\"type\":\"textinput\",\"placeholder\":\"");
                let plen = (*placeholder_len).min(placeholder.len());
                buf_write_escaped(buf, pos, &placeholder[..plen]);
                buf_write(buf, pos, "\",\"value\":\"");
                let vlen = (*value_len).min(value.len());
                buf_write_escaped(buf, pos, &value[..vlen]);
                buf_write(buf, pos, "\",\"cursor\":");
                buf_write_num(buf, pos, *cursor_pos as u32);
                buf_write(buf, pos, ",\"action_id\":");
                buf_write_num(buf, pos, *action_id);
                buf_write(buf, pos, "}");
            }
            UiWidget::VStack { children, spacing } => {
                buf_write(buf, pos, "{\"type\":\"vstack\",\"spacing\":");
                buf_write_num(buf, pos, *spacing as u32);
                buf_write(buf, pos, ",\"children\":[");
                let mut child_first = true;
                emit_widgets(children, buf, pos, &mut child_first, focused_idx);
                buf_write(buf, pos, "]}");
            }
            UiWidget::HStack { children, spacing } => {
                buf_write(buf, pos, "{\"type\":\"hstack\",\"spacing\":");
                buf_write_num(buf, pos, *spacing as u32);
                buf_write(buf, pos, ",\"children\":[");
                let mut child_first = true;
                emit_widgets(children, buf, pos, &mut child_first, focused_idx);
                buf_write(buf, pos, "]}");
            }
            UiWidget::Spacer { height } => {
                buf_write(buf, pos, "{\"type\":\"spacer\",\"height\":");
                buf_write_num(buf, pos, *height as u32);
                buf_write(buf, pos, "}");
            }
        }
    }
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
const MSG_CREATE_UI_WINDOW: u64 = 0x06;
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

    // Alt+Tab HUD state
    let mut hud_title: [u8; 32] = [0; 32];
    let mut hud_title_len: usize = 0;
    let mut hud_show_until: u64 = 0;  // uptime ms when HUD should disappear

    // ===== Clipboard buffer (Milestone 20) =====
    let mut clipboard_buf: [u8; 256] = [0; 256];
    let mut clipboard_len: usize = 0;

    // ===== Async Inference / Token Streaming State =====
    let mut inference_ring_handle: u32 = 0;     // shmem handle for TokenRing (0 = no active stream)
    let mut inference_ring_read_idx: usize = 0;  // bytes already read from ring
    let mut inference_win_id: u32 = 0;           // window receiving streamed tokens
    let mut inference_query_handle: u32 = 0;     // query shmem handle (for cleanup)

    // ===== Tool Calling State Machine =====
    // Detects <|tool|>...<|/tool|> in AI stream, hides from display, executes via IPC
    let mut tool_state: u8 = 0;      // 0=scanning open, 1=buffering body, 3=completed
    let mut tool_open_match: usize = 0;
    let mut tool_close_match: usize = 0;
    let mut tool_buf: [u8; 512] = [0; 512];
    let mut tool_buf_len: usize = 0;
    let mut tool_pending: [u8; 9] = [0; 9]; // max tag length for flush on partial match fail
    let mut tool_pending_len: usize = 0;

    const TOOL_OPEN: &[u8] = b"<|tool|>";    // 8 bytes
    const TOOL_CLOSE: &[u8] = b"<|/tool|>";  // 9 bytes

    // ===== Think Tag Filter =====
    // Hides <think>...</think> reasoning from display (Qwen3 thinking mode)
    let mut think_state: u8 = 0;     // 0=scanning open, 1=inside think block (hidden)
    let mut think_open_match: usize = 0;
    let mut think_close_match: usize = 0;
    let mut think_pending: [u8; 8] = [0; 8]; // max tag length for flush
    let mut think_pending_len: usize = 0;

    const THINK_OPEN: &[u8] = b"<think>";    // 7 bytes
    const THINK_CLOSE: &[u8] = b"</think>";  // 8 bytes

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

    loop {
        // Track if we did any work this iteration
        let mut did_work = false;
        // Consolidated redraw flag — any subsystem can set this
        let mut need_redraw = false;

        // Check if Alt+Tab HUD has expired — clear HUD area and trigger redraw
        if hud_show_until > 0 && uptime() >= hud_show_until {
            // Clear the HUD area before resetting state
            let old_hud_w = hud_title_len * 8 + 24;
            let old_hud_x = (fb.width.saturating_sub(old_hud_w)) / 2;
            let old_hud_y = fb.height.saturating_sub(40);
            fb.fill_rect(old_hud_x, old_hud_y, old_hud_w, 24, folk_dark);
            hud_show_until = 0;
            hud_title_len = 0;
            need_redraw = true;
        }

        // ===== Process mouse input =====
        // Accumulate all pending mouse events, then draw cursor ONCE
        let mut accumulated_dx: i32 = 0;
        let mut accumulated_dy: i32 = 0;
        let mut latest_buttons: u8 = last_buttons;
        let mut had_mouse_events = false;

        while let Some(event) = read_mouse() {
            did_work = true;
            had_mouse_events = true;
            accumulated_dx += event.dx as i32;
            accumulated_dy -= event.dy as i32; // Invert Y (mouse up = negative dy in PS/2)
            latest_buttons = event.buttons;
        }

        if had_mouse_events {
            // Sanity check cursor position
            if cursor_x < 0 || cursor_x >= fb.width as i32 || cursor_y < 0 || cursor_y >= fb.height as i32 {
                cursor_x = (fb.width / 2) as i32;
                cursor_y = (fb.height / 2) as i32;
                cursor_bg_dirty = true;
                cursor_drawn = false;
            }

            // Determine cursor color based on button state
            let cursor_fill = match (latest_buttons & 1 != 0, latest_buttons & 2 != 0) {
                (true, true) => cursor_magenta,
                (true, false) => cursor_red,
                (false, true) => cursor_blue,
                (false, false) => cursor_white,
            };

            // First mouse event ever: draw cursor at center
            if !cursor_drawn {
                fb.save_rect(cursor_x as usize, cursor_y as usize, CURSOR_W, CURSOR_H, &mut cursor_bg.0);
                fb.draw_cursor(cursor_x as usize, cursor_y as usize, cursor_fill, cursor_outline);
                cursor_drawn = true;
                last_buttons = latest_buttons;
            }

            // Calculate new position from accumulated delta
            let new_x = cursor_x.saturating_add(accumulated_dx);
            let new_y = cursor_y.saturating_add(accumulated_dy);

            // Clamp to screen bounds
            let new_x = if new_x < 0 { 0 } else if new_x >= fb.width as i32 { fb.width as i32 - 1 } else { new_x };
            let new_y = if new_y < 0 { 0 } else if new_y >= fb.height as i32 { fb.height as i32 - 1 } else { new_y };

            // ===== Milestone 1.4 + 2.2: Mouse Click Hit-Testing + Window Dragging =====
            let left_now = latest_buttons & 1 != 0;
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
                            // Prevent token stream from hijacking a recycled window ID
                            if win_id == inference_win_id {
                                inference_win_id = 0;
                            }
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
                            // Check if App window widget was clicked
                            let mut btn_info: Option<(u32, u32)> = None; // (action_id, owner)
                            let mut focus_click = false;
                            // Determine what was clicked: Button → IPC, TextInput → focus only
                            if let Some(win) = wm.get_window(win_id) {
                                if matches!(win.kind, compositor::window_manager::WindowKind::App)
                                    && !win.widgets.is_empty()
                                {
                                    let content_x = win.x as usize + BORDER_W + 6;
                                    let content_y = win.y as usize + BORDER_W + TITLE_BAR_H + 6;
                                    let owner = win.owner_task;

                                    match compositor::window_manager::hit_test_widgets(
                                        &win.widgets, content_x, content_y, cx as usize, cy as usize
                                    ) {
                                        Some(compositor::window_manager::FocusableKind::Button { action_id }) => {
                                            btn_info = Some((action_id, owner));
                                        }
                                        Some(compositor::window_manager::FocusableKind::TextInput { .. }) => {
                                            focus_click = true;
                                        }
                                        None => {}
                                    }
                                    // Set focus index via hit_test_focusable_index
                                    if focus_click || btn_info.is_some() {
                                        // We'll set focus below after releasing borrow
                                    }
                                }
                            }
                            // Set focused_widget for click on any focusable
                            if focus_click || btn_info.is_some() {
                                if let Some(win) = wm.get_window(win_id) {
                                    let content_x = win.x as usize + BORDER_W + 6;
                                    let content_y = win.y as usize + BORDER_W + TITLE_BAR_H + 6;
                                    let idx = compositor::window_manager::hit_test_focusable_index(
                                        &win.widgets, content_x, content_y, cx as usize, cy as usize
                                    );
                                    if let Some(win) = wm.get_window_mut(win_id) {
                                        win.focused_widget = idx;
                                    }
                                }
                            } else {
                                // Click on non-focusable area clears focus
                                if let Some(win) = wm.get_window_mut(win_id) {
                                    win.focused_widget = None;
                                }
                            }
                            // Send button IPC outside of borrow
                            if let Some((action_id, owner)) = btn_info {
                                if owner != 0 {
                                    let event_payload = 0xAC10_u64
                                        | ((action_id as u64) << 16)
                                        | ((win_id as u64) << 48);
                                    let reply = unsafe {
                                        libfolk::syscall::syscall3(
                                            libfolk::syscall::SYS_IPC_SEND,
                                            owner as u64,
                                            event_payload,
                                            0
                                        )
                                    };
                                    let reply_magic = (reply >> 48) as u16;
                                    if reply_magic == 0x5549 {
                                        let ui_handle = (reply & 0xFFFFFFFF) as u32;
                                        update_window_widgets(&mut wm, win_id, ui_handle);
                                    }
                                }
                            }
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

            // Redraw cursor if it moved, button state changed, or background is dirty
            if new_x != cursor_x || new_y != cursor_y || latest_buttons != last_buttons || cursor_bg_dirty {
                // Erase old cursor by restoring saved background
                if !cursor_bg_dirty {
                    fb.restore_rect(cursor_x as usize, cursor_y as usize, CURSOR_W, CURSOR_H, &cursor_bg.0);
                }
                cursor_bg_dirty = false;

                // Update position
                cursor_x = new_x;
                cursor_y = new_y;
                last_buttons = latest_buttons;

                // Save background at new position, then draw cursor on top
                fb.save_rect(cursor_x as usize, cursor_y as usize, CURSOR_W, CURSOR_H, &mut cursor_bg.0);
                fb.draw_cursor(cursor_x as usize, cursor_y as usize, cursor_fill, cursor_outline);
            }
        } // end if had_mouse_events


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
            const KEY_SHIFT_TAB: u8 = 0x87;
            const KEY_ALT_TAB: u8 = 0x88;
            const KEY_CTRL_F12: u8 = 0x89;
            const KEY_CTRL_C: u8 = 0x8A;
            const KEY_CTRL_V: u8 = 0x8B;

            // Ctrl+F12: UI state dump to serial (for MCP automation)
            if key == KEY_CTRL_F12 {
                emit_ui_dump(&wm, omnibar_visible, &text_buffer, text_len, cursor_pos);
                continue;
            }

            // ===== Ctrl+C: Copy to clipboard =====
            if key == KEY_CTRL_C {
                let mut copied = false;

                if !omnibar_visible {
                    if let Some(focused_id) = wm.focused_id {
                        let win_is_interactive = wm.get_window(focused_id).map(|w| w.interactive).unwrap_or(false);
                        let win_is_app = wm.get_window(focused_id)
                            .map(|w| matches!(w.kind, compositor::window_manager::WindowKind::App) && !w.widgets.is_empty())
                            .unwrap_or(false);

                        if win_is_app {
                            if let Some(win) = wm.get_window(focused_id) {
                                // Priority 1 & 2: Focused TextInput or Button
                                if let Some(idx) = win.focused_widget {
                                    if let Some((buf, len)) = compositor::window_manager::nth_focusable_text(&win.widgets, idx) {
                                        if len > 0 {
                                            let copy_len = len.min(256);
                                            clipboard_buf[..copy_len].copy_from_slice(&buf[..copy_len]);
                                            clipboard_len = copy_len;
                                            copied = true;
                                        }
                                    }
                                }
                                // Priority 3: First Label (e.g. Calc display)
                                if !copied {
                                    if let Some((buf, len)) = compositor::window_manager::first_label_text(&win.widgets) {
                                        if len > 0 {
                                            let copy_len = len.min(256);
                                            clipboard_buf[..copy_len].copy_from_slice(&buf[..copy_len]);
                                            clipboard_len = copy_len;
                                            copied = true;
                                        }
                                    }
                                }
                            }
                        } else if win_is_interactive {
                            // Priority 4: Terminal input_buf
                            if let Some(win) = wm.get_window(focused_id) {
                                if win.input_len > 0 {
                                    let copy_len = win.input_len.min(256);
                                    clipboard_buf[..copy_len].copy_from_slice(&win.input_buf[..copy_len]);
                                    clipboard_len = copy_len;
                                    copied = true;
                                }
                            }
                        }
                    }
                }

                // Priority 5: Omnibar text
                if !copied && omnibar_visible && text_len > 0 {
                    let copy_len = text_len.min(256);
                    clipboard_buf[..copy_len].copy_from_slice(&text_buffer[..copy_len]);
                    clipboard_len = copy_len;
                    copied = true;
                }

                if copied {
                    // Show HUD confirmation
                    hud_title = [0u8; 32];
                    let prefix = b"Copied: ";
                    hud_title[..prefix.len()].copy_from_slice(prefix);
                    let show_len = clipboard_len.min(32 - prefix.len());
                    hud_title[prefix.len()..prefix.len() + show_len].copy_from_slice(&clipboard_buf[..show_len]);
                    hud_title_len = prefix.len() + show_len;
                    hud_show_until = uptime() + 1000;
                    need_redraw = true;
                }
                continue;
            }

            // ===== Ctrl+V: Paste from clipboard =====
            if key == KEY_CTRL_V && clipboard_len > 0 {
                let mut pasted = false;

                if !omnibar_visible {
                    if let Some(focused_id) = wm.focused_id {
                        let win_is_interactive = wm.get_window(focused_id).map(|w| w.interactive).unwrap_or(false);
                        let win_is_app = wm.get_window(focused_id)
                            .map(|w| matches!(w.kind, compositor::window_manager::WindowKind::App) && !w.widgets.is_empty())
                            .unwrap_or(false);

                        if win_is_app {
                            // Priority 1: Focused TextInput
                            let focused_idx = wm.get_window(focused_id).and_then(|w| w.focused_widget);
                            let is_text_input = if let Some(idx) = focused_idx {
                                wm.get_window(focused_id)
                                    .and_then(|w| compositor::window_manager::nth_focusable(&w.widgets, idx))
                                    .map(|k| matches!(k, compositor::window_manager::FocusableKind::TextInput { .. }))
                                    .unwrap_or(false)
                            } else { false };

                            if is_text_input {
                                if let Some(idx) = focused_idx {
                                    if let Some(win) = wm.get_window_mut(focused_id) {
                                        if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                            if let compositor::window_manager::UiWidget::TextInput { value, value_len, cursor_pos, max_len, .. } = w {
                                                let available = (*max_len as usize).saturating_sub(*value_len);
                                                let paste_len = clipboard_len.min(available);
                                                if paste_len > 0 {
                                                    value.copy_within(*cursor_pos..*value_len, *cursor_pos + paste_len);
                                                    value[*cursor_pos..*cursor_pos + paste_len].copy_from_slice(&clipboard_buf[..paste_len]);
                                                    *value_len += paste_len;
                                                    *cursor_pos += paste_len;
                                                    need_redraw = true;
                                                    pasted = true;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        } else if win_is_interactive {
                            // Priority 2: Terminal input_buf
                            if let Some(win) = wm.get_window_mut(focused_id) {
                                let available = 126usize.saturating_sub(win.input_len);
                                let paste_len = clipboard_len.min(available);
                                if paste_len > 0 {
                                    // Shift existing text right
                                    let mut i = win.input_len;
                                    while i > win.input_cursor {
                                        win.input_buf[i + paste_len - 1] = win.input_buf[i - 1];
                                        i -= 1;
                                    }
                                    win.input_buf[win.input_cursor..win.input_cursor + paste_len].copy_from_slice(&clipboard_buf[..paste_len]);
                                    win.input_len += paste_len;
                                    win.input_cursor += paste_len;
                                    need_redraw = true;
                                    pasted = true;
                                }
                            }
                        }
                    }
                }

                // Priority 3: Omnibar
                if !pasted && omnibar_visible {
                    let available = (MAX_TEXT_LEN - 1).saturating_sub(text_len);
                    let paste_len = clipboard_len.min(available);
                    if paste_len > 0 {
                        // Shift existing text right using copy_within
                        text_buffer.copy_within(cursor_pos..text_len, cursor_pos + paste_len);
                        text_buffer[cursor_pos..cursor_pos + paste_len].copy_from_slice(&clipboard_buf[..paste_len]);
                        text_len += paste_len;
                        cursor_pos += paste_len;
                        need_redraw = true;
                    }
                }
                continue;
            }

            // Alt+Tab: cycle window focus (highest priority, before all other routing)
            if key == KEY_ALT_TAB {
                if let Some((title, tlen)) = wm.cycle_next_window() {
                    hud_title = title;
                    hud_title_len = tlen;
                    hud_show_until = uptime() + 1000;
                }
                omnibar_visible = false;
                need_redraw = true;
                continue;
            }

            // Route keys to focused interactive window when omnibar is hidden
            if !omnibar_visible {
                let mut key_consumed = false;
                if let Some(focused_id) = wm.focused_id {
                    // Check window type first with immutable borrow
                    let win_is_interactive = wm.get_window(focused_id).map(|w| w.interactive).unwrap_or(false);
                    let win_is_app_with_widgets = wm.get_window(focused_id)
                        .map(|w| matches!(w.kind, compositor::window_manager::WindowKind::App) && !w.widgets.is_empty())
                        .unwrap_or(false);

                    if win_is_interactive {
                        if let Some(win) = wm.get_window_mut(focused_id) {
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
                            key_consumed = true;
                        }
                    } else if win_is_app_with_widgets {
                        // App window keyboard navigation (Tab/Shift+Tab/Enter/Space/Text editing)
                        let mut activate_info: Option<(u32, u32, u32)> = None; // (action_id, owner, win_id)
                        let mut text_submit_info: Option<(u32, u32, u32, [u8; 64], usize)> = None; // (action_id, owner, win_id, text, len)

                        // Determine current focused widget kind
                        let focused_kind = if let Some(win) = wm.get_window(focused_id) {
                            win.focused_widget.and_then(|idx| compositor::window_manager::nth_focusable(&win.widgets, idx))
                        } else { None };

                        if let Some(win) = wm.get_window_mut(focused_id) {
                            match key {
                                b'\t' => {
                                    let fc = compositor::window_manager::count_focusable(&win.widgets);
                                    if fc > 0 {
                                        let cur = win.focused_widget.unwrap_or(fc.wrapping_sub(1));
                                        win.focused_widget = Some((cur + 1) % fc);
                                        need_redraw = true;
                                    }
                                    key_consumed = true;
                                }
                                KEY_SHIFT_TAB => {
                                    let fc = compositor::window_manager::count_focusable(&win.widgets);
                                    if fc > 0 {
                                        let cur = win.focused_widget.unwrap_or(1);
                                        win.focused_widget = Some(cur.checked_sub(1).unwrap_or(fc - 1));
                                        need_redraw = true;
                                    }
                                    key_consumed = true;
                                }
                                b'\n' | b'\r' => {
                                    if let Some(idx) = win.focused_widget {
                                        match focused_kind {
                                            Some(compositor::window_manager::FocusableKind::Button { action_id }) => {
                                                activate_info = Some((action_id, win.owner_task, win.id));
                                            }
                                            Some(compositor::window_manager::FocusableKind::TextInput { action_id }) => {
                                                // Grab text from the widget for IPC submit
                                                if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                                    if let compositor::window_manager::UiWidget::TextInput { value, value_len, .. } = w {
                                                        let mut buf = [0u8; 64];
                                                        let len = *value_len;
                                                        buf[..len].copy_from_slice(&value[..len]);
                                                        text_submit_info = Some((action_id, win.owner_task, win.id, buf, len));
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                    key_consumed = true;
                                }
                                b' ' => {
                                    match focused_kind {
                                        Some(compositor::window_manager::FocusableKind::TextInput { .. }) => {
                                            // Type space into TextInput
                                            if let Some(idx) = win.focused_widget {
                                                if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                                    if let compositor::window_manager::UiWidget::TextInput { value, value_len, cursor_pos, max_len, .. } = w {
                                                        if *value_len < (*max_len as usize) {
                                                            // Shift right and insert
                                                            let mut i = *value_len;
                                                            while i > *cursor_pos { value[i] = value[i - 1]; i -= 1; }
                                                            value[*cursor_pos] = b' ';
                                                            *value_len += 1;
                                                            *cursor_pos += 1;
                                                            need_redraw = true;
                                                        }
                                                    }
                                                }
                                            }
                                            key_consumed = true;
                                        }
                                        Some(compositor::window_manager::FocusableKind::Button { action_id }) => {
                                            activate_info = Some((action_id, win.owner_task, win.id));
                                            key_consumed = true;
                                        }
                                        _ => { key_consumed = true; }
                                    }
                                }
                                0x08 | 0x7F => {
                                    // Backspace — only for TextInput
                                    if matches!(focused_kind, Some(compositor::window_manager::FocusableKind::TextInput { .. })) {
                                        if let Some(idx) = win.focused_widget {
                                            if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                                if let compositor::window_manager::UiWidget::TextInput { value, value_len, cursor_pos, .. } = w {
                                                    if *cursor_pos > 0 {
                                                        let mut i = *cursor_pos - 1;
                                                        while i < *value_len - 1 { value[i] = value[i + 1]; i += 1; }
                                                        *value_len -= 1;
                                                        value[*value_len] = 0;
                                                        *cursor_pos -= 1;
                                                        need_redraw = true;
                                                    }
                                                }
                                            }
                                        }
                                        key_consumed = true;
                                    }
                                }
                                KEY_ARROW_LEFT => {
                                    if matches!(focused_kind, Some(compositor::window_manager::FocusableKind::TextInput { .. })) {
                                        if let Some(idx) = win.focused_widget {
                                            if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                                if let compositor::window_manager::UiWidget::TextInput { cursor_pos, .. } = w {
                                                    if *cursor_pos > 0 { *cursor_pos -= 1; need_redraw = true; }
                                                }
                                            }
                                        }
                                        key_consumed = true;
                                    }
                                }
                                KEY_ARROW_RIGHT => {
                                    if matches!(focused_kind, Some(compositor::window_manager::FocusableKind::TextInput { .. })) {
                                        if let Some(idx) = win.focused_widget {
                                            if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                                if let compositor::window_manager::UiWidget::TextInput { value_len, cursor_pos, .. } = w {
                                                    if *cursor_pos < *value_len { *cursor_pos += 1; need_redraw = true; }
                                                }
                                            }
                                        }
                                        key_consumed = true;
                                    }
                                }
                                0x21..=0x7E => {
                                    // Printable chars — type into TextInput if focused
                                    if matches!(focused_kind, Some(compositor::window_manager::FocusableKind::TextInput { .. })) {
                                        if let Some(idx) = win.focused_widget {
                                            if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                                if let compositor::window_manager::UiWidget::TextInput { value, value_len, cursor_pos, max_len, .. } = w {
                                                    if *value_len < (*max_len as usize) {
                                                        let mut i = *value_len;
                                                        while i > *cursor_pos { value[i] = value[i - 1]; i -= 1; }
                                                        value[*cursor_pos] = key;
                                                        *value_len += 1;
                                                        *cursor_pos += 1;
                                                        need_redraw = true;
                                                    }
                                                }
                                            }
                                        }
                                        key_consumed = true;
                                    }
                                }
                                0x1B => {
                                    omnibar_visible = true;
                                    need_redraw = true;
                                    key_consumed = true;
                                }
                                _ => {}
                            }
                        }
                        // Send button activation IPC outside of borrow
                        if let Some((action_id, owner, win_id)) = activate_info {
                            if owner != 0 {
                                let event_payload = 0xAC10_u64
                                    | ((action_id as u64) << 16)
                                    | ((win_id as u64) << 48);
                                let reply = unsafe {
                                    libfolk::syscall::syscall3(
                                        libfolk::syscall::SYS_IPC_SEND,
                                        owner as u64,
                                        event_payload,
                                        0
                                    )
                                };
                                let reply_magic = (reply >> 48) as u16;
                                if reply_magic == 0x5549 {
                                    let ui_handle = (reply & 0xFFFFFFFF) as u32;
                                    update_window_widgets(&mut wm, win_id, ui_handle);
                                    clamp_focus(&mut wm, win_id);
                                }
                                need_redraw = true;
                            }
                        }
                        // Send text submit IPC (0xAC11) outside of borrow
                        if let Some((action_id, owner, win_id, text_buf, text_len)) = text_submit_info {
                            if owner != 0 && text_len > 0 {
                                if let Ok(handle) = shmem_create(text_len + 2) {
                                    let _ = shmem_grant(handle, owner);
                                    if shmem_map(handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                        let dst = unsafe {
                                            core::slice::from_raw_parts_mut(COMPOSITOR_SHMEM_VADDR as *mut u8, text_len + 2)
                                        };
                                        dst[0..2].copy_from_slice(&(text_len as u16).to_le_bytes());
                                        dst[2..2+text_len].copy_from_slice(&text_buf[..text_len]);
                                        let _ = shmem_unmap(handle, COMPOSITOR_SHMEM_VADDR);
                                    }
                                    let payload = 0xAC11_u64
                                        | ((action_id as u64) << 16)
                                        | ((handle as u64) << 32)
                                        | ((win_id as u64) << 48);
                                    let reply = unsafe {
                                        libfolk::syscall::syscall3(
                                            libfolk::syscall::SYS_IPC_SEND,
                                            owner as u64,
                                            payload,
                                            0
                                        )
                                    };
                                    let _ = shmem_destroy(handle);
                                    let reply_magic = (reply >> 48) as u16;
                                    if reply_magic == 0x5549 {
                                        let ui_handle = (reply & 0xFFFFFFFF) as u32;
                                        update_window_widgets(&mut wm, win_id, ui_handle);
                                        clamp_focus(&mut wm, win_id);
                                    }
                                    need_redraw = true;
                                }
                            }
                        }
                    }
                }
                if key_consumed {
                    continue;
                }
                // No interactive/app window focused — Escape reopens omnibar
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
        // Track deferred app creation from omnibar (no terminal window needed)
        let mut deferred_app_handle: u32 = 0;

        if execute_command && text_len > 0 {
            if let Ok(cmd_str) = core::str::from_utf8(&text_buffer[..text_len]) {

                // Special case: `open <app>` creates app window directly (no terminal)
                let is_open_cmd = cmd_str.starts_with("open ");
                if is_open_cmd {
                    let app_name = cmd_str[5..].trim();
                    if !app_name.is_empty() {
                        // Build filename: "calc" → "calc.fkui"
                        let mut fname = [0u8; 64];
                        let nb = app_name.as_bytes();
                        let ext = b".fkui";
                        let mut vfs_loaded = false;
                        if nb.len() + ext.len() < 64 {
                            fname[..nb.len()].copy_from_slice(nb);
                            fname[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                            let fname_str = unsafe { core::str::from_utf8_unchecked(&fname[..nb.len()+ext.len()]) };

                            // Try VFS first (Synapse read_file_shmem)
                            match libfolk::sys::synapse::read_file_shmem(fname_str) {
                                Ok(resp) => {
                                    deferred_app_handle = resp.shmem_handle;
                                    vfs_loaded = true;
                                    write_str("[WM] App loaded from VFS: ");
                                    write_str(fname_str);
                                    write_str("\n");
                                }
                                Err(_) => {}
                            }
                        }

                        // Fallback: Shell IPC (M10 path)
                        if !vfs_loaded {
                            let name_hash = shell_hash_name(app_name) as u64;
                            let shell_payload = SHELL_OP_OPEN_APP | (name_hash << 8);
                            let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                                | (shell_payload << 8);
                            let ipc_result = unsafe {
                                libfolk::syscall::syscall3(
                                    libfolk::syscall::SYS_IPC_SEND,
                                    libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                                )
                            };
                            let magic = (ipc_result >> 48) as u16;
                            if magic == 0x5549 {
                                deferred_app_handle = (ipc_result & 0xFFFFFFFF) as u32;
                                write_str("[WM] App launch via Shell fallback\n");
                            } else {
                                write_str("[WM] Unknown app\n");
                            }
                        }
                    }
                }

                if !is_open_cmd {
                // M13: Try semantic intent match BEFORE creating terminal window
                if let Some(app_name) = try_intent_match(cmd_str) {
                    let mut fname = [0u8; 64];
                    let nb = app_name.as_bytes();
                    let ext = b".fkui";
                    if nb.len() + ext.len() < 64 {
                        fname[..nb.len()].copy_from_slice(nb);
                        fname[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                        let fname_str = unsafe {
                            core::str::from_utf8_unchecked(&fname[..nb.len()+ext.len()])
                        };
                        if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(fname_str) {
                            deferred_app_handle = resp.shmem_handle;
                            write_str("[WM] Intent match: ");
                            write_str(app_name);
                            write_str("\n");
                        }
                    }
                }

                if deferred_app_handle == 0 {
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
                    } else if cmd_str.starts_with("save ") {
                        // VFS write: save <filename> <content>
                        let args = &cmd_str[5..];
                        let mut parts = args.splitn(2, ' ');
                        if let (Some(filename), Some(content)) = (parts.next(), parts.next()) {
                            match libfolk::sys::synapse::write_file(filename, content.as_bytes()) {
                                Ok(()) => {
                                    win.push_line("Saved to SQLite!");
                                    // Show filename and size
                                    let mut line = [0u8; 64];
                                    let prefix = b"  ";
                                    line[0..2].copy_from_slice(prefix);
                                    let nlen = filename.len().min(30);
                                    line[2..2+nlen].copy_from_slice(&filename.as_bytes()[..nlen]);
                                    let suffix = b" written";
                                    let slen = suffix.len();
                                    line[2+nlen..2+nlen+slen].copy_from_slice(suffix);
                                    if let Ok(s) = core::str::from_utf8(&line[..2+nlen+slen]) {
                                        win.push_line(s);
                                    }
                                }
                                Err(_) => {
                                    win.push_line("Save failed!");
                                }
                            }
                        } else {
                            win.push_line("Usage: save <filename> <text>");
                        }
                    } else if cmd_str == "poweroff" || cmd_str == "shutdown" {
                        // M12: Save app states and shut down
                        let name_hash = shell_hash_name("poweroff") as u64;
                        let shell_payload = SHELL_OP_EXEC | (name_hash << 8);
                        let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                            | (shell_payload << 8);
                        let _ = unsafe {
                            libfolk::syscall::syscall3(
                                libfolk::syscall::SYS_IPC_SEND,
                                libfolk::sys::intent::INTENT_TASK_ID as u64,
                                intent_req, 0
                            )
                        };
                        win.push_line("Shutting down...");
                    } else if cmd_str == "ai-status" {
                        // Query inference server status
                        use libfolk::sys::inference;
                        match inference::status() {
                            Ok((has_model, arena_size)) => {
                                if has_model {
                                    win.push_line("AI: model loaded");
                                } else {
                                    win.push_line("AI: stub mode (no model)");
                                }
                                let kb = arena_size / 1024;
                                let mut buf = [0u8; 32];
                                let s = format_arena_line(&mut buf, kb);
                                win.push_line(s);
                            }
                            Err(_) => {
                                win.push_line("AI: server unavailable");
                            }
                        }
                    } else if cmd_str.starts_with("ask ") || cmd_str.starts_with("infer ") {
                        // AI inference command — async streaming via TokenRing
                        use libfolk::sys::inference;
                        let query = if cmd_str.starts_with("ask ") {
                            &cmd_str[4..]
                        } else {
                            &cmd_str[6..]
                        };
                        let query = query.trim();
                        if query.is_empty() {
                            win.push_line("Usage: ask <question>");
                        } else if inference_ring_handle != 0 {
                            // ULTRA 42: Already generating
                            win.push_line("[AI is busy]");
                        } else {
                            match inference::ping() {
                                Ok(has_model) => {
                                    if !has_model {
                                        win.push_line("[AI] No model loaded (stub mode)");
                                    } else {
                                        // Create TokenRing shmem (4 pages = 16KB)
                                        let ring_ok = if let Ok(rh) = shmem_create(16384) {
                                            let _ = shmem_grant(rh, inference::inference_task_id());
                                            // Create query shmem
                                            let query_bytes = query.as_bytes();
                                            if let Ok(qh) = shmem_create(4096) {
                                                let _ = shmem_grant(qh, inference::inference_task_id());
                                                if shmem_map(qh, ASK_QUERY_VADDR).is_ok() {
                                                    unsafe {
                                                        let ptr = ASK_QUERY_VADDR as *mut u8;
                                                        core::ptr::copy_nonoverlapping(
                                                            query_bytes.as_ptr(), ptr, query_bytes.len()
                                                        );
                                                    }
                                                    let _ = shmem_unmap(qh, ASK_QUERY_VADDR);

                                                    // Send async request
                                                    match inference::ask_async(qh, query_bytes.len(), rh) {
                                                        Ok(()) => {
                                                            // Map ring for polling
                                                            if shmem_map(rh, RING_VADDR).is_ok() {
                                                                inference_ring_handle = rh;
                                                                inference_ring_read_idx = 0;
                                                                inference_win_id = win_id;
                                                                inference_query_handle = qh;
                                                                win.push_line("[AI] Thinking...");
                                                                win.typing = true;
                                                                true
                                                            } else {
                                                                let _ = shmem_destroy(rh);
                                                                let _ = shmem_destroy(qh);
                                                                win.push_line("[AI] Ring map failed");
                                                                false
                                                            }
                                                        }
                                                        Err(_) => {
                                                            let _ = shmem_destroy(rh);
                                                            let _ = shmem_destroy(qh);
                                                            win.push_line("[AI] Server offline — AI Core may need restart");
                                                            false
                                                        }
                                                    }
                                                } else {
                                                    let _ = shmem_destroy(rh);
                                                    let _ = shmem_destroy(qh);
                                                    win.push_line("[AI] Query map failed");
                                                    false
                                                }
                                            } else {
                                                let _ = shmem_destroy(rh);
                                                win.push_line("[AI] Query alloc failed");
                                                false
                                            }
                                        } else {
                                            win.push_line("[AI] Ring alloc failed");
                                            false
                                        };
                                        let _ = ring_ok; // suppress unused warning
                                    }
                                }
                                Err(_) => {
                                    win.push_line("[AI] Inference server unavailable (may have crashed)");
                                    win.push_line("[AI] Try again — server may auto-recover on next request");
                                }
                            }
                        }
                    } else {
                        win.push_line("Sent to shell...");
                    }
                    if !win.interactive {
                        win.push_line("---");
                    }
                }
                } // end if deferred_app_handle == 0
                } // end if !is_open_cmd

                // Clear the omnibar input after executing
                text_len = 0;
                cursor_pos = 0;
                for i in 0..MAX_TEXT_LEN { text_buffer[i] = 0; }
                show_results = false;
                cursor_bg_dirty = true;
            }
        }

        // ===== Deferred app creation from omnibar `open` command =====
        if deferred_app_handle != 0 {
            if shmem_map(deferred_app_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
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
                        app_win.owner_task = SHELL_TASK_ID;
                        let (root, _) = parse_widget_tree(header.widget_data);
                        if let Some(widget) = root {
                            app_win.widgets.push(widget);
                        }
                    }
                    write_str("[WM] Created app: ");
                    write_str(header.title);
                    write_str("\n");
                    need_redraw = true;
                }
                let _ = shmem_unmap(deferred_app_handle, COMPOSITOR_SHMEM_VADDR);
            }
            let _ = shmem_destroy(deferred_app_handle);
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
                    } else if cmd_str.starts_with("find ") || cmd_str.starts_with("search ") {
                        let query = if cmd_str.starts_with("find ") {
                            cmd_str[5..].trim()
                        } else {
                            cmd_str[7..].trim()
                        };
                        if query.is_empty() {
                            win.push_line("usage: find <query>");
                        } else {
                            // Create shmem with query string
                            let qb = query.as_bytes();
                            let qlen = qb.len().min(63);
                            if let Ok(qh) = shmem_create(64) {
                                for tid in 2..=8 { let _ = shmem_grant(qh, tid); }
                                if shmem_map(qh, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                    let buf = unsafe {
                                        core::slice::from_raw_parts_mut(COMPOSITOR_SHMEM_VADDR as *mut u8, 64)
                                    };
                                    buf[..qlen].copy_from_slice(&qb[..qlen]);
                                    buf[qlen] = 0;
                                    let _ = shmem_unmap(qh, COMPOSITOR_SHMEM_VADDR);
                                }
                                let shell_req = SHELL_OP_SEARCH
                                    | ((qh as u64) << 8)
                                    | ((qlen as u64) << 40);
                                let ipc_result = unsafe {
                                    libfolk::syscall::syscall3(
                                        libfolk::syscall::SYS_IPC_SEND,
                                        SHELL_TASK_ID as u64, shell_req, 0
                                    )
                                };
                                let _ = shmem_destroy(qh);

                                if ipc_result != u64::MAX && ipc_result != 0 {
                                    let count = (ipc_result >> 32) as usize;
                                    let rh = (ipc_result & 0xFFFFFFFF) as u32;
                                    if rh != 0 && count > 0 {
                                        let mut mb = [0u8; 24];
                                        let prefix = b"Matches: ";
                                        mb[..prefix.len()].copy_from_slice(prefix);
                                        let mut nb = [0u8; 16];
                                        let ns = format_usize(count, &mut nb);
                                        let nl = ns.len();
                                        mb[prefix.len()..prefix.len()+nl].copy_from_slice(ns.as_bytes());
                                        if let Ok(s) = core::str::from_utf8(&mb[..prefix.len()+nl]) {
                                            win.push_line(s);
                                        }
                                        if shmem_map(rh, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                            let buf = unsafe {
                                                core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, count * 32)
                                            };
                                            for i in 0..count.min(10) {
                                                let off = i * 32;
                                                let ne = buf[off..off+24].iter()
                                                    .position(|&b| b == 0).unwrap_or(24);
                                                let name = unsafe {
                                                    core::str::from_utf8_unchecked(&buf[off..off+ne])
                                                };
                                                let size = u32::from_le_bytes([
                                                    buf[off+24], buf[off+25], buf[off+26], buf[off+27]
                                                ]);
                                                let mut line = [0u8; 48];
                                                line[0] = b' '; line[1] = b' ';
                                                let nnl = name.len().min(24);
                                                line[2..2+nnl].copy_from_slice(&name.as_bytes()[..nnl]);
                                                let mut sb = [0u8; 16];
                                                let ss = format_usize(size as usize, &mut sb);
                                                let sl = ss.len();
                                                line[3+nnl..3+nnl+sl].copy_from_slice(ss.as_bytes());
                                                if let Ok(s) = core::str::from_utf8(&line[..3+nnl+sl]) {
                                                    win.push_line(s);
                                                }
                                            }
                                            let _ = shmem_unmap(rh, COMPOSITOR_SHMEM_VADDR);
                                            let _ = shmem_destroy(rh);
                                        }
                                    } else {
                                        win.push_line("No matches");
                                    }
                                } else {
                                    win.push_line("No matches");
                                }
                            }
                        }
                    } else if cmd_str.starts_with("open ") {
                        // Open app — try VFS first, fallback to Shell
                        let app_name = cmd_str[5..].trim();
                        if app_name.is_empty() {
                            win.push_line("usage: open <app>");
                        } else {
                            let mut vfs_ok = false;
                            // Build filename: "calc" → "calc.fkui"
                            let mut fname = [0u8; 64];
                            let nb = app_name.as_bytes();
                            let ext = b".fkui";
                            if nb.len() + ext.len() < 64 {
                                fname[..nb.len()].copy_from_slice(nb);
                                fname[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                                let fname_str = unsafe { core::str::from_utf8_unchecked(&fname[..nb.len()+ext.len()]) };

                                if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(fname_str) {
                                    win.push_line("App loaded from VFS!");
                                    win.input_buf[0..4].copy_from_slice(&resp.shmem_handle.to_le_bytes());
                                    win.input_buf[4] = 0xAA; // marker
                                    vfs_ok = true;
                                }
                            }

                            if !vfs_ok {
                                // Fallback: Shell IPC
                                let name_hash = shell_hash_name(app_name) as u64;
                                let shell_payload = SHELL_OP_OPEN_APP | (name_hash << 8);
                                let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                                    | (shell_payload << 8);
                                let ipc_result = unsafe {
                                    libfolk::syscall::syscall3(
                                        libfolk::syscall::SYS_IPC_SEND,
                                        libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                                    )
                                };
                                let magic = (ipc_result >> 48) as u16;
                                if magic == 0x5549 {
                                    let ui_handle = (ipc_result & 0xFFFFFFFF) as u32;
                                    win.push_line("App opened via Shell!");
                                    win.input_buf[0..4].copy_from_slice(&ui_handle.to_le_bytes());
                                    win.input_buf[4] = 0xAA; // marker
                                } else {
                                    win.push_line("Unknown app. Try: open calc");
                                }
                            }
                        }
                    } else if cmd_str == "ui_test" || cmd_str == "app" {
                        // Route to Shell via Intent Service — Shell builds UI shmem
                        let name_hash = shell_hash_name("ui_test") as u64;
                        let shell_payload = SHELL_OP_EXEC | (name_hash << 8);
                        let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                            | (shell_payload << 8);
                        let ipc_result = unsafe {
                            libfolk::syscall::syscall3(
                                libfolk::syscall::SYS_IPC_SEND,
                                libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                            )
                        };
                        // Check for UI shmem response: magic 0x5549 in upper 16 bits
                        let magic = (ipc_result >> 48) as u16;
                        if magic == 0x5549 {
                            let ui_handle = (ipc_result & 0xFFFFFFFF) as u32;
                            win.push_line("App received from Shell!");
                            // Signal: create UI window after win borrow released
                            // Store handle in input_buf temporarily
                            win.input_buf[0..4].copy_from_slice(&ui_handle.to_le_bytes());
                            win.input_buf[4] = 0xAA; // marker
                        } else {
                            win.push_line("App launch failed");
                        }
                    } else if cmd_str == "poweroff" || cmd_str == "shutdown" {
                        // M12: Route poweroff to Shell via Intent Service
                        // Shell will save app states before shutting down
                        let name_hash = shell_hash_name("poweroff") as u64;
                        let shell_payload = SHELL_OP_EXEC | (name_hash << 8);
                        let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                            | (shell_payload << 8);
                        let _ = unsafe {
                            libfolk::syscall::syscall3(
                                libfolk::syscall::SYS_IPC_SEND,
                                libfolk::sys::intent::INTENT_TASK_ID as u64,
                                intent_req, 0
                            )
                        };
                        win.push_line("Shutting down...");
                    } else if cmd_str == "help" {
                        win.push_line("ls ps cat find uptime open app poweroff help");
                    } else if let Some(app_name) = try_intent_match(cmd_str) {
                        // M13: Semantic intent match — open app from terminal
                        let mut fname = [0u8; 64];
                        let nb = app_name.as_bytes();
                        let ext = b".fkui";
                        if nb.len() + ext.len() < 64 {
                            fname[..nb.len()].copy_from_slice(nb);
                            fname[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                            let fname_str = unsafe {
                                core::str::from_utf8_unchecked(&fname[..nb.len()+ext.len()])
                            };
                            if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(fname_str) {
                                win.input_buf[0..4].copy_from_slice(&resp.shmem_handle.to_le_bytes());
                                win.input_buf[4] = 0xAA; // marker
                                win.push_line("Intent match: opening ");
                                win.push_line(app_name);
                            } else {
                                win.push_line("Intent matched but app not found");
                            }
                        }
                    } else {
                        win.push_line("Unknown command. Try: help");
                    }
                }
            }
        }

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

        // Only redraw once after processing all keys
        if need_redraw {
            if omnibar_visible {
                // ===== Draw Glass Omnibar (alpha-blended) =====
                let omnibar_alpha: u8 = 180; // 70% opaque — scene bleeds through

                // Outer glow (subtle, semi-transparent)
                fb.fill_rect_alpha(text_box_x.saturating_sub(2), text_box_y.saturating_sub(2), text_box_w + 4, text_box_h + 4, 0x333333, omnibar_alpha / 2);
                // Main glass box
                fb.fill_rect_alpha(text_box_x, text_box_y, text_box_w, text_box_h, 0x1a1a2e, omnibar_alpha);
                fb.draw_rect(text_box_x, text_box_y, text_box_w, text_box_h, omnibar_border);

                // Draw user input text (single line for omnibar)
                // Text foreground is opaque, background is transparent (alpha-blended)
                if text_len > 0 {
                    if let Ok(_input_str) = core::str::from_utf8(&text_buffer[..text_len]) {
                        // Truncate if too long
                        let display_len = if text_len > chars_per_line { chars_per_line } else { text_len };
                        if let Ok(display_str) = core::str::from_utf8(&text_buffer[..display_len]) {
                            fb.draw_string_alpha(text_box_x + TEXT_PADDING, text_box_y + 12, display_str, white, 0x1a1a2e, omnibar_alpha);
                        }
                    }
                } else {
                    // Show placeholder when empty
                    fb.draw_string_alpha(text_box_x + TEXT_PADDING, text_box_y + 12, "Ask anything...", gray, 0x1a1a2e, omnibar_alpha);
                }

                // Draw blinking text caret at cursor position
                let caret_x_pos = text_box_x + TEXT_PADDING + (cursor_pos.min(chars_per_line) * 8);
                if caret_x_pos < text_box_x + text_box_w - 30 {
                    let caret_char = if caret_visible { "|" } else { " " };
                    fb.draw_string_alpha(caret_x_pos, text_box_y + 10, caret_char, folk_accent, 0x1a1a2e, omnibar_alpha);
                }

                // Draw ">" icon on right
                fb.draw_string_alpha(text_box_x + text_box_w - 24, text_box_y + 12, ">", folk_accent, 0x1a1a2e, omnibar_alpha);

                // Context hints below omnibar
                let hint = "find <query> | open calc | help";
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
                            fb.draw_string(results_x + 12, results_y + 36, "Open app:", white, results_bg);
                            let app_name = &cmd_str[5..];
                            fb.draw_string(results_x + 12, results_y + 56, app_name, folk_accent, results_bg);
                            fb.draw_string(results_x + 12, results_y + 80, "Press Enter to launch", dark_gray, results_bg);
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

            // ===== Alt+Tab HUD overlay =====
            if hud_show_until > 0 && hud_title_len > 0 {
                let hud_text = unsafe { core::str::from_utf8_unchecked(&hud_title[..hud_title_len]) };
                let hud_w = hud_title_len * 8 + 24;
                let hud_x = (fb.width.saturating_sub(hud_w)) / 2;
                let hud_y = fb.height.saturating_sub(40);
                fb.fill_rect_alpha(hud_x, hud_y, hud_w, 24, 0x1a1a2e, 200);
                fb.draw_rect(hud_x, hud_y, hud_w, 24, folk_accent);
                fb.draw_string(hud_x + 12, hud_y + 8, hud_text, white, folk_dark);
            }

            // After full redraw: re-save cursor background and redraw cursor on top
            // This ensures cursor is always the topmost element
            if cursor_drawn {
                let cursor_fill = match (last_buttons & 1 != 0, last_buttons & 2 != 0) {
                    (true, true) => cursor_magenta,
                    (true, false) => cursor_red,
                    (false, true) => cursor_blue,
                    (false, false) => cursor_white,
                };
                fb.save_rect(cursor_x as usize, cursor_y as usize, CURSOR_W, CURSOR_H, &mut cursor_bg.0);
                fb.draw_cursor(cursor_x as usize, cursor_y as usize, cursor_fill, cursor_outline);
                cursor_bg_dirty = false;
            }
        }

        // ===== Process IPC messages (non-blocking) =====
        match recv_async() {
            Ok(msg) => {
                did_work = true;
                let opcode = msg.payload0 & 0xFF;

                if opcode == MSG_CREATE_UI_WINDOW {
                    // Create UI window from shmem widget description
                    let shmem_handle = ((msg.payload0 >> 8) & 0xFFFFFFFF) as u32;
                    let mut response = u64::MAX;

                    if shmem_handle != 0 {
                        if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                            // Read shmem to get UI description size (max 4KB)
                            let buf = unsafe {
                                core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, 4096)
                            };

                            if let Some(header) = libfolk::ui::parse_header(buf) {
                                // Create App window
                                let win_count = wm.windows.len() as i32;
                                let wx = 100 + win_count * 30;
                                let wy = 80 + win_count * 30;
                                let win_id = wm.create_terminal(
                                    header.title,
                                    wx, wy,
                                    header.width as u32,
                                    header.height as u32,
                                );

                                if let Some(win) = wm.get_window_mut(win_id) {
                                    win.kind = compositor::window_manager::WindowKind::App;
                                    win.owner_task = msg.sender;

                                    // Parse widget tree recursively
                                    let (root_widget, _) = parse_widget_tree(header.widget_data);
                                    if let Some(widget) = root_widget {
                                        win.widgets.push(widget);
                                    }
                                }

                                write_str("[WM] Created UI window: ");
                                write_str(header.title);
                                write_str("\n");
                                response = win_id as u64;
                            }

                            let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
                            let _ = shmem_destroy(shmem_handle);
                        }
                    }
                    let _ = reply_with_token(msg.token, response, 0);
                    need_redraw = true;
                } else {
                    let response = handle_message(&mut compositor, msg.payload0);
                    let _ = reply_with_token(msg.token, response, 0);
                }
            }
            Err(IpcError::WouldBlock) => {}
            Err(_) => {}
        }

        // ===== Token Streaming: Poll TokenRing (ULTRA 37, 38, 46, 47) =====
        if inference_ring_handle != 0 {
            use core::sync::atomic::Ordering;
            // Read ring header atomically
            let ring_ptr = RING_VADDR as *const u32;
            let write_idx_atomic = unsafe { &*(ring_ptr as *const core::sync::atomic::AtomicU32) };
            let status_atomic = unsafe { &*((ring_ptr as *const core::sync::atomic::AtomicU32).add(1)) };

            let new_write = write_idx_atomic.load(Ordering::Acquire) as usize;
            if new_write > inference_ring_read_idx {
                did_work = true;
                // ULTRA 38: Batch-read ALL new bytes at once
                let data_ptr = unsafe { (RING_VADDR as *const u8).add(RING_HEADER_SIZE) };
                let new_data = unsafe {
                    core::slice::from_raw_parts(
                        data_ptr.add(inference_ring_read_idx),
                        new_write - inference_ring_read_idx,
                    )
                };
                // ULTRA 47: Data guaranteed valid UTF-8 by inference server
                // Tool call interception: scan for <|tool|>...<|/tool|> tags
                let mut visible_buf: [u8; 512] = [0; 512];
                let mut vis_len: usize = 0;

                for &byte in new_data.iter() {
                    // ── Layer 1: Think tag filter ──
                    // Intercepts <think>...</think> blocks and drops them entirely.
                    // Bytes inside a think block never reach the tool/visible layer.
                    if think_state == 0 {
                        // Scanning for THINK_OPEN
                        if byte == THINK_OPEN[think_open_match] {
                            think_pending[think_pending_len] = byte;
                            think_pending_len += 1;
                            think_open_match += 1;
                            if think_open_match == THINK_OPEN.len() {
                                // Entered think block — drop all content
                                think_state = 1;
                                think_open_match = 0;
                                think_pending_len = 0;
                            }
                            continue; // Don't pass to tool/visible layer yet
                        } else if think_open_match > 0 {
                            // Partial match failed — flush pending to tool/visible layer below
                            // (fall through with pending bytes + current byte)
                            // We need to process each pending byte through tool layer
                            let pending_count = think_pending_len;
                            think_open_match = 0;
                            think_pending_len = 0;
                            // Process each pending byte through tool/visible layer
                            for j in 0..pending_count {
                                let pb = think_pending[j];
                                // (inline the tool/visible logic for flushed bytes)
                                match tool_state {
                                    0 => {
                                        if pb == TOOL_OPEN[tool_open_match] {
                                            tool_pending[tool_pending_len] = pb;
                                            tool_pending_len += 1;
                                            tool_open_match += 1;
                                            if tool_open_match == TOOL_OPEN.len() {
                                                tool_state = 1; tool_open_match = 0;
                                                tool_pending_len = 0; tool_buf_len = 0;
                                            }
                                        } else if tool_open_match > 0 {
                                            for k in 0..tool_pending_len {
                                                if vis_len < visible_buf.len() { visible_buf[vis_len] = tool_pending[k]; vis_len += 1; }
                                            }
                                            tool_open_match = 0; tool_pending_len = 0;
                                            if vis_len < visible_buf.len() { visible_buf[vis_len] = pb; vis_len += 1; }
                                        } else if vis_len < visible_buf.len() { visible_buf[vis_len] = pb; vis_len += 1; }
                                    }
                                    1 => {
                                        if pb == TOOL_CLOSE[tool_close_match] {
                                            tool_close_match += 1;
                                            if tool_close_match == TOOL_CLOSE.len() { tool_state = 3; tool_close_match = 0; }
                                        } else {
                                            for k in 0..tool_close_match { if tool_buf_len < tool_buf.len() { tool_buf[tool_buf_len] = TOOL_CLOSE[k]; tool_buf_len += 1; } }
                                            tool_close_match = 0;
                                            if tool_buf_len < tool_buf.len() { tool_buf[tool_buf_len] = pb; tool_buf_len += 1; }
                                        }
                                    }
                                    _ => { if vis_len < visible_buf.len() { visible_buf[vis_len] = pb; vis_len += 1; } }
                                }
                            }
                            // Now fall through to process current byte normally
                        }
                        // else: no partial match, byte falls through to tool/visible
                    } else {
                        // think_state == 1: Inside <think> block — scan for </think>
                        if byte == THINK_CLOSE[think_close_match] {
                            think_close_match += 1;
                            if think_close_match == THINK_CLOSE.len() {
                                // Exited think block — resume normal display
                                think_state = 0;
                                think_close_match = 0;
                            }
                        } else {
                            think_close_match = 0;
                        }
                        continue; // Drop ALL bytes inside think block
                    }

                    // ── Layer 2: Tool tag filter + visible output ──
                    match tool_state {
                        0 => {
                            // Scanning for TOOL_OPEN tag
                            if byte == TOOL_OPEN[tool_open_match] {
                                tool_pending[tool_pending_len] = byte;
                                tool_pending_len += 1;
                                tool_open_match += 1;
                                if tool_open_match == TOOL_OPEN.len() {
                                    tool_state = 1;
                                    tool_open_match = 0;
                                    tool_pending_len = 0;
                                    tool_buf_len = 0;
                                }
                            } else if tool_open_match > 0 {
                                for j in 0..tool_pending_len {
                                    if vis_len < visible_buf.len() {
                                        visible_buf[vis_len] = tool_pending[j];
                                        vis_len += 1;
                                    }
                                }
                                tool_open_match = 0;
                                tool_pending_len = 0;
                                if vis_len < visible_buf.len() {
                                    visible_buf[vis_len] = byte;
                                    vis_len += 1;
                                }
                            } else {
                                if vis_len < visible_buf.len() {
                                    visible_buf[vis_len] = byte;
                                    vis_len += 1;
                                }
                            }
                        }
                        1 => {
                            // Buffering tool body, scanning for TOOL_CLOSE
                            if byte == TOOL_CLOSE[tool_close_match] {
                                tool_close_match += 1;
                                if tool_close_match == TOOL_CLOSE.len() {
                                    tool_state = 3;
                                    tool_close_match = 0;
                                }
                            } else {
                                for j in 0..tool_close_match {
                                    if tool_buf_len < tool_buf.len() {
                                        tool_buf[tool_buf_len] = TOOL_CLOSE[j];
                                        tool_buf_len += 1;
                                    }
                                }
                                tool_close_match = 0;
                                if tool_buf_len < tool_buf.len() {
                                    tool_buf[tool_buf_len] = byte;
                                    tool_buf_len += 1;
                                }
                            }
                        }
                        _ => {
                            if vis_len < visible_buf.len() {
                                visible_buf[vis_len] = byte;
                                vis_len += 1;
                            }
                        }
                    }
                }

                // Append visible (non-tool) text to window
                if vis_len > 0 {
                    if let Some(win) = wm.get_window_mut(inference_win_id) {
                        win.append_text(&visible_buf[..vis_len]);
                    }
                }

                // Execute completed tool call
                if tool_state == 3 {
                    let tool_content = core::str::from_utf8(&tool_buf[..tool_buf_len]).unwrap_or("");
                    let result = execute_tool_call(tool_content);
                    if let Some(win) = wm.get_window_mut(inference_win_id) {
                        win.push_line(result);
                    }
                    tool_state = 0;
                    tool_buf_len = 0;
                    need_redraw = true;
                }
                inference_ring_read_idx = new_write;
                need_redraw = true;
            }

            let status = status_atomic.load(Ordering::Acquire);
            if status != 0 {
                // DONE (1) or ERROR (2) — cleanup
                did_work = true;
                let _ = shmem_unmap(inference_ring_handle, RING_VADDR);
                let _ = shmem_destroy(inference_ring_handle);
                let _ = shmem_destroy(inference_query_handle);
                inference_ring_handle = 0;
                inference_query_handle = 0;
                // Flush incomplete tool tag if generation ended mid-tag
                if tool_state != 0 {
                    tool_state = 0;
                    tool_open_match = 0;
                    tool_close_match = 0;
                    tool_buf_len = 0;
                    tool_pending_len = 0;
                }
                if let Some(win) = wm.get_window_mut(inference_win_id) {
                    win.typing = false;
                    win.push_line(""); // new line after AI response
                    if status == 2 {
                        win.push_line("[AI] Error during generation");
                    }
                }
                need_redraw = true;
            }
        }

        // Only yield CPU if we did no work this iteration (ULTRA 46)
        if !did_work {
            yield_cpu();
        }
    }
}

/// Clamp focused_widget index after widget tree update
/// Execute a tool call parsed from the AI stream.
/// Called by the compositor when <|tool|>COMMAND args<|/tool|> is detected.
fn execute_tool_call(tool_content: &str) -> &'static str {
    let trimmed = tool_content.trim();
    let (cmd, args) = if let Some(pos) = trimmed.find(' ') {
        (&trimmed[..pos], trimmed[pos + 1..].trim())
    } else {
        (trimmed, "")
    };

    match cmd {
        "write" => {
            if let Some(pos) = args.find(' ') {
                let filename = args[..pos].trim();
                let content = args[pos + 1..].trim();
                if filename.is_empty() || content.is_empty() {
                    return "[Tool: write requires FILENAME CONTENT]";
                }
                // Security: no path traversal, max 4KB
                if filename.contains("..") || content.len() > 4096 {
                    return "[Tool: write denied (security)]";
                }
                match libfolk::sys::synapse::write_file(filename, content.as_bytes()) {
                    Ok(()) => "[Tool: File written]",
                    Err(_) => "[Tool: Write failed]",
                }
            } else {
                "[Tool: write requires FILENAME CONTENT]"
            }
        }
        "read" => {
            if args.is_empty() {
                return "[Tool: read requires FILENAME]";
            }
            match libfolk::sys::synapse::read_file_shmem(args) {
                Ok(_resp) => "[Tool: File read]",
                Err(_) => "[Tool: File not found]",
            }
        }
        "ls" => {
            match libfolk::sys::shell::list_files() {
                Ok(_) => "[Tool: Files listed]",
                Err(_) => "[Tool: List failed]",
            }
        }
        _ => "[Tool: Unknown command]",
    }
}

fn clamp_focus(wm: &mut WindowManager, win_id: u32) {
    if let Some(win) = wm.get_window_mut(win_id) {
        if let Some(idx) = win.focused_widget {
            let fc = compositor::window_manager::count_focusable(&win.widgets);
            if fc == 0 {
                win.focused_widget = None;
            } else if idx >= fc {
                win.focused_widget = Some(fc - 1);
            }
        }
    }
}

/// Update a window's widget tree from a shmem UI buffer.
/// Maps the shmem, parses the FKUI header and widget tree,
/// replaces the window's widgets in-place, then cleans up shmem.
fn update_window_widgets(wm: &mut WindowManager, win_id: u32, shmem_handle: u32) {
    if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
        let buf = unsafe {
            core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, 4096)
        };
        if let Some(header) = libfolk::ui::parse_header(buf) {
            let (root, _) = parse_widget_tree(header.widget_data);
            if let Some(widget) = root {
                if let Some(win) = wm.get_window_mut(win_id) {
                    // clear() + push reuses Vec capacity — no new allocation in bump allocator
                    win.widgets.clear();
                    win.widgets.push(widget);
                }
            }
        }
        let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
    }
    let _ = shmem_destroy(shmem_handle);
}

/// Recursively parse widget tree from wire format into UiWidget
fn parse_widget_tree(data: &[u8]) -> (Option<compositor::window_manager::UiWidget>, usize) {
    use compositor::window_manager::UiWidget;
    use libfolk::ui::{parse_widget, ParsedWidget as PW};

    match parse_widget(data) {
        Some((PW::Label { text, color }, consumed)) => {
            (Some(UiWidget::label(text, color)), consumed)
        }
        Some((PW::Button { label, action_id, bg, fg }, consumed)) => {
            (Some(UiWidget::button(label, action_id, bg, fg)), consumed)
        }
        Some((PW::Spacer { height }, consumed)) => {
            (Some(UiWidget::Spacer { height }), consumed)
        }
        Some((PW::TextInput { placeholder, action_id, max_len }, consumed)) => {
            (Some(UiWidget::text_input(placeholder, action_id, max_len)), consumed)
        }
        Some((PW::VStackBegin { spacing, child_count }, mut consumed)) => {
            let mut children = alloc::vec::Vec::new();
            for _ in 0..child_count {
                let (child, child_consumed) = parse_widget_tree(&data[consumed..]);
                if let Some(c) = child {
                    children.push(c);
                }
                consumed += child_consumed;
            }
            (Some(UiWidget::VStack { children, spacing }), consumed)
        }
        Some((PW::HStackBegin { spacing, child_count }, mut consumed)) => {
            let mut children = alloc::vec::Vec::new();
            for _ in 0..child_count {
                let (child, child_consumed) = parse_widget_tree(&data[consumed..]);
                if let Some(c) = child {
                    children.push(c);
                }
                consumed += child_consumed;
            }
            (Some(UiWidget::HStack { children, spacing }), consumed)
        }
        _ => (None, 0),
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
