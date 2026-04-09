//! SemanticMail — Intent-Sorted Email for Folkering OS
//!
//! No inbox. No dates. Just three Kanban columns:
//!   ACTION — Emails requiring your action
//!   QUESTION — Questions to answer
//!   FYI — Read-only notifications
//!
//! AI categorizes each email via folk_slm_generate().
//! Emails stored in Synapse VFS with category tags.

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
    fn folk_http_get(url_ptr: i32, url_len: i32, buf_ptr: i32, max_len: i32) -> i32;
    fn folk_slm_generate(prompt_ptr: i32, prompt_len: i32, buf_ptr: i32, max_len: i32) -> i32;
    fn folk_write_file(path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
}

// ── Colors ──────────────────────────────────────────────────────────────

const BG: i32 = 0x0D1117;
const PANEL_BG: i32 = 0x161B22;
const BORDER: i32 = 0x30363D;
const TEXT: i32 = 0xC9D1D9;
const TEXT_DIM: i32 = 0x484F58;
const ACCENT: i32 = 0x58A6FF;

// Column colors
const ACTION_COLOR: i32 = 0xF85149;   // Red — urgent
const ACTION_BG: i32 = 0x2D1117;
const QUESTION_COLOR: i32 = 0xD29922; // Yellow — questions
const QUESTION_BG: i32 = 0x2D2617;
const FYI_COLOR: i32 = 0x3FB950;      // Green — informational
const FYI_BG: i32 = 0x112D17;
const CARD_BG: i32 = 0x161B22;
const SELECTED_BORDER: i32 = 0x58A6FF;

// Layout
const HEADER_H: i32 = 32;
const HELP_H: i32 = 18;
const COL_PAD: i32 = 8;
const CARD_H: i32 = 60;
const CARD_PAD: i32 = 6;
const MARGIN: i32 = 8;
const FONT_H: i32 = 16;
const FONT_W: i32 = 8;

// Limits
const MAX_EMAILS: usize = 15;
const MAX_SUBJECT: usize = 48;
const MAX_SNIPPET: usize = 80;

// ── Email types ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
enum Category {
    Action = 0,
    Question = 1,
    Fyi = 2,
    Uncategorized = 3,
}

#[derive(Clone, Copy)]
struct Email {
    subject: [u8; MAX_SUBJECT],
    subject_len: u8,
    snippet: [u8; MAX_SNIPPET],
    snippet_len: u8,
    from: [u8; 24],
    from_len: u8,
    category: Category,
}

impl Email {
    const fn empty() -> Self {
        Self {
            subject: [0u8; MAX_SUBJECT], subject_len: 0,
            snippet: [0u8; MAX_SNIPPET], snippet_len: 0,
            from: [0u8; 24], from_len: 0,
            category: Category::Uncategorized,
        }
    }
}

// ── State ───────────────────────────────────────────────────────────────

static mut EMAILS: [Email; MAX_EMAILS] = [Email::empty(); MAX_EMAILS];
static mut EMAIL_COUNT: usize = 0;
static mut SELECTED_COL: usize = 0; // 0=Action, 1=Question, 2=Fyi
static mut SELECTED_ROW: usize = 0;
static mut DETAIL_VIEW: bool = false;

static mut HTTP_BUF: [u8; 2048] = [0u8; 2048];
static mut AI_PROMPT: [u8; 512] = [0u8; 512];
static mut AI_RESP: [u8; 64] = [0u8; 64];
static mut EVT: [i32; 4] = [0i32; 4];

static mut INITIALIZED: bool = false;
static mut FETCHING: bool = false;
static mut LAST_FETCH_MS: i32 = 0;

// ── Helpers ─────────────────────────────────────────────────────────────

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

unsafe fn draw(x: i32, y: i32, text: &[u8], color: i32) {
    folk_draw_text(x, y, text.as_ptr() as i32, text.len() as i32, color);
}

fn copy_to(dst: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(dst.len());
    for i in 0..n { dst[i] = src[i]; }
    n
}

// ── Load demo emails (simulated inbox) ──────────────────────────────────

unsafe fn load_demo_emails() {
    // In production: folk_http_get to an IMAP proxy. For now: built-in demo.
    let demos: [(&[u8], &[u8], &[u8]); 8] = [
        (b"Build fails on Proxmox", b"knut@folkering.dev",
         b"CI pipeline broke after the virtio-gpu refactor. Need fix before Friday deploy."),
        (b"Meeting notes Q2 planning", b"team@folkering.dev",
         b"Attached are the notes from yesterday's planning session. Key decisions inside."),
        (b"Question: WASM fuel limits", b"dev@folkering.dev",
         b"What should the default fuel budget be for background WASM apps? Current 1M seems low."),
        (b"Invoice #2847 overdue", b"billing@vendor.no",
         b"Your invoice is 15 days overdue. Please process payment ASAP to avoid service disruption."),
        (b"PR Review: WebSocket impl", b"claude@folkering.dev",
         b"Can you review the WebSocket kernel module? Specifically the frame masking logic."),
        (b"Weekly system report", b"draug@folkering.os",
         b"AutoDream ran 3 cycles. Pattern-Mining found 2 insights. 0 crashes detected."),
        (b"Bug: Serial proxy crash", b"knut@folkering.dev",
         b"The serial-gemini proxy segfaults when receiving >8KB responses. Stack trace attached."),
        (b"Newsletter: Rust 2026", b"news@rust-lang.org",
         b"This month: async generators stabilized, new borrow checker improvements, WASM GC."),
    ];

    let emails = core::ptr::addr_of_mut!(EMAILS) as *mut Email;
    for (i, (subj, from, body)) in demos.iter().enumerate() {
        if i >= MAX_EMAILS { break; }
        let e = &mut *emails.add(i);
        *e = Email::empty();
        e.subject_len = copy_to(&mut e.subject, subj) as u8;
        e.from_len = copy_to(&mut e.from, from) as u8;
        e.snippet_len = copy_to(&mut e.snippet, body) as u8;
        e.category = Category::Uncategorized;
    }
    EMAIL_COUNT = demos.len().min(MAX_EMAILS);
}

/// Categorize an email using AI
unsafe fn categorize_email(idx: usize) {
    if idx >= EMAIL_COUNT { return; }
    let emails = core::ptr::addr_of_mut!(EMAILS) as *mut Email;
    let e = &mut *emails.add(idx);
    if e.category != Category::Uncategorized { return; }

    let prompt_ptr = core::ptr::addr_of_mut!(AI_PROMPT) as *mut u8;
    let mut m = Msg::new(core::slice::from_raw_parts_mut(prompt_ptr, 512));
    m.s(b"Categorize this email as ONE of: URGENT_ACTION, QUESTION, FYI, SPAM. ");
    m.s(b"Reply with ONLY the category name. Subject: ");
    m.s(&e.subject[..e.subject_len as usize]);
    m.s(b" Body: ");
    m.s(&e.snippet[..e.snippet_len as usize]);
    let prompt_len = m.len();

    let resp_ptr = core::ptr::addr_of_mut!(AI_RESP) as *mut u8;
    let resp = folk_slm_generate(
        core::ptr::addr_of!(AI_PROMPT) as i32, prompt_len as i32,
        resp_ptr as i32, 64);

    if resp > 0 {
        let response = core::slice::from_raw_parts(resp_ptr, resp as usize);
        // Parse category from response
        e.category = if contains(response, b"URGENT") || contains(response, b"ACTION") {
            Category::Action
        } else if contains(response, b"QUESTION") {
            Category::Question
        } else if contains(response, b"FYI") || contains(response, b"INFO") {
            Category::Fyi
        } else {
            // Default based on content heuristics
            if contains(&e.snippet[..e.snippet_len as usize], b"?") {
                Category::Question
            } else if contains(&e.subject[..e.subject_len as usize], b"Bug")
                || contains(&e.subject[..e.subject_len as usize], b"fail")
                || contains(&e.subject[..e.subject_len as usize], b"overdue")
            {
                Category::Action
            } else {
                Category::Fyi
            }
        };
    } else {
        // Heuristic fallback if AI unavailable
        let subj = &e.subject[..e.subject_len as usize];
        let snip = &e.snippet[..e.snippet_len as usize];
        e.category = if contains(subj, b"Bug") || contains(subj, b"fail") || contains(subj, b"overdue") {
            Category::Action
        } else if contains(snip, b"?") || contains(subj, b"Question") {
            Category::Question
        } else {
            Category::Fyi
        };
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() { return false; }
    for i in 0..=haystack.len() - needle.len() {
        let mut ok = true;
        for j in 0..needle.len() {
            let a = if haystack[i+j] >= b'A' && haystack[i+j] <= b'Z' { haystack[i+j]+32 } else { haystack[i+j] };
            let b = if needle[j] >= b'A' && needle[j] <= b'Z' { needle[j]+32 } else { needle[j] };
            if a != b { ok = false; break; }
        }
        if ok { return true; }
    }
    false
}

/// Count emails in a category
unsafe fn count_in_category(cat: Category) -> usize {
    let emails = core::ptr::addr_of!(EMAILS) as *const Email;
    let mut c = 0;
    for i in 0..EMAIL_COUNT {
        if (*emails.add(i)).category == cat { c += 1; }
    }
    c
}

/// Get nth email in a category
unsafe fn nth_in_category(cat: Category, n: usize) -> Option<usize> {
    let emails = core::ptr::addr_of!(EMAILS) as *const Email;
    let mut c = 0;
    for i in 0..EMAIL_COUNT {
        if (*emails.add(i)).category == cat {
            if c == n { return Some(i); }
            c += 1;
        }
    }
    None
}

// ── Input ───────────────────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let evt_ptr = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(evt_ptr as i32) == 0 { break; }
        if *evt_ptr.add(0) != 3 { continue; }
        let key = *evt_ptr.add(3) as u8;

        match key {
            0x1B => { DETAIL_VIEW = false; } // Esc
            0x82 => { if SELECTED_COL > 0 { SELECTED_COL -= 1; SELECTED_ROW = 0; } } // Left
            0x83 => { if SELECTED_COL < 2 { SELECTED_COL += 1; SELECTED_ROW = 0; } } // Right
            0x80 => { if SELECTED_ROW > 0 { SELECTED_ROW -= 1; } } // Up
            0x81 => { SELECTED_ROW += 1; } // Down
            0x0D => { // Enter — toggle detail
                DETAIL_VIEW = !DETAIL_VIEW;
                folk_log_telemetry(3, SELECTED_COL as i32, 0);
            }
            _ => {}
        }
    }
}

// ── Rendering ───────────────────────────────────────────────────────────

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();

    folk_fill_screen(BG);

    // Header
    folk_draw_rect(0, 0, sw, HEADER_H, PANEL_BG);
    draw(MARGIN, 8, b"SemanticMail", ACCENT);
    draw(120, 8, b"Intent-Based Inbox", TEXT_DIM);

    let mut cb = [0u8; 16];
    let cl = { let mut m = Msg::new(&mut cb); m.u32(EMAIL_COUNT as u32); m.s(b" emails"); m.len() };
    draw(sw - 100, 8, &cb[..cl], TEXT_DIM);

    // Three Kanban columns
    let col_w = (sw - COL_PAD * 4) / 3;
    let content_y = HEADER_H + 4;
    let content_h = sh - HEADER_H - HELP_H - 8;

    let cols: [(Category, &[u8], i32, i32); 3] = [
        (Category::Action, b"ACTION", ACTION_COLOR, ACTION_BG),
        (Category::Question, b"QUESTION", QUESTION_COLOR, QUESTION_BG),
        (Category::Fyi, b"FYI", FYI_COLOR, FYI_BG),
    ];

    let emails = core::ptr::addr_of!(EMAILS) as *const Email;

    for (ci, (cat, label, color, bg)) in cols.iter().enumerate() {
        let cx = COL_PAD + (ci as i32) * (col_w + COL_PAD);
        let is_selected_col = ci == SELECTED_COL;

        // Column background
        folk_draw_rect(cx, content_y, col_w, content_h, *bg);

        // Column header
        folk_draw_rect(cx, content_y, col_w, 24, PANEL_BG);
        folk_draw_text(cx + 8, content_y + 4, label.as_ptr() as i32, label.len() as i32, *color);

        let count = count_in_category(*cat);
        let mut nb = [0u8; 8];
        let nl = { let mut m = Msg::new(&mut nb); m.s(b"("); m.u32(count as u32); m.s(b")"); m.len() };
        draw(cx + col_w - 40, content_y + 4, &nb[..nl], TEXT_DIM);

        // Email cards
        let mut card_y = content_y + 28;
        let mut card_idx = 0usize;

        for ei in 0..EMAIL_COUNT {
            let e = &*emails.add(ei);
            if e.category != *cat { continue; }
            if card_y + CARD_H > content_y + content_h { break; }

            let is_selected = is_selected_col && card_idx == SELECTED_ROW;

            // Card background
            folk_draw_rect(cx + 4, card_y, col_w - 8, CARD_H, CARD_BG);

            // Selection indicator
            if is_selected {
                folk_draw_rect(cx + 4, card_y, 3, CARD_H, SELECTED_BORDER);
                folk_draw_rect(cx + 4, card_y, col_w - 8, 1, SELECTED_BORDER);
            }

            // Subject (truncated to fit)
            let max_subj = ((col_w - 20) / FONT_W) as usize;
            let show_subj = (e.subject_len as usize).min(max_subj);
            folk_draw_text(cx + 10, card_y + 4,
                e.subject.as_ptr() as i32, show_subj as i32,
                if is_selected { TEXT } else { TEXT_DIM });

            // From
            folk_draw_text(cx + 10, card_y + 22,
                e.from.as_ptr() as i32, e.from_len as i32, *color);

            // Snippet (first line)
            let max_snip = ((col_w - 20) / FONT_W) as usize;
            let show_snip = (e.snippet_len as usize).min(max_snip);
            folk_draw_text(cx + 10, card_y + 40,
                e.snippet.as_ptr() as i32, show_snip as i32, TEXT_DIM);

            card_y += CARD_H + CARD_PAD;
            card_idx += 1;
        }
    }

    // Detail overlay if selected
    if DETAIL_VIEW {
        let cat = cols[SELECTED_COL].0;
        if let Some(ei) = nth_in_category(cat, SELECTED_ROW) {
            let e = &*emails.add(ei);
            let ow = sw - 80;
            let oh = sh - 120;
            let ox = 40;
            let oy = 60;

            folk_draw_rect(ox - 2, oy - 2, ow + 4, oh + 4, BORDER);
            folk_draw_rect(ox, oy, ow, oh, BG);

            // Subject
            folk_draw_text(ox + 12, oy + 12,
                e.subject.as_ptr() as i32, e.subject_len as i32, TEXT);

            // From
            draw(ox + 12, oy + 34, b"From: ", TEXT_DIM);
            folk_draw_text(ox + 60, oy + 34,
                e.from.as_ptr() as i32, e.from_len as i32, cols[SELECTED_COL].2);

            // Category badge
            draw(ox + 12, oy + 56, b"Category: ", TEXT_DIM);
            draw(ox + 92, oy + 56, cols[SELECTED_COL].1, cols[SELECTED_COL].2);

            // Body
            folk_draw_line(ox + 12, oy + 76, ox + ow - 12, oy + 76, BORDER);
            let max_chars = ((ow - 24) / FONT_W) as usize;
            let mut line = 0i32;
            let mut col = 0i32;
            for i in 0..e.snippet_len as usize {
                let b = e.snippet[i];
                if b == b'\n' || (col as usize) >= max_chars { line += 1; col = 0; }
                if b >= 0x20 && b < 0x7F {
                    folk_draw_text(ox + 12 + col * FONT_W, oy + 84 + line * 18,
                        e.snippet.as_ptr().add(i) as i32, 1, TEXT);
                    col += 1;
                }
            }

            draw(ox + 12, oy + oh - 24, b"[Esc] Close", TEXT_DIM);
        }
    }

    // Help
    folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
    draw(MARGIN, sh - HELP_H + 1,
        b"[</>] Column  [Up/Dn] Select  [Enter] Detail  [Esc] Close",
        TEXT_DIM);
}

// ── Main ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            load_demo_emails();
            // Categorize all emails using AI
            for i in 0..EMAIL_COUNT {
                categorize_email(i);
            }
            folk_log_telemetry(0, 0, 0); // AppOpened
            INITIALIZED = true;
        }

        handle_input();
        render();
    }
}
