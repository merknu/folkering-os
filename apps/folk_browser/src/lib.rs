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

mod png;
mod jpeg;
mod gif;
mod webp;

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
    fn folk_draw_pixels(x: i32, y: i32, w: i32, h: i32, pixel_ptr: i32, pixel_len: i32) -> i32;
    fn folk_http_get_large(url_ptr: i32, url_len: i32, buf_ptr: i32, max_len: i32) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
    fn folk_semantic_extract(html_ptr: i32, html_len: i32, buf_ptr: i32, max_len: i32) -> i32;
}

// ── Colors ──────────────────────────────────────────────────────────────

const BG: i32 = 0x0D1117;        // dark background (matches OS theme)
const TEXT_COLOR: i32 = 0xC9D1D9; // light grey text
const LINK_COLOR: i32 = 0x58A6FF; // blue links
const H1_COLOR: i32 = 0xFFFFFF;   // white headings
const H2_COLOR: i32 = 0xE6EDF3;   // slightly dimmer headings
const CODE_BG: i32 = 0x161B22;    // dark code block background
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
const MAX_HTML: usize = 8192;
const MAX_ELEMENTS: usize = 256;
const MAX_TEXT_PER_ELEM: usize = 200;
const MAX_DISPLAY_LIST: usize = 16384;
const MAX_SEMANTIC: usize = 2048;
const MAX_LINKS: usize = 48;
const MAX_HREF: usize = 200;
const HISTORY_SIZE: usize = 10;
const MAX_FORMS: usize = 8;
const MAX_INPUTS: usize = 32;
const MAX_FORM_FIELD: usize = 64;
const NO_FORM: u16 = 0xFFFF;

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
    Img = 14,    // image — text field stores src URL
    Unknown = 15,
    Form = 16,   // invisible container; metadata in FORMS table
    Input = 17,  // interactive input field; metadata in INPUTS table
}

#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
enum InputType {
    Text = 0,
    Submit = 1,
    Hidden = 2,
    Password = 3,
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

/// Sparse link table: maps an element index to its href.
/// Populated during HTML parsing for every <a href="..."> encountered.
#[derive(Clone, Copy)]
struct LinkRect {
    elem_idx: u16,
    href_len: u16,
    href: [u8; MAX_HREF],
}

impl LinkRect {
    const fn empty() -> Self {
        Self { elem_idx: 0, href_len: 0, href: [0u8; MAX_HREF] }
    }
}

/// Form metadata captured during HTML parsing.
#[derive(Clone, Copy)]
#[repr(C)]
struct FormInfo {
    action: [u8; MAX_HREF],
    action_len: u16,
    method: u8, // 0 = GET, 1 = POST
}

impl FormInfo {
    const fn empty() -> Self {
        Self { action: [0u8; MAX_HREF], action_len: 0, method: 0 }
    }
}

/// Input field metadata. `value` is the live editable buffer for text
/// inputs and the button label for submit buttons.
#[derive(Clone, Copy)]
#[repr(C)]
struct InputInfo {
    elem_idx: u16,    // index into ELEMENTS, or u16::MAX for hidden inputs
    form_idx: u16,    // index into FORMS, or NO_FORM (0xFFFF) if loose
    input_type: u8,   // InputType discriminant
    name_len: u8,
    value_len: u8,
    _pad: u8,
    name: [u8; MAX_FORM_FIELD],
    value: [u8; MAX_FORM_FIELD],
}

impl InputInfo {
    const fn empty() -> Self {
        Self {
            elem_idx: 0, form_idx: NO_FORM, input_type: 0,
            name_len: 0, value_len: 0, _pad: 0,
            name: [0u8; MAX_FORM_FIELD],
            value: [0u8; MAX_FORM_FIELD],
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

// Image cache — one loaded image at a time
static mut IMG_PIXELS: [u8; 131072] = [0u8; 131072]; // 128KB RGBA (enough for ~180x180)
static mut IMG_RAW: [u8; 65536] = [0u8; 65536];      // 64KB raw image data
static mut IMG_W: u32 = 0;
static mut IMG_H: u32 = 0;
static mut IMG_LOADED: bool = false;
static mut IMG_ELEM_IDX: usize = 0; // which element this image belongs to

static mut SCROLL_Y: i32 = 0;
static mut CONTENT_HEIGHT: i32 = 0;
static mut MODE: ViewMode = ViewMode::Standard;
static mut LOADING: bool = false;
static mut PAGE_TITLE: [u8; 64] = [0u8; 64];
static mut TITLE_LEN: usize = 0;
static mut LINK_COUNT: u16 = 0;
static mut EDITING_URL: bool = true;

// Clickable link rectangles (sparse, indexed by parse order)
static mut LINKS: [LinkRect; MAX_LINKS] = [LinkRect::empty(); MAX_LINKS];
static mut LINK_RECT_COUNT: usize = 0;

// History stack — last N visited URLs (oldest at index 0).
// On Back, we pop the most recent and navigate to it.
static mut HISTORY: [[u8; MAX_URL]; HISTORY_SIZE] = [[0u8; MAX_URL]; HISTORY_SIZE];
static mut HISTORY_LENS: [usize; HISTORY_SIZE] = [0; HISTORY_SIZE];
static mut HISTORY_COUNT: usize = 0;

// Form/input parser state
static mut FORMS: [FormInfo; MAX_FORMS] = [FormInfo::empty(); MAX_FORMS];
static mut FORM_COUNT: usize = 0;
static mut CURRENT_FORM_IDX: u16 = NO_FORM;

static mut INPUTS: [InputInfo; MAX_INPUTS] = [InputInfo::empty(); MAX_INPUTS];
static mut INPUT_COUNT: usize = 0;

/// Index into INPUTS of the currently focused text input, or -1 if none.
static mut FOCUSED_INPUT: i32 = -1;

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
    LINK_RECT_COUNT = 0;
    FORM_COUNT = 0;
    INPUT_COUNT = 0;
    CURRENT_FORM_IDX = NO_FORM;
    FOCUSED_INPUT = -1;
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
            let attrs_start = i;

            // Skip attributes + closing >
            while i < html.len() && html[i] != b'>' { i += 1; }
            let attrs_end = i;
            if i < html.len() { i += 1; }

            // Attribute area (between tag name and closing '>'). Used for
            // extracting href on <a> tags and src on <img> tags.
            let attrs = &html[attrs_start..attrs_end];

            if closing {
                // Detect </form> to clear active form context
                if match_tag(tag_name) == ElemType::Form {
                    CURRENT_FORM_IDX = NO_FORM;
                }
                continue;
            }

            let elem_type = match_tag(tag_name);

            // <form> opens a context — record action/method, no element
            if elem_type == ElemType::Form {
                if FORM_COUNT < MAX_FORMS {
                    let f = &mut *(core::ptr::addr_of_mut!(FORMS) as *mut FormInfo)
                        .add(FORM_COUNT);
                    f.action_len = 0;
                    f.method = 0;
                    if let Some(action) = extract_attr(attrs, b"action") {
                        let alen = action.len().min(MAX_HREF);
                        for j in 0..alen { f.action[j] = action[j]; }
                        f.action_len = alen as u16;
                    }
                    f.method = if let Some(m) = extract_attr(attrs, b"method") {
                        if m.len() == 4
                            && (m[0] == b'P' || m[0] == b'p')
                            && (m[1] == b'O' || m[1] == b'o')
                            && (m[2] == b'S' || m[2] == b's')
                            && (m[3] == b'T' || m[3] == b't')
                        { 1 } else { 0 }
                    } else { 0 };
                    CURRENT_FORM_IDX = FORM_COUNT as u16;
                    FORM_COUNT += 1;
                }
                continue;
            }

            // Skip script/style content entirely (find matching closing tag)
            if elem_type == ElemType::Unknown {
                let lower_tag: [u8; 8] = {
                    let mut l = [0u8; 8];
                    for (j, &b) in tag_name.iter().take(8).enumerate() {
                        l[j] = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
                    }
                    l
                };
                let tlen = tag_name.len().min(8);
                let is_skip = &lower_tag[..tlen] == b"script" || &lower_tag[..tlen] == b"style"
                    || &lower_tag[..tlen] == b"noscript" || &lower_tag[..tlen] == b"svg";
                if is_skip {
                    // Scan forward to </script> or </style> etc.
                    let mut close_tag = [0u8; 12];
                    close_tag[0] = b'<'; close_tag[1] = b'/';
                    let ct_len = 2 + tlen + 1; // "</tag>"
                    for j in 0..tlen { close_tag[2 + j] = lower_tag[j]; }
                    close_tag[2 + tlen] = b'>';

                    while i < html.len() {
                        if html[i] == b'<' && i + ct_len <= html.len() {
                            let mut found = true;
                            for j in 0..ct_len {
                                let h = if html[i+j] >= b'A' && html[i+j] <= b'Z' { html[i+j] + 32 } else { html[i+j] };
                                if h != close_tag[j] { found = false; break; }
                            }
                            if found { i += ct_len; break; }
                        }
                        i += 1;
                    }
                    continue;
                }
            }

            // Self-closing tags (including img and input)
            if elem_type == ElemType::Br
                || elem_type == ElemType::Hr
                || elem_type == ElemType::Img
                || elem_type == ElemType::Input
            {
                if elem_type == ElemType::Img && count < MAX_ELEMENTS {
                    if let Some(src) = extract_attr(attrs, b"src") {
                        let e = &mut *elems.add(count);
                        *e = HtmlElement::empty();
                        e.elem_type = ElemType::Img;
                        let copy = src.len().min(MAX_TEXT_PER_ELEM);
                        for j in 0..copy { e.text[j] = src[j]; }
                        e.text_len = copy;
                        count += 1;
                    }
                } else if elem_type == ElemType::Input {
                    // Determine input type
                    let mut input_type = InputType::Text;
                    if let Some(t) = extract_attr(attrs, b"type") {
                        if attr_eq_ci(t, b"submit") { input_type = InputType::Submit; }
                        else if attr_eq_ci(t, b"hidden") { input_type = InputType::Hidden; }
                        else if attr_eq_ci(t, b"password") { input_type = InputType::Password; }
                        else if attr_eq_ci(t, b"button") { input_type = InputType::Submit; }
                        // text, search, email, url etc. all default to Text
                    }

                    let visible = input_type != InputType::Hidden;

                    if INPUT_COUNT < MAX_INPUTS {
                        let inp = &mut *(core::ptr::addr_of_mut!(INPUTS) as *mut InputInfo)
                            .add(INPUT_COUNT);
                        inp.name_len = 0;
                        inp.value_len = 0;
                        inp.elem_idx = if visible && count < MAX_ELEMENTS {
                            count as u16
                        } else {
                            u16::MAX
                        };
                        inp.form_idx = CURRENT_FORM_IDX;
                        inp.input_type = input_type as u8;

                        if let Some(name) = extract_attr(attrs, b"name") {
                            let nl = name.len().min(MAX_FORM_FIELD);
                            for j in 0..nl { inp.name[j] = name[j]; }
                            inp.name_len = nl as u8;
                        }
                        if let Some(value) = extract_attr(attrs, b"value") {
                            let vl = value.len().min(MAX_FORM_FIELD);
                            for j in 0..vl { inp.value[j] = value[j]; }
                            inp.value_len = vl as u8;
                        }
                        INPUT_COUNT += 1;
                    }

                    if visible && count < MAX_ELEMENTS {
                        let e = &mut *elems.add(count);
                        *e = HtmlElement::empty();
                        e.elem_type = ElemType::Input;
                        count += 1;
                    }
                } else {
                    let e = &mut *elems.add(count);
                    *e = HtmlElement::empty();
                    e.elem_type = elem_type;
                    count += 1;
                }
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

                if elem_type == ElemType::A {
                    LINK_COUNT += 1;
                    // Record clickable link with its href URL
                    if LINK_RECT_COUNT < MAX_LINKS {
                        if let Some(href) = extract_attr(attrs, b"href") {
                            let lr_ptr = core::ptr::addr_of_mut!(LINKS) as *mut LinkRect;
                            let lr = &mut *lr_ptr.add(LINK_RECT_COUNT);
                            lr.elem_idx = count as u16;
                            let copy = href.len().min(MAX_HREF);
                            for j in 0..copy { lr.href[j] = href[j]; }
                            lr.href_len = copy as u16;
                            LINK_RECT_COUNT += 1;
                        }
                    }
                }
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
    else if s == b"img" { ElemType::Img }
    else if s == b"form" { ElemType::Form }
    else if s == b"input" { ElemType::Input }
    else { ElemType::Unknown }
}

/// Case-insensitive ASCII byte-slice equality.
fn attr_eq_ci(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    for i in 0..a.len() {
        let av = if a[i] >= b'A' && a[i] <= b'Z' { a[i] + 32 } else { a[i] };
        let bv = if b[i] >= b'A' && b[i] <= b'Z' { b[i] + 32 } else { b[i] };
        if av != bv { return false; }
    }
    true
}

fn trim_bytes(s: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = s.len();
    while start < end && (s[start] == b' ' || s[start] == b'\n' || s[start] == b'\r' || s[start] == b'\t') { start += 1; }
    while end > start && (s[end-1] == b' ' || s[end-1] == b'\n' || s[end-1] == b'\r' || s[end-1] == b'\t') { end -= 1; }
    &s[start..end]
}

/// Extract an attribute value from a tag string (e.g., src="..." from img tag)
fn extract_attr<'a>(tag: &'a [u8], attr_name: &[u8]) -> Option<&'a [u8]> {
    // Search for attr_name= or attr_name="
    let name_len = attr_name.len();
    for i in 0..tag.len().saturating_sub(name_len + 2) {
        let mut matches = true;
        for j in 0..name_len {
            let a = if tag[i+j] >= b'A' && tag[i+j] <= b'Z' { tag[i+j] + 32 } else { tag[i+j] };
            let b = if attr_name[j] >= b'A' && attr_name[j] <= b'Z' { attr_name[j] + 32 } else { attr_name[j] };
            if a != b { matches = false; break; }
        }
        if matches && i + name_len < tag.len() && tag[i + name_len] == b'=' {
            let start = i + name_len + 1;
            if start >= tag.len() { return None; }
            // Handle quoted and unquoted values
            if tag[start] == b'"' || tag[start] == b'\'' {
                let quote = tag[start];
                let val_start = start + 1;
                let mut val_end = val_start;
                while val_end < tag.len() && tag[val_end] != quote { val_end += 1; }
                return Some(&tag[val_start..val_end]);
            } else {
                let val_start = start;
                let mut val_end = val_start;
                while val_end < tag.len() && tag[val_end] != b' ' && tag[val_end] != b'>' { val_end += 1; }
                return Some(&tag[val_start..val_end]);
            }
        }
    }
    None
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
            ElemType::Img => {
                // Reserve space for image (placeholder 200x150, actual size after load)
                e.x = MARGIN;
                e.y = y;
                e.w = 200; // default placeholder width
                e.h = 150; // default placeholder height
                y += e.h + PARA_GAP;
            }
            ElemType::Title => {
                e.x = 0; e.y = 0; e.w = 0; e.h = 0; // invisible
            }
            ElemType::Form => {
                // Form is a logical container, not laid out
                e.x = 0; e.y = 0; e.w = 0; e.h = 0;
            }
            ElemType::Input => {
                // Look up the input's type to size it appropriately
                let mut input_type = 0u8;
                let inputs_p = core::ptr::addr_of!(INPUTS) as *const InputInfo;
                for j in 0..INPUT_COUNT {
                    let inp = &*inputs_p.add(j);
                    if inp.elem_idx == i as u16 {
                        input_type = inp.input_type;
                        break;
                    }
                }
                e.x = MARGIN;
                e.y = y;
                if input_type == InputType::Submit as u8 {
                    e.w = 120;
                    e.h = 28;
                } else {
                    e.w = 320;
                    e.h = 26;
                }
                y += e.h + 6;
            }
        }
    }
    CONTENT_HEIGHT = y + MARGIN;
}

/// Find the INPUTS index whose `elem_idx` points at the given element index.
/// Returns `usize::MAX` if no match.
unsafe fn input_for_elem(elem_idx: usize) -> usize {
    let inputs_p = core::ptr::addr_of!(INPUTS) as *const InputInfo;
    for j in 0..INPUT_COUNT {
        let inp = &*inputs_p.add(j);
        if inp.elem_idx == elem_idx as u16 { return j; }
    }
    usize::MAX
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
        if e.text_len == 0
            && e.elem_type != ElemType::Hr
            && e.elem_type != ElemType::Br
            && e.elem_type != ElemType::Img
            && e.elem_type != ElemType::Input
        { continue; }

        match e.elem_type {
            ElemType::Hr => {
                // Line command
                if pos + 13 <= MAX_DISPLAY_LIST {
                    *dl.add(pos) = 0x03; pos += 1;
                    let vals: [u16; 4] = [e.x as u16, ey as u16, (e.x + e.w) as u16, ey as u16];
                    for v in &vals { let b = v.to_le_bytes(); *dl.add(pos) = b[0]; *dl.add(pos+1) = b[1]; pos += 2; }
                    let cb = (0x30363Du32).to_le_bytes();
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
            ElemType::Img => {
                if IMG_LOADED && i == IMG_ELEM_IDX && ey >= -600 && ey < viewport_h {
                    // Render actual image via folk_draw_pixels
                    folk_draw_pixels(
                        e.x, ey, IMG_W as i32, IMG_H as i32,
                        core::ptr::addr_of!(IMG_PIXELS) as i32,
                        (IMG_W * IMG_H * 4) as i32,
                    );
                } else {
                    // Placeholder rect
                    if pos + 13 <= MAX_DISPLAY_LIST {
                        *dl.add(pos) = 0x01; pos += 1;
                        let vals: [u16; 4] = [e.x as u16, ey.max(0) as u16, e.w as u16, e.h as u16];
                        for v in &vals { let b = v.to_le_bytes(); *dl.add(pos) = b[0]; *dl.add(pos+1) = b[1]; pos += 2; }
                        let cb = (0x1A2332u32).to_le_bytes();
                        for j in 0..4 { *dl.add(pos+j) = cb[j]; } pos += 4;
                    }
                    if e.text_len > 0 {
                        pos = emit_text(dl, pos, e, ey + 4, 0x484F58);
                    }
                }
            }
            ElemType::A => {
                pos = emit_text(dl, pos, e, ey, LINK_COLOR);
            }
            ElemType::Input => {
                let inp_idx = input_for_elem(i);
                if inp_idx == usize::MAX { continue; }
                let inputs_p = core::ptr::addr_of!(INPUTS) as *const InputInfo;
                let inp = &*inputs_p.add(inp_idx);
                let is_submit = inp.input_type == InputType::Submit as u8;
                let is_focused = FOCUSED_INPUT == inp_idx as i32;

                if is_submit {
                    // Green submit button background
                    pos = emit_rect(dl, pos, e.x, ey, e.w, e.h, 0x238636);
                    // Label = inp.value (or "Submit" fallback)
                    if inp.value_len > 0 {
                        pos = emit_text_raw(
                            dl, pos, e.x + 12, ey + 6,
                            inp.value.as_ptr() as u32, inp.value_len as u16, 0xFFFFFF,
                        );
                    }
                } else {
                    // White text input background
                    pos = emit_rect(dl, pos, e.x, ey, e.w, e.h, 0xF5F5F5);
                    // Border (focused = blue, unfocused = light gray)
                    let border_color = if is_focused { 0x58A6FF } else { 0xCCCCCC };
                    pos = emit_rect(dl, pos, e.x, ey, e.w, 1, border_color);
                    pos = emit_rect(dl, pos, e.x, ey + e.h - 1, e.w, 1, border_color);
                    pos = emit_rect(dl, pos, e.x, ey, 1, e.h, border_color);
                    pos = emit_rect(dl, pos, e.x + e.w - 1, ey, 1, e.h, border_color);
                    // Current value (rendered in dark text on the white box)
                    if inp.value_len > 0 {
                        pos = emit_text_raw(
                            dl, pos, e.x + 6, ey + 5,
                            inp.value.as_ptr() as u32, inp.value_len as u16, 0x0D1117,
                        );
                    }
                    // Cursor (only when focused)
                    if is_focused {
                        let cursor_x = e.x + 6 + (inp.value_len as i32) * FONT_W;
                        pos = emit_rect(dl, pos, cursor_x, ey + 5, 2, FONT_H, 0x0D1117);
                    }
                }
            }
            _ => {
                pos = emit_text(dl, pos, e, ey, TEXT_COLOR);
            }
        }
    }

    DL_LEN = pos;
}

/// Emit a filled rectangle (display list opcode 0x01).
unsafe fn emit_rect(dl: *mut u8, mut pos: usize, x: i32, y: i32, w: i32, h: i32, color: i32) -> usize {
    if pos + 13 > MAX_DISPLAY_LIST { return pos; }
    *dl.add(pos) = 0x01; pos += 1;
    let vals: [u16; 4] = [x as u16, y as u16, w as u16, h as u16];
    for v in &vals {
        let b = v.to_le_bytes();
        *dl.add(pos) = b[0]; *dl.add(pos + 1) = b[1]; pos += 2;
    }
    let cb = (color as u32).to_le_bytes();
    for j in 0..4 { *dl.add(pos + j) = cb[j]; }
    pos += 4;
    pos
}

/// Emit a text run from a raw pointer + length (display list opcode 0x02).
/// Used for input fields whose text lives outside the ELEMENTS array.
unsafe fn emit_text_raw(
    dl: *mut u8,
    mut pos: usize,
    x: i32,
    y: i32,
    text_ptr: u32,
    text_len: u16,
    color: i32,
) -> usize {
    if text_len == 0 || pos + 15 > MAX_DISPLAY_LIST { return pos; }
    if y < -100 || y > 800 { return pos; }

    *dl.add(pos) = 0x02; pos += 1;
    let xb = (x as u16).to_le_bytes();
    *dl.add(pos) = xb[0]; *dl.add(pos + 1) = xb[1]; pos += 2;
    let yb = (y as u16).to_le_bytes();
    *dl.add(pos) = yb[0]; *dl.add(pos + 1) = yb[1]; pos += 2;
    let pb = text_ptr.to_le_bytes();
    for j in 0..4 { *dl.add(pos + j) = pb[j]; }
    pos += 4;
    let lb = text_len.to_le_bytes();
    *dl.add(pos) = lb[0]; *dl.add(pos + 1) = lb[1]; pos += 2;
    let cb = (color as u32).to_le_bytes();
    for j in 0..4 { *dl.add(pos + j) = cb[j]; }
    pos += 4;
    pos
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

// ── Image Loading ───────────────────────────────────────────────────────

unsafe fn try_load_first_image() {
    if IMG_LOADED { return; }

    let elems = core::ptr::addr_of!(ELEMENTS) as *const HtmlElement;
    for i in 0..ELEM_COUNT {
        let e = &*elems.add(i);
        if e.elem_type != ElemType::Img || e.text_len == 0 { continue; }

        let url = &e.text[..e.text_len];

        // Skip data URIs and relative paths for now (need base URL resolution)
        if url.len() < 8 { continue; }
        if url[0] != b'h' && url[0] != b'H' { continue; } // must start with http

        // Fetch image via folk_http_get_large
        let raw = core::ptr::addr_of_mut!(IMG_RAW) as *mut u8;
        let bytes = folk_http_get_large(
            url.as_ptr() as i32, url.len() as i32,
            raw as i32, 65536,
        );
        if bytes <= 0 { continue; }

        let raw_data = core::slice::from_raw_parts(raw, bytes as usize);
        let pixels = core::ptr::addr_of_mut!(IMG_PIXELS) as *mut u8;
        let pixel_buf = core::slice::from_raw_parts_mut(pixels, 131072);

        // Detect format and decode
        let (w, h) = if raw_data.len() > 8 && &raw_data[0..8] == b"\x89PNG\r\n\x1A\n" {
            png::decode_png(raw_data, pixel_buf)
        } else if raw_data.len() > 2 && raw_data[0] == 0xFF && raw_data[1] == 0xD8 {
            jpeg::decode_jpeg(raw_data, pixel_buf)
        } else if raw_data.len() > 4 && &raw_data[0..4] == b"GIF8" {
            gif::decode_gif(raw_data, pixel_buf)
        } else if raw_data.len() > 12 && &raw_data[0..4] == b"RIFF" && &raw_data[8..12] == b"WEBP" {
            webp::decode_webp(raw_data, pixel_buf)
        } else {
            (0, 0)
        };

        if w > 0 && h > 0 {
            IMG_W = w;
            IMG_H = h;
            IMG_LOADED = true;
            IMG_ELEM_IDX = i;

            // Update layout element size to match actual image
            let elems_mut = core::ptr::addr_of_mut!(ELEMENTS) as *mut HtmlElement;
            let em = &mut *elems_mut.add(i);
            em.w = w.min(800) as i32;
            em.h = h.min(600) as i32;

            break; // Only load first image for now
        }
    }
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
        // Try to load the first image found on the page
        IMG_LOADED = false;
        try_load_first_image();
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

    // Send raw HTML directly to folk_semantic_extract — it handles
    // stripping <script>/<style>/<nav> and LLM semantic extraction.
    let html_ptr = core::ptr::addr_of!(HTML_BUF) as *const u8;
    let sem_ptr = core::ptr::addr_of_mut!(SEMANTIC_BUF) as *mut u8;
    let resp = folk_semantic_extract(
        html_ptr as i32, HTML_LEN as i32,
        sem_ptr as i32, MAX_SEMANTIC as i32);

    SEMANTIC_LEN = if resp > 0 { resp as usize } else { 0 };

    // If semantic extract failed, show a fallback message
    if SEMANTIC_LEN == 0 {
        let fallback = b"[Semantic extraction unavailable - connect to proxy]";
        let copy = fallback.len().min(MAX_SEMANTIC);
        for i in 0..copy { *sem_ptr.add(i) = fallback[i]; }
        SEMANTIC_LEN = copy;
    }
}

// ── History & URL resolution ────────────────────────────────────────────

/// Push the current URL onto the back stack. If the stack is full, drop the
/// oldest entry to make room (FIFO eviction).
unsafe fn history_push(url: &[u8]) {
    if HISTORY_COUNT == HISTORY_SIZE {
        // Shift everything down by one — drop oldest
        for i in 0..HISTORY_SIZE - 1 {
            let h = core::ptr::addr_of_mut!(HISTORY) as *mut [u8; MAX_URL];
            let src = &*h.add(i + 1);
            let dst = &mut *h.add(i);
            *dst = *src;
            HISTORY_LENS[i] = HISTORY_LENS[i + 1];
        }
        HISTORY_COUNT = HISTORY_SIZE - 1;
    }
    let len = url.len().min(MAX_URL);
    let h = core::ptr::addr_of_mut!(HISTORY) as *mut [u8; MAX_URL];
    let entry = &mut *h.add(HISTORY_COUNT);
    for i in 0..len { entry[i] = url[i]; }
    HISTORY_LENS[HISTORY_COUNT] = len;
    HISTORY_COUNT += 1;
}

/// Pop the most recent entry from the back stack into URL. Returns true on
/// success, false if the stack was empty.
unsafe fn history_back() -> bool {
    if HISTORY_COUNT == 0 { return false; }
    HISTORY_COUNT -= 1;
    let h = core::ptr::addr_of!(HISTORY) as *const [u8; MAX_URL];
    let entry = &*h.add(HISTORY_COUNT);
    let len = HISTORY_LENS[HISTORY_COUNT];
    let u = core::ptr::addr_of_mut!(URL) as *mut u8;
    for i in 0..len { *u.add(i) = entry[i]; }
    URL_LEN = len;
    CURSOR_POS = len;
    true
}

/// Resolve a possibly-relative href against the current URL and write the
/// result into the URL buffer. Handles four common cases:
///   - "https://...", "http://..."  → use as-is
///   - "//host/..."                  → prepend "https:"
///   - "/path"                       → prepend scheme://host of current URL
///   - "relative"                    → take current URL up to last '/' and append
unsafe fn resolve_url(href: &[u8]) {
    let u = core::ptr::addr_of_mut!(URL) as *mut u8;

    // Already absolute?
    if href.len() >= 7 && &href[..7] == b"http://" {
        let len = href.len().min(MAX_URL);
        for i in 0..len { *u.add(i) = href[i]; }
        URL_LEN = len; CURSOR_POS = len;
        return;
    }
    if href.len() >= 8 && &href[..8] == b"https://" {
        let len = href.len().min(MAX_URL);
        for i in 0..len { *u.add(i) = href[i]; }
        URL_LEN = len; CURSOR_POS = len;
        return;
    }

    // Protocol-relative ("//host/...") — assume https
    if href.len() >= 2 && href[0] == b'/' && href[1] == b'/' {
        let prefix = b"https:";
        let mut pos = 0;
        for &b in prefix { if pos < MAX_URL { *u.add(pos) = b; pos += 1; } }
        for &b in href { if pos < MAX_URL { *u.add(pos) = b; pos += 1; } }
        URL_LEN = pos; CURSOR_POS = pos;
        return;
    }

    // Snapshot the current URL — we mutate URL in-place below
    let mut cur = [0u8; MAX_URL];
    let cur_len = URL_LEN;
    for i in 0..cur_len { cur[i] = *u.add(i); }

    // Find end of "scheme://"
    let mut scheme_end = 0usize;
    let mut k = 0usize;
    while k + 2 < cur_len {
        if cur[k] == b':' && cur[k+1] == b'/' && cur[k+2] == b'/' {
            scheme_end = k + 3;
            break;
        }
        k += 1;
    }

    // Find end of host (first '/' or '?' or '#' after scheme)
    let mut host_end = scheme_end;
    while host_end < cur_len {
        let c = cur[host_end];
        if c == b'/' || c == b'?' || c == b'#' { break; }
        host_end += 1;
    }

    // Absolute path: "/foo" → scheme://host + href
    if !href.is_empty() && href[0] == b'/' {
        let mut pos = 0;
        for i in 0..host_end {
            if pos < MAX_URL { *u.add(pos) = cur[i]; pos += 1; }
        }
        for &b in href {
            if pos < MAX_URL { *u.add(pos) = b; pos += 1; }
        }
        URL_LEN = pos; CURSOR_POS = pos;
        return;
    }

    // Pure relative: take everything up to (and including) the last '/'
    // in the path portion of the current URL.
    let mut last_slash = host_end;
    let mut k2 = host_end;
    while k2 < cur_len {
        let c = cur[k2];
        if c == b'?' || c == b'#' { break; }
        if c == b'/' { last_slash = k2; }
        k2 += 1;
    }
    let base_end = if last_slash > host_end { last_slash + 1 } else { host_end };

    let mut pos = 0;
    for i in 0..base_end {
        if pos < MAX_URL { *u.add(pos) = cur[i]; pos += 1; }
    }
    // If the current URL had no slash after the host (e.g. https://example.com),
    // synthesize one before appending the relative href.
    if base_end == host_end && pos < MAX_URL {
        *u.add(pos) = b'/'; pos += 1;
    }
    for &b in href {
        if pos < MAX_URL { *u.add(pos) = b; pos += 1; }
    }
    URL_LEN = pos; CURSOR_POS = pos;
}

// ── Form submission ─────────────────────────────────────────────────────

#[inline]
fn hex_nibble(n: u8) -> u8 {
    if n < 10 { b'0' + n } else { b'A' + n - 10 }
}

/// Build a URL-encoded query string from all inputs that belong to the
/// given form. Returns the number of bytes written into `out`.
unsafe fn build_query_string(form_idx: u16, out: &mut [u8]) -> usize {
    let mut qpos = 0usize;
    let mut first = true;
    let inputs_p = core::ptr::addr_of!(INPUTS) as *const InputInfo;

    for j in 0..INPUT_COUNT {
        let inp = &*inputs_p.add(j);
        if inp.form_idx != form_idx { continue; }
        if inp.input_type == InputType::Submit as u8 { continue; }
        if inp.name_len == 0 { continue; }

        if !first {
            if qpos < out.len() { out[qpos] = b'&'; qpos += 1; } else { break; }
        }
        first = false;

        // Encode name
        for k in 0..(inp.name_len as usize) {
            if qpos >= out.len() { return qpos; }
            out[qpos] = inp.name[k];
            qpos += 1;
        }
        if qpos < out.len() { out[qpos] = b'='; qpos += 1; } else { return qpos; }

        // Encode value (RFC 3986 unreserved + space → '+')
        for k in 0..(inp.value_len as usize) {
            let b = inp.value[k];
            let unreserved = (b >= b'A' && b <= b'Z')
                || (b >= b'a' && b <= b'z')
                || (b >= b'0' && b <= b'9')
                || b == b'-' || b == b'_' || b == b'.' || b == b'~';
            if unreserved {
                if qpos >= out.len() { return qpos; }
                out[qpos] = b;
                qpos += 1;
            } else if b == b' ' {
                if qpos >= out.len() { return qpos; }
                out[qpos] = b'+';
                qpos += 1;
            } else {
                if qpos + 3 > out.len() { return qpos; }
                out[qpos] = b'%';
                out[qpos + 1] = hex_nibble(b >> 4);
                out[qpos + 2] = hex_nibble(b & 0xF);
                qpos += 3;
            }
        }
    }
    qpos
}

/// Submit a form by index. Currently handles GET only — POST is a no-op
/// stub until `folk_http_post` is wired up.
unsafe fn submit_form(form_idx: u16) {
    if form_idx == NO_FORM || (form_idx as usize) >= FORM_COUNT { return; }
    let form_p = core::ptr::addr_of!(FORMS) as *const FormInfo;
    let form = &*form_p.add(form_idx as usize);

    if form.method == 1 {
        // POST not yet implemented
        return;
    }

    // Snapshot current URL for history
    let url_p = core::ptr::addr_of!(URL) as *const u8;
    let mut saved_url = [0u8; MAX_URL];
    let saved_len = URL_LEN;
    for i in 0..saved_len { saved_url[i] = *url_p.add(i); }

    // Resolve action against current URL (empty action → keep current URL)
    if form.action_len > 0 {
        let action_slice = &form.action[..form.action_len as usize];
        resolve_url(action_slice);
    }

    // Build query string
    let mut query = [0u8; 1024];
    let qlen = build_query_string(form_idx, &mut query);

    // Append "?query" or "&query" depending on whether the URL already
    // contains a query separator.
    if qlen > 0 {
        let u = core::ptr::addr_of_mut!(URL) as *mut u8;
        let mut has_query = false;
        for i in 0..URL_LEN {
            if *u.add(i) == b'?' { has_query = true; break; }
        }
        let sep = if has_query { b'&' } else { b'?' };
        if URL_LEN < MAX_URL {
            *u.add(URL_LEN) = sep;
            URL_LEN += 1;
        }
        for k in 0..qlen {
            if URL_LEN >= MAX_URL { break; }
            *u.add(URL_LEN) = query[k];
            URL_LEN += 1;
        }
        CURSOR_POS = URL_LEN;
    }

    // Push old URL onto history and navigate
    history_push(&saved_url[..saved_len]);
    fetch_page();
}

// ── Input ───────────────────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let e = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(e as i32) == 0 { break; }
        let event_type = *e.add(0);

        // Mouse click handling
        if event_type == 2 {
            let mx = *e.add(1);
            let my = *e.add(2);

            // STD/SEM mode toggle button
            if mx >= 4 && mx < 36 && my >= 4 && my < 20 {
                if MODE == ViewMode::Standard {
                    MODE = ViewMode::Semantic;
                    if SEMANTIC_LEN == 0 { generate_semantic_view(); }
                } else {
                    MODE = ViewMode::Standard;
                }
                continue;
            }

            // BACK button (24×16 to the right of STD/SEM)
            if mx >= 40 && mx < 64 && my >= 4 && my < 20 {
                if history_back() {
                    fetch_page();
                    return;
                }
                continue;
            }

            // Link / input hit detection — only in standard mode + content area
            let sh = folk_screen_height();
            if MODE == ViewMode::Standard
                && my >= URLBAR_H
                && my < sh - STATUS_H
            {
                let elems_p = core::ptr::addr_of!(ELEMENTS) as *const HtmlElement;

                // 1) Inputs (text fields & submit buttons) — checked first
                //    so they win over any overlapping link rect.
                let inputs_p = core::ptr::addr_of!(INPUTS) as *const InputInfo;
                let mut input_hit = false;
                for j in 0..INPUT_COUNT {
                    let inp = &*inputs_p.add(j);
                    if inp.elem_idx == u16::MAX { continue; } // hidden
                    let idx = inp.elem_idx as usize;
                    if idx >= ELEM_COUNT { continue; }
                    let el = &*elems_p.add(idx);
                    let sx = el.x;
                    let sy = el.y - SCROLL_Y;
                    if mx >= sx && mx < sx + el.w && my >= sy && my < sy + el.h {
                        if inp.input_type == InputType::Submit as u8 {
                            FOCUSED_INPUT = -1;
                            submit_form(inp.form_idx);
                            return;
                        } else {
                            FOCUSED_INPUT = j as i32;
                            input_hit = true;
                            break;
                        }
                    }
                }
                if input_hit { continue; }

                // 2) Links
                let lr_ptr = core::ptr::addr_of!(LINKS) as *const LinkRect;
                for li in 0..LINK_RECT_COUNT {
                    let lr = &*lr_ptr.add(li);
                    let idx = lr.elem_idx as usize;
                    if idx >= ELEM_COUNT { continue; }
                    let el = &*elems_p.add(idx);
                    let sx = el.x;
                    let sy = el.y - SCROLL_Y;
                    if mx >= sx && mx < sx + el.w && my >= sy && my < sy + el.h {
                        // Hit! Push current URL onto history, navigate
                        let cur_url = core::slice::from_raw_parts(
                            core::ptr::addr_of!(URL) as *const u8, URL_LEN);
                        history_push(cur_url);

                        let href_len = lr.href_len as usize;
                        let mut href_local = [0u8; MAX_HREF];
                        for j in 0..href_len { href_local[j] = lr.href[j]; }
                        resolve_url(&href_local[..href_len]);

                        fetch_page();
                        return;
                    }
                }

                // 3) Click on empty area — unfocus any text input
                FOCUSED_INPUT = -1;
            }
            continue;
        }

        if event_type != 3 { continue; }
        let key = *e.add(3) as u8;

        // If a text input is focused, route keys there first
        if FOCUSED_INPUT >= 0 && (FOCUSED_INPUT as usize) < INPUT_COUNT && !EDITING_URL {
            let inp_p = core::ptr::addr_of_mut!(INPUTS) as *mut InputInfo;
            let inp = &mut *inp_p.add(FOCUSED_INPUT as usize);
            match key {
                0x0D | 0x0A => {
                    // Enter — submit the parent form (if any)
                    let fidx = inp.form_idx;
                    FOCUSED_INPUT = -1;
                    if fidx != NO_FORM {
                        submit_form(fidx);
                        return;
                    }
                    continue;
                }
                0x08 => {
                    // Backspace
                    if inp.value_len > 0 { inp.value_len -= 1; }
                    continue;
                }
                0x1B => {
                    // Esc — unfocus
                    FOCUSED_INPUT = -1;
                    continue;
                }
                0x20..=0x7E => {
                    if (inp.value_len as usize) < MAX_FORM_FIELD - 1 {
                        inp.value[inp.value_len as usize] = key;
                        inp.value_len += 1;
                    }
                    continue;
                }
                _ => { continue; }
            }
        }

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
                } else if !EDITING_URL && history_back() {
                    fetch_page();
                    return;
                }
            }
            0x82 => { // Left
                if EDITING_URL && CURSOR_POS > 0 { CURSOR_POS -= 1; }
            }
            0x83 => { // Right
                if EDITING_URL && CURSOR_POS < URL_LEN { CURSOR_POS += 1; }
            }
            0x80 => { // Up — scroll up
                if !EDITING_URL { SCROLL_Y = (SCROLL_Y - 40).max(0); }
            }
            0x81 => { // Down — scroll down
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

    // Mode toggle button (clickable)
    let (mode_text, mode_bg, mode_fg) = if MODE == ViewMode::Standard {
        (b"STD" as &[u8], 0x1B2838_i32, 0x3FB950_i32)
    } else {
        (b"SEM" as &[u8], 0x2D1B4E_i32, 0xBC8CFF_i32)
    };
    folk_draw_rect(4, 4, 32, 16, mode_bg);
    draw(8, 8, mode_text, mode_fg);

    // BACK button — dimmed when history is empty
    let back_active = HISTORY_COUNT > 0;
    let (back_bg, back_fg) = if back_active {
        (0x1B2838_i32, 0xC9D1D9_i32)
    } else {
        (0x161B22_i32, 0x484F58_i32)
    };
    folk_draw_rect(40, 4, 24, 16, back_bg);
    draw(48, 8, b"<", back_fg);

    // URL text (shifted right to make room for BACK button)
    if URL_LEN > 0 {
        let url = core::slice::from_raw_parts(core::ptr::addr_of!(URL) as *const u8, URL_LEN);
        let show = URL_LEN.min(((usable_w - 132) / FONT_W) as usize);
        folk_draw_text(72, 8, url.as_ptr() as i32, show as i32, URLBAR_TEXT);
    } else {
        draw(72, 8, b"Type URL and press Enter...", 0x484F58);
    }

    // Cursor in URL bar
    if EDITING_URL {
        let cx = 72 + (CURSOR_POS as i32) * FONT_W;
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
        m.u32(ELEM_COUNT as u32); m.s(b" el | ");
        m.u32(LINK_COUNT as u32); m.s(b" lnk | ");
        m.u32(FORM_COUNT as u32); m.s(b" frm | ");
        m.u32(INPUT_COUNT as u32); m.s(b" inp | ");
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
            let default = b"https://news.ycombinator.com";
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
