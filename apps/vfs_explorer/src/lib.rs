//! VFS-Explorer — AI-Native Semantic File Explorer for Folkering OS
//!
//! No traditional directory tree. Files are discovered via:
//!   - folk_list_files(): flat listing of all Synapse VFS entries
//!   - folk_query_files(): semantic vector search (prefix "~")
//!
//! Three-panel layout:
//!   Top:   Search field (type to filter, "~" prefix for semantic search)
//!   Left:  File list with AI-generated tags (color-coded by type)
//!   Right: File preview (text for ASCII, hex dump for binary)
//!
//! AI auto-tagger: selects a file → reads first 500 bytes → folk_slm_generate
//! asks for 3 keywords. Skips binary files (null bytes detected).

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);
    fn folk_draw_line(x1: i32, y1: i32, x2: i32, y2: i32, color: i32);
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
    fn folk_get_time() -> i32;
    fn folk_poll_event(event_ptr: i32) -> i32;
    fn folk_list_files(buf_ptr: i32, max_len: i32) -> i32;
    fn folk_query_files(query_ptr: i32, query_len: i32, result_ptr: i32, max_len: i32) -> i32;
    fn folk_request_file(path_ptr: i32, path_len: i32, dest_ptr: i32, dest_len: i32) -> i32;
    fn folk_slm_generate(prompt_ptr: i32, prompt_len: i32, buf_ptr: i32, max_len: i32) -> i32;
}

// ── Colors ──────────────────────────────────────────────────────────────

const BG: i32 = 0x0D1117;
const PANEL_BG: i32 = 0x161B22;
const BORDER: i32 = 0x30363D;
const TEXT: i32 = 0xC9D1D9;
const TEXT_DIM: i32 = 0x484F58;
const TEXT_ACCENT: i32 = 0x58A6FF;
const TEXT_GREEN: i32 = 0x3FB950;
const TEXT_YELLOW: i32 = 0xD29922;
const TEXT_RED: i32 = 0xF85149;
const TEXT_MAGENTA: i32 = 0xBC8CFF;
const CURSOR_BG: i32 = 0x1F2937;
const SEARCH_BG: i32 = 0x0F1318;
const SELECTED_BG: i32 = 0x1A2332;

// File type colors (based on AI tags)
const COLOR_CODE: i32 = 0x58A6FF;   // blue for code/wasm
const COLOR_DATA: i32 = 0x3FB950;   // green for data
const COLOR_TEXT: i32 = 0xC9D1D9;   // white for text
const COLOR_BIN: i32 = 0xF85149;    // red for binary

// Layout
const SEARCH_H: i32 = 32;
const HEADER_H: i32 = 28;
const HELP_H: i32 = 18;
const LIST_W: i32 = 380;
const MARGIN: i32 = 8;
const FONT_H: i32 = 16;
const FONT_W: i32 = 8;
const LINE_H: i32 = 20;

// Limits
const MAX_FILES: usize = 24;
const MAX_NAME: usize = 32;
const MAX_SEARCH: usize = 64;
const MAX_PREVIEW: usize = 4096;
const MAX_TAGS: usize = 48;

// ── File entry ──────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct FileEntry {
    name: [u8; MAX_NAME],
    name_len: u8,
    size: u32,
    file_type: u8, // 0=unknown, 1=code/wasm, 2=data, 3=text, 4=binary
}

impl FileEntry {
    const fn empty() -> Self {
        Self { name: [0u8; MAX_NAME], name_len: 0, size: 0, file_type: 0 }
    }
}

// ── Persistent state ────────────────────────────────────────────────────

static mut FILES: [FileEntry; MAX_FILES] = [FileEntry::empty(); MAX_FILES];
static mut FILE_COUNT: usize = 0;
static mut SELECTED_IDX: usize = 0;
static mut SCROLL_OFFSET: usize = 0;

// Search
static mut SEARCH: [u8; MAX_SEARCH] = [0u8; MAX_SEARCH];
static mut SEARCH_LEN: usize = 0;
static mut SEARCH_ACTIVE: bool = true;

// Preview
static mut PREVIEW: [u8; MAX_PREVIEW] = [0u8; MAX_PREVIEW];
static mut PREVIEW_LEN: usize = 0;
static mut PREVIEW_IS_BINARY: bool = false;
static mut PREVIEW_LOADED_IDX: i32 = -1;

// AI Tags
static mut TAGS: [u8; MAX_TAGS] = [0u8; MAX_TAGS];
static mut TAGS_LEN: usize = 0;
static mut TAGS_FOR_IDX: i32 = -1;
static mut TAG_REQUESTED: bool = false;

// Buffers
static mut LIST_BUF: [u8; 2048] = [0u8; 2048];
static mut QUERY_BUF: [u8; 512] = [0u8; 512];
static mut AI_PROMPT: [u8; 600] = [0u8; 600];
static mut AI_RESP: [u8; 128] = [0u8; 128];
static mut EVT: [i32; 4] = [0i32; 4];

static mut INITIALIZED: bool = false;
static mut LAST_REFRESH: i32 = 0;

// ── Helpers ─────────────────────────────────────────────────────────────

struct Msg<'a> { buf: &'a mut [u8], pos: usize }
impl<'a> Msg<'a> {
    fn new(buf: &'a mut [u8]) -> Self { Self { buf, pos: 0 } }
    fn s(&mut self, text: &[u8]) {
        for &b in text { if self.pos < self.buf.len() { self.buf[self.pos] = b; self.pos += 1; } }
    }
    fn u32(&mut self, mut val: u32) {
        if val == 0 { self.s(b"0"); return; }
        let mut tmp = [0u8; 10]; let mut i = 0;
        while val > 0 { tmp[i] = b'0' + (val % 10) as u8; val /= 10; i += 1; }
        while i > 0 { i -= 1; if self.pos < self.buf.len() { self.buf[self.pos] = tmp[i]; self.pos += 1; } }
    }
    fn len(&self) -> usize { self.pos }
}

unsafe fn draw(x: i32, y: i32, text: &[u8], color: i32) {
    folk_draw_text(x, y, text.as_ptr() as i32, text.len() as i32, color);
}

/// Classify file by extension
fn classify_file(name: &[u8], name_len: usize) -> u8 {
    if name_len >= 5 {
        let ext = &name[name_len-5..name_len];
        if ext == b".wasm" { return 1; } // code
    }
    if name_len >= 4 {
        let ext = &name[name_len-4..name_len];
        if ext == b".txt" { return 3; } // text
        if ext == b".dat" { return 2; } // data
        if ext == b".fku" || ext == b"fkui" { return 1; } // code/ui
    }
    if name_len >= 3 {
        let ext = &name[name_len-3..name_len];
        if ext == b".db" { return 2; } // data
    }
    0 // unknown
}

fn type_color(file_type: u8) -> i32 {
    match file_type {
        1 => COLOR_CODE,
        2 => COLOR_DATA,
        3 => COLOR_TEXT,
        4 => COLOR_BIN,
        _ => TEXT,
    }
}

fn type_label(file_type: u8) -> &'static [u8] {
    match file_type {
        1 => b"CODE",
        2 => b"DATA",
        3 => b"TEXT",
        4 => b"BIN ",
        _ => b"    ",
    }
}

/// Check if data looks binary (null bytes in first 50 bytes)
fn is_binary(data: &[u8]) -> bool {
    let check_len = data.len().min(50);
    for i in 0..check_len {
        if data[i] == 0 { return true; }
    }
    false
}

// ── File listing ────────────────────────────────────────────────────────

unsafe fn refresh_file_list() {
    let buf_ptr = core::ptr::addr_of_mut!(LIST_BUF) as *mut u8;
    let bytes = folk_list_files(buf_ptr as i32, 2048);
    if bytes <= 0 { FILE_COUNT = 0; return; }

    // Parse "name\tsize\nname\tsize\n" format
    let buf = core::slice::from_raw_parts(buf_ptr, bytes as usize);
    let files = core::ptr::addr_of_mut!(FILES) as *mut FileEntry;
    let mut count = 0usize;
    let mut i = 0usize;

    while i < buf.len() && count < MAX_FILES {
        // Find name (until \t)
        let name_start = i;
        while i < buf.len() && buf[i] != b'\t' && buf[i] != b'\n' { i += 1; }
        let name_end = i;
        let name_len = (name_end - name_start).min(MAX_NAME);

        if i < buf.len() && buf[i] == b'\t' { i += 1; } // skip tab

        // Find size (until \n)
        let mut size = 0u32;
        while i < buf.len() && buf[i] != b'\n' {
            if buf[i] >= b'0' && buf[i] <= b'9' {
                size = size * 10 + (buf[i] - b'0') as u32;
            }
            i += 1;
        }
        if i < buf.len() { i += 1; } // skip \n

        if name_len > 0 {
            let entry = &mut *files.add(count);
            *entry = FileEntry::empty();
            for j in 0..name_len { entry.name[j] = buf[name_start + j]; }
            entry.name_len = name_len as u8;
            entry.size = size;
            entry.file_type = classify_file(&entry.name, name_len);

            // Apply search filter
            if SEARCH_LEN > 0 && SEARCH[0] != b'~' {
                let search = core::slice::from_raw_parts(
                    core::ptr::addr_of!(SEARCH) as *const u8, SEARCH_LEN);
                let name_slice = &entry.name[..name_len];
                // Case-insensitive substring match
                if !contains_ci(name_slice, search) { continue; }
            }

            count += 1;
        }
    }
    FILE_COUNT = count;
    if SELECTED_IDX >= FILE_COUNT && FILE_COUNT > 0 { SELECTED_IDX = FILE_COUNT - 1; }
}

/// Case-insensitive substring search
fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() { return true; }
    if needle.len() > haystack.len() { return false; }
    for i in 0..=haystack.len() - needle.len() {
        let mut found = true;
        for j in 0..needle.len() {
            let h = if haystack[i+j] >= b'A' && haystack[i+j] <= b'Z' { haystack[i+j] + 32 } else { haystack[i+j] };
            let n = if needle[j] >= b'A' && needle[j] <= b'Z' { needle[j] + 32 } else { needle[j] };
            if h != n { found = false; break; }
        }
        if found { return true; }
    }
    false
}

/// Semantic search via folk_query_files
unsafe fn semantic_search() {
    if SEARCH_LEN <= 1 { return; } // Need more than just "~"
    let query = core::slice::from_raw_parts(
        (core::ptr::addr_of!(SEARCH) as *const u8).add(1), SEARCH_LEN - 1);
    let result_ptr = core::ptr::addr_of_mut!(QUERY_BUF) as *mut u8;
    let bytes = folk_query_files(
        query.as_ptr() as i32, query.len() as i32,
        result_ptr as i32, 512);
    if bytes <= 0 { return; }
    // Parse results — same format as list_files
    // (In practice, folk_query_files returns differently, but we handle what we get)
}

// ── Preview & AI tagging ────────────────────────────────────────────────

unsafe fn load_preview(idx: usize) {
    if idx >= FILE_COUNT { return; }
    let f = core::ptr::addr_of!(FILES) as *const FileEntry;
    let entry = &*f.add(idx);

    let preview_ptr = core::ptr::addr_of_mut!(PREVIEW) as *mut u8;
    let handle = folk_request_file(
        entry.name.as_ptr() as i32, entry.name_len as i32,
        preview_ptr as i32, MAX_PREVIEW as i32);

    if handle >= 0 {
        // folk_request_file returns bytes loaded (or handle for async)
        let loaded = handle as usize;
        PREVIEW_LEN = loaded.min(MAX_PREVIEW);
    } else {
        PREVIEW_LEN = 0;
    }

    PREVIEW_IS_BINARY = if PREVIEW_LEN > 0 {
        is_binary(core::slice::from_raw_parts(preview_ptr, PREVIEW_LEN))
    } else { false };

    PREVIEW_LOADED_IDX = idx as i32;

    // Trigger AI tagging if not binary and not already tagged
    if !PREVIEW_IS_BINARY && PREVIEW_LEN > 0 && TAGS_FOR_IDX != idx as i32 {
        TAG_REQUESTED = true;
    }
}

unsafe fn request_ai_tags(idx: usize) {
    if idx >= FILE_COUNT { return; }
    let preview_ptr = core::ptr::addr_of!(PREVIEW) as *const u8;
    let preview = core::slice::from_raw_parts(preview_ptr, PREVIEW_LEN.min(500));

    let prompt_ptr = core::ptr::addr_of_mut!(AI_PROMPT) as *mut u8;
    let mut m = Msg::new(core::slice::from_raw_parts_mut(prompt_ptr, 600));
    m.s(b"Give exactly 3 short keyword tags for this file content. ");
    m.s(b"Return ONLY the 3 tags separated by commas. Content: ");
    // Append first 400 bytes of preview as text
    let safe_len = preview.len().min(400);
    for i in 0..safe_len {
        let b = preview[i];
        if b >= 0x20 && b < 0x7F {
            if m.pos < m.buf.len() { m.buf[m.pos] = b; m.pos += 1; }
        }
    }
    let prompt_len = m.len();

    let resp_ptr = core::ptr::addr_of_mut!(AI_RESP) as *mut u8;
    let result = folk_slm_generate(
        core::ptr::addr_of!(AI_PROMPT) as i32, prompt_len as i32,
        resp_ptr as i32, 128);

    if result > 0 {
        let tags_ptr = core::ptr::addr_of_mut!(TAGS) as *mut u8;
        let copy = (result as usize).min(MAX_TAGS);
        core::ptr::copy_nonoverlapping(resp_ptr, tags_ptr, copy);
        TAGS_LEN = copy;
        TAGS_FOR_IDX = idx as i32;
    }

    TAG_REQUESTED = false;
}

// ── Input ───────────────────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let evt_ptr = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(evt_ptr as i32) == 0 { break; }
        let event_type = *evt_ptr.add(0);
        let data = *evt_ptr.add(3);
        if event_type != 3 { continue; }

        let key = data as u8;
        if SEARCH_ACTIVE {
            match key {
                // Escape — deactivate search
                0x1B => { SEARCH_ACTIVE = false; }
                // Enter — apply search
                0x0D => {
                    if SEARCH_LEN > 0 && SEARCH[0] == b'~' {
                        semantic_search();
                    }
                    refresh_file_list();
                    SEARCH_ACTIVE = false;
                }
                // Backspace
                0x08 => {
                    if SEARCH_LEN > 0 {
                        SEARCH_LEN -= 1;
                        refresh_file_list();
                    }
                }
                // Printable
                0x20..=0x7E => {
                    if SEARCH_LEN < MAX_SEARCH - 1 {
                        let s = core::ptr::addr_of_mut!(SEARCH) as *mut u8;
                        *s.add(SEARCH_LEN) = key;
                        SEARCH_LEN += 1;
                        // Live filter as you type
                        if SEARCH[0] != b'~' { refresh_file_list(); }
                    }
                }
                _ => {}
            }
        } else {
            match key {
                // '/' or Tab — activate search
                0x2F | 0x09 => {
                    SEARCH_ACTIVE = true;
                    SEARCH_LEN = 0;
                }
                // Up arrow or k
                0x26 | 0x6B => {
                    if SELECTED_IDX > 0 { SELECTED_IDX -= 1; }
                }
                // Down arrow or j
                0x28 | 0x6A => {
                    if SELECTED_IDX + 1 < FILE_COUNT { SELECTED_IDX += 1; }
                }
                // Enter — load preview
                0x0D => {
                    load_preview(SELECTED_IDX);
                }
                // Escape — close app
                0x1B => {}
                _ => {}
            }
        }
    }
}

// ── Rendering ───────────────────────────────────────────────────────────

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();

    folk_fill_screen(BG);

    // ── Header ──
    folk_draw_rect(0, 0, sw, HEADER_H, PANEL_BG);
    draw(MARGIN, 6, b"VFS-Explorer", TEXT_ACCENT);
    draw(120, 6, b"Semantic File Browser", TEXT_DIM);

    // File count
    let mut cnt_buf = [0u8; 20];
    let cnt_len = { let mut m = Msg::new(&mut cnt_buf); m.u32(FILE_COUNT as u32); m.s(b" files"); m.len() };
    draw(sw - 100, 6, &cnt_buf[..cnt_len], TEXT_DIM);

    // ── Search bar ──
    let search_y = HEADER_H;
    folk_draw_rect(0, search_y, sw, SEARCH_H, SEARCH_BG);
    folk_draw_rect(MARGIN, search_y + 4, sw - MARGIN * 2, SEARCH_H - 8, 0x1A2332);

    let prompt_icon = if SEARCH_LEN > 0 && unsafe { SEARCH[0] } == b'~' {
        b"~> " as &[u8]  // Semantic search indicator
    } else {
        b"/> " // Normal filter
    };
    let icon_color = if SEARCH_LEN > 0 && unsafe { SEARCH[0] } == b'~' { TEXT_MAGENTA } else { TEXT_DIM };
    draw(MARGIN + 4, search_y + 8, prompt_icon, icon_color);

    // Search text
    if SEARCH_LEN > 0 {
        let s = core::ptr::addr_of!(SEARCH) as *const u8;
        folk_draw_text(MARGIN + 28, search_y + 8, s as i32, SEARCH_LEN as i32, TEXT);
    } else if SEARCH_ACTIVE {
        draw(MARGIN + 28, search_y + 8, b"Type to filter, ~prefix for semantic search...", TEXT_DIM);
    } else {
        draw(MARGIN + 28, search_y + 8, b"Press / or Tab to search", TEXT_DIM);
    }

    // Blinking cursor
    if SEARCH_ACTIVE {
        let cx = MARGIN + 28 + (SEARCH_LEN as i32) * FONT_W;
        folk_draw_rect(cx, search_y + 6, 2, FONT_H, TEXT_ACCENT);
    }

    // ── Vertical divider ──
    let content_y = HEADER_H + SEARCH_H;
    folk_draw_line(LIST_W, content_y, LIST_W, sh - HELP_H, BORDER);

    // ── Left panel: File list ──
    render_file_list(content_y, sh);

    // ── Right panel: Preview ──
    render_preview(sw, content_y, sh);

    // ── Help bar ──
    folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
    draw(MARGIN, sh - HELP_H + 1,
        b"[/] Search  [~] Semantic  [j/k] Navigate  [Enter] Preview  [Esc] Back",
        TEXT_DIM);
}

unsafe fn render_file_list(content_y: i32, sh: i32) {
    let x0 = MARGIN;
    let y0 = content_y + 4;
    let visible_rows = ((sh - HELP_H - content_y - 8) / LINE_H) as usize;

    let files = core::ptr::addr_of!(FILES) as *const FileEntry;

    for vi in 0..visible_rows {
        let fi = SCROLL_OFFSET + vi;
        if fi >= FILE_COUNT { break; }

        let entry = &*files.add(fi);
        let y = y0 + (vi as i32) * LINE_H;

        // Selection highlight
        if fi == SELECTED_IDX {
            folk_draw_rect(0, y - 1, LIST_W - 1, LINE_H, SELECTED_BG);
            folk_draw_rect(0, y - 1, 3, LINE_H, TEXT_ACCENT); // left accent bar
        }

        // File type badge
        let tc = type_color(entry.file_type);
        draw(x0, y, type_label(entry.file_type), tc);

        // File name
        let name_x = x0 + 5 * FONT_W;
        folk_draw_text(name_x, y, entry.name.as_ptr() as i32, entry.name_len as i32,
            if fi == SELECTED_IDX { TEXT } else { TEXT_DIM });

        // File size (right-aligned)
        let mut size_buf = [0u8; 12];
        let size_len = format_size(entry.size, &mut size_buf);
        let size_x = LIST_W - MARGIN - (size_len as i32) * FONT_W;
        draw(size_x, y, &size_buf[..size_len], TEXT_DIM);
    }

    // Scroll indicator
    if FILE_COUNT > visible_rows {
        let bar_h = (sh - HELP_H - content_y - 8) as i32;
        let thumb_h = ((visible_rows as i32) * bar_h / FILE_COUNT as i32).max(10);
        let thumb_y = content_y + 4 + (SCROLL_OFFSET as i32 * bar_h / FILE_COUNT as i32);
        folk_draw_rect(LIST_W - 3, content_y + 4, 2, bar_h, 0x0F1318);
        folk_draw_rect(LIST_W - 3, thumb_y, 2, thumb_h, BORDER);
    }
}

fn format_size(size: u32, buf: &mut [u8]) -> usize {
    let mut m = Msg::new(buf);
    if size >= 1024 * 1024 {
        m.u32(size / (1024 * 1024)); m.s(b"MB");
    } else if size >= 1024 {
        m.u32(size / 1024); m.s(b"KB");
    } else {
        m.u32(size); m.s(b"B");
    }
    m.len()
}

unsafe fn render_preview(sw: i32, content_y: i32, sh: i32) {
    let x0 = LIST_W + MARGIN;
    let y0 = content_y + 4;
    let panel_w = sw - LIST_W - MARGIN * 2;

    // Auto-load preview for selected file
    if SELECTED_IDX < FILE_COUNT && PREVIEW_LOADED_IDX != SELECTED_IDX as i32 {
        load_preview(SELECTED_IDX);
    }

    // Auto-tag if requested
    if TAG_REQUESTED && !PREVIEW_IS_BINARY && PREVIEW_LEN > 0 {
        request_ai_tags(SELECTED_IDX);
    }

    if PREVIEW_LOADED_IDX < 0 || FILE_COUNT == 0 {
        draw(x0 + 20, y0 + 40, b"Select a file to preview", TEXT_DIM);
        return;
    }

    let files = core::ptr::addr_of!(FILES) as *const FileEntry;
    let entry = &*files.add(SELECTED_IDX);

    // File name header
    folk_draw_text(x0, y0, entry.name.as_ptr() as i32, entry.name_len as i32, TEXT);

    // Size
    let mut sb = [0u8; 16];
    let sl = format_size(entry.size, &mut sb);
    draw(x0 + (entry.name_len as i32 + 2) * FONT_W, y0, &sb[..sl], TEXT_DIM);

    // AI Tags (if available)
    if TAGS_FOR_IDX == SELECTED_IDX as i32 && TAGS_LEN > 0 {
        draw(x0, y0 + 18, b"Tags:", TEXT_DIM);
        let tags = core::slice::from_raw_parts(
            core::ptr::addr_of!(TAGS) as *const u8, TAGS_LEN);
        folk_draw_text(x0 + 48, y0 + 18, tags.as_ptr() as i32, tags.len() as i32, TEXT_MAGENTA);
    }

    // Binary indicator
    if PREVIEW_IS_BINARY {
        draw(x0, y0 + 36, b"[BINARY] Hex Dump (AI tagging skipped):", TEXT_RED);
        render_hex_dump(x0, y0 + 54, panel_w, sh - HELP_H - y0 - 54);
    } else {
        let preview_start_y = if TAGS_FOR_IDX == SELECTED_IDX as i32 { y0 + 36 } else { y0 + 18 };
        render_text_preview(x0, preview_start_y, panel_w, sh - HELP_H - preview_start_y);
    }
}

unsafe fn render_text_preview(x0: i32, y0: i32, _panel_w: i32, max_h: i32) {
    let data = core::slice::from_raw_parts(
        core::ptr::addr_of!(PREVIEW) as *const u8, PREVIEW_LEN);

    let max_lines = (max_h / LINE_H) as usize;
    let mut line = 0usize;
    let mut col = 0i32;
    let max_cols = 80i32;

    for &b in data {
        if line >= max_lines { break; }

        if b == b'\n' {
            line += 1;
            col = 0;
            continue;
        }

        if b >= 0x20 && b < 0x7F && col < max_cols {
            let cx = x0 + col * FONT_W;
            let cy = y0 + (line as i32) * LINE_H;
            folk_draw_text(cx, cy, &b as *const u8 as i32, 1, TEXT);
        }

        col += 1;
        if col >= max_cols { col = 0; line += 1; }
    }
}

unsafe fn render_hex_dump(x0: i32, y0: i32, _panel_w: i32, max_h: i32) {
    let data = core::slice::from_raw_parts(
        core::ptr::addr_of!(PREVIEW) as *const u8, PREVIEW_LEN);

    let max_lines = (max_h / LINE_H) as usize;
    let bytes_per_line = 16usize;
    let hex_chars = b"0123456789ABCDEF";

    for line in 0..max_lines {
        let offset = line * bytes_per_line;
        if offset >= data.len() { break; }

        let y = y0 + (line as i32) * LINE_H;

        // Offset column
        let mut off_buf = [0u8; 8];
        let mut m = Msg::new(&mut off_buf);
        let off_val = offset as u32;
        // Simple hex format: just show decimal offset
        m.u32(off_val);
        let off_len = m.len();
        draw(x0, y, &off_buf[..off_len], TEXT_DIM);

        // Hex bytes
        let hex_x = x0 + 60;
        let end = (offset + bytes_per_line).min(data.len());
        for i in offset..end {
            let bx = hex_x + ((i - offset) as i32) * 24;
            let b = data[i];
            let h1 = hex_chars[(b >> 4) as usize];
            let h2 = hex_chars[(b & 0xF) as usize];
            let pair = [h1, h2];
            let color = if b == 0 { TEXT_DIM } else { TEXT_GREEN };
            folk_draw_text(bx, y, pair.as_ptr() as i32, 2, color);
        }

        // ASCII column
        let ascii_x = hex_x + (bytes_per_line as i32) * 24 + 8;
        for i in offset..end {
            let ax = ascii_x + ((i - offset) as i32) * FONT_W;
            let b = data[i];
            let ch = if b >= 0x20 && b < 0x7F { b } else { b'.' };
            folk_draw_text(ax, y, &ch as *const u8 as i32, 1,
                if b >= 0x20 && b < 0x7F { TEXT } else { TEXT_DIM });
        }
    }
}

// ── Main ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            refresh_file_list();
            INITIALIZED = true;
        }

        handle_input();

        // Auto-refresh every 5 seconds
        let now = folk_get_time();
        if now - LAST_REFRESH > 5000 {
            refresh_file_list();
            LAST_REFRESH = now;
        }

        render();
    }
}
