//! ContextWeaver — Semantic Second Brain for Folkering OS
//!
//! A distraction-free editor where the right margin ("The Weaver")
//! dynamically shows semantically related files from Synapse VFS.
//! Uses debounced folk_query_files() to avoid UI freezes.
//!
//! Layout:
//!   Center: Clean text editor (80 cols)
//!   Right:  "The Weaver" — live semantic links to related files
//!   Bottom: Status bar (word count, save status, weaver state)
//!
//! Tab on highlighted link → inject summary into editor.

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
    fn folk_query_files(query_ptr: i32, query_len: i32, result_ptr: i32, max_len: i32) -> i32;
    fn folk_request_file(path_ptr: i32, path_len: i32, dest_ptr: i32, dest_len: i32) -> i32;
    fn folk_write_file(path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32) -> i32;
    fn folk_slm_generate(prompt_ptr: i32, prompt_len: i32, buf_ptr: i32, max_len: i32) -> i32;
    fn folk_log_telemetry(action_type: i32, target_id: i32, duration_ms: i32);
}

// ── Colors — warm, paper-like writing theme ─────────────────────────────

const BG: i32 = 0x0D1117;
const EDITOR_BG: i32 = 0x0F1318;
const WEAVER_BG: i32 = 0x131921;
const BORDER: i32 = 0x30363D;
const TEXT: i32 = 0xC9D1D9;
const TEXT_DIM: i32 = 0x484F58;
const TEXT_ACCENT: i32 = 0x58A6FF;
const TEXT_GREEN: i32 = 0x3FB950;
const TEXT_MAGENTA: i32 = 0xBC8CFF;
const TEXT_YELLOW: i32 = 0xD29922;
const CURSOR: i32 = 0xF5C2E7;
const LINK_BG: i32 = 0x1A2332;
const LINK_ACTIVE: i32 = 0x213352;
const STATUS_BG: i32 = 0x161B22;

// Layout
const WEAVER_W: i32 = 260; // right panel width (fits 1024px VGA mirror)
const STATUS_H: i32 = 20;
const TOP_H: i32 = 32;
const MARGIN: i32 = 16;
const FONT_H: i32 = 16;
const FONT_W: i32 = 8;
const LINE_H: i32 = 20;

// Limits
const MAX_TEXT: usize = 4096;
const MAX_LINKS: usize = 8;
const MAX_LINK_NAME: usize = 32;
const MAX_QUERY_RESULT: usize = 512;
const DEBOUNCE_MS: i32 = 800;

// Telemetry action types
const ACTION_APP_OPENED: i32 = 0;
const ACTION_UI_INTERACTION: i32 = 3;
const ACTION_FILE_WRITTEN: i32 = 7;

// ── Weaver link ─────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct WeaverLink {
    name: [u8; MAX_LINK_NAME],
    name_len: u8,
    size: u32,
}

impl WeaverLink {
    const fn empty() -> Self {
        Self { name: [0u8; MAX_LINK_NAME], name_len: 0, size: 0 }
    }
}

// ── Persistent state ────────────────────────────────────────────────────

// Editor
static mut DOC: [u8; MAX_TEXT] = [0u8; MAX_TEXT];
static mut DOC_LEN: usize = 0;
static mut CURSOR_POS: usize = 0;
static mut SCROLL_LINE: usize = 0;
static mut MODIFIED: bool = false;

// Weaver
static mut LINKS: [WeaverLink; MAX_LINKS] = [WeaverLink::empty(); MAX_LINKS];
static mut LINK_COUNT: usize = 0;
static mut SELECTED_LINK: usize = 0;
static mut WEAVER_ACTIVE: bool = false; // true = focus on weaver panel

// Debounce
static mut LAST_EDIT_MS: i32 = 0;
static mut LAST_QUERY_MS: i32 = 0;
static mut QUERY_PENDING: bool = false;

// Buffers
static mut QUERY_BUF: [u8; MAX_QUERY_RESULT] = [0u8; MAX_QUERY_RESULT];
static mut PREVIEW_BUF: [u8; 512] = [0u8; 512];
static mut PREVIEW_LEN: usize = 0;
static mut AI_BUF: [u8; 400] = [0u8; 400];
static mut EVT: [i32; 4] = [0i32; 4];

static mut INITIALIZED: bool = false;

// ── Helpers ─────────────────────────────────────────────────────────────

unsafe fn draw(x: i32, y: i32, text: &[u8], color: i32) {
    folk_draw_text(x, y, text.as_ptr() as i32, text.len() as i32, color);
}

struct Msg<'a> { buf: &'a mut [u8], pos: usize }
impl<'a> Msg<'a> {
    fn new(buf: &'a mut [u8]) -> Self { Self { buf, pos: 0 } }
    fn s(&mut self, t: &[u8]) {
        for &b in t { if self.pos < self.buf.len() { self.buf[self.pos] = b; self.pos += 1; } }
    }
    fn u32(&mut self, mut v: u32) {
        if v == 0 { self.s(b"0"); return; }
        let mut t = [0u8; 10]; let mut i = 0;
        while v > 0 { t[i] = b'0' + (v % 10) as u8; v /= 10; i += 1; }
        while i > 0 { i -= 1; if self.pos < self.buf.len() { self.buf[self.pos] = t[i]; self.pos += 1; } }
    }
    fn len(&self) -> usize { self.pos }
}

/// Count words in document
unsafe fn word_count() -> usize {
    let d = core::ptr::addr_of!(DOC) as *const u8;
    let mut count = 0usize;
    let mut in_word = false;
    for i in 0..DOC_LEN {
        let c = *d.add(i);
        if c == b' ' || c == b'\n' || c == b'\t' {
            if in_word { count += 1; in_word = false; }
        } else {
            in_word = true;
        }
    }
    if in_word { count += 1; }
    count
}

/// Extract the last ~60 chars before cursor as query context
unsafe fn get_query_context(buf: &mut [u8]) -> usize {
    let d = core::ptr::addr_of!(DOC) as *const u8;
    let ctx_start = if CURSOR_POS > 60 { CURSOR_POS - 60 } else { 0 };
    let ctx_len = CURSOR_POS - ctx_start;
    let copy = ctx_len.min(buf.len());
    for i in 0..copy { buf[i] = *d.add(ctx_start + i); }
    copy
}

// ── Semantic query (debounced) ──────────────────────────────────────────

unsafe fn run_semantic_query() {
    // Extract context from text near cursor
    let mut ctx = [0u8; 80];
    let ctx_len = get_query_context(&mut ctx);
    if ctx_len < 5 { return; } // Too short to search

    let result_ptr = core::ptr::addr_of_mut!(QUERY_BUF) as *mut u8;
    let bytes = folk_query_files(
        ctx.as_ptr() as i32, ctx_len as i32,
        result_ptr as i32, MAX_QUERY_RESULT as i32,
    );

    if bytes <= 0 {
        LINK_COUNT = 0;
        return;
    }

    // Parse results: "name\tsize\n" format
    let result = core::slice::from_raw_parts(result_ptr, bytes as usize);
    let links = core::ptr::addr_of_mut!(LINKS) as *mut WeaverLink;
    let mut count = 0usize;
    let mut i = 0usize;

    while i < result.len() && count < MAX_LINKS {
        let name_start = i;
        while i < result.len() && result[i] != b'\t' && result[i] != b'\n' { i += 1; }
        let name_end = i;
        let name_len = (name_end - name_start).min(MAX_LINK_NAME);

        if i < result.len() && result[i] == b'\t' { i += 1; }

        let mut size = 0u32;
        while i < result.len() && result[i] != b'\n' {
            if result[i] >= b'0' && result[i] <= b'9' {
                size = size * 10 + (result[i] - b'0') as u32;
            }
            i += 1;
        }
        if i < result.len() { i += 1; }

        if name_len > 0 {
            let link = &mut *links.add(count);
            *link = WeaverLink::empty();
            for j in 0..name_len { link.name[j] = result[name_start + j]; }
            link.name_len = name_len as u8;
            link.size = size;
            count += 1;
        }
    }
    LINK_COUNT = count;
    if SELECTED_LINK >= LINK_COUNT && LINK_COUNT > 0 { SELECTED_LINK = 0; }
    LAST_QUERY_MS = folk_get_time();
    QUERY_PENDING = false;

    // Telemetry: UiInteraction for semantic search
    folk_log_telemetry(ACTION_UI_INTERACTION, ctx_len as i32, 0);
}

/// Inject a summary of the selected link into the document at cursor position.
unsafe fn inject_link_summary() {
    if SELECTED_LINK >= LINK_COUNT { return; }

    let links = core::ptr::addr_of!(LINKS) as *const WeaverLink;
    let link = &*links.add(SELECTED_LINK);
    let name = &link.name[..link.name_len as usize];

    // Load file preview
    let preview_ptr = core::ptr::addr_of_mut!(PREVIEW_BUF) as *mut u8;
    let loaded = folk_request_file(
        name.as_ptr() as i32, link.name_len as i32,
        preview_ptr as i32, 512,
    );
    if loaded <= 0 { return; }
    PREVIEW_LEN = (loaded as usize).min(512);

    // Ask AI to summarize
    let ai_ptr = core::ptr::addr_of_mut!(AI_BUF) as *mut u8;
    let mut prompt = [0u8; 600];
    let prompt_len = {
        let mut m = Msg::new(&mut prompt);
        m.s(b"Summarize this file in 1-2 sentences for inline citation: ");
        let safe = PREVIEW_LEN.min(400);
        let preview = core::slice::from_raw_parts(preview_ptr, safe);
        for &b in preview {
            if b >= 0x20 && b < 0x7F {
                if m.pos < m.buf.len() { m.buf[m.pos] = b; m.pos += 1; }
            }
        }
        m.len()
    };

    let resp = folk_slm_generate(
        prompt.as_ptr() as i32, prompt_len as i32,
        ai_ptr as i32, 200,
    );

    if resp <= 0 { return; }

    // Build injection text: "\n[From: filename] summary\n"
    let mut inject = [0u8; 300];
    let inject_len = {
        let mut m = Msg::new(&mut inject);
        m.s(b"\n[From: ");
        m.s(name);
        m.s(b"] ");
        let summary = core::slice::from_raw_parts(ai_ptr, (resp as usize).min(200));
        m.s(summary);
        m.s(b"\n");
        m.len()
    };

    // Insert at cursor position
    let d = core::ptr::addr_of_mut!(DOC) as *mut u8;
    if DOC_LEN + inject_len < MAX_TEXT {
        // Shift text right
        let mut i = DOC_LEN;
        while i > CURSOR_POS {
            *d.add(i + inject_len - 1) = *d.add(i - 1);
            i -= 1;
        }
        // Copy injection
        for i in 0..inject_len {
            *d.add(CURSOR_POS + i) = inject[i];
        }
        DOC_LEN += inject_len;
        CURSOR_POS += inject_len;
        MODIFIED = true;

        // Telemetry: UiInteraction for link injection
        folk_log_telemetry(ACTION_UI_INTERACTION, 1, 0);
    }
}

// ── Save document ───────────────────────────────────────────────────────

unsafe fn save_document() {
    let path = b"notes/context_weaver.txt";
    folk_write_file(
        path.as_ptr() as i32, path.len() as i32,
        core::ptr::addr_of!(DOC) as i32, DOC_LEN as i32,
    );
    MODIFIED = false;
    folk_log_telemetry(ACTION_FILE_WRITTEN, 0, 0);
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
        match key {
            // Ctrl+S — Save
            0x13 => { save_document(); }
            // Escape — toggle weaver focus
            0x1B => { WEAVER_ACTIVE = false; }
            // Tab — if weaver active, inject link; else switch to weaver
            0x09 => {
                if WEAVER_ACTIVE && LINK_COUNT > 0 {
                    inject_link_summary();
                    WEAVER_ACTIVE = false;
                } else if LINK_COUNT > 0 {
                    WEAVER_ACTIVE = true;
                }
            }
            _ => {
                if WEAVER_ACTIVE {
                    match key {
                        0x26 | 0x6B => { if SELECTED_LINK > 0 { SELECTED_LINK -= 1; } } // Up
                        0x28 | 0x6A => { if SELECTED_LINK + 1 < LINK_COUNT { SELECTED_LINK += 1; } } // Down
                        _ => {}
                    }
                } else {
                    handle_editor_key(key);
                }
            }
        }
    }
}

unsafe fn handle_editor_key(key: u8) {
    let d = core::ptr::addr_of_mut!(DOC) as *mut u8;
    match key {
        // Backspace
        0x08 => {
            if CURSOR_POS > 0 && DOC_LEN > 0 {
                let mut i = CURSOR_POS - 1;
                while i < DOC_LEN - 1 { *d.add(i) = *d.add(i + 1); i += 1; }
                DOC_LEN -= 1;
                CURSOR_POS -= 1;
                MODIFIED = true;
                LAST_EDIT_MS = folk_get_time();
                QUERY_PENDING = true;
            }
        }
        0x25 => { if CURSOR_POS > 0 { CURSOR_POS -= 1; } } // Left
        0x27 => { if CURSOR_POS < DOC_LEN { CURSOR_POS += 1; } } // Right
        0x26 => { // Up — move cursor up one line
            let col = cursor_col();
            let line_start = find_line_start(CURSOR_POS);
            if line_start > 0 {
                let prev_line_start = find_line_start(line_start - 1);
                let prev_line_len = line_start - 1 - prev_line_start;
                CURSOR_POS = prev_line_start + col.min(prev_line_len);
            }
        }
        0x28 => { // Down — move cursor down one line
            let col = cursor_col();
            let next_nl = find_next_newline(CURSOR_POS);
            if next_nl < DOC_LEN {
                let next_line_start = next_nl + 1;
                let next_line_end = find_next_newline(next_line_start);
                let next_line_len = next_line_end - next_line_start;
                CURSOR_POS = next_line_start + col.min(next_line_len);
            }
        }
        0x24 => { CURSOR_POS = find_line_start(CURSOR_POS); } // Home
        0x23 => { CURSOR_POS = find_next_newline(CURSOR_POS); } // End
        // Enter
        0x0D => {
            if DOC_LEN < MAX_TEXT - 1 {
                let mut i = DOC_LEN;
                while i > CURSOR_POS { *d.add(i) = *d.add(i - 1); i -= 1; }
                *d.add(CURSOR_POS) = b'\n';
                DOC_LEN += 1; CURSOR_POS += 1;
                MODIFIED = true; LAST_EDIT_MS = folk_get_time(); QUERY_PENDING = true;
            }
        }
        // Printable ASCII
        0x20..=0x7E => {
            if DOC_LEN < MAX_TEXT - 1 {
                let mut i = DOC_LEN;
                while i > CURSOR_POS { *d.add(i) = *d.add(i - 1); i -= 1; }
                *d.add(CURSOR_POS) = key;
                DOC_LEN += 1; CURSOR_POS += 1;
                MODIFIED = true; LAST_EDIT_MS = folk_get_time(); QUERY_PENDING = true;
            }
        }
        _ => {}
    }
}

unsafe fn cursor_col() -> usize {
    let d = core::ptr::addr_of!(DOC) as *const u8;
    let mut col = 0;
    let mut i = CURSOR_POS;
    while i > 0 {
        i -= 1;
        if *d.add(i) == b'\n' { break; }
        col += 1;
    }
    col
}

unsafe fn find_line_start(pos: usize) -> usize {
    let d = core::ptr::addr_of!(DOC) as *const u8;
    let mut i = pos;
    while i > 0 { i -= 1; if *d.add(i) == b'\n' { return i + 1; } }
    0
}

unsafe fn find_next_newline(pos: usize) -> usize {
    let d = core::ptr::addr_of!(DOC) as *const u8;
    let mut i = pos;
    while i < DOC_LEN { if *d.add(i) == b'\n' { return i; } i += 1; }
    DOC_LEN
}

// ── Rendering ───────────────────────────────────────────────────────────

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();
    // Clamp to 1024 for VGA mirror compatibility (VirtIO-GPU is 1280 but screendump is 1024)
    let usable_w = if sw > 1024 { 1024 } else { sw };
    let editor_w = usable_w - WEAVER_W;

    folk_fill_screen(BG);

    // ── Top bar ──
    folk_draw_rect(0, 0, sw, TOP_H, STATUS_BG);
    draw(MARGIN, 8, b"ContextWeaver", TEXT_ACCENT);

    let mode = if WEAVER_ACTIVE { b"WEAVER" as &[u8] } else { b"WRITE" };
    let mode_c = if WEAVER_ACTIVE { TEXT_MAGENTA } else { TEXT_GREEN };
    draw(140, 8, mode, mode_c);

    if MODIFIED {
        draw(220, 8, b"[modified]", TEXT_YELLOW);
    }

    // Word count
    let wc = word_count();
    let mut wc_buf = [0u8; 16];
    let wc_len = { let mut m = Msg::new(&mut wc_buf); m.u32(wc as u32); m.s(b" words"); m.len() };
    draw(usable_w - 120, 8, &wc_buf[..wc_len], TEXT_DIM);

    // ── Editor panel ──
    let ed_x = MARGIN;
    let ed_y = TOP_H + 4;
    let ed_w = editor_w - MARGIN * 2;
    let ed_h = sh - TOP_H - STATUS_H - 8;
    folk_draw_rect(ed_x - 4, ed_y - 4, ed_w + 8, ed_h + 8, EDITOR_BG);

    let d = core::ptr::addr_of!(DOC) as *const u8;
    let max_cols = ((ed_w) / FONT_W) as usize;
    let max_lines = (ed_h / LINE_H) as usize;

    // Count lines to scroll position
    let mut line = 0usize;
    let mut col = 0usize;
    let mut i = 0usize;

    // Skip to scroll offset
    let mut skip_lines = SCROLL_LINE;
    let mut render_start = 0usize;
    if skip_lines > 0 {
        let mut sl = 0usize;
        while sl < DOC_LEN && skip_lines > 0 {
            if *d.add(sl) == b'\n' { skip_lines -= 1; }
            sl += 1;
        }
        render_start = sl;
    }

    i = render_start;
    while i <= DOC_LEN && line < max_lines {
        // Draw cursor
        if i == CURSOR_POS && !WEAVER_ACTIVE {
            let cx = ed_x + (col as i32) * FONT_W;
            let cy = ed_y + (line as i32) * LINE_H;
            folk_draw_rect(cx, cy, 2, FONT_H, CURSOR);
        }

        if i >= DOC_LEN { break; }
        let ch = *d.add(i);

        if ch == b'\n' {
            line += 1; col = 0;
        } else {
            if col < max_cols {
                let cx = ed_x + (col as i32) * FONT_W;
                let cy = ed_y + (line as i32) * LINE_H;
                folk_draw_text(cx, cy, d.add(i) as i32, 1, TEXT);
            }
            col += 1;
            if col >= max_cols { col = 0; line += 1; }
        }
        i += 1;
    }

    // Auto-scroll to keep cursor visible
    // (Simple: if cursor line > visible, scroll down)

    // ── Vertical divider ──
    folk_draw_line(editor_w, TOP_H, editor_w, sh - STATUS_H, BORDER);
    // All right-panel drawing uses editor_w as left edge (fits in usable_w)

    // ── Weaver panel (right) ──
    let wx = editor_w + 8;
    let wy = TOP_H + 4;
    folk_draw_rect(editor_w + 1, TOP_H, WEAVER_W - 1, sh - TOP_H - STATUS_H, WEAVER_BG);

    draw(wx, wy, b"The Weaver", TEXT_MAGENTA);

    if LINK_COUNT == 0 {
        draw(wx, wy + 24, b"Start writing to", TEXT_DIM);
        draw(wx, wy + 42, b"discover connections...", TEXT_DIM);
    } else {
        draw(wx, wy + 20, b"Related files:", TEXT_DIM);

        let links = core::ptr::addr_of!(LINKS) as *const WeaverLink;
        for li in 0..LINK_COUNT {
            let link = &*links.add(li);
            let ly = wy + 40 + (li as i32) * 28;

            // Highlight selected
            let bg = if li == SELECTED_LINK && WEAVER_ACTIVE { LINK_ACTIVE } else { LINK_BG };
            folk_draw_rect(wx - 4, ly - 2, WEAVER_W - 16, 24, bg);

            // Link icon
            draw(wx, ly, b">", TEXT_ACCENT);

            // File name
            folk_draw_text(
                wx + 12, ly,
                link.name.as_ptr() as i32, link.name_len as i32,
                if li == SELECTED_LINK { TEXT } else { TEXT_DIM },
            );

            // Size
            let mut sb = [0u8; 10];
            let sl = { let mut m = Msg::new(&mut sb); m.u32(link.size / 1024); m.s(b"KB"); m.len() };
            draw(wx + WEAVER_W - 80, ly, &sb[..sl], TEXT_DIM);
        }

        // Hint
        if WEAVER_ACTIVE {
            let hint_y = wy + 40 + (LINK_COUNT as i32) * 28 + 12;
            draw(wx, hint_y, b"[Tab] Inject summary", TEXT_ACCENT);
            draw(wx, hint_y + 18, b"[Esc] Back to editor", TEXT_DIM);
        } else if LINK_COUNT > 0 {
            let hint_y = wy + 40 + (LINK_COUNT as i32) * 28 + 12;
            draw(wx, hint_y, b"[Tab] Focus weaver", TEXT_DIM);
        }
    }

    // Debounce indicator
    if QUERY_PENDING {
        draw(wx, sh - STATUS_H - 24, b"Searching...", TEXT_YELLOW);
    }

    // ── Status bar ──
    folk_draw_rect(0, sh - STATUS_H, sw, STATUS_H, STATUS_BG);

    let mut st_buf = [0u8; 48];
    let st_len = {
        let mut m = Msg::new(&mut st_buf);
        m.s(b"L:");
        m.u32(cursor_line() as u32 + 1);
        m.s(b" C:");
        m.u32(cursor_col() as u32 + 1);
        m.s(b"  |  ");
        m.u32(DOC_LEN as u32);
        m.s(b"/");
        m.u32(MAX_TEXT as u32);
        m.len()
    };
    draw(MARGIN, sh - STATUS_H + 2, &st_buf[..st_len], TEXT_DIM);

    draw(sw / 2, sh - STATUS_H + 2,
        b"[Ctrl+S] Save  [Tab] Weaver  [Esc] Editor",
        TEXT_DIM);
}

unsafe fn cursor_line() -> usize {
    let d = core::ptr::addr_of!(DOC) as *const u8;
    let mut lines = 0;
    for i in 0..CURSOR_POS { if *d.add(i) == b'\n' { lines += 1; } }
    lines
}

// ── Main ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            // Default content
            let default = b"# ContextWeaver\n\nStart writing here. The Weaver panel on the right\nwill find related files as you type.\n";
            let d = core::ptr::addr_of_mut!(DOC) as *mut u8;
            for i in 0..default.len() { *d.add(i) = default[i]; }
            DOC_LEN = default.len();
            CURSOR_POS = DOC_LEN;
            INITIALIZED = true;

            // Telemetry: AppOpened
            folk_log_telemetry(ACTION_APP_OPENED, 0, 0);
        }

        handle_input();

        // Debounced semantic query: runs 800ms after last edit
        let now = folk_get_time();
        if QUERY_PENDING && (now - LAST_EDIT_MS) > DEBOUNCE_MS {
            run_semantic_query();
        }

        render();
    }
}
