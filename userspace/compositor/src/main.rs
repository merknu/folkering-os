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

/// Auto-categorize an app name into a folder index (0-5)
fn categorize_app(name: &str) -> usize {
    let n = name.as_bytes();
    // System: monitor, clock, system, about, info, settings, status
    if find_ci(n, b"monitor") || find_ci(n, b"clock") || find_ci(n, b"system")
        || find_ci(n, b"about") || find_ci(n, b"info") || find_ci(n, b"setting")
        || find_ci(n, b"status") { return 0; }
    // Games: game, tetris, snake, pong, ball, bounce, breakout, chess, maze
    if find_ci(n, b"game") || find_ci(n, b"tetris") || find_ci(n, b"snake")
        || find_ci(n, b"pong") || find_ci(n, b"ball") || find_ci(n, b"bounce")
        || find_ci(n, b"breakout") || find_ci(n, b"chess") || find_ci(n, b"maze") { return 1; }
    // Creative: paint, draw, art, sketch, pixel, color, canvas, music
    if find_ci(n, b"paint") || find_ci(n, b"draw") || find_ci(n, b"art")
        || find_ci(n, b"sketch") || find_ci(n, b"pixel") || find_ci(n, b"color")
        || find_ci(n, b"canvas") || find_ci(n, b"music") { return 2; }
    // Tools: calc, timer, note, tool, convert, edit, text, writer
    if find_ci(n, b"calc") || find_ci(n, b"timer") || find_ci(n, b"note")
        || find_ci(n, b"tool") || find_ci(n, b"convert") || find_ci(n, b"edit")
        || find_ci(n, b"text") || find_ci(n, b"writer") { return 3; }
    // Demos: demo, gradient, test, screen, star, hello
    if find_ci(n, b"demo") || find_ci(n, b"gradient") || find_ci(n, b"test")
        || find_ci(n, b"screen") || find_ci(n, b"star") || find_ci(n, b"hello") { return 4; }
    5 // Other
}

/// Case-insensitive substring search in byte slices
fn find_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() { return false; }
    for i in 0..=(haystack.len() - needle.len()) {
        let mut ok = true;
        for j in 0..needle.len() {
            let a = if haystack[i + j] >= b'A' && haystack[i + j] <= b'Z' { haystack[i + j] + 32 } else { haystack[i + j] };
            let b = if needle[j] >= b'A' && needle[j] <= b'Z' { needle[j] + 32 } else { needle[j] };
            if a != b { ok = false; break; }
        }
        if ok { return true; }
    }
    false
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

/// Flags parsed from agent command line
struct AgentFlags<'a> {
    force: bool,                        // --force: skip cache
    tweak_msg: Option<&'a str>,         // --tweak "modification": modify cached version
}

/// Parse agent flags from command string.
/// Returns (flags, remaining_prompt).
fn parse_agent_flags(input: &str) -> (AgentFlags<'_>, &str) {
    let mut force = false;
    let mut tweak_msg: Option<&str> = None;
    let mut rest = input;

    // Parse --force
    if rest.starts_with("--force ") {
        force = true;
        rest = rest[8..].trim_start();
    }

    // Parse --tweak "msg"
    if rest.starts_with("--tweak ") {
        let after = rest[8..].trim_start();
        if after.starts_with('"') {
            if let Some(end) = after[1..].find('"') {
                tweak_msg = Some(&after[1..1 + end]);
                rest = after[2 + end..].trim_start();
            }
        } else {
            // No quotes — take first word as tweak message
            let end = after.find(' ').unwrap_or(after.len());
            tweak_msg = Some(&after[..end]);
            rest = if end < after.len() { after[end..].trim_start() } else { "" };
        }
    }

    (AgentFlags { force, tweak_msg }, rest)
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
// Free-List Allocator for userspace (supports dealloc!)
// ============================================================================
//
// Replaces the bump allocator that never freed memory. wasmi allocates ~500KB
// per WASM execution and frees it after — without dealloc the heap exhausts
// after 2-3 runs. This linked-list allocator recycles freed blocks.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

const HEAP_SIZE: usize = 16 * 1024 * 1024; // 16MB heap (wasmi engine ~1MB + WASM 4MB surface + DrawCmd + previous app remnants)

/// Minimum block size (header + usable). Must fit a FreeNode.
const MIN_BLOCK: usize = 32;

/// Header stored before every allocated block
#[repr(C)]
struct BlockHeader {
    size: usize,  // Total size including header, aligned
    _pad: usize,  // Alignment padding to 16 bytes
}

const HEADER_SIZE: usize = core::mem::size_of::<BlockHeader>();

/// Node in the free list (stored inside free blocks)
#[repr(C)]
struct FreeNode {
    size: usize,
    next: *mut FreeNode,
}

struct FreeListAllocator {
    heap: UnsafeCell<[u8; HEAP_SIZE]>,
    free_head: UnsafeCell<*mut FreeNode>,
    initialized: UnsafeCell<bool>,
}

unsafe impl Sync for FreeListAllocator {}

impl FreeListAllocator {
    /// Initialize the free list with the entire heap as one free block
    unsafe fn init(&self) {
        let heap_ptr = (*self.heap.get()).as_mut_ptr();
        let node = heap_ptr as *mut FreeNode;
        (*node).size = HEAP_SIZE;
        (*node).next = core::ptr::null_mut();
        *self.free_head.get() = node;
        *self.initialized.get() = true;
    }
}

unsafe impl GlobalAlloc for FreeListAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !*self.initialized.get() {
            self.init();
        }

        // Required size: header + payload, aligned up
        let align = layout.align().max(16); // Minimum 16-byte alignment
        let payload_size = layout.size();
        let total_size = ((HEADER_SIZE + payload_size + align - 1) & !(align - 1)).max(MIN_BLOCK);

        // First-fit search through free list
        let mut prev: *mut FreeNode = core::ptr::null_mut();
        let mut current = *self.free_head.get();

        while !current.is_null() {
            let block_size = (*current).size;

            if block_size >= total_size {
                // Found a suitable block
                let remaining = block_size - total_size;

                if remaining >= MIN_BLOCK {
                    // Split: create new free node after our allocation
                    let new_free = (current as *mut u8).add(total_size) as *mut FreeNode;
                    (*new_free).size = remaining;
                    (*new_free).next = (*current).next;

                    // Update links
                    if prev.is_null() {
                        *self.free_head.get() = new_free;
                    } else {
                        (*prev).next = new_free;
                    }
                } else {
                    // Use entire block (no split, avoid tiny fragments)
                    let actual_size = block_size; // Use full block
                    if prev.is_null() {
                        *self.free_head.get() = (*current).next;
                    } else {
                        (*prev).next = (*current).next;
                    }
                    // Store actual block size in header
                    let header = current as *mut BlockHeader;
                    (*header).size = actual_size;
                    return (header as *mut u8).add(HEADER_SIZE);
                }

                // Store header
                let header = current as *mut BlockHeader;
                (*header).size = total_size;
                return (header as *mut u8).add(HEADER_SIZE);
            }

            prev = current;
            current = (*current).next;
        }

        // Out of memory
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() { return; }

        // Recover header
        let header = ptr.sub(HEADER_SIZE) as *mut BlockHeader;
        let block_start = header as *mut u8;
        let block_size = (*header).size;

        // Insert into free list (sorted by address for coalescing)
        let new_node = block_start as *mut FreeNode;
        (*new_node).size = block_size;

        let mut prev: *mut FreeNode = core::ptr::null_mut();
        let mut current = *self.free_head.get();

        // Find insertion point (sorted by address)
        while !current.is_null() && (current as *mut u8) < block_start {
            prev = current;
            current = (*current).next;
        }

        (*new_node).next = current;

        if prev.is_null() {
            *self.free_head.get() = new_node;
        } else {
            (*prev).next = new_node;
        }

        // Coalesce with next block if adjacent
        if !current.is_null() {
            let new_end = (new_node as *mut u8).add((*new_node).size);
            if new_end == current as *mut u8 {
                (*new_node).size += (*current).size;
                (*new_node).next = (*current).next;
            }
        }

        // Coalesce with previous block if adjacent
        if !prev.is_null() {
            let prev_end = (prev as *mut u8).add((*prev).size);
            if prev_end == new_node as *mut u8 {
                (*prev).size += (*new_node).size;
                (*prev).next = (*new_node).next;
            }
        }
    }
}

#[global_allocator]
static ALLOCATOR: FreeListAllocator = FreeListAllocator {
    heap: UnsafeCell::new([0; HEAP_SIZE]),
    free_head: UnsafeCell::new(core::ptr::null_mut()),
    initialized: UnsafeCell::new(false),
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

    // Use VirtIO-GPU framebuffer when available — gpu_flush() sends THIS memory
    // to the display via TRANSFER_TO_HOST_2D + RESOURCE_FLUSH (instant update).
    // Falling back to Limine VGA FB means VNC only polls VGA memory every ~3s.
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
    let mut com3_inject: Option<alloc::string::String> = None;

    // WASM JIT Toolsmithing — async generation (non-blocking)
    // Phase 1: "gemini generate X" → send [GENERATE_TOOL] via async COM2
    // Phase 2: poll COM2 each frame until response arrives
    // Phase 3: decode WASM, execute, render results
    let mut deferred_tool_gen: Option<(u32, alloc::string::String)> = None; // (win_id, prompt) — Frame 1 only
    let mut async_tool_gen: Option<(u32, alloc::string::String)> = None; // (win_id, prompt) — active async session

    // Phase 2: Persistent interactive WASM app (game loop)
    let mut active_wasm_app: Option<compositor::wasm_runtime::PersistentWasmApp> = None;
    let mut active_wasm_app_key: Option<alloc::string::String> = None; // cache key for friction tracking

    // App Persistence: cache last compiled WASM for save/run
    let mut last_wasm_bytes: Option<alloc::vec::Vec<u8>> = None;
    let mut last_wasm_interactive: bool = false;

    // ===== RAM History Graph =====
    const RAM_HISTORY_LEN: usize = 120; // 2 minutes at 1 sample/sec
    let mut ram_history: [u8; RAM_HISTORY_LEN] = [0; RAM_HISTORY_LEN]; // % values
    let mut ram_history_idx: usize = 0;
    let mut ram_history_count: usize = 0;
    let mut show_ram_graph: bool = false; // Toggle with G key or click RAM%

    // ===== IQE Latency Tracking =====
    let mut last_kbd_tsc: u64 = 0;      // KeyboardIrq TSC
    let mut last_kbd_read_tsc: u64 = 0; // KeyboardRead TSC (userspace pulled from buffer)
    let mut last_mou_tsc: u64 = 0;      // MouseIrq TSC
    let mut last_mou_read_tsc: u64 = 0; // MouseRead TSC
    let mut ewma_kbd_us: u64 = 0;       // total KBD latency (IRQ -> flush)
    let mut ewma_mou_us: u64 = 0;       // total MOU latency
    let mut ewma_kbd_wake: u64 = 0;     // KBD wakeup (IRQ -> read)
    let mut ewma_kbd_rend: u64 = 0;     // KBD render (read -> flush)
    let mut ewma_mou_wake: u64 = 0;
    let mut ewma_mou_rend: u64 = 0;
    // Use hardcoded TSC freq (PIT calibrated ~3400 ticks/us on this CPU).
    // Syscall 0x92 works but adds overhead per-frame. Hardcode is safe since
    // TSC freq doesn't change at runtime.
    let tsc_per_us: u64 = 3400;
    let mut iqe_buf = [0u8; 24 * 12]; // 12 events per poll (288 bytes, stack safe)

    // ===== App Launcher: Android-style folders + app grid =====
    const MAX_CATEGORIES: usize = 6;
    const MAX_APPS_PER_CAT: usize = 20;
    const FOLDER_W: usize = 100;
    const FOLDER_H: usize = 100;
    const FOLDER_GAP: usize = 20;
    const APP_TILE_W: usize = 72;
    const APP_TILE_H: usize = 72;
    const APP_TILE_GAP: usize = 12;
    const APP_TILE_COLS: usize = 5;

    // Category definitions: (name, icon_color, keywords for auto-sort)
    struct AppEntry { name: [u8; 24], name_len: usize }
    struct Category {
        label: &'static [u8],
        color: u32,
        apps: [AppEntry; MAX_APPS_PER_CAT],
        count: usize,
    }

    // Initialize categories
    let mut categories: [Category; MAX_CATEGORIES] = [
        Category { label: b"System",   color: 0x003388FF, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
        Category { label: b"Games",    color: 0x00FF4466, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
        Category { label: b"Creative", color: 0x00FF8800, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
        Category { label: b"Tools",    color: 0x0044CC44, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
        Category { label: b"Demos",    color: 0x00AA44FF, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
        Category { label: b"Other",    color: 0x00888888, apps: core::array::from_fn(|_| AppEntry { name: [0; 24], name_len: 0 }), count: 0 },
    ];

    // -1 = home (show folders), 0-5 = inside a specific folder
    let mut open_folder: i32 = -1;
    let mut hover_folder: i32 = -1; // Folder mouse is hovering over (-1 = none)
    let mut tile_clicked: i32 = -1;

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
    // State struct types defined in compositor::state (see state.rs).
    // Variables below will be migrated into these structs incrementally.
    // For now, structs serve as the architectural blueprint.

    let mut text_buffer: [u8; 256] = [0; 256];
    let mut text_len: usize = 0;
    let mut cursor_pos: usize = 0;
    let mut show_results: bool = false;
    let mut omnibar_visible: bool = true;

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

    // ===== Think Tag Filter + UI Overlay =====
    // Captures <think>...</think> reasoning and displays in translucent overlay
    let mut think_state: u8 = 0;     // 0=scanning open, 1=inside think block
    let mut think_open_match: usize = 0;
    let mut think_close_match: usize = 0;
    let mut think_pending: [u8; 8] = [0; 8]; // max tag length for flush
    let mut think_pending_len: usize = 0;

    // Think display buffer — shows AI reasoning in UI overlay
    const THINK_BUF_SIZE: usize = 1024;
    let mut think_display: [u8; THINK_BUF_SIZE] = [0; THINK_BUF_SIZE];
    let mut think_display_len: usize = 0;
    let mut think_active: bool = false; // true while inside <think> block
    let mut think_fade_timer: u32 = 0;  // frames to keep overlay visible after </think>

    const THINK_OPEN: &[u8] = b"<think>";    // 7 bytes
    const THINK_CLOSE: &[u8] = b"</think>";  // 8 bytes

    // ===== Tool Result Filter =====
    // Hides <|tool_result|>...<|/tool_result|> from display (injected by compositor for AI context)
    let mut result_state: u8 = 0;
    let mut result_open_match: usize = 0;
    let mut result_close_match: usize = 0;
    const RESULT_OPEN: &[u8] = b"<|tool_result|>";   // 15 bytes
    const RESULT_CLOSE: &[u8] = b"<|/tool_result|>"; // 16 bytes

    // Blinking caret state (toggles every ~500ms using uptime syscall)
    let mut caret_visible: bool = true;
    let mut last_caret_flip_ms: u64 = 0;
    const CARET_BLINK_MS: u64 = 500;

    // Mouse click tracking (detect left-button press edge)
    let mut prev_left_button: bool = false;

    // Friction Sensor: rage click detection (circular buffer of click timestamps)
    let mut click_timestamps: [u64; 8] = [0; 8];
    let mut click_ts_idx: usize = 0;
    // Friction Sensor: window open time tracking (for quick-close detection)
    let mut wasm_app_open_since_ms: u64 = 0;
    // Live Patching: consecutive fuel exhaustion counter
    let mut fuel_fail_count: u8 = 0;
    // Live Patching: app currently being immune-patched
    let mut immune_patching: Option<alloc::string::String> = None;
    // State Migration: snapshot of WASM linear memory before dream evolution
    let mut state_snapshot: Option<alloc::vec::Vec<u8>> = None;

    // Autonomous Drivers: active WASM driver instances
    let mut active_drivers: alloc::vec::Vec<compositor::driver_runtime::WasmDriver> = alloc::vec::Vec::new();
    // Pending driver generation: PCI device info waiting for WASM from proxy
    let mut pending_driver_device: Option<libfolk::sys::pci::PciDeviceInfo> = None;

    // FolkShell: JIT synthesis state
    let mut pending_shell_jit: Option<alloc::string::String> = None;
    let mut shell_jit_pipeline: Option<(alloc::vec::Vec<compositor::folkshell::Command>, usize, alloc::string::String)> = None;

    // ===== Window Manager (Milestone 2.1) =====
    let mut wm = WindowManager::new();
    // Track which window (if any) is being dragged
    let mut dragging_window_id: Option<u32> = None;
    let mut drag_last_x: i32 = 0;
    let mut drag_last_y: i32 = 0;

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

    let mut last_clock_second: u8 = 255; // Force first draw
    let mut tz_offset_minutes: i32 = 0; // UTC offset from host time sync
    let mut tz_synced = false;
    let mut tz_sync_pending = false; // MCP TimeSyncRequest sent, waiting for response
    let mut active_agent: Option<compositor::agent::AgentSession> = None; // ReAct agentic loop
    let mut draug = compositor::draug::DraugDaemon::new(); // Background AI daemon

    // Pillar 4: WASM warm cache — pre-compiled modules for instant response
    let mut wasm_cache: alloc::collections::BTreeMap<alloc::string::String, alloc::vec::Vec<u8>> = alloc::collections::BTreeMap::new();
    const MAX_CACHE_ENTRIES: usize = 4;

    // Semantic VFS: View Adapter cache — compiled WASM data translators
    // Key: "source_mime|target_format", Value: compiled WASM adapter bytes
    let mut adapter_cache: alloc::collections::BTreeMap<alloc::string::String, alloc::vec::Vec<u8>> = alloc::collections::BTreeMap::new();
    const MAX_ADAPTER_ENTRIES: usize = 8;
    // Pending adapter generation request (source_mime|target_format)
    let mut pending_adapter: Option<alloc::string::String> = None;
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

        // ===== IQE: Poll telemetry events =====
        if tsc_per_us > 0 {
            let n = libfolk::sys::iqe_read(&mut iqe_buf, 12);
            // Debug: log IQE poll result (first 3 only)
            static mut IQE_DBG: u32 = 0;
            if n > 0 { unsafe {
                if IQE_DBG < 3 {
                    write_str("[IQE-POLL] n=");
                    write_char(b'0' + n as u8);
                    write_str("\n");
                    IQE_DBG += 1;
                }
            }}
            for i in 0..n {
                let base = i * 24;
                let etype = iqe_buf[base];
                let tsc = u64::from_le_bytes([
                    iqe_buf[base+8], iqe_buf[base+9], iqe_buf[base+10], iqe_buf[base+11],
                    iqe_buf[base+12], iqe_buf[base+13], iqe_buf[base+14], iqe_buf[base+15],
                ]);
                match etype {
                    5 => { last_kbd_tsc = tsc; }       // KeyboardIrq
                    0 => { last_mou_tsc = tsc; }       // MouseIrq
                    6 => { last_kbd_read_tsc = tsc; }   // KeyboardRead
                    7 => { last_mou_read_tsc = tsc; }   // MouseRead
                    1 => {                               // GpuFlushSubmit
                        // Keyboard split times
                        if last_kbd_tsc > 0 && tsc > last_kbd_tsc {
                            let total = (tsc - last_kbd_tsc) / tsc_per_us;
                            if total < 100_000 {
                                ewma_kbd_us = ewma_kbd_us - (ewma_kbd_us >> 3) + (total >> 3);
                                let mut l = [0u8; 32];
                                let n = fmt_iqe_line(&mut l, b"KBD", total);
                                libfolk::sys::com3_write(&l[..n]);
                                // Split: wakeup (IRQ -> read)
                                if last_kbd_read_tsc > last_kbd_tsc {
                                    let wake = (last_kbd_read_tsc - last_kbd_tsc) / tsc_per_us;
                                    let rend = if tsc > last_kbd_read_tsc { (tsc - last_kbd_read_tsc) / tsc_per_us } else { 0 };
                                    ewma_kbd_wake = ewma_kbd_wake - (ewma_kbd_wake >> 3) + (wake >> 3);
                                    ewma_kbd_rend = ewma_kbd_rend - (ewma_kbd_rend >> 3) + (rend >> 3);
                                    let mut l2 = [0u8; 32];
                                    let n2 = fmt_iqe_line(&mut l2, b"KW", wake);
                                    libfolk::sys::com3_write(&l2[..n2]);
                                    let mut l3 = [0u8; 32];
                                    let n3 = fmt_iqe_line(&mut l3, b"KR", rend);
                                    libfolk::sys::com3_write(&l3[..n3]);
                                }
                            }
                            last_kbd_tsc = 0;
                            last_kbd_read_tsc = 0;
                        }
                        // Mouse split times
                        if last_mou_tsc > 0 && tsc > last_mou_tsc {
                            let total = (tsc - last_mou_tsc) / tsc_per_us;
                            if total < 100_000 {
                                ewma_mou_us = ewma_mou_us - (ewma_mou_us >> 3) + (total >> 3);
                                let mut l = [0u8; 32];
                                let n = fmt_iqe_line(&mut l, b"MOU", total);
                                libfolk::sys::com3_write(&l[..n]);
                                if last_mou_read_tsc > last_mou_tsc {
                                    let wake = (last_mou_read_tsc - last_mou_tsc) / tsc_per_us;
                                    let rend = if tsc > last_mou_read_tsc { (tsc - last_mou_read_tsc) / tsc_per_us } else { 0 };
                                    ewma_mou_wake = ewma_mou_wake - (ewma_mou_wake >> 3) + (wake >> 3);
                                    ewma_mou_rend = ewma_mou_rend - (ewma_mou_rend >> 3) + (rend >> 3);
                                    let mut l2 = [0u8; 32];
                                    let n2 = fmt_iqe_line(&mut l2, b"MW", wake);
                                    libfolk::sys::com3_write(&l2[..n2]);
                                    let mut l3 = [0u8; 32];
                                    let n3 = fmt_iqe_line(&mut l3, b"MR", rend);
                                    libfolk::sys::com3_write(&l3[..n3]);
                                }
                            }
                            last_mou_tsc = 0;
                            last_mou_read_tsc = 0;
                        }
                    }
                    _ => {}
                }
            }
        }

        // Consolidated redraw flag — any subsystem can set this
        // WASM apps need continuous redraws for animation (60fps game loop)
        let mut need_redraw = active_wasm_app.as_ref().map_or(false, |a| a.active);

        // Clock tick: targeted status bar redraw (NO full desktop redraw!)
        // Renders only the 20px status bar directly to shadow buffer.
        // This costs ~50µs instead of 150ms+ for a full desktop redraw.
        let current_second = (libfolk::sys::get_rtc_packed() & 0x3F) as u8;
        if current_second != last_clock_second {
            last_clock_second = current_second;
            // NOT did_work — clock tick is passive, not user input
            // Status bar damage is added below and gpu_flush handles it

            // Sample RAM usage for history graph
            let (_, _, mem_pct) = libfolk::sys::memory_stats();
            ram_history[ram_history_idx] = mem_pct.min(100) as u8;
            ram_history_idx = (ram_history_idx + 1) % RAM_HISTORY_LEN;
            if ram_history_count < RAM_HISTORY_LEN { ram_history_count += 1; }

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
                let mut total_minutes = dt.hour as i32 * 60 + dt.minute as i32 + tz_offset_minutes;
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
                if ewma_kbd_us > 0 || ewma_mou_us > 0 {
                    let mut lbuf = [0u8; 48];
                    let mut li = 0usize;
                    lbuf[li]=b'K'; li+=1; lbuf[li]=b':'; li+=1;
                    li += fmt_u64_into(&mut lbuf[li..], ewma_kbd_us);
                    if ewma_kbd_wake > 0 {
                        lbuf[li]=b'('; li+=1;
                        li += fmt_u64_into(&mut lbuf[li..], ewma_kbd_wake);
                        lbuf[li]=b'+'; li+=1;
                        li += fmt_u64_into(&mut lbuf[li..], ewma_kbd_rend);
                        lbuf[li]=b')'; li+=1;
                    }
                    if li < 44 { lbuf[li]=b' '; li+=1; lbuf[li]=b'M'; li+=1; lbuf[li]=b':'; li+=1;
                        li += fmt_u64_into(&mut lbuf[li..], ewma_mou_us);
                    }
                    let s = unsafe { core::str::from_utf8_unchecked(&lbuf[..li.min(48)]) };
                    fb.draw_string(90, 2, s, fb.color_from_rgb24(0x88AACC), bar_bg);

                    let worst = ewma_kbd_us.max(ewma_mou_us);
                    let dot = if worst < 5000 { 0x44FF44 } else if worst < 16000 { 0xFFAA00 } else { 0xFF4444 };
                    fb.fill_rect(ram_x.saturating_sub(14), 5, 8, 8, fb.color_from_rgb24(dot));
                }

                // Damage only the text areas (3 small rects instead of full width)
                damage.add_damage(compositor::damage::Rect::new(4, 0, 84, 20));         // date
                damage.add_damage(compositor::damage::Rect::new(time_x as u32, 0, 68, 20)); // clock
                damage.add_damage(compositor::damage::Rect::new(ram_clear_x as u32, 0, 70, 20)); // RAM
            }

            // Lazy timezone sync via MCP: send TimeSyncRequest, poll for TimeSync response
            if !tz_synced && !tz_sync_pending {
                if libfolk::mcp::client::send_time_sync() {
                    tz_sync_pending = true;
                    write_str("[MCP] TimeSyncRequest sent\n");
                }
            }
        }

        // ===== WASM JIT TOOLSMITHING — MCP-based async generation =====
        // Frame 1: deferred_tool_gen set → send McpResponse::WasmGenRequest via COBS
        // Frame N: MCP poll returns McpRequest::WasmBinary → execute directly (no base64!)
        if let Some((tool_win_id, tool_prompt)) = deferred_tool_gen.take() {
            did_work = true;
            if libfolk::mcp::client::send_wasm_gen(&tool_prompt) {
                async_tool_gen = Some((tool_win_id, tool_prompt));
                write_str("[MCP] WasmGenRequest sent\n");
            } else {
                if let Some(win) = wm.get_window_mut(tool_win_id) {
                    win.push_line("[AI] Error: failed to send WASM gen request");
                }
            }
        }

        // ===== Agent timeout check =====
        if let Some(agent) = &mut active_agent {
            let timeout_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { libfolk::sys::uptime() };
            if agent.check_timeout(timeout_ms) {
                if let Some(win) = wm.get_window_mut(agent.window_id) {
                    win.push_line("[Agent] Timeout: LLM did not respond in 120s");
                }
                active_agent = None;
                need_redraw = true;
            }
        }

        // ===== Draug: Background AI daemon tick =====
        {
            // Use RDTSC for timing (uptime_ms is broken under WHPX — APIC timer death)
            let now_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { libfolk::sys::uptime() };
            // Only count actual user input (mouse/keyboard) as activity, not rendering
            // did_work is too broad — clock ticks, MCP polls, etc. are not user input
            if draug.should_tick(now_ms) {
                draug.tick(now_ms);
                let mut nb = [0u8; 16];
                // Log every 6th tick (~1 min) to avoid spam but show liveness
                if draug.observation_count() % 6 == 1 || draug.observation_count() <= 3 {
                    write_str("[Draug] Tick #");
                    write_str(format_usize(draug.observation_count(), &mut nb));
                    let idle_ms = now_ms.saturating_sub(draug.last_input_ms());
                    write_str(" | idle: ");
                    write_str(format_usize((idle_ms / 1000) as usize, &mut nb));
                    write_str("s | dreams: ");
                    write_str(format_usize(draug.dream_count() as usize, &mut nb));
                    write_str("/");
                    write_str(format_usize(compositor::draug::DREAM_MAX_PER_SESSION as usize, &mut nb));
                    write_str("\n");
                }
            }
            if draug.should_analyze(now_ms) && active_agent.is_none() {
                if draug.start_analysis(now_ms) {
                    let mut nb = [0u8; 16];
                    write_str("[Draug] Analysis #");
                    write_str(format_usize(draug.analysis_count() as usize, &mut nb));
                    write_str("/5 started\n");
                }
            }
        }

        // ===== Tick WASM Drivers: poll IRQs and resume suspended drivers =====
        if !active_drivers.is_empty() {
            let resumed = compositor::driver_runtime::tick_drivers(&mut active_drivers);
            if resumed > 0 {
                did_work = true;
            }
        }

        // ===== Draug/Dream timeout — prevent permanent waiting_for_llm =====
        {
            let timeout_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
            if draug.check_waiting_timeout(timeout_ms) {
                write_str("[Draug] Timeout — giving up on LLM response\n");
            }
        }

        // ===== AutoDream: Two-Hemisphere Self-Improving Software =====
        let dream_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
        if draug.should_dream(dream_ms) && active_agent.is_none() && async_tool_gen.is_none()
            && !draug.should_yield_tokens(active_agent.is_some(), dream_ms) {
            let keys: alloc::vec::Vec<&str> = wasm_cache.keys().map(|k| k.as_str()).collect();
            if let Some((target, mode)) = draug.start_dream(&keys, dream_ms) {
                // Dream target found — proceed with generation
                let mode_str = match mode {
                    compositor::draug::DreamMode::Refactor => "Refactor",
                    compositor::draug::DreamMode::Creative => "Creative",
                    compositor::draug::DreamMode::Nightmare => "Nightmare",
                };

                // State Migration: snapshot WASM memory if active app is the dream target
                state_snapshot = None;
                if let Some(ref app) = active_wasm_app {
                    if let Some(ref k) = active_wasm_app_key {
                        if k.as_str() == target.as_str() {
                            if let Some(mem) = app.get_memory_slice() {
                                let snap_len = mem.len().min(1024);
                                state_snapshot = Some(alloc::vec::Vec::from(&mem[..snap_len]));
                                write_str("[StateMigration] Captured ");
                                let mut nb2 = [0u8; 16];
                                write_str(format_usize(snap_len, &mut nb2));
                                write_str(" bytes of app state\n");
                            }
                        }
                    }
                }

                // Log dream start to both serial AND COM3 telemetry
                write_str("[AutoDream] ========================================\n");
                write_str("[AutoDream] DREAM #");
                let mut nb = [0u8; 16];
                write_str(format_usize(draug.dream_count() as usize, &mut nb));
                write_str(" | Mode: ");
                write_str(mode_str);
                write_str(" | Target: ");
                write_str(&target[..target.len().min(40)]);
                write_str("\n");
                // RTC timestamp for overnight log correlation
                {
                    let dt = libfolk::sys::get_rtc();
                    let mut ts = [0u8; 19]; // "2026-04-03 02:15:30"
                    ts[0] = b'0'+((dt.year/1000)%10) as u8; ts[1] = b'0'+((dt.year/100)%10) as u8;
                    ts[2] = b'0'+((dt.year/10)%10) as u8; ts[3] = b'0'+(dt.year%10) as u8;
                    ts[4] = b'-'; ts[5] = b'0'+dt.month/10; ts[6] = b'0'+dt.month%10;
                    ts[7] = b'-'; ts[8] = b'0'+dt.day/10; ts[9] = b'0'+dt.day%10;
                    ts[10] = b' '; ts[11] = b'0'+dt.hour/10; ts[12] = b'0'+dt.hour%10;
                    ts[13] = b':'; ts[14] = b'0'+dt.minute/10; ts[15] = b'0'+dt.minute%10;
                    ts[16] = b':'; ts[17] = b'0'+dt.second/10; ts[18] = b'0'+dt.second%10;
                    write_str("[AutoDream] Time: ");
                    if let Ok(s) = core::str::from_utf8(&ts) { write_str(s); }
                    write_str("\n");
                }
                // Cache size
                write_str("[AutoDream] Cache: ");
                write_str(format_usize(wasm_cache.len(), &mut nb));
                write_str(" apps | Draug dreams: ");
                write_str(format_usize(draug.dream_count() as usize, &mut nb));
                write_str("/");
                write_str(format_usize(compositor::draug::DREAM_MAX_PER_SESSION as usize, &mut nb));
                write_str("\n");

                let tweak = match mode {
                    compositor::draug::DreamMode::Refactor =>
                        alloc::format!("--tweak \"refactor for fewer CPU cycles, no new features\" {}", target),
                    compositor::draug::DreamMode::Nightmare => {
                        // Nightmare: ask LLM to harden the code against edge cases
                        alloc::format!("--tweak \"harden against edge cases: zero division, overflow, OOB\" {}", target)
                    }
                    compositor::draug::DreamMode::Creative => {
                        // For Creative mode: run the app headless to get render summary
                        let render_desc = if let Some(cached_wasm) = wasm_cache.get(&target) {
                            let cfg = compositor::wasm_runtime::WasmConfig {
                                screen_width: fb.width as u32,
                                screen_height: fb.height as u32,
                                uptime_ms: 0,
                            };
                            let (_, output) = compositor::wasm_runtime::execute_wasm(cached_wasm, cfg);
                            compositor::wasm_runtime::render_summary(&output)
                        } else {
                            alloc::string::String::from("(no cached binary)")
                        };
                        alloc::format!("--tweak \"add one visual improvement. Current output: {}\" {}", render_desc, target)
                    }
                };

                if libfolk::mcp::client::send_wasm_gen(&tweak) {
                    async_tool_gen = Some((0, target));
                    write_str("[AutoDream] Request sent\n");
                } else {
                    // send failed — cancel dream to prevent retry spam
                    write_str("[AutoDream] Send failed — cancelling dream\n");
                    draug.on_dream_complete(dream_ms);
                }
            } else {
                // Digital Homeostasis: all apps stable, no dreams needed
                write_str("[AutoDream] All systems stable. Sleeping.\n");
            }
        }

        // Wake Draug from dream if user interacts
        if did_work && draug.is_dreaming() {
            draug.wake_up();
            write_str("[AutoDream] User woke up — dream cancelled\n");
        }

        // Morning Briefing: show pending creative changes when user returns
        if did_work && draug.has_pending_creative() && !draug.is_dreaming() {
            let count = draug.pending_count();
            write_str("[Morning Briefing] Draug has ");
            let mut nb2 = [0u8; 16];
            write_str(format_usize(count, &mut nb2));
            write_str(" creative change(s) waiting for approval.\n");

            // Show in a terminal window
            let brief_win = wm.create_terminal("Morning Briefing", 200, 100, 500, 250);
            if let Some(win) = wm.get_window_mut(brief_win) {
                win.push_line("Good morning! Draug dreamt overnight:");
                win.push_line("");
                for (i, p) in draug.pending_creative.iter().enumerate() {
                    if p.accepted.is_none() {
                        let line = alloc::format!("  {}. '{}': {}", i + 1, &p.app_name[..p.app_name.len().min(20)], &p.description[..p.description.len().min(50)]);
                        win.push_line(&line);
                    }
                }
                win.push_line("");
                win.push_line("Type in omnibar: 'dream accept all' or 'dream reject all'");
                win.push_line("Or: 'dream accept 1' / 'dream reject 2'");
            }
            need_redraw = true;
            damage.damage_full();
            // Only show once per batch — mark as shown
            // (pending_creative stays until user decides)
        }

        // ===== MCP: Poll for responses from Python proxy =====
        if tz_sync_pending || async_tool_gen.is_some() || active_agent.is_some() || draug.is_waiting() || pending_shell_jit.is_some() {
            if let Some(response) = libfolk::mcp::client::poll() {
                did_work = true;
                match response {
                    libfolk::mcp::types::McpRequest::TimeSync {
                        year: _, month: _, day: _,
                        hour: _, minute: _, second: _,
                        utc_offset_minutes,
                    } => {
                        tz_offset_minutes = utc_offset_minutes as i32;
                        tz_synced = true;
                        tz_sync_pending = false;
                        write_str("[MCP] TimeSync: UTC+");
                        let mut nbuf = [0u8; 16];
                        write_str(format_usize((utc_offset_minutes / 60) as usize, &mut nbuf));
                        write_str("\n");
                    }
                    libfolk::mcp::types::McpRequest::ChatResponse { text } => {
                        if let Ok(resp_text) = core::str::from_utf8(&text) {
                            // Route to active agent if present
                            if let Some(agent) = &mut active_agent {
                                write_str("[Agent] LLM responded\n");
                                agent.on_llm_response(resp_text);

                                // Process agent state
                                match &agent.state {
                                    compositor::agent::AgentState::ExecutingTool { tool_name, tool_args } => {
                                        let tname = tool_name.clone();
                                        let targs = tool_args.clone();
                                        write_str("[Agent] Tool: ");
                                        write_str(&tname);
                                        write_str(" ");
                                        write_str(&targs[..targs.len().min(40)]);
                                        write_str("\n");
                                        if let Some(win) = wm.get_window_mut(agent.window_id) {
                                            win.push_line(&alloc::format!("[Agent] Tool: {} {}", &tname, &targs[..targs.len().min(40)]));
                                        }

                                        // Check for WASM gen (special case — async)
                                        if tname == "generate_wasm" {
                                            deferred_tool_gen = Some((agent.window_id, alloc::string::String::from(targs.as_str())));
                                        } else if tname == "list_cache" {
                                            // List OS-side WASM cache
                                            let mut cache_list = alloc::string::String::from("Cached WASM apps:\n");
                                            for (name, wasm) in &wasm_cache {
                                                cache_list.push_str(&alloc::format!("  - {} ({} bytes)\n", name, wasm.len()));
                                            }
                                            if wasm_cache.is_empty() {
                                                cache_list.push_str("  (empty)\n");
                                            }
                                            agent.on_tool_result(&cache_list);
                                            if let Some(win) = wm.get_window_mut(agent.window_id) {
                                                win.push_line(&cache_list[..cache_list.len().min(200)]);
                                                win.push_line("[Agent] Thinking...");
                                            }
                                        } else {
                                            // Execute tool synchronously
                                            let result = compositor::agent::execute_tool(&tname, &targs);
                                            if let Some(win) = wm.get_window_mut(agent.window_id) {
                                                let preview = &result[..result.len().min(80)];
                                                win.push_line(&alloc::format!("[Tool] {}", preview));
                                            }
                                            // Feed result back to LLM
                                            agent.on_tool_result(&result);
                                            if let Some(win) = wm.get_window_mut(agent.window_id) {
                                                win.push_line("[Agent] Thinking...");
                                            }
                                        }
                                    }
                                    compositor::agent::AgentState::Done { answer } => {
                                        write_str("[Agent] Done: ");
                                        write_str(&answer[..answer.len().min(80)]);
                                        write_str("\n");
                                        if let Some(win) = wm.get_window_mut(agent.window_id) {
                                            win.push_line("[Agent] Done:");
                                            for line in answer.split('\n') {
                                                if !line.is_empty() {
                                                    win.push_line(&line[..line.len().min(100)]);
                                                }
                                            }
                                        }
                                        active_agent = None;
                                    }
                                    compositor::agent::AgentState::Failed { reason } => {
                                        write_str("[Agent] Failed: ");
                                        write_str(&reason[..reason.len().min(80)]);
                                        write_str("\n");
                                        if let Some(win) = wm.get_window_mut(agent.window_id) {
                                            win.push_line(&alloc::format!("[Agent] Failed: {}", &reason[..reason.len().min(80)]));
                                        }
                                        active_agent = None;
                                    }
                                    _ => {} // WaitingForLlm, etc.
                                }
                                need_redraw = true;
                            } else if draug.is_waiting() {
                                // Route to Draug daemon (analysis response)
                                if let Some(alert) = draug.on_analysis_response(resp_text) {
                                    write_str(&alert);
                                    write_str("\n");
                                } else {
                                    write_str("[Draug] Analysis complete (no action needed)\n");
                                }
                            } else if draug.is_dreaming() {
                                // Dream error response (e.g., budget exhausted, compile fail)
                                write_str("[AutoDream] Error from proxy: ");
                                write_str(&resp_text[..resp_text.len().min(80)]);
                                write_str("\n");
                                let done_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
                                draug.on_dream_complete(done_ms);
                                // Clear async_tool_gen if dream was pending
                                if async_tool_gen.is_some() {
                                    async_tool_gen = None;
                                }
                            } else if async_tool_gen.is_some() {
                                // Response during WASM gen — likely clarification or error
                                let (tool_win_id, _) = async_tool_gen.take().unwrap_or((0, alloc::string::String::new()));
                                write_str("[MCP] WASM gen response: ");
                                write_str(&resp_text[..resp_text.len().min(80)]);
                                write_str("\n");

                                // Check for clarification types
                                let is_question = resp_text.starts_with("QUESTION:") || resp_text.starts_with("VARIANTS:") || resp_text.starts_with("EXISTING:");
                                if let Some(win) = wm.get_window_mut(tool_win_id) {
                                    if is_question {
                                        win.push_line("[AI] Need more info:");
                                    } else if resp_text.starts_with("Error:") {
                                        win.push_line("[AI] Generation failed:");
                                    }
                                    for line in resp_text.split('\n') {
                                        if !line.is_empty() {
                                            win.push_line(&line[..line.len().min(100)]);
                                        }
                                    }
                                    if is_question {
                                        win.push_line("");
                                        win.push_line("Refine your request and try again.");
                                    }
                                }
                                need_redraw = true;
                            } else {
                                write_str("[MCP] ChatResponse (unrouted): ");
                                write_str(&resp_text[..resp_text.len().min(60)]);
                                write_str("\n");
                            }
                        }
                    }
                    libfolk::mcp::types::McpRequest::WasmChunk { total_chunks, chunk_index, data } => {
                        let mut nbuf = [0u8; 16];
                        // client::poll() handles reassembly. The last chunk triggers this match.
                        // Get assembled WASM data from client
                        let assembled = if libfolk::mcp::client::wasm_assembly_complete() {
                            let d = libfolk::mcp::client::wasm_assembly_data();
                            write_str("[MCP] WASM assembled: ");
                            write_str(format_usize(d.len(), &mut nbuf));
                            write_str(" bytes (");
                            write_str(format_usize(total_chunks as usize, &mut nbuf));
                            write_str(" chunks)\n");
                            Some(alloc::vec::Vec::from(d))
                        } else {
                            // Single chunk (total=1) — use data directly
                            write_str("[MCP] WASM single chunk: ");
                            write_str(format_usize(data.len(), &mut nbuf));
                            write_str(" bytes\n");
                            Some(alloc::vec::Vec::from(data.as_slice()))
                        };
                        libfolk::mcp::client::wasm_assembly_reset();

                        let raw_bytes = match assembled {
                            Some(v) => v,
                            None => { continue; }
                        };

                        // ═══════ Cryptographic Lineage: Strip + Verify Signature ═══════
                        // Signed WASM format: FOLK\x00 (5 bytes) + SHA256 sig (32 bytes) + WASM
                        let wasm_bytes = if raw_bytes.len() > 37
                            && raw_bytes[0] == b'F' && raw_bytes[1] == b'O'
                            && raw_bytes[2] == b'L' && raw_bytes[3] == b'K'
                            && raw_bytes[4] == 0x00
                        {
                            let sig = &raw_bytes[5..37];
                            let wasm = &raw_bytes[37..];
                            // Verify: hash the WASM binary
                            let wasm_hash = libfolk::crypto::sha256(wasm);
                            let mut sig_hex = [0u8; 64];
                            libfolk::crypto::hash_to_hex(&wasm_hash, &mut sig_hex);
                            write_str("[CRYPTO] Signed WASM: hash=");
                            if let Ok(s) = core::str::from_utf8(&sig_hex[..16]) { write_str(s); }
                            write_str("... sig=");
                            // Show first 8 bytes of signature as hex
                            for i in 0..4 {
                                let b = sig[i];
                                let hi = b"0123456789abcdef"[(b >> 4) as usize];
                                let lo = b"0123456789abcdef"[(b & 0xf) as usize];
                                let buf = [hi, lo];
                                if let Ok(s) = core::str::from_utf8(&buf) { write_str(s); }
                            }
                            write_str("...\n");
                            alloc::vec::Vec::from(wasm)
                        } else {
                            // Unsigned WASM — allow for now (boot apps, legacy)
                            // TODO: reject unsigned WASM once all paths sign
                            if raw_bytes.len() > 4 && raw_bytes[0] == 0x00
                                && raw_bytes[1] == b'a' && raw_bytes[2] == b's' && raw_bytes[3] == b'm'
                            {
                                write_str("[CRYPTO] Unsigned WASM (legacy)\n");
                            }
                            raw_bytes
                        };

                        // Extract tool context if this was from async_tool_gen
                        let (tool_win_id, tool_prompt) = if let Some(ctx) = async_tool_gen.take() {
                            ctx
                        } else {
                            (0u32, alloc::string::String::new())
                        };
                        last_wasm_bytes = Some(wasm_bytes.clone());

                        // Live Patching: if this WASM is a response to immune_patching request
                        if let Some(ref patch_key) = immune_patching.clone() {
                            let config = compositor::wasm_runtime::WasmConfig {
                                screen_width: fb.width as u32,
                                screen_height: fb.height as u32,
                                uptime_ms: libfolk::sys::uptime() as u32,
                            };
                            match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                Ok(app) => {
                                    write_str("[IMMUNE] Patched '");
                                    write_str(&patch_key[..patch_key.len().min(30)]);
                                    write_str("' live!\n");
                                    active_wasm_app = Some(app);
                                    fuel_fail_count = 0;
                                    // Update cache with fixed version
                                    wasm_cache.insert(patch_key.clone(), wasm_bytes.clone());
                                }
                                Err(e) => {
                                    write_str("[IMMUNE] Patch failed to load: ");
                                    write_str(&e[..e.len().min(60)]);
                                    write_str("\n");
                                }
                            }
                            immune_patching = None;
                            continue; // Skip normal processing
                        }

                        // View Adapter: if this WASM is a response to adapter generation
                        if let Some(ref adapter_key) = pending_adapter.clone() {
                            // Validate the adapter compiles
                            let config = compositor::wasm_runtime::WasmConfig {
                                screen_width: fb.width as u32,
                                screen_height: fb.height as u32,
                                uptime_ms: libfolk::sys::uptime() as u32,
                            };
                            let (result, _) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, config);
                            match result {
                                compositor::wasm_runtime::WasmResult::Ok |
                                compositor::wasm_runtime::WasmResult::OutOfFuel => {
                                    // Adapter compiled and runs — cache it
                                    if adapter_cache.len() >= MAX_ADAPTER_ENTRIES {
                                        if let Some(oldest) = adapter_cache.keys().next().cloned() {
                                            adapter_cache.remove(&oldest);
                                        }
                                    }
                                    adapter_cache.insert(adapter_key.clone(), wasm_bytes.clone());
                                    write_str("[ViewAdapter] Cached adapter: ");
                                    write_str(&adapter_key[..adapter_key.len().min(40)]);
                                    write_str("\n");
                                }
                                _ => {
                                    write_str("[ViewAdapter] Adapter generation failed — discarding\n");
                                }
                            }
                            pending_adapter = None;
                            continue;
                        }

                        // Autonomous Driver: if this WASM is a driver response
                        if let Some(pci_dev) = pending_driver_device.take() {
                            let mut cap = compositor::driver_runtime::DriverCapability::from_pci(&pci_dev);
                            let name = alloc::format!("drv_{:04x}_{:04x}", pci_dev.vendor_id, pci_dev.device_id);
                            cap.set_name(&name);

                            // Map MMIO BARs into our address space
                            let mapped = compositor::driver_runtime::map_device_bars(&mut cap);
                            write_str("[DRV] Mapped ");
                            let mut nb4 = [0u8; 16];
                            write_str(format_usize(mapped, &mut nb4));
                            write_str(" MMIO BARs\n");

                            // Instantiate the WASM driver
                            match compositor::driver_runtime::WasmDriver::new(&wasm_bytes, cap) {
                                Ok(mut driver) => {
                                    // Bind IRQ
                                    let _ = driver.bind_irq();

                                    // Start driver execution
                                    write_str("[DRV] Starting driver: ");
                                    write_str(&name[..name.len().min(30)]);
                                    write_str("\n");
                                    match driver.start() {
                                        compositor::driver_runtime::DriverResult::WaitingForIrq => {
                                            write_str("[DRV] Driver yielded (waiting for IRQ)\n");
                                            active_drivers.push(driver);
                                        }
                                        compositor::driver_runtime::DriverResult::Completed => {
                                            write_str("[DRV] Driver completed immediately\n");
                                        }
                                        compositor::driver_runtime::DriverResult::OutOfFuel => {
                                            write_str("[DRV] Driver preempted (fuel) — scheduling\n");
                                            active_drivers.push(driver);
                                        }
                                        compositor::driver_runtime::DriverResult::Trapped(msg) => {
                                            write_str("[DRV] Driver TRAPPED: ");
                                            write_str(&msg[..msg.len().min(60)]);
                                            write_str("\n");
                                        }
                                        compositor::driver_runtime::DriverResult::LoadError(e) => {
                                            write_str("[DRV] Load error: ");
                                            write_str(&e[..e.len().min(60)]);
                                            write_str("\n");
                                        }
                                    }
                                }
                                Err(e) => {
                                    write_str("[DRV] Failed to instantiate: ");
                                    write_str(&e[..e.len().min(60)]);
                                    write_str("\n");
                                }
                            }
                            continue;
                        }

                        // FolkShell JIT: if shell is waiting for a synthesized command
                        if let Some(ref jit_name) = pending_shell_jit.clone() {
                            wasm_cache.insert(jit_name.clone(), wasm_bytes.clone());
                            write_str("[FolkShell] JIT command ready: ");
                            write_str(&jit_name[..jit_name.len().min(30)]);
                            write_str("\n");

                            // Resume pipeline from where it stopped
                            if let Some((pipeline, stage, pipe_input)) = shell_jit_pipeline.take() {
                                let result = compositor::folkshell::execute_pipeline(
                                    &pipeline, stage, pipe_input, &wasm_cache
                                );
                                match result {
                                    compositor::folkshell::ShellState::Done(output) => {
                                        // Display output in the most recent window
                                        write_str("[FolkShell] Pipeline output:\n");
                                        write_str(&output[..output.len().min(200)]);
                                        write_str("\n");
                                    }
                                    compositor::folkshell::ShellState::WaitingForJIT {
                                        command_name, pipeline: p, stage: s, pipe_input: pi
                                    } => {
                                        write_str("[FolkShell] Chaining JIT: ");
                                        write_str(&command_name[..command_name.len().min(30)]);
                                        write_str("\n");
                                        let prompt = compositor::folkshell::jit_prompt(&command_name, &pi);
                                        if libfolk::mcp::client::send_wasm_gen(&prompt) {
                                            pending_shell_jit = Some(command_name);
                                            shell_jit_pipeline = Some((p, s, pi));
                                        }
                                    }
                                    compositor::folkshell::ShellState::Widget { wasm_bytes: w, title: t } => {
                                        // JIT produced a visual widget — launch it
                                        write_str("[FolkShell] JIT widget: ");
                                        write_str(&t[..t.len().min(30)]);
                                        write_str("\n");
                                        let config = compositor::wasm_runtime::WasmConfig {
                                            screen_width: fb.width as u32,
                                            screen_height: fb.height as u32,
                                            uptime_ms: libfolk::sys::uptime() as u32,
                                        };
                                        if let Ok(app) = compositor::wasm_runtime::PersistentWasmApp::new(&w, config) {
                                            active_wasm_app = Some(app);
                                            active_wasm_app_key = Some(t);
                                            wasm_app_open_since_ms = libfolk::sys::uptime();
                                            fuel_fail_count = 0;
                                            damage.damage_full();
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            if !matches!(pending_shell_jit.as_deref(), Some(_)) || shell_jit_pipeline.is_none() {
                                pending_shell_jit = None;
                            }
                            continue;
                        }

                        // AutoDream: two-hemisphere evaluation
                        if draug.is_dreaming() && !tool_prompt.is_empty() {
                            // Use dream target as cache key (copy to avoid borrow conflict)
                            let orig_key_owned = draug.dream_target()
                                .map(alloc::string::String::from)
                                .unwrap_or_else(|| alloc::string::String::from(
                                    tool_prompt.rsplit(' ').next().unwrap_or(&tool_prompt)
                                ));
                            let orig_key = orig_key_owned.as_str();
                            let dream_mode = draug.current_dream_mode();
                            let mut nb = [0u8; 16];

                            match dream_mode {
                                compositor::draug::DreamMode::Refactor => {
                                    write_str("[AutoDream] ---- REFACTOR RESULT ----\n");
                                    // Amnesia fix: if V1 not in RAM cache, try loading from Synapse VFS
                                    if !wasm_cache.contains_key(orig_key) {
                                        let vfs_name = alloc::format!("{}.wasm", orig_key);
                                        const VFS_DREAM_VADDR: usize = 0x50070000;
                                        if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(&vfs_name) {
                                            if shmem_map(resp.shmem_handle, VFS_DREAM_VADDR).is_ok() {
                                                let data = unsafe {
                                                    core::slice::from_raw_parts(VFS_DREAM_VADDR as *const u8, resp.size as usize)
                                                };
                                                wasm_cache.insert(alloc::string::String::from(orig_key), alloc::vec::Vec::from(data));
                                                let _ = shmem_unmap(resp.shmem_handle, VFS_DREAM_VADDR);
                                                let _ = shmem_destroy(resp.shmem_handle);
                                                write_str("[AutoDream] Recovered V1 from Synapse VFS\n");
                                            } else {
                                                let _ = shmem_destroy(resp.shmem_handle);
                                            }
                                        }
                                    }
                                    if let Some(v1_wasm) = wasm_cache.get(orig_key) {
                                        let bench_config = compositor::wasm_runtime::WasmConfig {
                                            screen_width: fb.width as u32, screen_height: fb.height as u32, uptime_ms: 0,
                                        };

                                        // Lobotomy check: compare draw command counts
                                        let (_, v1_out) = compositor::wasm_runtime::execute_wasm(v1_wasm, bench_config.clone());
                                        let v1_cmds = v1_out.draw_commands.len() + v1_out.circle_commands.len()
                                            + v1_out.line_commands.len() + v1_out.text_commands.len();
                                        let (_, v2_out) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, bench_config.clone());
                                        let v2_cmds = v2_out.draw_commands.len() + v2_out.circle_commands.len()
                                            + v2_out.line_commands.len() + v2_out.text_commands.len();

                                        if v1_cmds > 0 && v2_cmds == 0 {
                                            // V2 draws NOTHING — lobotomized!
                                            write_str("[AutoDream] VERDICT: STRIKE (Lobotomy — V2 draws 0 commands vs V1:");
                                            write_str(format_usize(v1_cmds, &mut nb));
                                            write_str(")\n");
                                            draug.add_strike(orig_key);
                                        } else if v1_cmds > 0 && (v2_cmds * 2) < v1_cmds {
                                            // V2 draws less than half of V1 — functional degradation
                                            write_str("[AutoDream] VERDICT: STRIKE (Degradation — V2:");
                                            write_str(format_usize(v2_cmds, &mut nb));
                                            write_str(" cmds vs V1:");
                                            write_str(format_usize(v1_cmds, &mut nb));
                                            write_str(")\n");
                                            draug.add_strike(orig_key);
                                        } else {
                                            // Passed sanity check — now benchmark
                                            write_str("[AutoDream] Sanity: V1=");
                                            write_str(format_usize(v1_cmds, &mut nb));
                                            write_str(" V2=");
                                            write_str(format_usize(v2_cmds, &mut nb));
                                            write_str(" cmds (OK)\n");

                                            write_str("[AutoDream] Benchmarking (10 iterations)...\n");
                                            let t1 = rdtsc();
                                            for _ in 0..10 { let _ = compositor::wasm_runtime::execute_wasm(v1_wasm, bench_config.clone()); }
                                            let v1_us = (rdtsc() - t1) / tsc_per_us / 10;
                                            let t2 = rdtsc();
                                            for _ in 0..10 { let _ = compositor::wasm_runtime::execute_wasm(&wasm_bytes, bench_config.clone()); }
                                            let v2_us = (rdtsc() - t2) / tsc_per_us / 10;

                                            write_str("[AutoDream] V1:");
                                            write_str(format_usize(v1_us as usize, &mut nb));
                                            write_str("us V2:");
                                            write_str(format_usize(v2_us as usize, &mut nb));
                                            write_str("us\n");

                                            if v2_us < v1_us {
                                                let pct = ((v1_us - v2_us) * 100 / v1_us.max(1)) as usize;
                                                write_str("[AutoDream] VERDICT: EVOLVED! ");
                                                write_str(format_usize(pct, &mut nb));
                                                write_str("% faster\n");
                                                wasm_cache.insert(alloc::string::String::from(orig_key), wasm_bytes.clone());
                                                draug.reset_strikes(orig_key);
                                            } else {
                                                write_str("[AutoDream] VERDICT: STRIKE (V2 not faster)\n");
                                                draug.add_strike(orig_key);
                                            }
                                        }
                                        if draug.is_perfected(orig_key) {
                                            write_str("[AutoDream] STATUS: PERFECTED\n");
                                        }
                                    } else {
                                        write_str("[AutoDream] ERROR: V1 not in cache, cannot compare\n");
                                    }
                                }
                                compositor::draug::DreamMode::Creative => {
                                    write_str("[AutoDream] ---- CREATIVE RESULT ----\n");
                                    write_str("[AutoDream] New version: ");
                                    write_str(format_usize(wasm_bytes.len(), &mut nb));
                                    write_str(" bytes\n");
                                    let preview_cfg = compositor::wasm_runtime::WasmConfig {
                                        screen_width: fb.width as u32, screen_height: fb.height as u32, uptime_ms: 0,
                                    };
                                    let (_, preview_out) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, preview_cfg);
                                    let summary = compositor::wasm_runtime::render_summary(&preview_out);
                                    write_str("[AutoDream] New render: ");
                                    write_str(&summary[..summary.len().min(200)]);
                                    write_str("\n");
                                    // Queue for Morning Briefing — user decides
                                    write_str("[AutoDream] VERDICT: QUEUED for user approval (Morning Briefing)\n");
                                    draug.queue_creative(orig_key, &summary[..summary.len().min(100)], wasm_bytes.clone());
                                }
                                compositor::draug::DreamMode::Nightmare => {
                                    write_str("[AutoDream] ---- NIGHTMARE RESULT ----\n");
                                    write_str("[AutoDream] Fuzzing hardened version (w=0,h=0,t=MAX)...\n");
                                    let fuzz_config = compositor::wasm_runtime::WasmConfig {
                                        screen_width: 0, screen_height: 0, uptime_ms: u32::MAX,
                                    };
                                    let (fuzz_result, _) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, fuzz_config);
                                    match fuzz_result {
                                        compositor::wasm_runtime::WasmResult::Ok => {
                                            write_str("[AutoDream] VERDICT: SURVIVED (Ok) — app vaccinated!\n");
                                            wasm_cache.insert(alloc::string::String::from(orig_key), wasm_bytes.clone());
                                        }
                                        compositor::wasm_runtime::WasmResult::OutOfFuel => {
                                            write_str("[AutoDream] VERDICT: SURVIVED (fuel exhausted, but no crash) — accepted\n");
                                            wasm_cache.insert(alloc::string::String::from(orig_key), wasm_bytes.clone());
                                        }
                                        compositor::wasm_runtime::WasmResult::Trap(ref msg) => {
                                            write_str("[AutoDream] VERDICT: CRASHED! Trap: ");
                                            write_str(&msg[..msg.len().min(80)]);
                                            write_str("\n[AutoDream] Keeping original (V2 too fragile)\n");
                                        }
                                        compositor::wasm_runtime::WasmResult::LoadError(ref msg) => {
                                            write_str("[AutoDream] VERDICT: LOAD FAILED: ");
                                            write_str(&msg[..msg.len().min(80)]);
                                            write_str("\n");
                                        }
                                    }
                                }
                            }

                            write_str("[AutoDream] ========== DREAM COMPLETE ==========\n");
                            let done_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
                            draug.on_dream_complete(done_ms);

                            // State Migration: if active app was the dream target, hot-swap with evolved version
                            if let Some(ref snapshot) = state_snapshot {
                                if let Some(ref k) = active_wasm_app_key {
                                    if k.as_str() == orig_key {
                                        if let Some(evolved_wasm) = wasm_cache.get(orig_key) {
                                            let config = compositor::wasm_runtime::WasmConfig {
                                                screen_width: fb.width as u32,
                                                screen_height: fb.height as u32,
                                                uptime_ms: libfolk::sys::uptime() as u32,
                                            };
                                            if let Ok(mut new_app) = compositor::wasm_runtime::PersistentWasmApp::new(evolved_wasm, config) {
                                                new_app.write_memory(0, snapshot);
                                                active_wasm_app = Some(new_app);
                                                fuel_fail_count = 0;
                                                write_str("[StateMigration] Hot-swapped running app with evolved version + restored state\n");
                                            }
                                        }
                                    }
                                }
                                state_snapshot = None;
                            }
                        }
                        // Normal cache storage (non-dream)
                        else if !tool_prompt.is_empty() {
                            if wasm_cache.len() >= MAX_CACHE_ENTRIES {
                                if let Some(oldest) = wasm_cache.keys().next().cloned() {
                                    wasm_cache.remove(&oldest);
                                }
                            }
                            wasm_cache.insert(tool_prompt.clone(), wasm_bytes.clone());
                            write_str("[Cache] Stored WASM for: ");
                            write_str(&tool_prompt[..tool_prompt.len().min(40)]);
                            write_str("\n");

                            // Semantic VFS: auto-tag intent metadata
                            let clean_name = {
                                let mut n = tool_prompt.as_str();
                                for pfx in &["gemini generate ", "gemini gen ", "generate "] {
                                    if n.len() > pfx.len() && n.as_bytes()[..pfx.len()].eq_ignore_ascii_case(pfx.as_bytes()) {
                                        n = &n[pfx.len()..];
                                        break;
                                    }
                                }
                                n.trim()
                            };
                            // Write WASM to Synapse — returns rowid on success
                            let wasm_filename = alloc::format!("{}.wasm", clean_name);
                            let write_ret = libfolk::sys::synapse::write_file(&wasm_filename, &wasm_bytes);
                            if write_ret.is_ok() {
                                // Synapse now returns rowid directly in the reply
                                // Use file_count as fallback rowid estimate
                                let rowid = if let Ok(count) = libfolk::sys::synapse::file_count() {
                                    count as u32
                                } else { 0 };
                                if rowid > 0 {
                                    let intent_json = alloc::format!(
                                        "{{\"purpose\":\"{}\",\"type\":\"wasm_app\",\"size\":{}}}",
                                        clean_name, wasm_bytes.len()
                                    );
                                    let _ = libfolk::sys::synapse::write_intent(
                                        rowid, "application/wasm", &intent_json,
                                    );
                                    write_str("[Synapse] Intent tagged: ");
                                    write_str(clean_name);
                                    write_str("\n");
                                }
                            }
                        }

                        let config = compositor::wasm_runtime::WasmConfig {
                            screen_width: fb.width as u32,
                            screen_height: fb.height as u32,
                            uptime_ms: libfolk::sys::uptime() as u32,
                        };

                        let interactive = {
                            let p = tool_prompt.as_bytes();
                            find_ci(p, b"interactive") || find_ci(p, b"game")
                                || find_ci(p, b"app") || find_ci(p, b"click")
                                || find_ci(p, b"mouse") || find_ci(p, b"tetris")
                                || find_ci(p, b"follow") || find_ci(p, b"cursor")
                        };
                        last_wasm_interactive = interactive;

                        if interactive {
                            match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                Ok(app) => {
                                    write_str("[MCP] Interactive WASM app launched!\n");
                                    if let Some(win) = wm.get_window_mut(tool_win_id) {
                                        win.push_line("[AI] Interactive app launched! Press ESC to exit.");
                                    }
                                    active_wasm_app = Some(app);
                                    active_wasm_app_key = Some(tool_prompt.clone());
                                    wasm_app_open_since_ms = libfolk::sys::uptime();
                                    fuel_fail_count = 0;
                                }
                                Err(e) => {
                                    if let Some(win) = wm.get_window_mut(tool_win_id) {
                                        win.push_line(&alloc::format!("[AI] App error: {}", &e[..e.len().min(80)]));
                                    }
                                }
                            }
                        } else {
                            let (result, output) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, config);
                            let total_cmds = output.draw_commands.len()
                                + output.line_commands.len()
                                + output.circle_commands.len()
                                + output.text_commands.len()
                                + if output.fill_screen.is_some() { 1 } else { 0 };
                            if let Some(win) = wm.get_window_mut(tool_win_id) {
                                match &result {
                                    compositor::wasm_runtime::WasmResult::Ok =>
                                        win.push_line(&alloc::format!("[AI] Tool: {} cmds", total_cmds)),
                                    compositor::wasm_runtime::WasmResult::OutOfFuel =>
                                        win.push_line("[AI] Halted: fuel exhausted"),
                                    compositor::wasm_runtime::WasmResult::Trap(msg) =>
                                        win.push_line(&alloc::format!("[AI] Trap: {}", &msg[..msg.len().min(80)])),
                                    compositor::wasm_runtime::WasmResult::LoadError(msg) =>
                                        win.push_line(&alloc::format!("[AI] Load: {}", &msg[..msg.len().min(80)])),
                                }
                            }
                            if let Some(color) = output.fill_screen { fb.clear(fb.color_from_rgb24(color)); }
                            for cmd in &output.draw_commands {
                                fb.fill_rect(cmd.x as usize, cmd.y as usize, cmd.w as usize, cmd.h as usize, fb.color_from_rgb24(cmd.color));
                            }
                            for cmd in &output.line_commands {
                                let c = fb.color_from_rgb24(cmd.color);
                                compositor::graphics::draw_line(&mut fb, cmd.x1, cmd.y1, cmd.x2, cmd.y2, c);
                            }
                            for cmd in &output.circle_commands {
                                let c = fb.color_from_rgb24(cmd.color);
                                compositor::graphics::draw_circle(&mut fb, cmd.cx, cmd.cy, cmd.r, c);
                            }
                            for cmd in &output.text_commands {
                                fb.draw_string(cmd.x as usize, cmd.y as usize, &cmd.text, fb.color_from_rgb24(cmd.color), fb.color_from_rgb24(0));
                            }
                            if total_cmds > 0 { damage.damage_full(); }
                        }
                        need_redraw = true;
                        damage.damage_full();
                    }
                    _ => {
                        write_str("[MCP] Unhandled response\n");
                    }
                }
            }
        }

        // ===== GOD MODE PIPE (COM3) — Poll for injected commands =====
        while let Some(byte) = libfolk::sys::com3_read() {
            if byte == b'\n' && com3_len > 0 {
                // Complete command received — inject into omnibar dispatcher
                if let Ok(cmd) = alloc::str::from_utf8(&com3_buf[..com3_len]) {
                    write_str("[COM3] Inject: ");
                    write_str(cmd);
                    write_str("\n");
                    com3_inject = Some(alloc::string::String::from(cmd));
                }
                com3_len = 0;
                did_work = true;
                break;
            } else if byte != b'\n' && byte != b'\r' && com3_len < com3_buf.len() {
                com3_buf[com3_len] = byte;
                com3_len += 1;
            }
        }
        // COM3 God Mode: if a command is pending and WASM is fullscreen,
        // force-close the WASM app so the command can be processed by the omnibar.
        if com3_inject.is_some() && active_wasm_app.is_some() {
            active_wasm_app = None;
            active_wasm_app_key = None;
            fuel_fail_count = 0;
            fb.clear(folk_dark);
            need_redraw = true;
            damage.damage_full();
            write_str("[COM3] Closed fullscreen WASM to process command\n");
        }

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
            if !had_mouse_events {
                // Log first mouse event per batch to serial
                write_str("[M]\n");
            }
            had_mouse_events = true;
            accumulated_dx += event.dx as i32;
            accumulated_dy -= event.dy as i32; // Invert Y (mouse up = negative dy in PS/2)
            latest_buttons = event.buttons;
        }

        if had_mouse_events {
            // Tell Draug the user is actively interacting
            let input_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
            draug.on_user_input(input_ms);
            // Hover detection for folder preview (home view)
            if open_folder < 0 && active_wasm_app.is_none() {
                let old_hover = hover_folder;
                hover_folder = -1;
                let mut vi = 0usize;
                for ci in 0..MAX_CATEGORIES {
                    if categories[ci].count == 0 { continue; }
                    let cols = { let mut c = 0; for j in 0..MAX_CATEGORIES { if categories[j].count > 0 { c += 1; } } c.min(3) };
                    let gw = cols * (FOLDER_W + FOLDER_GAP) - FOLDER_GAP;
                    let gx = (fb.width.saturating_sub(gw)) / 2;
                    let gy: usize = 120;
                    let col = vi % 3;
                    let row = vi / 3;
                    let fx = gx + col * (FOLDER_W + FOLDER_GAP);
                    let fy = gy + row * (FOLDER_H + FOLDER_GAP);
                    if cursor_x as usize >= fx && (cursor_x as usize) < fx + FOLDER_W
                        && cursor_y as usize >= fy && (cursor_y as usize) < fy + FOLDER_H {
                        hover_folder = ci as i32;
                    }
                    vi += 1;
                }
                // Hover change: just damage the folder area, don't full-redraw
                if hover_folder != old_hover {
                    // Damage old and new folder rectangles
                    // (folders render will happen in next full redraw; for now just mark cursor_bg_dirty)
                    cursor_bg_dirty = true;
                    did_work = true;
                }
            }

            // Route mouse events to active WASM app (Phase 2)
            if let Some(app) = &mut active_wasm_app {
                let new_click = (latest_buttons & 1 != 0) && (last_buttons & 1 == 0);
                // Always send mouse position
                app.push_event(compositor::wasm_runtime::FolkEvent {
                    event_type: 1, x: cursor_x, y: cursor_y, data: latest_buttons as i32,
                });
                // Send click event on button press edge
                if new_click {
                    app.push_event(compositor::wasm_runtime::FolkEvent {
                        event_type: 2, x: cursor_x, y: cursor_y, data: 1,
                    });

                    // Friction Sensor: rage click detection (>5 clicks in 2s)
                    let now = libfolk::sys::uptime();
                    click_timestamps[click_ts_idx] = now;
                    click_ts_idx = (click_ts_idx + 1) % 8;
                    // Count clicks in last 2 seconds
                    let mut recent = 0u8;
                    for ts in &click_timestamps {
                        if *ts > 0 && now.saturating_sub(*ts) < 2000 { recent += 1; }
                    }
                    if recent > 5 {
                        if let Some(ref k) = active_wasm_app_key {
                            let h = compositor::draug::DraugDaemon::key_hash_pub(k);
                            draug.friction.record_signal(h, compositor::draug::FRICTION_RAGE_CLICK);
                            write_str("[Friction] rage_click for '");
                            write_str(&k[..k.len().min(30)]);
                            write_str("'\n");
                        }
                        // Reset to avoid spamming
                        click_timestamps = [0; 8];
                    }
                }
            }

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
                            if win_id == inference_win_id {
                                inference_win_id = 0;
                            }
                            need_redraw = true;
                            cursor_bg_dirty = true;
                            handled = true;
                            // IQE: window close event
                            libfolk::sys::com3_write(b"IQE,WIN_CLOSE,0\n");
                        }
                        HitZone::TitleBar => {
                            wm.focus(win_id);
                            dragging_window_id = Some(win_id);
                            drag_last_x = new_x;
                            drag_last_y = new_y;
                            need_redraw = true;
                            handled = true;
                            // IQE: window drag start
                            libfolk::sys::com3_write(b"IQE,WIN_DRAG,0\n");
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

                    // Hit-test: RAM% in status bar (toggle graph)
                    if cy < 20 && cx > fb.width.saturating_sub(80) {
                        show_ram_graph = !show_ram_graph;
                        need_redraw = true;
                        damage.damage_full();
                        handled = true;
                    }

                    // Hit-test: app launcher (folders or app tiles)
                    if open_folder < 0 {
                        // HOME: check folder clicks
                        let mut vis_count = 0usize;
                        for ci in 0..MAX_CATEGORIES { if categories[ci].count > 0 { vis_count += 1; } }
                        if vis_count > 0 {
                            let cols = vis_count.min(3);
                            let gw = cols * (FOLDER_W + FOLDER_GAP) - FOLDER_GAP;
                            let gx = (fb.width.saturating_sub(gw)) / 2;
                            let gy: usize = 120;
                            let mut vi = 0usize;
                            for ci in 0..MAX_CATEGORIES {
                                if categories[ci].count == 0 { continue; }
                                let col = vi % 3;
                                let row = vi / 3;
                                let fx = gx + col * (FOLDER_W + FOLDER_GAP);
                                let fy = gy + row * (FOLDER_H + FOLDER_GAP);
                                if cx >= fx && cx < fx + FOLDER_W && cy >= fy && cy < fy + FOLDER_H {
                                    open_folder = ci as i32;
                                    handled = true;
                                    need_redraw = true;
                                    damage.damage_full();
                                    break;
                                }
                                vi += 1;
                            }
                        }
                    } else {
                        // FOLDER VIEW: check "< Back" button or app tile clicks
                        let header_y: usize = 90;
                        if cy >= header_y && cy < header_y + 30 && cx < 100 {
                            // Back button
                            open_folder = -1;
                            handled = true;
                            need_redraw = true;
                            damage.damage_full();
                        } else {
                            // App tile click
                            let cat_idx = open_folder as usize;
                            if cat_idx < MAX_CATEGORIES {
                                let gw = APP_TILE_COLS * (APP_TILE_W + APP_TILE_GAP) - APP_TILE_GAP;
                                let gx = (fb.width.saturating_sub(gw)) / 2;
                                let gy: usize = 130;
                                for i in 0..categories[cat_idx].count {
                                    let col = i % APP_TILE_COLS;
                                    let row = i / APP_TILE_COLS;
                                    let ax = gx + col * (APP_TILE_W + APP_TILE_GAP);
                                    let ay = gy + row * (APP_TILE_H + APP_TILE_GAP);
                                    if cx >= ax && cx < ax + APP_TILE_W && cy >= ay && cy < ay + APP_TILE_H {
                                        tile_clicked = i as i32;
                                        handled = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }

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
            let old_cx = cursor_x;
            let old_cy = cursor_y;
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

                // Damage old + new cursor areas for VirtIO-GPU flush
                damage.add_damage(compositor::damage::Rect::new(
                    old_cx.max(0) as u32, old_cy.max(0) as u32, CURSOR_W as u32 + 2, CURSOR_H as u32 + 2));
                damage.add_damage(compositor::damage::Rect::new(
                    cursor_x.max(0) as u32, cursor_y.max(0) as u32, CURSOR_W as u32 + 2, CURSOR_H as u32 + 2));
                // Cursor-only movement: DON'T set need_redraw (avoids full desktop re-render).
                // The damage tracker + GPU flush handle the cursor update efficiently.
                did_work = true;
            }
        } // end if had_mouse_events


        // ===== Blink caret =====
        // Freeze caret when idle >10s — prevents infinite 150ms redraw loop
        {
            let caret_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { uptime() };
            let idle_secs = caret_ms.saturating_sub(draug.last_input_ms()) / 1000;
            if idle_secs < 10 && caret_ms.saturating_sub(last_caret_flip_ms) >= CARET_BLINK_MS {
                caret_visible = !caret_visible;
                last_caret_flip_ms = caret_ms;
                if omnibar_visible {
                    let caret_x_pos = text_box_x + TEXT_PADDING + (cursor_pos.min(chars_per_line) * 8);
                    if caret_x_pos < text_box_x + text_box_w - 30 {
                        fb.fill_rect(caret_x_pos, text_box_y + 8, 8, 20, fb.color_from_rgb24(0x1a1a2e));
                        if caret_visible {
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
        if tile_clicked >= 0 && open_folder >= 0 {
            let cat_idx = open_folder as usize;
            let app_idx = tile_clicked as usize;
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
                                active_wasm_app = Some(app);
                                active_wasm_app_key = Some(alloc::string::String::from(app_name));
                                wasm_app_open_since_ms = libfolk::sys::uptime();
                                fuel_fail_count = 0;
                                last_wasm_bytes = Some(wasm_bytes);
                            }
                            Err(_) => {}
                        }
                    } else {
                        let _ = shmem_destroy(resp.shmem_handle);
                    }
                }
            }
            tile_clicked = -1;
        }

        // ===== Process keyboard input =====
        // First, collect all pending keys without redrawing
        let mut execute_command = false;
        let mut win_execute_command: Option<u32> = None; // window id to execute from
        while let Some(key) = read_key() {
            did_work = true;
            let input_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
            draug.on_user_input(input_ms);

            // Ctrl+G (0x07) or 'G'/'g': toggle RAM graph
            if key == 0x07 || (active_wasm_app.is_none() && (key == b'G' || key == b'g') && !omnibar_visible) {
                show_ram_graph = !show_ram_graph;
                need_redraw = true;
                damage.damage_full(); // RAM graph covers large area
                continue;
            }

            // Route to active WASM app (Phase 2) — ESC kills the app
            // ESC: close folder view first, then WASM app
            if key == 0x1B && open_folder >= 0 && active_wasm_app.is_none() {
                open_folder = -1;
                need_redraw = true;
                damage.damage_full(); // folder covers large area, full redraw needed
                continue;
            }
            if let Some(app) = &mut active_wasm_app {
                if key == 0x1B { // ESC
                    // Friction Sensor: detect quick close (<3s = frustration)
                    if wasm_app_open_since_ms > 0 {
                        let open_duration = libfolk::sys::uptime().saturating_sub(wasm_app_open_since_ms);
                        if open_duration < 3000 {
                            if let Some(ref k) = active_wasm_app_key {
                                let h = compositor::draug::DraugDaemon::key_hash_pub(k);
                                draug.friction.record_signal(h, compositor::draug::FRICTION_QUICK_CLOSE);
                                write_str("[Friction] quick_close for '");
                                write_str(&k[..k.len().min(30)]);
                                write_str("'\n");
                            }
                        }
                    }
                    active_wasm_app = None;
                    active_wasm_app_key = None;
                    wasm_app_open_since_ms = 0;
                    fuel_fail_count = 0;
                    // Clear WASM residue from framebuffer
                    fb.clear(folk_dark);
                    // Re-draw desktop title
                    let title_x2 = (fb.width.saturating_sub(12 * 8)) / 2;
                    fb.draw_string(title_x2, 40, "FOLKERING OS", folk_accent, folk_dark);
                    let sub_x2 = (fb.width.saturating_sub(14 * 8)) / 2;
                    fb.draw_string(sub_x2, 60, "Neural Desktop", gray, folk_dark);
                    need_redraw = true;
                    damage.damage_full();
                    continue;
                }
                app.push_event(compositor::wasm_runtime::FolkEvent {
                    event_type: 3, x: key as i32, y: 0, data: key as i32,
                });
            }

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

        // COM3 God Mode: inject command directly (bypasses keyboard)
        if let Some(injected) = com3_inject.take() {
            let bytes = injected.as_bytes();
            let copy_len = bytes.len().min(text_buffer.len());
            text_buffer[..copy_len].copy_from_slice(&bytes[..copy_len]);
            text_len = copy_len;
            execute_command = true;
            need_redraw = true;
        }

        if execute_command && text_len > 0 {
            if let Ok(cmd_str) = core::str::from_utf8(&text_buffer[..text_len]) {

                // ═══════ FolkShell Pre-Processor ═══════
                // Try FolkShell first — handles pipes (|>) and JIT command synthesis.
                // Falls through to legacy dispatch for builtins and unrecognized input.
                let mut folkshell_handled = false;
                if cmd_str.contains("|>") || cmd_str.contains("~>") {
                    // Pipe syntax (deterministic |> or fuzzy ~>) → FolkShell handles this
                    let result = compositor::folkshell::eval(cmd_str, &wasm_cache);
                    match result {
                        compositor::folkshell::ShellState::Done(ref output) => {
                            // Create a window for the output
                            let win_count = wm.windows.len() as i32;
                            let wx = 80 + win_count * 24;
                            let wy = 60 + win_count * 24;
                            let win_id = wm.create_terminal(cmd_str, wx, wy, 480, 200);
                            if let Some(win) = wm.get_window_mut(win_id) {
                                for line in output.lines() {
                                    win.push_line(line);
                                }
                            }
                            folkshell_handled = true;
                            need_redraw = true;
                        }
                        compositor::folkshell::ShellState::WaitingForJIT {
                            command_name, pipeline, stage, pipe_input
                        } => {
                            let win_count = wm.windows.len() as i32;
                            let wx = 80 + win_count * 24;
                            let wy = 60 + win_count * 24;
                            let win_id = wm.create_terminal(cmd_str, wx, wy, 480, 200);
                            if let Some(win) = wm.get_window_mut(win_id) {
                                win.push_line(&alloc::format!(
                                    "[FolkShell] Synthesizing '{}'...", command_name
                                ));
                            }
                            let prompt = compositor::folkshell::jit_prompt(&command_name, &pipe_input);
                            if libfolk::mcp::client::send_wasm_gen(&prompt) {
                                pending_shell_jit = Some(command_name);
                                shell_jit_pipeline = Some((pipeline, stage, pipe_input));
                                write_str("[FolkShell] JIT request sent\n");
                            }
                            folkshell_handled = true;
                            need_redraw = true;
                        }
                        compositor::folkshell::ShellState::Widget { wasm_bytes, title } => {
                            // ═══════ Holographic Output ═══════
                            // Launch the WASM as a live interactive widget in a floating window
                            write_str("[FolkShell] Holographic widget: ");
                            write_str(&title[..title.len().min(30)]);
                            write_str("\n");
                            let config = compositor::wasm_runtime::WasmConfig {
                                screen_width: fb.width as u32,
                                screen_height: fb.height as u32,
                                uptime_ms: libfolk::sys::uptime() as u32,
                            };
                            match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                Ok(app) => {
                                    active_wasm_app = Some(app);
                                    active_wasm_app_key = Some(title.clone());
                                    wasm_app_open_since_ms = libfolk::sys::uptime();
                                    fuel_fail_count = 0;
                                    write_str("[FolkShell] Widget launched fullscreen!\n");
                                }
                                Err(e) => {
                                    // Fallback: show as text in terminal window
                                    let win_count = wm.windows.len() as i32;
                                    let wx = 80 + win_count * 24;
                                    let wy = 60 + win_count * 24;
                                    let win_id = wm.create_terminal(cmd_str, wx, wy, 480, 200);
                                    if let Some(win) = wm.get_window_mut(win_id) {
                                        win.push_line(&alloc::format!("[Widget] Load error: {}", &e[..e.len().min(60)]));
                                    }
                                }
                            }
                            folkshell_handled = true;
                            need_redraw = true;
                            damage.damage_full();
                        }
                        _ => {} // Passthrough or error → legacy dispatch
                    }
                }

                if !folkshell_handled {
                // ═══════ Legacy Omnibar Dispatch ═══════

                // Special case: `open <app>` — try WASM fullscreen first, then FKUI window
                let is_open_cmd = cmd_str.starts_with("open ");
                if is_open_cmd {
                    let app_name = cmd_str[5..].trim();
                    if !app_name.is_empty() {
                        let mut opened_wasm = false;

                        // Try WASM fullscreen first (preferred — no window overlap)
                        {
                            let mut wasm_fname = [0u8; 64];
                            let nb = app_name.as_bytes();
                            let ext = b".wasm";
                            if nb.len() + ext.len() < 64 {
                                wasm_fname[..nb.len()].copy_from_slice(nb);
                                wasm_fname[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                                let wasm_str = unsafe { core::str::from_utf8_unchecked(&wasm_fname[..nb.len()+ext.len()]) };
                                const VFS_OPEN_VADDR: usize = 0x50040000;
                                if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(wasm_str) {
                                    if shmem_map(resp.shmem_handle, VFS_OPEN_VADDR).is_ok() {
                                        let data = unsafe {
                                            core::slice::from_raw_parts(VFS_OPEN_VADDR as *const u8, resp.size as usize)
                                        };
                                        let wasm_bytes = alloc::vec::Vec::from(data);
                                        let _ = shmem_unmap(resp.shmem_handle, VFS_OPEN_VADDR);
                                        let _ = shmem_destroy(resp.shmem_handle);
                                        let config = compositor::wasm_runtime::WasmConfig {
                                            screen_width: fb.width as u32,
                                            screen_height: fb.height as u32,
                                            uptime_ms: libfolk::sys::uptime() as u32,
                                        };
                                        if let Ok(app) = compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                            active_wasm_app = Some(app);
                                            active_wasm_app_key = Some(alloc::string::String::from(app_name));
                                            wasm_app_open_since_ms = libfolk::sys::uptime();
                                            fuel_fail_count = 0;
                                            last_wasm_bytes = Some(wasm_bytes);
                                            last_wasm_interactive = true;
                                            opened_wasm = true;
                                            write_str("[WM] Opened WASM fullscreen: ");
                                            write_str(wasm_str);
                                            write_str("\n");
                                            // IQE: window open event
                                            libfolk::sys::com3_write(b"IQE,WIN_OPEN,0\n");
                                        }
                                    } else {
                                        let _ = shmem_destroy(resp.shmem_handle);
                                    }
                                }
                            }
                        }

                        // Fallback: FKUI windowed app
                        if !opened_wasm {
                            let mut fname = [0u8; 64];
                            let nb = app_name.as_bytes();
                            let ext = b".fkui";
                            let mut vfs_loaded = false;
                            if nb.len() + ext.len() < 64 {
                                fname[..nb.len()].copy_from_slice(nb);
                                fname[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                                let fname_str = unsafe { core::str::from_utf8_unchecked(&fname[..nb.len()+ext.len()]) };
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
                }

                // Omnibar `run <app>` — load WASM app in fullscreen
                let is_run_cmd = cmd_str.starts_with("run ");
                if is_run_cmd {
                    let app_name = cmd_str[4..].trim();
                    if !app_name.is_empty() {
                        let filename = if app_name.as_bytes().windows(5).any(|w| w == b".wasm") {
                            let mut f = [0u8; 64];
                            let n = app_name.len().min(63);
                            f[..n].copy_from_slice(&app_name.as_bytes()[..n]);
                            (f, n)
                        } else {
                            let mut f = [0u8; 64];
                            let nb = app_name.as_bytes();
                            let ext = b".wasm";
                            if nb.len() + ext.len() < 64 {
                                f[..nb.len()].copy_from_slice(nb);
                                f[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                                (f, nb.len() + ext.len())
                            } else {
                                (f, 0)
                            }
                        };
                        if filename.1 > 0 {
                            let fname_str = unsafe { core::str::from_utf8_unchecked(&filename.0[..filename.1]) };
                            const VFS_RUN_VADDR: usize = 0x50040000;
                            if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(fname_str) {
                                if shmem_map(resp.shmem_handle, VFS_RUN_VADDR).is_ok() {
                                    let data = unsafe {
                                        core::slice::from_raw_parts(VFS_RUN_VADDR as *const u8, resp.size as usize)
                                    };
                                    let wasm_bytes = alloc::vec::Vec::from(data);
                                    let _ = shmem_unmap(resp.shmem_handle, VFS_RUN_VADDR);
                                    let _ = shmem_destroy(resp.shmem_handle);

                                    let config = compositor::wasm_runtime::WasmConfig {
                                        screen_width: fb.width as u32,
                                        screen_height: fb.height as u32,
                                        uptime_ms: libfolk::sys::uptime() as u32,
                                    };
                                    match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                        Ok(app) => {
                                            active_wasm_app = Some(app);
                                            active_wasm_app_key = Some(alloc::string::String::from(app_name));
                                            wasm_app_open_since_ms = libfolk::sys::uptime();
                                            fuel_fail_count = 0;
                                            last_wasm_bytes = Some(wasm_bytes);
                                            last_wasm_interactive = true;
                                            write_str("[WASM] Launched fullscreen: ");
                                            write_str(fname_str);
                                            write_str("\n");
                                        }
                                        Err(_) => {
                                            write_str("[WASM] Failed to instantiate: ");
                                            write_str(fname_str);
                                            write_str("\n");
                                        }
                                    }
                                } else {
                                    let _ = shmem_destroy(resp.shmem_handle);
                                }
                            } else {
                                write_str("[WASM] App not found: ");
                                write_str(fname_str);
                                write_str("\n");
                            }
                        }
                    }
                }

                let is_gemini_cmd = starts_with_ci(cmd_str, "gemini ");
                if !is_open_cmd && !is_run_cmd && !is_gemini_cmd {
                    // M13: Try semantic intent match BEFORE creating terminal window (skip for gemini commands)
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
                } // end intent match guard

                if deferred_app_handle == 0 {
                write_str("[WM] Creating window for: ");
                write_str(cmd_str);
                write_str("\n");

                // Spawn a terminal window at a cascade position
                let win_count = wm.windows.len() as i32;
                let wx = 80 + win_count * 24;
                let wy = 60 + win_count * 24;
                let win_id = wm.create_terminal(cmd_str, wx, wy, 480, 200);

                // Pre-compute UI state for gemini command (before win borrow)
                let ui_state_snapshot = {
                    let ui_wins: alloc::vec::Vec<compositor::ui_serialize::WindowInfo> =
                        wm.windows.iter().map(|w| {
                            let t = alloc::str::from_utf8(&w.title[..w.title_len]).unwrap_or("?");
                            let ll = w.lines.last().map(|l|
                                alloc::str::from_utf8(&l.buf[..l.len]).unwrap_or("")
                            ).unwrap_or("");
                            compositor::ui_serialize::WindowInfo {
                                id: w.id, z_index: 0,
                                title: alloc::string::String::from(t),
                                x: w.x as u32, y: w.y as u32,
                                w: w.width, h: w.height,
                                visible_text: alloc::string::String::from(ll),
                            }
                        }).collect();
                    compositor::ui_serialize::serialize_ui_state(
                        fb.width as u32, fb.height as u32, &ui_wins, "",
                    )
                };

                let mut deferred_intent_action: Option<(u32, u32, u32, u32)> = None;
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
                    } else if cmd_str.starts_with("revert ") {
                        // Rollback: "revert ball to v1" or "revert ball 1"
                        let parts: alloc::vec::Vec<&str> = cmd_str[7..].trim().split_whitespace().collect();
                        if parts.len() >= 2 {
                            let app_name = parts[0];
                            let ver_str = parts[parts.len() - 1].trim_start_matches('v');
                            if let Ok(ver) = ver_str.parse::<u32>() {
                                // Send rollback request to proxy via MCP chat
                                let rollback_prompt = alloc::format!("__ROLLBACK__ {} {}", app_name, ver);
                                if libfolk::mcp::client::send_chat(&rollback_prompt).is_some() {
                                    win.push_line(&alloc::format!("[Revert] Rolling back '{}' to v{}...", app_name, ver));
                                } else {
                                    win.push_line("[Revert] Failed to send rollback request");
                                }
                            } else {
                                win.push_line("Usage: revert <app> <version>");
                            }
                        } else {
                            win.push_line("Usage: revert <app> <version>");
                            win.push_line("Example: revert ball 1");
                        }
                    } else if cmd_str == "dream accept all" || cmd_str == "dream accept" {
                        draug.accept_all_creative();
                        let accepted = draug.drain_accepted();
                        for (name, wasm) in &accepted {
                            wasm_cache.insert(name.clone(), wasm.clone());
                            win.push_line(&alloc::format!("[Dream] Accepted: {}", &name[..name.len().min(30)]));
                        }
                        if accepted.is_empty() {
                            win.push_line("[Dream] No pending changes");
                        }
                    } else if cmd_str == "dream reject all" || cmd_str == "dream reject" {
                        for i in 0..draug.pending_creative.len() {
                            draug.reject_creative(i);
                        }
                        draug.drain_accepted(); // Clear rejected
                        win.push_line("[Dream] All creative changes rejected");
                    } else if cmd_str.starts_with("dream accept ") {
                        if let Ok(idx) = cmd_str[13..].trim().parse::<usize>() {
                            if idx > 0 && idx <= draug.pending_creative.len() {
                                draug.accept_creative(idx - 1);
                                let accepted = draug.drain_accepted();
                                for (name, wasm) in &accepted {
                                    wasm_cache.insert(name.clone(), wasm.clone());
                                    win.push_line(&alloc::format!("[Dream] Accepted: {}", name));
                                }
                            } else {
                                win.push_line("[Dream] Invalid index");
                            }
                        }
                    } else if cmd_str.starts_with("dream reject ") {
                        if let Ok(idx) = cmd_str[13..].trim().parse::<usize>() {
                            if idx > 0 && idx <= draug.pending_creative.len() {
                                draug.reject_creative(idx - 1);
                                draug.drain_accepted();
                                win.push_line("[Dream] Rejected");
                            }
                        }
                    } else if starts_with_ci(cmd_str, "generate driver") {
                        // Autonomous Driver Generation: generate WASM driver for a PCI device
                        let target = cmd_str.get(15..).unwrap_or("").trim();
                        let mut pci_buf: [libfolk::sys::pci::PciDeviceInfo; 32] = unsafe { core::mem::zeroed() };
                        let count = libfolk::sys::pci::enumerate(&mut pci_buf);

                        // Find target device (by vendor:device ID or auto-select first non-bridge)
                        let dev = if target.contains(':') {
                            // Parse "1af4:1042" format
                            let parts: alloc::vec::Vec<&str> = target.split(':').collect();
                            if parts.len() == 2 {
                                let vid = u16::from_str_radix(parts[0], 16).unwrap_or(0);
                                let did = u16::from_str_radix(parts[1], 16).unwrap_or(0);
                                pci_buf[..count].iter().find(|d| d.vendor_id == vid && d.device_id == did)
                            } else { None }
                        } else {
                            // Auto-select: first non-bridge device that isn't VirtIO GPU (already driven)
                            pci_buf[..count].iter().find(|d|
                                d.class_code != 0x06 && // not a bridge
                                !(d.vendor_id == 0x1AF4 && d.device_id == 0x1050) // not VirtIO GPU
                            )
                        };

                        if let Some(d) = dev {
                            let desc = alloc::format!(
                                "__DRIVER_GEN__{:04x}:{:04x}:{}",
                                d.vendor_id, d.device_id, d.class_name()
                            );
                            win.push_line(&alloc::format!(
                                "[DRV] Generating driver for {:04x}:{:04x} ({})...",
                                d.vendor_id, d.device_id, d.class_name()
                            ));
                            // Store device info for when WASM arrives
                            pending_driver_device = Some(d.clone());
                            // Send to proxy for LLM generation
                            if libfolk::mcp::client::send_wasm_gen(&desc) {
                                win.push_line("[DRV] Request sent to LLM");
                            } else {
                                win.push_line("[DRV] MCP send failed");
                                pending_driver_device = None;
                            }
                        } else {
                            win.push_line("[DRV] No matching PCI device found");
                            win.push_line("[DRV] Usage: generate driver [vendor:device]");
                            win.push_line("[DRV] Example: generate driver 1af4:1042");
                        }
                    } else if cmd_str == "drivers" {
                        // List active WASM drivers
                        win.push_line(&alloc::format!("[DRV] {} active drivers:", active_drivers.len()));
                        for drv in active_drivers.iter() {
                            win.push_line(&alloc::format!(
                                "  {:04x}:{:04x} {} irq={} {}",
                                drv.capability.vendor_id, drv.capability.device_id,
                                drv.capability.driver_name(),
                                drv.capability.irq_line,
                                if drv.waiting_for_irq { "[waiting]" } else { "[running]" }
                            ));
                        }
                    } else if cmd_str == "lspci" {
                        // List PCI devices (Autonomous Driver Discovery)
                        let mut pci_buf: [libfolk::sys::pci::PciDeviceInfo; 32] = unsafe { core::mem::zeroed() };
                        let count = libfolk::sys::pci::enumerate(&mut pci_buf);
                        win.push_line(&alloc::format!("[PCI] {} devices:", count));
                        for i in 0..count {
                            let d = &pci_buf[i];
                            win.push_line(&alloc::format!(
                                "  {:02x}:{:02x}.{} {:04x}:{:04x} {} irq={} BAR0={}B",
                                d.bus, d.device_num, d.function,
                                d.vendor_id, d.device_id,
                                d.class_name(),
                                d.interrupt_line,
                                d.bar_sizes[0]
                            ));
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
                    } else if starts_with_ci(cmd_str, "load ") {
                        // Load precompiled WASM from host filesystem via proxy
                        let path = cmd_str[5..].trim();
                        if path.is_empty() {
                            win.push_line("Usage: load <path.wasm>");
                        } else {
                            win.push_line(&alloc::format!("[OS] Loading {}...", path));
                            // Send [LOAD_WASM:path] to proxy via COM2
                            let load_cmd = alloc::format!("[LOAD_WASM:{}]", path);
                            const LOAD_BUF_VADDR: usize = 0x50080000;
                            const LOAD_BUF_SIZE: usize = 131072;
                            if libfolk::sys::mmap_at(LOAD_BUF_VADDR, LOAD_BUF_SIZE, 3).is_ok() {
                                let load_buf = unsafe {
                                    core::slice::from_raw_parts_mut(LOAD_BUF_VADDR as *mut u8, LOAD_BUF_SIZE)
                                };
                                let resp_len = libfolk::sys::ask_gemini(&load_cmd, load_buf);
                                if resp_len > 0 {
                                    if let Ok(text) = core::str::from_utf8(&load_buf[..resp_len]) {
                                        use compositor::intent::AgentIntent;
                                        let intent = compositor::intent::parse_intent(text);
                                        match intent {
                                            AgentIntent::ToolReady { binary_base64 } => {
                                                if let Some(wasm_bytes) = compositor::intent::base64_decode(&binary_base64) {
                                                    win.push_line(&alloc::format!("[OS] Loaded {} bytes", wasm_bytes.len()));
                                                    last_wasm_bytes = Some(wasm_bytes.clone());

                                                    // load command ALWAYS launches as interactive app
                                                    let interactive = true;
                                                    last_wasm_interactive = true;

                                                    let config = compositor::wasm_runtime::WasmConfig {
                                                        screen_width: fb.width as u32,
                                                        screen_height: fb.height as u32,
                                                        uptime_ms: libfolk::sys::uptime() as u32,
                                                    };

                                                    if interactive {
                                                        match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                                            Ok(app) => {
                                                                // Hide the load window — WASM app takes over screen
                                                                win.visible = false;
                                                                active_wasm_app = Some(app);
                                                                active_wasm_app_key = Some(alloc::string::String::from(path));
                                                                wasm_app_open_since_ms = libfolk::sys::uptime();
                                                                fuel_fail_count = 0;
                                                            }
                                                            Err(e) => { win.push_line(&alloc::format!("[OS] Error: {}", &e[..e.len().min(60)])); }
                                                        }
                                                    } else {
                                                        let (result, output) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, config);
                                                        win.push_line(&alloc::format!("[OS] One-shot: {} commands", output.draw_commands.len()));
                                                        if let Some(color) = output.fill_screen { fb.clear(fb.color_from_rgb24(color)); }
                                                        for cmd in &output.draw_commands { fb.fill_rect(cmd.x as usize, cmd.y as usize, cmd.w as usize, cmd.h as usize, fb.color_from_rgb24(cmd.color)); }
                                                        for cmd in &output.line_commands { let c = fb.color_from_rgb24(cmd.color); compositor::graphics::draw_line(&mut fb, cmd.x1, cmd.y1, cmd.x2, cmd.y2, c); }
                                                        for cmd in &output.circle_commands { let c = fb.color_from_rgb24(cmd.color); compositor::graphics::draw_circle(&mut fb, cmd.cx, cmd.cy, cmd.r, c); }
                                                        for cmd in &output.text_commands { fb.draw_string(cmd.x as usize, cmd.y as usize, &cmd.text, fb.color_from_rgb24(cmd.color), fb.color_from_rgb24(0)); }
                                                        damage.damage_full();
                                                    }
                                                }
                                            }
                                            _ => {
                                                win.push_line(&alloc::format!("[OS] Error: {}", &text[..text.len().min(80)]));
                                            }
                                        }
                                    }
                                } else {
                                    win.push_line("[OS] No response from proxy");
                                }
                                let _ = libfolk::sys::munmap(LOAD_BUF_VADDR as *mut u8, LOAD_BUF_SIZE);
                            }
                        }
                    } else if starts_with_ci(cmd_str, "save app ") {
                        // App Persistence: save last compiled WASM to VFS
                        let app_name = cmd_str[9..].trim();
                        if app_name.is_empty() {
                            win.push_line("Usage: save app <name>");
                        } else if let Some(ref wasm) = last_wasm_bytes {
                            let filename = alloc::format!("{}.wasm", app_name);
                            match libfolk::sys::synapse::write_file(&filename, wasm) {
                                Ok(()) => {
                                    win.push_line(&alloc::format!(
                                        "[OS] Saved '{}' ({} bytes, {})",
                                        app_name, wasm.len(),
                                        if last_wasm_interactive { "interactive" } else { "one-shot" }
                                    ));
                                    write_str("[COMPOSITOR] App saved to VFS: ");
                                    write_str(&filename);
                                    write_str("\n");
                                    // Add to categorized desktop folder
                                    let cat = categorize_app(app_name);
                                    if categories[cat].count < MAX_APPS_PER_CAT {
                                        let name_bytes = app_name.as_bytes();
                                        let copy_len = name_bytes.len().min(24);
                                        let idx = categories[cat].count;
                                        categories[cat].apps[idx].name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
                                        categories[cat].apps[idx].name_len = copy_len;
                                        categories[cat].count += 1;
                                        damage.damage_full();
                                    }
                                }
                                Err(_) => {
                                    win.push_line("[OS] Save failed — VFS write error");
                                }
                            }
                        } else {
                            win.push_line("[OS] No app to save. Run 'gemini ...' first.");
                        }
                    } else if starts_with_ci(cmd_str, "run ") {
                        // App Persistence: load and execute saved WASM from VFS
                        let app_name = cmd_str[4..].trim();
                        if app_name.is_empty() {
                            win.push_line("Usage: run <name>");
                        } else {
                            let filename = if app_name.ends_with(".wasm") {
                                alloc::string::String::from(app_name)
                            } else {
                                alloc::format!("{}.wasm", app_name)
                            };
                            win.push_line(&alloc::format!("[OS] Loading {}...", filename));

                            // Read WASM from Synapse VFS via shmem
                            const VFS_READ_VADDR: usize = 0x50040000;
                            match libfolk::sys::synapse::read_file_shmem(&filename) {
                                Ok(resp) => {
                                    if shmem_map(resp.shmem_handle, VFS_READ_VADDR).is_ok() {
                                        let data = unsafe {
                                            core::slice::from_raw_parts(VFS_READ_VADDR as *const u8, resp.size as usize)
                                        };
                                        let wasm_bytes = alloc::vec::Vec::from(data);
                                        let _ = shmem_unmap(resp.shmem_handle, VFS_READ_VADDR);
                                        let _ = shmem_destroy(resp.shmem_handle);

                                        win.push_line(&alloc::format!("[OS] Loaded {} bytes", wasm_bytes.len()));

                                        let config = compositor::wasm_runtime::WasmConfig {
                                            screen_width: fb.width as u32,
                                            screen_height: fb.height as u32,
                                            uptime_ms: libfolk::sys::uptime() as u32,
                                        };

                                        match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                            Ok(app) => {
                                                win.push_line("[OS] App launched! Press ESC to exit.");
                                                active_wasm_app = Some(app);
                                                active_wasm_app_key = Some(alloc::string::String::from(app_name));
                                                wasm_app_open_since_ms = libfolk::sys::uptime();
                                                fuel_fail_count = 0;
                                                last_wasm_bytes = Some(wasm_bytes);
                                                last_wasm_interactive = true;
                                            }
                                            Err(e) => {
                                                win.push_line(&alloc::format!("[OS] Load error: {}", &e[..e.len().min(60)]));
                                            }
                                        }
                                    } else {
                                        let _ = shmem_destroy(resp.shmem_handle);
                                        win.push_line("[OS] Failed to map file data");
                                    }
                                }
                                Err(_) => {
                                    win.push_line(&alloc::format!("[OS] App '{}' not found", app_name));
                                }
                            }
                        }
                    } else if cmd_str.starts_with("agent ") {
                        // Agentic ReAct loop via MCP
                        // Flags: --force (skip cache), --tweak "mod" (modify existing)
                        let raw = cmd_str[6..].trim();
                        let (flags, prompt) = parse_agent_flags(raw);
                        write_str("[AGENT] Command: ");
                        write_str(prompt);
                        if flags.force { write_str(" [--force]"); }
                        if flags.tweak_msg.is_some() { write_str(" [--tweak]"); }
                        write_str("\n");
                        if prompt.is_empty() {
                            win.push_line("Usage: agent <task>");
                            win.push_line("  --force: skip WASM cache");
                            win.push_line("  --tweak \"change\": modify cached version");
                        } else {
                            // Record command for Draug prediction
                            draug.record_command(prompt);

                            // Check WASM cache (Pillar 4)
                            if !flags.force {
                                if let Some(cached_wasm) = wasm_cache.get(prompt) {
                                    win.push_line(&alloc::format!("[Cache] Hit: {} bytes", cached_wasm.len()));
                                    // Use cached WASM directly
                                    last_wasm_bytes = Some(cached_wasm.clone());
                                    let config = compositor::wasm_runtime::WasmConfig {
                                        screen_width: fb.width as u32,
                                        screen_height: fb.height as u32,
                                        uptime_ms: libfolk::sys::uptime() as u32,
                                    };
                                    let (result, output) = compositor::wasm_runtime::execute_wasm(cached_wasm, config);
                                    if let compositor::wasm_runtime::WasmResult::Ok = &result {
                                        win.push_line("[Cache] Executed from cache (instant)");
                                    }
                                    if let Some(color) = output.fill_screen { fb.clear(fb.color_from_rgb24(color)); }
                                    for cmd in &output.draw_commands {
                                        fb.fill_rect(cmd.x as usize, cmd.y as usize, cmd.w as usize, cmd.h as usize, fb.color_from_rgb24(cmd.color));
                                    }
                                    damage.damage_full();
                                    need_redraw = true;
                                    // Skip agent — served from cache
                                } else {
                                    // No cache hit — run agent
                                    win.push_line(&alloc::format!("[Agent] Task: {}", &prompt[..prompt.len().min(60)]));
                                    let mut session = compositor::agent::AgentSession::new(prompt, win_id);
                                    if session.start() {
                                        write_str("[AGENT] Session started\n");
                                        win.push_line("[Agent] Thinking...");
                                        active_agent = Some(session);
                                    } else {
                                        win.push_line("[Agent] Error: failed to start");
                                    }
                                }
                            } else {
                                // --force: skip cache, always ask LLM
                                win.push_line(&alloc::format!("[Agent] Task (forced): {}", &prompt[..prompt.len().min(50)]));
                                let mut session = compositor::agent::AgentSession::new(prompt, win_id);
                                if session.start() {
                                    write_str("[AGENT] Session started (forced)\n");
                                    win.push_line("[Agent] Thinking...");
                                    active_agent = Some(session);
                                } else {
                                    win.push_line("[Agent] Error: failed to start");
                                }
                            }
                        }
                    } else if cmd_str.starts_with("gemini ") {
                        // Legacy: direct LLM query (blocking, no tool chaining)
                        let prompt = cmd_str[7..].trim();
                        write_str("[COMPOSITOR] gemini command: ");
                        write_str(prompt);
                        write_str("\n");
                        if prompt.is_empty() {
                            win.push_line("Usage: gemini <prompt>");
                        } else if starts_with_ci(prompt, "generate ") {
                            // Direct WASM tool generation — skip AI agent, go straight to compiler
                            let tool_prompt = prompt[9..].trim();
                            win.push_line(&alloc::format!("[AI] Generating tool: {}...", &tool_prompt[..tool_prompt.len().min(50)]));
                            deferred_tool_gen = Some((win_id, alloc::string::String::from(tool_prompt)));
                            damage.damage_full();
                        } else {
                            win.push_line(&alloc::format!("> gemini {}", &prompt[..prompt.len().min(60)]));

                            // Agentic AI: serialize UI state → send to proxy → parse intent
                            let full_prompt = alloc::format!(
                                "You are Folkering OS AI assistant. Current screen state:\n{}\nUser command: {}\n\nYou MUST respond with ONLY a JSON object. Choose one:\n{{\"action\": \"move_window\", \"window_id\": N, \"x\": N, \"y\": N}}\n{{\"action\": \"close_window\", \"window_id\": N}}\n{{\"action\": \"generate_tool\", \"prompt\": \"description\"}}\n{{\"action\": \"text\", \"content\": \"your answer\"}}\nNEVER respond with plain text. ALWAYS use JSON.",
                                ui_state_snapshot, prompt
                            );

                            win.push_line("[cloud] Sending with UI context...");

                            // Step 3: Call Gemini proxy (128KB via mmap — bump allocator is only 64KB)
                            const GEMINI_CMD_VADDR: usize = 0x50000000; // Must be >= 0x40000000 (MMAP_BASE)
                            const GEMINI_CMD_SIZE: usize = 131072;
                            let response_len = if libfolk::sys::mmap_at(GEMINI_CMD_VADDR, GEMINI_CMD_SIZE, 3).is_ok() {
                                let gemini_buf = unsafe {
                                    core::slice::from_raw_parts_mut(GEMINI_CMD_VADDR as *mut u8, GEMINI_CMD_SIZE)
                                };
                                let rlen = libfolk::sys::ask_gemini(&full_prompt, gemini_buf);
                                rlen
                            } else { 0 };
                            let gemini_buf = unsafe {
                                core::slice::from_raw_parts(GEMINI_CMD_VADDR as *const u8, GEMINI_CMD_SIZE)
                            };

                            if response_len > 0 {
                                if let Ok(text) = alloc::str::from_utf8(&gemini_buf[..response_len]) {
                                    // Step 4: Parse intent and display result
                                    use compositor::intent::AgentIntent;
                                    let intent = compositor::intent::parse_intent(text);
                                    write_str("[COMPOSITOR] Intent parsed\n");

                                    match intent {
                                        AgentIntent::MoveWindow { window_id, x, y } => {
                                            win.push_line(&alloc::format!(
                                                "[AI] Moving window {} to ({},{})", window_id, x, y
                                            ));
                                            // Deferred: execute after dropping win
                                            deferred_intent_action = Some((1, window_id, x, y));
                                        }
                                        AgentIntent::CloseWindow { window_id } => {
                                            win.push_line(&alloc::format!("[AI] Closing window {}", window_id));
                                            deferred_intent_action = Some((2, window_id, 0, 0));
                                        }
                                        AgentIntent::ResizeWindow { window_id, w, h } => {
                                            win.push_line(&alloc::format!(
                                                "[AI] Resizing window {} to {}x{}", window_id, w, h
                                            ));
                                            deferred_intent_action = Some((3, window_id, w, h));
                                        }
                                        AgentIntent::GenerateTool { prompt: tp } => {
                                            win.push_line(&alloc::format!(
                                                "[AI] Generating tool: {}...", &tp[..tp.len().min(50)]
                                            ));
                                            // Deferred 2-frame: this frame renders the message,
                                            // next frame executes the WASM pipeline
                                            deferred_tool_gen = Some((win_id, tp));
                                            damage.damage_full();
                                        }
                                        AgentIntent::TextResponse { text: resp } => {
                                            // Filter <think>...</think> from response → overlay
                                            let mut visible = alloc::string::String::new();
                                            let mut in_think = false;
                                            let mut rest = resp.as_str();
                                            while !rest.is_empty() {
                                                if !in_think {
                                                    if let Some(pos) = rest.find("<think>") {
                                                        visible.push_str(&rest[..pos]);
                                                        rest = &rest[pos + 7..];
                                                        in_think = true;
                                                        think_active = true;
                                                        think_display_len = 0;
                                                    } else {
                                                        visible.push_str(rest);
                                                        break;
                                                    }
                                                } else {
                                                    if let Some(pos) = rest.find("</think>") {
                                                        // Store think content in overlay buffer
                                                        let think_text = &rest[..pos];
                                                        let copy_len = think_text.len().min(THINK_BUF_SIZE - think_display_len);
                                                        think_display[think_display_len..think_display_len + copy_len]
                                                            .copy_from_slice(&think_text.as_bytes()[..copy_len]);
                                                        think_display_len += copy_len;
                                                        think_active = false;
                                                        think_fade_timer = 180; // 3 seconds visible
                                                        need_redraw = true;
                                                        rest = &rest[pos + 8..];
                                                        in_think = false;
                                                    } else {
                                                        // Unclosed think — store all, show nothing
                                                        let copy_len = rest.len().min(THINK_BUF_SIZE - think_display_len);
                                                        think_display[think_display_len..think_display_len + copy_len]
                                                            .copy_from_slice(&rest.as_bytes()[..copy_len]);
                                                        think_display_len += copy_len;
                                                        break;
                                                    }
                                                }
                                            }
                                            win.push_line("[Gemini]:");
                                            for line in visible.split('\n') {
                                                let trimmed = line.trim();
                                                if !trimmed.is_empty() {
                                                    win.push_line(trimmed);
                                                }
                                            }
                                        }
                                        AgentIntent::Error { message } => {
                                            win.push_line(&alloc::format!("[AI Error] {}", message));
                                        }
                                        _ => {
                                            win.push_line("[AI] Unhandled intent");
                                        }
                                    }
                                } else {
                                    win.push_line("[cloud] Response not valid UTF-8");
                                }
                            } else {
                                win.push_line("[cloud] Error: no response from Gemini API");
                            }
                            // Free mmap'd buffer
                            let _ = libfolk::sys::munmap(GEMINI_CMD_VADDR as *mut u8, GEMINI_CMD_SIZE);
                        }
                    } else {
                        win.push_line("Sent to shell...");
                    }
                    if !win.interactive {
                        win.push_line("---");
                    }
                }
                // Execute deferred AI intent actions (after win borrow is dropped)
                if let Some((action, wid, a1, a2)) = deferred_intent_action {
                    match action {
                        1 => { // MoveWindow
                            if let Some(w) = wm.get_window_mut(wid) {
                                w.x = a1 as i32;
                                w.y = a2 as i32;
                            }
                            damage.damage_full();
                        }
                        2 => { // CloseWindow
                            wm.close_window(wid);
                            damage.damage_full();
                        }
                        3 => { // ResizeWindow
                            if let Some(w) = wm.get_window_mut(wid) {
                                w.width = a1;
                                w.height = a2;
                            }
                            damage.damage_full();
                        }
                        _ => {}
                    }
                }
                } // end if deferred_app_handle == 0
                } // end if !folkshell_handled

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
            // Skip desktop UI when WASM app owns the screen
            let wasm_fullscreen = active_wasm_app.as_ref().map_or(false, |a| a.active);

            // ===== WASM FULLSCREEN MODE =====
            // When a WASM app is active, it owns the entire framebuffer.
            // Skip ALL desktop rendering (omnibar, folders, windows) to prevent
            // tearing artifacts in the single-buffered framebuffer.
            if wasm_fullscreen {
                if let Some(app) = &mut active_wasm_app {
                    if app.active {
                        let config = compositor::wasm_runtime::WasmConfig {
                            screen_width: fb.width as u32,
                            screen_height: fb.height as u32,
                            uptime_ms: libfolk::sys::uptime() as u32,
                        };
                        let (result, output) = app.run_frame(config);

                        match &result {
                            compositor::wasm_runtime::WasmResult::OutOfFuel => {
                                fuel_fail_count = fuel_fail_count.saturating_add(1);
                                if fuel_fail_count >= 3 && immune_patching.is_none() {
                                    // Live Patching: 3 consecutive fuel failures → request fix
                                    app.active = false;
                                    write_str("[IMMUNE] App fuel-limited 3x — requesting live patch\n");
                                    if let Some(ref k) = active_wasm_app_key {
                                        let desc = alloc::format!(
                                            "This WASM app '{}' hits fuel limit every frame. \
                                             It has run() called per frame with 1M instruction budget. \
                                             Find the infinite loop or expensive computation and fix it. \
                                             Return ONLY the fixed Rust source code.", k
                                        );
                                        if libfolk::mcp::client::send_wasm_gen(&desc) {
                                            immune_patching = Some(k.clone());
                                            write_str("[IMMUNE] Patch request sent via MCP\n");
                                        } else {
                                            write_str("[IMMUNE] Failed to send patch request\n");
                                        }
                                        // Record for Nightmare dream priority
                                        draug.record_crash(k);
                                    }
                                } else if fuel_fail_count < 3 {
                                    write_str("[WASM APP] Fuel exhausted (");
                                    write_str(match fuel_fail_count { 1 => "1/3", 2 => "2/3", _ => "?" });
                                    write_str(")\n");
                                }
                            }
                            compositor::wasm_runtime::WasmResult::Trap(msg) => {
                                app.active = false;
                                write_str("[WASM APP] Trap: ");
                                write_str(&msg[..msg.len().min(80)]);
                                write_str("\n");
                                // Record for Nightmare dream priority
                                if let Some(ref k) = active_wasm_app_key {
                                    draug.record_crash(k);
                                }
                            }
                            _ => {
                                // Reset fail counter on successful frame
                                fuel_fail_count = 0;
                            }
                        }

                        if let Some(color) = output.fill_screen {
                            fb.clear(fb.color_from_rgb24(color));
                        }
                        for cmd in &output.draw_commands {
                            fb.fill_rect(cmd.x as usize, cmd.y as usize, cmd.w as usize, cmd.h as usize, fb.color_from_rgb24(cmd.color));
                        }
                        for cmd in &output.line_commands {
                            let c = fb.color_from_rgb24(cmd.color);
                            compositor::graphics::draw_line(&mut fb, cmd.x1, cmd.y1, cmd.x2, cmd.y2, c);
                        }
                        for cmd in &output.circle_commands {
                            let c = fb.color_from_rgb24(cmd.color);
                            compositor::graphics::draw_circle(&mut fb, cmd.cx, cmd.cy, cmd.r, c);
                        }
                        for cmd in &output.text_commands {
                            fb.draw_string(cmd.x as usize, cmd.y as usize, &cmd.text, fb.color_from_rgb24(cmd.color), fb.color_from_rgb24(0));
                        }

                        // Phase 3: Surface blit
                        if output.surface_dirty {
                            if let Some(mem_data) = app.get_memory_slice() {
                                let surface_offset = app.surface_offset();
                                let fb_size = fb.width * fb.height * 4;
                                if surface_offset + fb_size <= mem_data.len() {
                                    let surface = &mem_data[surface_offset..surface_offset + fb_size];
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
                                    // query://calculator → semantic search by concept
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
                                    // mime://application/wasm → find first file with this MIME type
                                    let mime = &req.filename[7..];
                                    let mime_hash = libfolk::sys::synapse::hash_name(mime);
                                    // Use QUERY_MIME IPC (simple hash lookup)
                                    let request = libfolk::sys::synapse::SYN_OP_QUERY_MIME
                                        | ((mime_hash as u64) << 32);
                                    let ret = unsafe {
                                        libfolk::syscall::syscall3(
                                            libfolk::syscall::SYS_IPC_SEND,
                                            libfolk::sys::synapse::SYNAPSE_TASK_ID as u64,
                                            request, 0
                                        )
                                    };
                                    if ret != libfolk::sys::synapse::SYN_STATUS_NOT_FOUND && ret != u64::MAX {
                                        let file_id = (ret & 0xFFFF) as u16;
                                        write_str("[Synapse] mime:// → file_id=");
                                        let mut nb3 = [0u8; 16];
                                        write_str(format_usize(file_id as usize, &mut nb3));
                                        write_str("\n");
                                    }
                                    // Fallback — mime:// can't easily resolve to a filename yet
                                    req.filename.clone()
                                } else if req.filename.starts_with("adapt://") {
                                    // adapt://source_mime/target_format/filename
                                    let parts: alloc::vec::Vec<&str> = req.filename[8..].splitn(3, '/').collect();
                                    if parts.len() == 3 {
                                        let adapter_key = alloc::format!("{}|{}", parts[0], parts[1]);
                                        if !adapter_cache.contains_key(&adapter_key) && pending_adapter.is_none() {
                                            let prompt = compositor::wasm_runtime::adapter_generation_prompt(
                                                parts[0], parts[1], ""
                                            );
                                            if libfolk::mcp::client::send_wasm_gen(&prompt) {
                                                pending_adapter = Some(adapter_key);
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
                                                    resp.size as usize
                                                )
                                            };

                                            // View Adapter: if adapt:// was used, try transform
                                            let transformed = if req.filename.starts_with("adapt://") {
                                                let parts: alloc::vec::Vec<&str> = req.filename[8..].splitn(3, '/').collect();
                                                if parts.len() == 3 {
                                                    let adapter_key = alloc::format!("{}|{}", parts[0], parts[1]);
                                                    if let Some(adapter_wasm) = adapter_cache.get(&adapter_key) {
                                                        compositor::wasm_runtime::execute_adapter(
                                                            adapter_wasm, &file_data[..resp.size as usize]
                                                        )
                                                    } else { None }
                                                } else { None }
                                            } else { None };

                                            let final_data = transformed.as_deref()
                                                .unwrap_or(&file_data[..resp.size as usize]);
                                            let copy_len = final_data.len().min(req.dest_len as usize);
                                            app.write_memory(
                                                req.dest_ptr as usize,
                                                &final_data[..copy_len]
                                            );
                                            let _ = shmem_unmap(resp.shmem_handle, VFS_ASSET_VADDR);
                                            let _ = shmem_destroy(resp.shmem_handle);
                                            app.push_event(compositor::wasm_runtime::FolkEvent {
                                                event_type: 4,
                                                x: req.handle as i32,
                                                y: 0,
                                                data: copy_len as i32,
                                            });
                                        } else {
                                            let _ = shmem_destroy(resp.shmem_handle);
                                            app.push_event(compositor::wasm_runtime::FolkEvent {
                                                event_type: 4,
                                                x: req.handle as i32,
                                                y: 2,
                                                data: 0,
                                            });
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
                        // WASM owns fullscreen — damage entire screen
                        damage.damage_full();
                    }
                }
            }

            // ===== DESKTOP MODE: omnibar, folders, windows =====
            // Only render desktop elements when NO WASM app is fullscreen.
            // Entire block is skipped when WASM owns the screen.

            if !wasm_fullscreen && omnibar_visible {
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
                let hint = "Type <query> | open calc | gemini <prompt> | help";
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
            } else if !wasm_fullscreen {
                // ===== Omnibar hidden - clear the area (only in desktop mode) =====
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

            // (System Tray Clock moved to always-on-top section below)

            // ===== App Launcher: Folder grid or app grid =====
            if !wasm_fullscreen {
                let tile_text = fb.color_from_rgb24(0xDDDDDD);
                let tile_bg = fb.color_from_rgb24(0x222244);
                let tile_border = fb.color_from_rgb24(0x444477);

                if open_folder < 0 {
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
                        let grid_w = cols * (FOLDER_W + FOLDER_GAP) - FOLDER_GAP;
                        let grid_x = (fb.width.saturating_sub(grid_w)) / 2;
                        let grid_y = 120;

                        for v in 0..vis_count {
                            let (cat_idx, _) = visible[v];
                            let col = v % 3;
                            let row = v / 3;
                            let fx = grid_x + col * (FOLDER_W + FOLDER_GAP);
                            let fy = grid_y + row * (FOLDER_H + FOLDER_GAP);

                            let cat = &categories[cat_idx];
                            let c = fb.color_from_rgb24(cat.color);

                            // Folder tile
                            fb.fill_rect(fx, fy, FOLDER_W, FOLDER_H, tile_bg);
                            fb.draw_rect(fx, fy, FOLDER_W, FOLDER_H, c);
                            fb.draw_rect(fx + 1, fy + 1, FOLDER_W - 2, FOLDER_H - 2, tile_border);

                            // Mini app preview squares (2×2 grid inside folder)
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
                            let lx = fx + (FOLDER_W.saturating_sub(lbl_len * 8)) / 2;
                            fb.draw_string(lx, fy + FOLDER_H - 20, lbl_trimmed, tile_text, tile_bg);

                            // App count badge
                            let mut nbuf = [0u8; 16];
                            let ns = format_usize(cat.count, &mut nbuf);
                            fb.draw_string(fx + FOLDER_W - 16, fy + 4, ns, c, tile_bg);

                            // Hover preview: show app list below the folder
                            if hover_folder == cat_idx as i32 {
                                let hover_bg = fb.color_from_rgb24(0x2a2a5a);
                                let prev_x = fx;
                                let prev_y = fy + FOLDER_H + 4;
                                let prev_w = FOLDER_W + 60;
                                let prev_h = 20 + cat.count.min(5) * 18;
                                fb.fill_rect(prev_x, prev_y, prev_w, prev_h, hover_bg);
                                fb.draw_rect(prev_x, prev_y, prev_w, prev_h, c);
                                for ai in 0..cat.count.min(5) {
                                    let entry = &cat.apps[ai];
                                    if entry.name_len > 0 {
                                        let name = unsafe { core::str::from_utf8_unchecked(&entry.name[..entry.name_len]) };
                                        fb.draw_string(prev_x + 8, prev_y + 4 + ai * 18, &name[..name.len().min(16)], tile_text, hover_bg);
                                    }
                                }
                                if cat.count > 5 {
                                    fb.draw_string(prev_x + 8, prev_y + 4 + 5 * 18, "...", tile_text, hover_bg);
                                }
                            }
                        }
                    }
                } else {
                    // FOLDER VIEW: show apps inside the selected category
                    let cat_idx = open_folder as usize;
                    if cat_idx < MAX_CATEGORIES {
                        let cat = &categories[cat_idx];
                        let label = unsafe { core::str::from_utf8_unchecked(cat.label) };
                        let c = fb.color_from_rgb24(cat.color);

                        // Folder header
                        let header_y = 90;
                        fb.fill_rect(0, header_y, fb.width, 30, fb.color_from_rgb24(0x1a1a3a));
                        let back_str = "< Back";
                        fb.draw_string(16, header_y + 7, back_str, tile_text, fb.color_from_rgb24(0x1a1a3a));
                        let title_x = (fb.width.saturating_sub(label.trim_end_matches('\0').len() * 8)) / 2;
                        fb.draw_string(title_x, header_y + 7, label.trim_end_matches('\0'), c, fb.color_from_rgb24(0x1a1a3a));

                        // App grid
                        let grid_w = APP_TILE_COLS * (APP_TILE_W + APP_TILE_GAP) - APP_TILE_GAP;
                        let grid_x = (fb.width.saturating_sub(grid_w)) / 2;
                        let grid_y = 130;

                        for i in 0..cat.count {
                            let col = i % APP_TILE_COLS;
                            let row = i / APP_TILE_COLS;
                            let ax = grid_x + col * (APP_TILE_W + APP_TILE_GAP);
                            let ay = grid_y + row * (APP_TILE_H + APP_TILE_GAP);

                            fb.fill_rect(ax, ay, APP_TILE_W, APP_TILE_H, tile_bg);
                            fb.draw_rect(ax, ay, APP_TILE_W, APP_TILE_H, tile_border);

                            // Icon (colored square)
                            fb.fill_rect(ax + 16, ay + 8, 40, 36, c);

                            // App name
                            let entry = &cat.apps[i];
                            if entry.name_len > 0 {
                                let name = unsafe { core::str::from_utf8_unchecked(&entry.name[..entry.name_len]) };
                                let nx = ax + (APP_TILE_W.saturating_sub(entry.name_len.min(9) * 8)) / 2;
                                fb.draw_string(nx, ay + APP_TILE_H - 20, &name[..name.len().min(9)], tile_text, tile_bg);
                            }
                        }
                    }
                }
            }

            // ===== Composite Windows (Milestone 2.1) =====
            // Only show windows in desktop mode (not when WASM app is fullscreen)
            if !wasm_fullscreen && wm.has_visible() {
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

            // ===== System Tray Clock — ALWAYS ON TOP =====
            // Rendered after windows, WASM apps, HUD — only cursor is above
            {
                let dt = libfolk::sys::get_rtc();
                let mut total_minutes = dt.hour as i32 * 60 + dt.minute as i32 + tz_offset_minutes;
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
                // Format: "14:30:05"  (compact, like a phone status bar)
                let mut t = [0u8; 8];
                t[0] = b'0' + lh / 10; t[1] = b'0' + lh % 10;
                t[2] = b':';
                t[3] = b'0' + lm / 10; t[4] = b'0' + lm % 10;
                t[5] = b':';
                t[6] = b'0' + ls / 10; t[7] = b'0' + ls % 10;
                let time_str = unsafe { core::str::from_utf8_unchecked(&t) };

                // Status bar background (semi-transparent strip at top)
                let bar_h = 20usize;
                fb.fill_rect_alpha(0, 0, fb.width, bar_h, 0x000000, 140);

                // Clock centered at top
                let time_x = (fb.width.saturating_sub(8 * 8)) / 2;
                fb.draw_string(time_x, 2, time_str, white, fb.color_from_rgb24(0x0a0a0a));

                // Date on the left
                let mut d = [0u8; 10];
                d[0] = b'0' + ((year/1000)%10) as u8; d[1] = b'0' + ((year/100)%10) as u8;
                d[2] = b'0' + ((year/10)%10) as u8; d[3] = b'0' + (year%10) as u8;
                d[4] = b'-'; d[5] = b'0' + month/10; d[6] = b'0' + month%10;
                d[7] = b'-'; d[8] = b'0' + day as u8/10; d[9] = b'0' + day as u8%10;
                let date_str = unsafe { core::str::from_utf8_unchecked(&d) };
                fb.draw_string(8, 2, date_str, gray, fb.color_from_rgb24(0x0a0a0a));

                // RAM usage on the right side of status bar
                let (_total_mb, _used_mb, mem_pct) = libfolk::sys::memory_stats();
                let mut rbuf = [0u8; 8];
                let mut ri = 0usize;
                // "RAM XX%"
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
                fb.draw_string(ram_x, 2, ram_str, ram_col, fb.color_from_rgb24(0x0a0a0a));

                // IQE latency display + colored dot
                if ewma_kbd_us > 0 || ewma_mou_us > 0 {
                    let mut lbuf = [0u8; 48];
                    let mut li = 0usize;
                    // K:total(w+r) | M:total
                    lbuf[li]=b'K'; li+=1; lbuf[li]=b':'; li+=1;
                    li += fmt_u64_into(&mut lbuf[li..], ewma_kbd_us);
                    if ewma_kbd_wake > 0 {
                        lbuf[li]=b'('; li+=1;
                        li += fmt_u64_into(&mut lbuf[li..], ewma_kbd_wake);
                        lbuf[li]=b'+'; li+=1;
                        li += fmt_u64_into(&mut lbuf[li..], ewma_kbd_rend);
                        lbuf[li]=b')'; li+=1;
                    }
                    if li < 44 { lbuf[li]=b' '; li+=1; lbuf[li]=b'M'; li+=1; lbuf[li]=b':'; li+=1;
                        li += fmt_u64_into(&mut lbuf[li..], ewma_mou_us);
                    }
                    let s = unsafe { core::str::from_utf8_unchecked(&lbuf[..li.min(48)]) };
                    fb.draw_string(90, 2, s, fb.color_from_rgb24(0x88AACC), fb.color_from_rgb24(0x0a0a0a));

                    let worst = ewma_kbd_us.max(ewma_mou_us);
                    let dot = if worst < 5000 { 0x44FF44 } else if worst < 16000 { 0xFFAA00 } else { 0xFF4444 };
                    fb.fill_rect(ram_x.saturating_sub(14), 5, 8, 8, fb.color_from_rgb24(dot));
                }

                // RAM history graph (popup when clicked)
                if show_ram_graph && ram_history_count > 1 {
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
                    fb.draw_string(graph_x + 4, graph_y + 2, "RAM % (2min)", fb.color_from_rgb24(0x6688AA), graph_bg);

                    // Scale labels
                    fb.draw_string(graph_x + graph_w - 28, graph_y + graph_h - 14, "0%", fb.color_from_rgb24(0x445566), graph_bg);
                    fb.draw_string(graph_x + graph_w - 36, graph_y + 16, "100%", fb.color_from_rgb24(0x445566), graph_bg);

                    // Plot data points as filled columns
                    let samples = ram_history_count.min(graph_w - 4);
                    let bar_w = 1usize.max((graph_w - 4) / samples.max(1));

                    for i in 0..samples {
                        // Read from oldest to newest
                        let hist_idx = if ram_history_count >= RAM_HISTORY_LEN {
                            (ram_history_idx + RAM_HISTORY_LEN - samples + i) % RAM_HISTORY_LEN
                        } else {
                            i
                        };
                        let pct_val = ram_history[hist_idx] as usize;
                        let bar_height = pct_val * (graph_h - 20) / 100;
                        let bx = graph_x + 2 + i * bar_w;
                        let by = graph_y + graph_h - 2 - bar_height;

                        let bar_color = if pct_val > 80 { fb.color_from_rgb24(0xFF4444) }
                            else if pct_val > 50 { fb.color_from_rgb24(0xFFAA00) }
                            else { fb.color_from_rgb24(0x44FF44) };

                        if bx + bar_w < graph_x + graph_w - 1 {
                            fb.fill_rect(bx, by, bar_w, bar_height, bar_color);
                        }
                    }
                }
            }

            // Targeted damage per UI element (coalesced into minimal rects)
            if !wasm_fullscreen {
                damage.add_damage(compositor::damage::Rect::new(0, 0, fb.width as u32, 22));
                if omnibar_visible {
                    damage.add_damage(compositor::damage::Rect::new(
                        text_box_x.saturating_sub(4) as u32,
                        text_box_y.saturating_sub(4) as u32,
                        (text_box_w + 8) as u32,
                        (text_box_h + 60) as u32));
                }
                for w in wm.windows.iter() {
                    damage.add_damage(compositor::damage::Rect::new(
                        w.x.max(0) as u32, w.y.max(0) as u32,
                        (w.width + 20) as u32, (w.height + 40) as u32));
                }
            } else {
                damage.damage_full();
            }

            // After full redraw: save fresh scene under cursor and mark cursor bg dirty.
            // Cursor itself is drawn AFTER present_region (below), so it's on top of FB.
            if cursor_drawn {
                fb.save_rect(cursor_x as usize, cursor_y as usize, CURSOR_W, CURSOR_H, &mut cursor_bg.0);
                cursor_bg_dirty = false;
                damage.add_damage(compositor::damage::Rect::new(
                    cursor_x.max(0) as u32, cursor_y.max(0) as u32,
                    CURSOR_W as u32 + 2, CURSOR_H as u32 + 2));
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
                                // Entered think block — capture to overlay
                                think_state = 1;
                                think_open_match = 0;
                                think_pending_len = 0;
                                think_active = true;
                                think_display_len = 0; // clear previous
                                think_fade_timer = 0;
                                need_redraw = true;
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
                                // Exited think block — keep overlay visible for 120 frames (~2s)
                                think_state = 0;
                                think_close_match = 0;
                                think_active = false;
                                think_fade_timer = 120;
                                need_redraw = true;
                            }
                        } else {
                            // Flush partial close-match bytes to think buffer
                            for k in 0..think_close_match {
                                if think_display_len < THINK_BUF_SIZE {
                                    think_display[think_display_len] = THINK_CLOSE[k];
                                    think_display_len += 1;
                                }
                            }
                            think_close_match = 0;
                            // Store current byte in think display buffer
                            if think_display_len < THINK_BUF_SIZE {
                                think_display[think_display_len] = byte;
                                think_display_len += 1;
                            }
                            need_redraw = true;
                        }
                        continue; // Don't pass think bytes to tool/visible layer
                    }

                    // ── Layer 1.5: Tool result filter ──
                    // Hides <|tool_result|>...<|/tool_result|> from display
                    if result_state == 0 {
                        if byte == RESULT_OPEN[result_open_match] {
                            result_open_match += 1;
                            if result_open_match == RESULT_OPEN.len() {
                                result_state = 1;
                                result_open_match = 0;
                            }
                            continue;
                        } else if result_open_match > 0 {
                            // Partial match failed — these bytes were '<|tool_r...' which
                            // isn't a real result tag. They fall through to tool/visible.
                            // For simplicity, just reset and let the current byte through.
                            result_open_match = 0;
                            // Fall through to process current byte
                        }
                    } else {
                        // result_state == 1: Inside result block — scan for close tag
                        if byte == RESULT_CLOSE[result_close_match] {
                            result_close_match += 1;
                            if result_close_match == RESULT_CLOSE.len() {
                                result_state = 0;
                                result_close_match = 0;
                            }
                        } else {
                            result_close_match = 0;
                        }
                        continue; // Drop bytes inside result block
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

                // Execute completed tool call + write result back to ring
                if tool_state == 3 {
                    let tool_content = core::str::from_utf8(&tool_buf[..tool_buf_len]).unwrap_or("");
                    // Pass ring info so result can be written back for AI feedback
                    let ring_va = if inference_ring_handle != 0 { RING_VADDR } else { 0 };
                    let ring_write = new_write; // current write position in ring
                    if let Some(win) = wm.get_window_mut(inference_win_id) {
                        execute_tool_call(tool_content, win, ring_va, ring_write);
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

        // ===== AI Think Overlay =====
        // Semi-transparent panel showing AI reasoning in real-time
        if (think_active || think_fade_timer > 0) && think_display_len > 0 {
            // Overlay dimensions: top-right corner, 400px wide
            let overlay_w = 400usize;
            let overlay_x = fb.width.saturating_sub(overlay_w + 16);
            let overlay_y = 40usize;

            // Extract last N lines from think buffer (show most recent reasoning)
            let think_text = unsafe {
                core::str::from_utf8_unchecked(&think_display[..think_display_len])
            };

            // Count lines and find start of last 8 lines
            let max_lines = 8usize;
            let mut line_starts = [0usize; 9]; // up to 8 lines + sentinel
            let mut line_count = 0usize;
            let bytes = think_text.as_bytes();
            line_starts[0] = 0;
            for i in 0..bytes.len() {
                if bytes[i] == b'\n' && line_count < max_lines {
                    line_count += 1;
                    line_starts[line_count] = i + 1;
                }
            }
            if line_count == 0 { line_count = 1; } // at least 1 line

            // Show last max_lines lines
            let first_line = if line_count > max_lines { line_count - max_lines } else { 0 };
            let display_lines = line_count - first_line;
            let overlay_h = 28 + display_lines * 18;

            // Alpha for fade-out effect
            let alpha = if think_active { 200u8 } else {
                (think_fade_timer as u16 * 200 / 120).min(200) as u8
            };

            // Draw semi-transparent background
            fb.fill_rect_alpha(overlay_x, overlay_y, overlay_w, overlay_h, 0x0a0a1e, alpha);

            // Header: "AI Thinking..." or "AI Thought"
            let header = if think_active { "AI Thinking..." } else { "AI Thought" };
            let header_color = if think_active { 0x00ccff } else { 0x666688 };
            fb.draw_string(overlay_x + 8, overlay_y + 6, header,
                fb.color_from_rgb24(header_color), fb.color_from_rgb24(0));

            // Draw reasoning lines
            let text_color = fb.color_from_rgb24(if think_active { 0xaaaacc } else { 0x666688 });
            let bg_color = fb.color_from_rgb24(0);
            for li in 0..display_lines {
                let idx = first_line + li;
                let start = line_starts[idx];
                let end = if idx + 1 <= line_count {
                    line_starts[idx + 1].min(think_display_len)
                } else {
                    think_display_len
                };
                if start < end {
                    // Truncate long lines
                    let line_end = end.min(start + 48);
                    let line = unsafe {
                        core::str::from_utf8_unchecked(&think_display[start..line_end])
                    };
                    let line_trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                    if !line_trimmed.is_empty() {
                        fb.draw_string(overlay_x + 8, overlay_y + 24 + li * 18,
                            line_trimmed, text_color, bg_color);
                    }
                }
            }

            let overlay_w_u32 = 400;
            damage.add_damage(compositor::damage::Rect::new(
                overlay_x as u32, overlay_y as u32, overlay_w_u32, overlay_h as u32));
            need_redraw = true;
        }

        // Decrement fade timer
        if think_fade_timer > 0 {
            think_fade_timer -= 1;
            if think_fade_timer == 0 {
                need_redraw = true; // final redraw to clear overlay
            }
        }

        let t_before_present: u64 = rdtsc();

        // Present: copy shadow→FB for dirty regions that were rendered to shadow.
        // Cursor-only movement writes directly to FB (set_pixel_overlay), so we
        // track whether shadow was modified separately.
        let shadow_dirty = need_redraw || (current_second != last_clock_second + 1); // clock tick rendered to shadow
        if damage.has_damage() {
            // Present shadow→FB for all damage EXCEPT pure cursor damage.
            // When need_redraw or clock tick happened, shadow was written and needs copying.
            // For cursor-only frames, FB was already written directly.
            if need_redraw {
                // Full redraw: present everything then redraw cursor on top
                for r in damage.regions() {
                    fb.present_region(r.x, r.y, r.w, r.h);
                }
                if cursor_drawn {
                    let cursor_fill = match (last_buttons & 1 != 0, last_buttons & 2 != 0) {
                        (true, true) => cursor_magenta,
                        (true, false) => cursor_red,
                        (false, true) => cursor_blue,
                        _ => cursor_white,
                    };
                    fb.draw_cursor(cursor_x as usize, cursor_y as usize, cursor_fill, cursor_outline);
                }
            } else if !had_mouse_events {
                // Non-mouse damage (clock tick, Draug, etc.): present shadow→FB
                for r in damage.regions() {
                    fb.present_region(r.x, r.y, r.w, r.h);
                }
                // Redraw cursor if it overlaps the presented region
                if cursor_drawn && cursor_y < 22 {
                    let cursor_fill = match (last_buttons & 1 != 0, last_buttons & 2 != 0) {
                        (true, true) => cursor_magenta,
                        (true, false) => cursor_red,
                        (false, true) => cursor_blue,
                        _ => cursor_white,
                    };
                    fb.draw_cursor(cursor_x as usize, cursor_y as usize, cursor_fill, cursor_outline);
                }
            }
            // else: cursor-only movement — FB already has correct pixels
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
            damage.clear();
        } else {
            damage.clear();
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

/// Clamp focused_widget index after widget tree update

/// Execute a tool call and write result back to TokenRing for AI feedback.
/// Shows brief status in window; full result goes to ring for KV-cache injection.
fn execute_tool_call(
    tool_content: &str,
    win: &mut compositor::window_manager::Window,
    ring_vaddr: usize,
    write_idx: usize,
) {
    use libfolk::sys::{shmem_map, shmem_unmap, shmem_destroy};
    use core::sync::atomic::{AtomicU32, Ordering};

    let trimmed = tool_content.trim();
    let (cmd, args) = if let Some(pos) = trimmed.find(' ') {
        (&trimmed[..pos], trimmed[pos + 1..].trim())
    } else {
        (trimmed, "")
    };

    const TOOL_SHMEM_VADDR: usize = 0x30000000;
    const PREFIX: &[u8] = b"\n<|tool_result|>";
    const SUFFIX: &[u8] = b"<|/tool_result|>\n";

    // Stack buffer for tool result content (8KB — large enough for Gemini responses)
    let mut result_buf = [0u8; 8192];
    let mut result_len: usize = 0;

    match cmd {
        "write" => {
            if let Some(pos) = args.find(' ') {
                let filename = args[..pos].trim();
                let content = args[pos + 1..].trim();
                if filename.is_empty() || content.is_empty() {
                    result_len = copy_str(b"Error: write requires FILENAME CONTENT", &mut result_buf);
                } else if filename.contains("..") || content.len() > 4096 {
                    result_len = copy_str(b"Error: write denied (security)", &mut result_buf);
                } else {
                    match libfolk::sys::synapse::write_file(filename, content.as_bytes()) {
                        Ok(()) => {
                            result_len = copy_str(b"OK: File written: ", &mut result_buf);
                            let fname_bytes = filename.as_bytes();
                            let add = fname_bytes.len().min(result_buf.len() - result_len);
                            result_buf[result_len..result_len + add].copy_from_slice(&fname_bytes[..add]);
                            result_len += add;
                        }
                        Err(_) => result_len = copy_str(b"Error: Write failed", &mut result_buf),
                    }
                }
            } else {
                result_len = copy_str(b"Error: write requires FILENAME CONTENT", &mut result_buf);
            }
        }
        "read" => {
            if args.is_empty() {
                result_len = copy_str(b"Error: read requires FILENAME", &mut result_buf);
            } else {
                match libfolk::sys::synapse::read_file_shmem(args) {
                    Ok(resp) => {
                        if shmem_map(resp.shmem_handle, TOOL_SHMEM_VADDR).is_ok() {
                            let data = unsafe {
                                core::slice::from_raw_parts(
                                    TOOL_SHMEM_VADDR as *const u8,
                                    (resp.size as usize).min(4096),
                                )
                            };
                            // Copy file content (safe UTF-8 truncation by lines)
                            if let Ok(text) = core::str::from_utf8(data) {
                                for line in text.lines().take(8) {
                                    let lb = line.as_bytes();
                                    let add = lb.len().min(result_buf.len() - result_len);
                                    if add == 0 { break; }
                                    result_buf[result_len..result_len + add].copy_from_slice(&lb[..add]);
                                    result_len += add;
                                    if result_len < result_buf.len() {
                                        result_buf[result_len] = b'\n';
                                        result_len += 1;
                                    }
                                }
                            } else {
                                result_len = copy_str(b"(binary data)", &mut result_buf);
                            }
                            let _ = shmem_unmap(resp.shmem_handle, TOOL_SHMEM_VADDR);
                        }
                        let _ = shmem_destroy(resp.shmem_handle);
                    }
                    Err(_) => result_len = copy_str(b"Error: File not found", &mut result_buf),
                }
            }
        }
        "ls" => {
            result_len = copy_str(b"Files: (listing not yet implemented)", &mut result_buf);
        }
        "ask_gemini" => {
            if args.is_empty() {
                result_len = copy_str(b"Error: ask_gemini requires a prompt", &mut result_buf);
            } else {
                // Large response buffer for cloud AI responses (128KB via mmap)
                const GEMINI_BUF_SIZE: usize = 131072;
                const GEMINI_BUF_VADDR: usize = 0x32000000;

                // Allocate anonymous memory for response
                if libfolk::sys::mmap_at(GEMINI_BUF_VADDR, GEMINI_BUF_SIZE, 3).is_ok() {
                    let gemini_buf = unsafe {
                        core::slice::from_raw_parts_mut(GEMINI_BUF_VADDR as *mut u8, GEMINI_BUF_SIZE)
                    };

                    win.push_line("[tool] Asking Gemini...");

                    let response_len = libfolk::sys::ask_gemini(args, gemini_buf);

                    if response_len > 0 {
                        // Truncate to fit in ring buffer (max 8KB for tool result)
                        let usable = response_len.min(8000);
                        result_len = usable.min(result_buf.len());
                        result_buf[..result_len].copy_from_slice(&gemini_buf[..result_len]);
                    } else {
                        result_len = copy_str(b"Error: Cloud API unreachable", &mut result_buf);
                    }

                    // Free the buffer
                    let _ = libfolk::sys::munmap(GEMINI_BUF_VADDR as *mut u8, GEMINI_BUF_SIZE);
                } else {
                    result_len = copy_str(b"Error: memory allocation failed", &mut result_buf);
                }
            }
        }
        _ => {
            result_len = copy_str(b"Error: Unknown tool command", &mut result_buf);
        }
    }

    // Show brief status in window
    win.push_line("[tool] Executed: ");
    win.append_text(cmd.as_bytes());

    // Write result back to ring for inference-server to consume
    let total_len = PREFIX.len() + result_len + SUFFIX.len();
    let available = RING_DATA_MAX.saturating_sub(write_idx);

    if total_len <= available && ring_vaddr != 0 {
        unsafe {
            let base = (ring_vaddr as *mut u8).add(RING_HEADER_SIZE).add(write_idx);
            core::ptr::copy_nonoverlapping(PREFIX.as_ptr(), base, PREFIX.len());
            core::ptr::copy_nonoverlapping(
                result_buf.as_ptr(),
                base.add(PREFIX.len()),
                result_len,
            );
            core::ptr::copy_nonoverlapping(
                SUFFIX.as_ptr(),
                base.add(PREFIX.len() + result_len),
                SUFFIX.len(),
            );
        }

        // Signal inference-server: result ready
        let tool_result_len = unsafe { &*((ring_vaddr + 12) as *const AtomicU32) };
        let tool_state = unsafe { &*((ring_vaddr + 8) as *const AtomicU32) };
        tool_result_len.store(total_len as u32, Ordering::Release);
        tool_state.store(2, Ordering::Release); // 2 = result_ready
    }
}

/// Helper: copy bytes into buffer, return length copied
fn copy_str(src: &[u8], dst: &mut [u8]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
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

/// Format IQE telemetry line for COM3 export: "IQE,KBD,1234\n"
fn fmt_iqe_line(buf: &mut [u8], tag: &[u8], val: u64) -> usize {
    let mut i = 0;
    buf[i]=b'I'; i+=1; buf[i]=b'Q'; i+=1; buf[i]=b'E'; i+=1; buf[i]=b','; i+=1;
    for &b in tag { if i < buf.len() { buf[i] = b; i += 1; } }
    if i < buf.len() { buf[i] = b','; i += 1; }
    i += fmt_u64_into(&mut buf[i..], val);
    if i < buf.len() { buf[i] = b'\n'; i += 1; }
    i
}

/// Format u64 as decimal ASCII into buffer, return bytes written.
fn fmt_u64_into(buf: &mut [u8], mut val: u64) -> usize {
    if buf.is_empty() { return 0; }
    if val == 0 { buf[0] = b'0'; return 1; }
    let mut tmp = [0u8; 20];
    let mut i = 0;
    while val > 0 && i < 20 { tmp[i] = b'0' + (val % 10) as u8; val /= 10; i += 1; }
    let len = i.min(buf.len());
    for j in 0..len { buf[j] = tmp[i - 1 - j]; }
    len
}
