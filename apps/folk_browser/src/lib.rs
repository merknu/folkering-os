//! folk_browser — Semantic Web Agent for Folkering OS
//!
//! A pragmatic HTML renderer that uses folk_submit_display_list for
//! batch rendering (1000x less fuel than individual draw calls).
//!
//! Two modes:
//!   Standard View: Parses HTML, lays out boxes, renders to display list
//!   Semantic View: Sends content to AI for structured summary
//!
//! Uses folk_http_get for fetching pages over the existing TLS 1.3 stack.

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);
    fn folk_draw_line(x1: i32, y1: i32, x2: i32, y2: i32, color: i32);
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
    fn folk_get_time() -> i32;
    fn folk_poll_event(event_ptr: i32) -> i32;
    fn folk_http_get(url_ptr: i32, url_len: i32, buf_ptr: i32, max_len: i32) -> i32;
    fn folk_slm_generate(prompt_ptr: i32, prompt_len: i32, buf_ptr: i32, max_len: i32) -> i32;
    fn folk_submit_display_list(ptr: i32, len: i32) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
}

// ── Colors ──────────────────────────────────────────────────────────────

const BG: i32 = 0xF5F5F5;        // light page background
const TEXT_COLOR: i32 = 0x1A1A1A; // near-black text
const LINK_COLOR: i32 = 0x0645AD; // blue links
const H1_COLOR: i32 = 0x111111;
const H2_COLOR: i32 = 0x222222;
const CODE_BG: i32 = 0xE8E8E8;
const URLBAR_BG: i32 = 0x2D333B;
const URLBAR_TEXT: i32 = 0xC9D1D9;
const STATUS_BG: i32 = 0x161B22;
const STATUS_TEXT: i32 = 0x8B949E;
const CURSOR_COLOR: i32 = 0x58A6FF;
const SEMANTIC_BG: i32 = 0x1E1535;
const SEMANTIC_TEXT: i32 = 0xD2A8FF;

// Layout
const URLBAR_H: i32 = 32;
const STATUS_H: i32 = 20;
const MARGIN: i32 = 16;
const FONT_W: i32 = 8;
const FONT_H: i32 = 16;
const LINE_H: i32 = 20;
const PARA_GAP: i32 = 12;
const H1_SIZE: i32 = 24; // conceptual — rendered at FONT_H but bold
const H2_SIZE: i32 = 20;

// Limits
const MAX_URL: usize = 256;
const MAX_HTML: usize = 4096;
const MAX_ELEMENTS: usize = 128;
const MAX_TEXT_PER_ELEM: usize = 200;
const MAX_DISPLAY_LIST: usize = 8192;
const MAX_SEMANTIC: usize = 1024;

// ── HTML Element types ──────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
enum ElemType {
    Text = 0,     // raw text node
    H1 = 1,
    H2 = 2,
    H3 = 3,
    P = 4,
    Div = 5,
    A = 6,        // link
    Br = 7,
    Hr = 8,
    Li = 9,
    Pre = 10,
    B = 11,       // bold (inline)
    I = 12,       // italic (inline)
    Title = 13,
    Unknown = 14,
}

#[derive(Clone, Copy)]
struct HtmlElement {
    elem_type: ElemType,
    text: [u8; MAX_TEXT_PER_ELEM],
    text_len: usize,
    // Layout result
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl HtmlElement {
    const fn empty() -> Self {
        Self {
            elem_type: ElemType::Text, text: [0u8; MAX_TEXT_PER_ELEM],
            text_len: 0, x: 0, y: 0, w: 0, h: 0,
        }
    }
}

// ── View mode ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum ViewMode { Standard, Semantic }

// ── State ───────────────────────────────────────────────────────────────

static mut URL: [u8; MAX_URL] = [0u8; MAX_URL];
static mut URL_LEN: usize = 0;
static mut CURSOR_POS: usize = 0;

static mut HTML_BUF: [u8; MAX_HTML] = [0u8; MAX_HTML];
static mut HTML_LEN: usize = 0;

static mut ELEMENTS: [HtmlElement; MAX_ELEMENTS] = [HtmlElement::empty(); MAX_ELEMENTS];
static mut ELEM_COUNT: usize = 0;

static mut DISPLAY_LIST: [u8; MAX_DISPLAY_LIST] = [0u8; MAX_DISPLAY_LIST];
static mut DL_LEN: usize = 0;

static mut SEMANTIC_BUF: [u8; MAX_SEMANTIC] = [0u8; MAX_SEMANTIC];
static mut SEMANTIC_LEN: usize = 0;

static mut SCROLL_Y: i32 = 0;
static mut CONTENT_HEIGHT: i32 = 0;
static mut MODE: ViewMode = ViewMode::Standard;
static mut LOADING: bool = false;
static mut PAGE_TITLE: [u8; 64] = [0u8; 64];
static mut TITLE_LEN: usize = 0;
static mut LINK_COUNT: u16 = 0;
static mut EDITING_URL: bool = true;

static mut EVT: [i32; 4] = [0i32; 4];
static mut INITIALIZED: bool = false;

// ── Helpers ─────────────────────────────────────────────────────────────

unsafe fn draw(x: i32, y: i32, text: &[u8], color: i32) {
    folk_draw_text(x, y, text.as_ptr() as i32, text.len() as i32, color);
}

struct Msg<'a> { buf: &'a mut [u8], pos: usize }
impl<'a> Msg<'a> {
    fn new(buf: &'a mut [u8]) -> Self { Self { buf, pos: 0 } }
    fn s(&mut self, t: &[u8]) { for &b in t { if self.pos < self.buf.len() { self.buf[self.pos] = b; self.pos += 1; } } }
    fn u32(&mut self, mut v: u32) {
        if v == 0 { self.s(b"0"); return; }
        let mut t = [0u8; 10]; let mut i = 0;
        while v > 0 { t[i] = b'0' + (v % 10) as u8; v /= 10; i += 1; }
        while i > 0 { i -= 1; if self.pos < self.buf.len() { self.buf[self.pos] = t[i]; self.pos += 1; } }
    }
    fn len(&self) -> usize { self.pos }
}

// ── HTML Parser ─────────────────────────────────────────────────────────

unsafe fn parse_html() {
    let html = core::slice::from_raw_parts(core::ptr::addr_of!(HTML_BUF) as *const u8, HTML_LEN);
    let elems = core::ptr::addr_of_mut!(ELEMENTS) as *mut HtmlElement;
    let mut count = 0usize;
    let mut i = 0usize;
    LINK_COUNT = 0;
    TITLE_LEN = 0;

    while i < html.len() && count < MAX_ELEMENTS {
        // Skip whitespace
        while i < html.len() && (html[i] == b' ' || html[i] == b'\n' || html[i] == b'\r' || html[i] == b'\t') {
            i += 1;
        }
        if i >= html.len() { break; }

        if html[i] == b'<' {
            // Parse tag
            i += 1;
            let closing = i < html.len() && html[i] == b'/';
            if closing { i += 1; }

            // Read tag name
            let tag_start = i;
            while i < html.len() && html[i] != b'>' && html[i] != b' ' { i += 1; }
            let tag_name = &html[tag_start..i];

            // Skip attributes + closing >
            while i < html.len() && html[i] != b'>' { i += 1; }
            if i < html.len() { i += 1; }

            if closing { continue; }

            let elem_type = match_tag(tag_name);

            // Self-closing tags
            if elem_type == ElemType::Br || elem_type == ElemType::Hr {
                let e = &mut *elems.add(count);
                *e = HtmlElement::empty();
                e.elem_type = elem_type;
                count += 1;
                continue;
            }

            // Read text content until next tag
            let text_start = i;
            while i < html.len() && html[i] != b'<' { i += 1; }
            let raw_text = &html[text_start..i];

            // Trim and store
            let trimmed = trim_bytes(raw_text);
            if !trimmed.is_empty() || elem_type == ElemType::Hr {
                let e = &mut *elems.add(count);
                *e = HtmlElement::empty();
                e.elem_type = elem_type;
                let copy = trimmed.len().min(MAX_TEXT_PER_ELEM);
                for j in 0..copy { e.text[j] = trimmed[j]; }
                e.text_len = copy;

                if elem_type == ElemType::A { LINK_COUNT += 1; }
                if elem_type == ElemType::Title {
                    let tc = copy.min(64);
                    let title = core::ptr::addr_of_mut!(PAGE_TITLE) as *mut u8;
                    for j in 0..tc { *title.add(j) = trimmed[j]; }
                    TITLE_LEN = tc;
                }

                count += 1;
            }
        } else {
            // Raw text (outside tags)
            let text_start = i;
            while i < html.len() && html[i] != b'<' { i += 1; }
            let raw_text = &html[text_start..i];
            let trimmed = trim_bytes(raw_text);
            if !trimmed.is_empty() && count < MAX_ELEMENTS {
                let e = &mut *elems.add(count);
                *e = HtmlElement::empty();
                e.elem_type = ElemType::Text;
                let copy = trimmed.len().min(MAX_TEXT_PER_ELEM);
                for j in 0..copy { e.text[j] = trimmed[j]; }
                e.text_len = copy;
                count += 1;
            }
        }
    }
    ELEM_COUNT = count;
}

fn match_tag(name: &[u8]) -> ElemType {
    let lower: [u8; 8] = {
        let mut l = [0u8; 8];
        for (i, &b) in name.iter().take(8).enumerate() {
            l[i] = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
        }
        l
    };
    let len = name.len().min(8);
    let s = &lower[..len];

    if s == b"h1" { ElemType::H1 }
    else if s == b"h2" { ElemType::H2 }
    else if s == b"h3" { ElemType::H3 }
    else if s == b"p" { ElemType::P }
    else if s == b"div" { ElemType::Div }
    else if s == b"a" { ElemType::A }
    else if s == b"br" || s == b"br/" { ElemType::Br }
    else if s == b"hr" || s == b"hr/" { ElemType::Hr }
    else if s == b"li" { ElemType::Li }
    else if s == b"pre" { ElemType::Pre }
    else if s == b"b" || s == b"strong" { ElemType::B }
    else if s == b"i" || s == b"em" { ElemType::I }
    else if s == b"title" { ElemType::Title }
    else { ElemType::Unknown }
}

fn trim_bytes(s: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = s.len();
    while start < end && (s[start] == b' ' || s[start] == b'\n' || s[start] == b'\r' || s[start] == b'\t') { start += 1; }
    while end > start && (s[end-1] == b' ' || s[end-1] == b'\n' || s[end-1] == b'\r' || s[end-1] == b'\t') { end -= 1; }
    &s[start..end]
}

// ── Layout Engine ───────────────────────────────────────────────────────

unsafe fn layout_elements(viewport_w: i32) {
    let elems = core::ptr::addr_of_mut!(ELEMENTS) as *mut HtmlElement;
    let content_w = viewport_w - MARGIN * 2;
    let chars_per_line = (content_w / FONT_W) as usize;
    let mut y = MARGIN;

    for i in 0..ELEM_COUNT {
        let e = &mut *elems.add(i);

        match e.elem_type {
            ElemType::H1 => {
                y += PARA_GAP;
                e.x = MARGIN;
                e.y = y;
                let lines = (e.text_len + chars_per_line - 1) / chars_per_line.max(1);
                e.h = (lines as i32) * (LINE_H + 4);
                e.w = content_w;
                y += e.h + PARA_GAP;
            }
            ElemType::H2 | ElemType::H3 => {
                y += PARA_GAP / 2;
                e.x = MARGIN;
                e.y = y;
                let lines = (e.text_len + chars_per_line - 1) / chars_per_line.max(1);
                e.h = (lines as i32) * LINE_H;
                e.w = content_w;
                y += e.h + PARA_GAP / 2;
            }
            ElemType::P | ElemType::Div | ElemType::Text | ElemType::Unknown => {
                e.x = MARGIN;
                e.y = y;
                let lines = if e.text_len == 0 { 0 } else {
                    (e.text_len + chars_per_line - 1) / chars_per_line.max(1)
                };
                e.h = (lines as i32) * LINE_H;
                e.w = content_w;
                y += e.h + 4;
            }
            ElemType::A => {
                e.x = MARGIN;
                e.y = y;
                let lines = (e.text_len + chars_per_line - 1) / chars_per_line.max(1);
                e.h = (lines as i32) * LINE_H;
                e.w = content_w;
                y += e.h + 2;
            }
            ElemType::Li => {
                e.x = MARGIN + 20; // indent
                e.y = y;
                let adjusted_cpl = ((content_w - 20) / FONT_W) as usize;
                let lines = (e.text_len + adjusted_cpl - 1) / adjusted_cpl.max(1);
                e.h = (lines as i32) * LINE_H;
                e.w = content_w - 20;
                y += e.h + 2;
            }
            ElemType::Pre => {
                e.x = MARGIN + 8;
                e.y = y;
                let lines = (e.text_len + chars_per_line - 1) / chars_per_line.max(1);
                e.h = (lines as i32) * LINE_H + 8;
                e.w = content_w - 16;
                y += e.h + PARA_GAP;
            }
            ElemType::Br => {
                e.x = 0; e.y = y; e.w = 0; e.h = LINE_H;
                y += LINE_H;
            }
            ElemType::Hr => {
                e.x = MARGIN; e.y = y + 4; e.w = content_w; e.h = 2;
                y += 12;
            }
            ElemType::B | ElemType::I => {
                e.x = MARGIN;
                e.y = y;
                let lines = (e.text_len + chars_per_line - 1) / chars_per_line.max(1);
                e.h = (lines as i32) * LINE_H;
                e.w = content_w;
                y += e.h + 2;
            }
            ElemType::Title => {
                e.x = 0; e.y = 0; e.w = 0; e.h = 0; // invisible
            }
        }
    }
    CONTENT_HEIGHT = y + MARGIN;
}

// ── Display List Builder ────────────────────────────────────────────────

unsafe fn build_display_list(viewport_w: i32, viewport_h: i32) {
    let dl = core::ptr::addr_of_mut!(DISPLAY_LIST) as *mut u8;
    let mut pos = 0usize;
    let elems = core::ptr::addr_of!(ELEMENTS) as *const HtmlElement;
    let scroll = SCROLL_Y;
    let vis_top = scroll;
    let vis_bottom = scroll + viewport_h;

    // Fill background
    if pos + 5 <= MAX_DISPLAY_LIST {
        *dl.add(pos) = 0x04; pos += 1; // Fill opcode
        let bg_bytes = (BG as u32).to_le_bytes();
        for j in 0..4 { *dl.add(pos + j) = bg_bytes[j]; }
        pos += 4;
    }

    for i in 0..ELEM_COUNT {
        let e = &*elems.add(i);
        let ey = e.y - scroll; // screen-space Y

        // Culling: skip elements outside viewport
        if ey + e.h < 0 || ey > viewport_h { continue; }
        if e.text_len == 0 && e.elem_type != ElemType::Hr && e.elem_type != ElemType::Br { continue; }

        match e.elem_type {
            ElemType::Hr => {
                // Line command
                if pos + 13 <= MAX_DISPLAY_LIST {
                    *dl.add(pos) = 0x03; pos += 1;
                    let vals: [u16; 4] = [e.x as u16, ey as u16, (e.x + e.w) as u16, ey as u16];
                    for v in &vals { let b = v.to_le_bytes(); *dl.add(pos) = b[0]; *dl.add(pos+1) = b[1]; pos += 2; }
                    let cb = (0x888888u32).to_le_bytes();
                    for j in 0..4 { *dl.add(pos+j) = cb[j]; } pos += 4;
                }
            }
            ElemType::Pre => {
                // Background rect for code blocks
                if pos + 13 <= MAX_DISPLAY_LIST {
                    *dl.add(pos) = 0x01; pos += 1;
                    let vals: [u16; 4] = [(e.x - 4) as u16, ey as u16, (e.w + 8) as u16, e.h as u16];
                    for v in &vals { let b = v.to_le_bytes(); *dl.add(pos) = b[0]; *dl.add(pos+1) = b[1]; pos += 2; }
                    let cb = (CODE_BG as u32).to_le_bytes();
                    for j in 0..4 { *dl.add(pos+j) = cb[j]; } pos += 4;
                }
                // Text
                pos = emit_text(dl, pos, e, ey, TEXT_COLOR);
            }
            ElemType::Li => {
                // Bullet point
                if pos + 13 <= MAX_DISPLAY_LIST && ey >= 0 {
                    *dl.add(pos) = 0x01; pos += 1;
                    let vals: [u16; 4] = [(e.x - 12) as u16, (ey + 6) as u16, 4, 4];
                    for v in &vals { let b = v.to_le_bytes(); *dl.add(pos) = b[0]; *dl.add(pos+1) = b[1]; pos += 2; }
                    let cb = (TEXT_COLOR as u32).to_le_bytes();
                    for j in 0..4 { *dl.add(pos+j) = cb[j]; } pos += 4;
                }
                pos = emit_text(dl, pos, e, ey, TEXT_COLOR);
            }
            ElemType::H1 => {
                pos = emit_text(dl, pos, e, ey, H1_COLOR);
            }
            ElemType::H2 | ElemType::H3 => {
                pos = emit_text(dl, pos, e, ey, H2_COLOR);
            }
            ElemType::A => {
                pos = emit_text(dl, pos, e, ey, LINK_COLOR);
            }
            _ => {
                pos = emit_text(dl, pos, e, ey, TEXT_COLOR);
            }
        }
    }

    DL_LEN = pos;
}

/// Emit a text element into the display list using opcode 0x02
unsafe fn emit_text(dl: *mut u8, mut pos: usize, e: &HtmlElement, screen_y: i32, color: i32) -> usize {
    if e.text_len == 0 || pos + 15 > MAX_DISPLAY_LIST { return pos; }
    if screen_y < -100 || screen_y > 800 { return pos; } // hard cull

    *dl.add(pos) = 0x02; pos += 1;
    let x_bytes = (e.x as u16).to_le_bytes();
    *dl.add(pos) = x_bytes[0]; *dl.add(pos+1) = x_bytes[1]; pos += 2;
    let y_bytes = (screen_y as u16).to_le_bytes();
    *dl.add(pos) = y_bytes[0]; *dl.add(pos+1) = y_bytes[1]; pos += 2;
    // ptr — pointer into WASM memory where text lives (in ELEMENTS array)
    let text_ptr = e.text.as_ptr() as u32;
    let pb = text_ptr.to_le_bytes();
    for j in 0..4 { *dl.add(pos+j) = pb[j]; } pos += 4;
    let len_bytes = (e.text_len as u16).to_le_bytes();
    *dl.add(pos) = len_bytes[0]; *dl.add(pos+1) = len_bytes[1]; pos += 2;
    let cb = (color as u32).to_le_bytes();
    for j in 0..4 { *dl.add(pos+j) = cb[j]; } pos += 4;

    pos
}

// ── Page Fetch ──────────────────────────────────────────────────────────

unsafe fn fetch_page() {
    LOADING = true;
    SCROLL_Y = 0;
    ELEM_COUNT = 0;
    SEMANTIC_LEN = 0;

    let url = core::slice::from_raw_parts(core::ptr::addr_of!(URL) as *const u8, URL_LEN);
    let html_ptr = core::ptr::addr_of_mut!(HTML_BUF) as *mut u8;

    let bytes = folk_http_get(
        url.as_ptr() as i32, url.len() as i32,
        html_ptr as i32, MAX_HTML as i32);

    if bytes > 0 {
        HTML_LEN = bytes as usize;
        parse_html();
        let sw = folk_screen_width();
        layout_elements(sw.min(1024));
        folk_log_telemetry(10, bytes as i32, 0); // NetworkEvent
    } else {
        // Show error as HTML
        let err = b"<h1>Connection Failed</h1><p>Could not fetch the URL. Check network status.</p>";
        let copy = err.len().min(MAX_HTML);
        for j in 0..copy { *html_ptr.add(j) = err[j]; }
        HTML_LEN = copy;
        parse_html();
        let sw = folk_screen_width();
        layout_elements(sw.min(1024));
    }

    LOADING = false;
    EDITING_URL = false;
}

unsafe fn generate_semantic_view() {
    if HTML_LEN == 0 { return; }

    // Extract plain text from elements
    let mut plain = [0u8; 1500];
    let mut plen = 0usize;
    let elems = core::ptr::addr_of!(ELEMENTS) as *const HtmlElement;

    for i in 0..ELEM_COUNT {
        let e = &*elems.add(i);
        if e.text_len == 0 { continue; }
        if e.elem_type == ElemType::Title { continue; }

        let copy = e.text_len.min(1500 - plen - 1);
        if copy == 0 { break; }
        for j in 0..copy { plain[plen + j] = e.text[j]; }
        plen += copy;
        if plen < 1499 { plain[plen] = b' '; plen += 1; }
    }

    // Build AI prompt
    let mut prompt = [0u8; 1800];
    let prompt_len = {
        let mut m = Msg::new(&mut prompt);
        m.s(b"Summarize this web page content in 3-5 bullet points. Be concise. Content:\n");
        m.s(&plain[..plen]);
        m.len()
    };

    let sem_ptr = core::ptr::addr_of_mut!(SEMANTIC_BUF) as *mut u8;
    let resp = folk_slm_generate(
        prompt.as_ptr() as i32, prompt_len as i32,
        sem_ptr as i32, MAX_SEMANTIC as i32);

    SEMANTIC_LEN = if resp > 0 { resp as usize } else { 0 };
}

// ── Input ───────────────────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let e = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(e as i32) == 0 { break; }
        if *e.add(0) != 3 { continue; }
        let key = *e.add(3) as u8;

        match key {
            0x0D => { // Enter
                if EDITING_URL && URL_LEN > 0 {
                    fetch_page();
                }
            }
            0x09 => { // Tab — toggle view mode
                if MODE == ViewMode::Standard {
                    MODE = ViewMode::Semantic;
                    if SEMANTIC_LEN == 0 { generate_semantic_view(); }
                } else {
                    MODE = ViewMode::Standard;
                }
            }
            0x1B => { // Esc — back to URL bar
                EDITING_URL = true;
            }
            0x08 => { // Backspace
                if EDITING_URL && CURSOR_POS > 0 {
                    let u = core::ptr::addr_of_mut!(URL) as *mut u8;
                    let mut i = CURSOR_POS - 1;
                    while i < URL_LEN - 1 { *u.add(i) = *u.add(i+1); i += 1; }
                    URL_LEN -= 1; CURSOR_POS -= 1;
                }
            }
            0x25 => { // Left
                if EDITING_URL && CURSOR_POS > 0 { CURSOR_POS -= 1; }
            }
            0x27 => { // Right
                if EDITING_URL && CURSOR_POS < URL_LEN { CURSOR_POS += 1; }
            }
            0x26 => { // Up — scroll up
                if !EDITING_URL { SCROLL_Y = (SCROLL_Y - 40).max(0); }
            }
            0x28 => { // Down — scroll down
                if !EDITING_URL { SCROLL_Y = (SCROLL_Y + 40).min(CONTENT_HEIGHT); }
            }
            0x21 => { SCROLL_Y = (SCROLL_Y - 200).max(0); } // PgUp
            0x22 => { SCROLL_Y = (SCROLL_Y + 200).min(CONTENT_HEIGHT); } // PgDn
            0x24 => { SCROLL_Y = 0; } // Home
            0x23 => { SCROLL_Y = CONTENT_HEIGHT; } // End
            0x20..=0x7E => {
                if EDITING_URL && URL_LEN < MAX_URL - 1 {
                    let u = core::ptr::addr_of_mut!(URL) as *mut u8;
                    let mut i = URL_LEN;
                    while i > CURSOR_POS { *u.add(i) = *u.add(i-1); i -= 1; }
                    *u.add(CURSOR_POS) = key;
                    URL_LEN += 1; CURSOR_POS += 1;
                }
            }
            _ => {}
        }
    }
}

// ── Rendering ───────────────────────────────────────────────────────────

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();
    let usable_w = sw.min(1024);

    if MODE == ViewMode::Standard && ELEM_COUNT > 0 {
        // Use display list for efficient rendering
        build_display_list(usable_w, sh - URLBAR_H - STATUS_H);

        if DL_LEN > 0 {
            folk_submit_display_list(
                core::ptr::addr_of!(DISPLAY_LIST) as i32,
                DL_LEN as i32);
        }
    } else if MODE == ViewMode::Semantic {
        folk_fill_screen(SEMANTIC_BG);
    } else {
        folk_fill_screen(BG);
    }

    // URL bar (always on top, drawn with individual calls for clarity)
    folk_draw_rect(0, 0, usable_w, URLBAR_H, URLBAR_BG);

    // Mode indicator
    let mode_text = if MODE == ViewMode::Standard { b"STD" as &[u8] } else { b"SEM" };
    let mode_color = if MODE == ViewMode::Standard { 0x3FB950 } else { 0xBC8CFF };
    draw(8, 8, mode_text, mode_color);

    // URL text
    if URL_LEN > 0 {
        let url = core::slice::from_raw_parts(core::ptr::addr_of!(URL) as *const u8, URL_LEN);
        let show = URL_LEN.min(((usable_w - 100) / FONT_W) as usize);
        folk_draw_text(40, 8, url.as_ptr() as i32, show as i32, URLBAR_TEXT);
    } else {
        draw(40, 8, b"Type URL and press Enter...", 0x484F58);
    }

    // Cursor in URL bar
    if EDITING_URL {
        let cx = 40 + (CURSOR_POS as i32) * FONT_W;
        folk_draw_rect(cx, 6, 2, FONT_H, CURSOR_COLOR);
    }

    if LOADING {
        draw(usable_w - 80, 8, b"Loading...", 0xD29922);
    }

    // Semantic view content
    if MODE == ViewMode::Semantic && SEMANTIC_LEN > 0 {
        draw(MARGIN, URLBAR_H + 12, b"AI Summary:", SEMANTIC_TEXT);
        let sem = core::slice::from_raw_parts(
            core::ptr::addr_of!(SEMANTIC_BUF) as *const u8, SEMANTIC_LEN);
        let max_chars = ((usable_w - MARGIN * 2) / FONT_W) as usize;
        let mut line = 0i32;
        let mut col = 0i32;
        for &b in sem {
            if b == b'\n' { line += 1; col = 0; continue; }
            if (col as usize) >= max_chars { col = 0; line += 1; }
            if b >= 0x20 && b < 0x7F {
                folk_draw_text(MARGIN + col * FONT_W, URLBAR_H + 32 + line * LINE_H,
                    &b as *const u8 as i32, 1, SEMANTIC_TEXT);
                col += 1;
            }
        }
    } else if ELEM_COUNT == 0 && !LOADING {
        draw(MARGIN + 20, URLBAR_H + 60, b"Welcome to folk_browser", 0x484F58);
        draw(MARGIN + 20, URLBAR_H + 84, b"Type a URL above and press Enter", 0x484F58);
        draw(MARGIN + 20, URLBAR_H + 108, b"Try: https://httpbin.org/html", 0x58A6FF);
        draw(MARGIN + 20, URLBAR_H + 140, b"[Tab] Toggle Standard/Semantic view", 0x484F58);
        draw(MARGIN + 20, URLBAR_H + 160, b"[Up/Down] Scroll  [Esc] Edit URL", 0x484F58);
    }

    // Status bar
    folk_draw_rect(0, sh - STATUS_H, usable_w, STATUS_H, STATUS_BG);

    let mut sb = [0u8; 80];
    let sl = {
        let mut m = Msg::new(&mut sb);
        m.u32(ELEM_COUNT as u32); m.s(b" elements | ");
        m.u32(LINK_COUNT as u32); m.s(b" links | ");
        if MODE == ViewMode::Standard { m.s(b"Standard"); } else { m.s(b"Semantic"); }
        m.s(b" | Scroll: "); m.u32(SCROLL_Y as u32);
        m.s(b"/"); m.u32(CONTENT_HEIGHT as u32);
        m.len()
    };
    draw(8, sh - STATUS_H + 2, &sb[..sl], STATUS_TEXT);

    // Title in status bar right
    if TITLE_LEN > 0 {
        let title = core::slice::from_raw_parts(
            core::ptr::addr_of!(PAGE_TITLE) as *const u8, TITLE_LEN);
        let show = TITLE_LEN.min(30);
        folk_draw_text(usable_w - (show as i32 + 1) * FONT_W, sh - STATUS_H + 2,
            title.as_ptr() as i32, show as i32, STATUS_TEXT);
    }
}

// ── Main ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            // Default URL — auto-fetch on launch
            let default = b"https://httpbin.org/html";
            let u = core::ptr::addr_of_mut!(URL) as *mut u8;
            for i in 0..default.len() { *u.add(i) = default[i]; }
            URL_LEN = default.len();
            CURSOR_POS = URL_LEN;
            EDITING_URL = false;
            folk_log_telemetry(0, 0, 0);
            fetch_page(); // Auto-navigate on first launch
            INITIALIZED = true;
        }
        handle_input();
        render();
    }
}
