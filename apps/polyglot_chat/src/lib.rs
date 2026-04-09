//! PolyglotChat — OS Consciousness Interface for Folkering OS
//!
//! Not a simple chatbot — a "liquid" interface to the OS's internal state.
//! Interprets natural language queries and routes them to the correct
//! subsystem: KernelSnoop metrics, AutoDream insights, Synapse VFS,
//! or the AI engine for code translation.
//!
//! Color palette:
//!   Blue   — User messages
//!   Grey   — System data responses
//!   Purple — AI/AutoDream responses
//!
//! Safety: AI runs in READ-ONLY mode for system files.
//! No folk_write_file calls from AI — only user-initiated saves.

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
    fn folk_slm_generate(prompt_ptr: i32, prompt_len: i32, buf_ptr: i32, max_len: i32) -> i32;
    fn folk_os_metric(metric_id: i32) -> i32;
    fn folk_net_has_ip() -> i32;
    fn folk_fw_drops() -> i32;
    fn folk_query_files(q_ptr: i32, q_len: i32, r_ptr: i32, r_max: i32) -> i32;
    fn folk_request_file(p_ptr: i32, p_len: i32, d_ptr: i32, d_max: i32) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
}

// ── Colors ──────────────────────────────────────────────────────────────

const BG: i32 = 0x0D1117;
const PANEL_BG: i32 = 0x161B22;
const BORDER: i32 = 0x30363D;
const INPUT_BG: i32 = 0x0F1318;
const TEXT_DIM: i32 = 0x484F58;

// Message colors
const USER_COLOR: i32 = 0x58A6FF;    // Blue — user
const USER_BG: i32 = 0x122040;
const SYSTEM_COLOR: i32 = 0x8B949E;  // Grey — system data
const SYSTEM_BG: i32 = 0x161B22;
const AI_COLOR: i32 = 0xBC8CFF;      // Purple — AI/AutoDream
const AI_BG: i32 = 0x1E1535;
const CURSOR: i32 = 0xF5C2E7;
const ACCENT: i32 = 0x58A6FF;
const WARN: i32 = 0xD29922;

// Layout
const INPUT_H: i32 = 40;
const HEADER_H: i32 = 28;
const MARGIN: i32 = 10;
const FONT_H: i32 = 16;
const FONT_W: i32 = 8;
const MSG_H: i32 = 20; // per line of message
const MSG_PAD: i32 = 6;

// Limits
const MAX_INPUT: usize = 512;
const MAX_MESSAGES: usize = 20;
const MAX_MSG_TEXT: usize = 400;

// ── Message types ───────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
#[repr(u8)]
enum MsgRole {
    User = 0,
    System = 1,
    Ai = 2,
}

#[derive(Clone, Copy)]
struct ChatMessage {
    role: MsgRole,
    text: [u8; MAX_MSG_TEXT],
    text_len: usize,
}

impl ChatMessage {
    const fn empty() -> Self {
        Self { role: MsgRole::System, text: [0u8; MAX_MSG_TEXT], text_len: 0 }
    }
}

// ── State ───────────────────────────────────────────────────────────────

static mut MESSAGES: [ChatMessage; MAX_MESSAGES] = [ChatMessage::empty(); MAX_MESSAGES];
static mut MSG_COUNT: usize = 0;
static mut SCROLL: usize = 0;

static mut INPUT: [u8; MAX_INPUT] = [0u8; MAX_INPUT];
static mut INPUT_LEN: usize = 0;

static mut AI_BUF: [u8; 1024] = [0u8; 1024];
static mut FILE_BUF: [u8; 1024] = [0u8; 1024];
static mut QUERY_BUF: [u8; 512] = [0u8; 512];
static mut EVT: [i32; 4] = [0i32; 4];

static mut PROCESSING: bool = false;
static mut INITIALIZED: bool = false;

// ── Helpers ─────────────────────────────────────────────────────────────

struct Msg<'a> { buf: &'a mut [u8], pos: usize }
impl<'a> Msg<'a> {
    fn new(buf: &'a mut [u8]) -> Self { Self { buf, pos: 0 } }
    fn s(&mut self, t: &[u8]) {
        for &b in t { if self.pos < self.buf.len() { self.buf[self.pos] = b; self.pos += 1; } }
    }
    fn i32(&mut self, v: i32) {
        if v < 0 { self.s(b"-"); self.u32((-v) as u32); } else { self.u32(v as u32); }
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

unsafe fn push_message(role: MsgRole, text: &[u8]) {
    let msgs = core::ptr::addr_of_mut!(MESSAGES) as *mut ChatMessage;
    let idx = if MSG_COUNT < MAX_MESSAGES {
        let i = MSG_COUNT; MSG_COUNT += 1; i
    } else {
        // Shift everything up
        for i in 0..MAX_MESSAGES - 1 { *msgs.add(i) = *msgs.add(i + 1); }
        MAX_MESSAGES - 1
    };
    let msg = &mut *msgs.add(idx);
    *msg = ChatMessage::empty();
    msg.role = role;
    msg.text_len = text.len().min(MAX_MSG_TEXT);
    for i in 0..msg.text_len { msg.text[i] = text[i]; }
}

/// Check if input matches a pattern (case-insensitive prefix)
fn starts_with_ci(input: &[u8], pattern: &[u8]) -> bool {
    if input.len() < pattern.len() { return false; }
    for i in 0..pattern.len() {
        let a = if input[i] >= b'A' && input[i] <= b'Z' { input[i] + 32 } else { input[i] };
        let b = if pattern[i] >= b'A' && pattern[i] <= b'Z' { pattern[i] + 32 } else { pattern[i] };
        if a != b { return false; }
    }
    true
}

fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() { return false; }
    for i in 0..=haystack.len() - needle.len() {
        if starts_with_ci(&haystack[i..], needle) { return true; }
    }
    false
}

// ── Intent Router — maps natural language to system actions ─────────────

unsafe fn route_intent(input: &[u8]) {
    // ── Magic slash commands ──
    if starts_with_ci(input, b"/insight") {
        handle_autodream_query();
        return;
    }
    if starts_with_ci(input, b"/translate_opendaq") {
        let code_start = if input.len() > 18 { 19 } else { input.len() };
        handle_opendaq_translation(&input[code_start..]);
        return;
    }

    // ── Network status queries ──
    if contains_ci(input, b"nettverk") || contains_ci(input, b"network")
        || contains_ci(input, b"online") || contains_ci(input, b"ip")
    {
        handle_network_status();
        return;
    }

    // ── Firewall / security queries ──
    if contains_ci(input, b"firewall") || contains_ci(input, b"drops")
        || contains_ci(input, b"sikkerhet") || contains_ci(input, b"security")
    {
        handle_firewall_status();
        return;
    }

    // ── AutoDream insights ──
    if contains_ci(input, b"autodream") || contains_ci(input, b"dream")
        || contains_ci(input, b"insight") || contains_ci(input, b"natt")
    {
        handle_autodream_query();
        return;
    }

    // ── Code translation ──
    if contains_ci(input, b"oversett") || contains_ci(input, b"translate")
        || contains_ci(input, b"c++") || contains_ci(input, b"rust")
        || contains_ci(input, b"konverter") || contains_ci(input, b"convert")
    {
        handle_code_translation(input);
        return;
    }

    // ── System status ──
    if contains_ci(input, b"status") || contains_ci(input, b"system")
        || contains_ci(input, b"minne") || contains_ci(input, b"memory")
    {
        handle_system_status();
        return;
    }

    // ── File search ──
    if starts_with_ci(input, b"finn ") || starts_with_ci(input, b"find ")
        || starts_with_ci(input, b"sok ") || starts_with_ci(input, b"search ")
    {
        let query_start = input.iter().position(|&b| b == b' ').unwrap_or(0) + 1;
        handle_file_search(&input[query_start..]);
        return;
    }

    // ── Fallback: send to AI ──
    handle_ai_query(input);
}

// ── Intent Handlers ─────────────────────────────────────────────────────

unsafe fn handle_network_status() {
    let online = folk_net_has_ip();
    let metric = folk_os_metric(0);

    let mut buf = [0u8; 120];
    let len = {
        let mut m = Msg::new(&mut buf);
        if online == 1 {
            m.s(b"Network: ONLINE | IP acquired via DHCP");
        } else {
            m.s(b"Network: OFFLINE | No IP assigned");
        }
        m.s(b" | Metric: ");
        m.i32(metric);
        m.len()
    };
    push_message(MsgRole::System, &buf[..len]);
}

unsafe fn handle_firewall_status() {
    let drops = folk_fw_drops();
    let suspicious = folk_os_metric(3);

    let mut buf = [0u8; 120];
    let len = {
        let mut m = Msg::new(&mut buf);
        m.s(b"Firewall: ");
        m.i32(drops);
        m.s(b" packets dropped | Suspicious: ");
        m.i32(suspicious);
        if drops > 10 {
            m.s(b" | WARNING: High drop rate");
        } else {
            m.s(b" | Status: Normal");
        }
        m.len()
    };
    push_message(MsgRole::System, &buf[..len]);
}

unsafe fn handle_autodream_query() {
    // Search for AutoDream insight files
    let query = b"autodream insight";
    let result_ptr = core::ptr::addr_of_mut!(QUERY_BUF) as *mut u8;
    let bytes = folk_query_files(
        query.as_ptr() as i32, query.len() as i32,
        result_ptr as i32, 512,
    );

    if bytes <= 0 {
        push_message(MsgRole::Ai, b"No AutoDream insights found yet. The system needs to run idle for 5+ minutes to generate insights.");
        return;
    }

    // Try to load first result
    let result = core::slice::from_raw_parts(result_ptr, bytes as usize);
    // Find first filename (before \t)
    let name_end = result.iter().position(|&b| b == b'\t' || b == b'\n').unwrap_or(bytes as usize);
    let name = &result[..name_end];

    if name.is_empty() {
        push_message(MsgRole::Ai, b"AutoDream insight files found but could not parse filenames.");
        return;
    }

    // Load file content
    let file_ptr = core::ptr::addr_of_mut!(FILE_BUF) as *mut u8;
    let loaded = folk_request_file(
        name.as_ptr() as i32, name.len() as i32,
        file_ptr as i32, 1024,
    );

    if loaded > 0 {
        let content = core::slice::from_raw_parts(file_ptr, (loaded as usize).min(MAX_MSG_TEXT));
        push_message(MsgRole::Ai, content);
    } else {
        push_message(MsgRole::Ai, b"Found insight file but could not read it.");
    }
}

unsafe fn handle_code_translation(input: &[u8]) {
    // Build prompt: ask AI to translate the code
    let ai_ptr = core::ptr::addr_of_mut!(AI_BUF) as *mut u8;
    let mut prompt = [0u8; 1024];
    let prompt_len = {
        let mut m = Msg::new(&mut prompt);
        m.s(b"Translate this code to no_std Rust compatible with Folkering OS ");
        m.s(b"(bare-metal, use libfolk syscalls instead of std). ");
        m.s(b"Return ONLY the Rust code, no explanation. Code: ");
        let code_start = input.iter()
            .position(|&b| b == b':' || b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let code = &input[code_start..];
        m.s(code);
        m.len()
    };

    push_message(MsgRole::System, b"Translating code to no_std Rust...");

    let resp = folk_slm_generate(
        prompt.as_ptr() as i32, prompt_len as i32,
        ai_ptr as i32, 800,
    );

    if resp > 0 {
        let response = core::slice::from_raw_parts(ai_ptr, (resp as usize).min(MAX_MSG_TEXT));
        push_message(MsgRole::Ai, response);
    } else {
        push_message(MsgRole::Ai, b"Translation failed - AI returned empty response.");
    }
}

unsafe fn handle_opendaq_translation(code: &[u8]) {
    let ai_ptr = core::ptr::addr_of_mut!(AI_BUF) as *mut u8;
    let mut prompt = [0u8; 1024];
    let prompt_len = {
        let mut m = Msg::new(&mut prompt);
        m.s(b"You are a senior systems architect. Translate this C++ code from ");
        m.s(b"openDAQ to no_std Rust compatible with Folkering OS (bare-metal, ");
        m.s(b"use libfolk syscalls). Return ONLY Rust code. Code:\n");
        m.s(code);
        m.len()
    };

    push_message(MsgRole::System, b"Translating openDAQ C++ to no_std Rust...");

    let resp = folk_slm_generate(
        prompt.as_ptr() as i32, prompt_len as i32,
        ai_ptr as i32, 800,
    );

    if resp > 0 {
        let response = core::slice::from_raw_parts(ai_ptr, (resp as usize).min(MAX_MSG_TEXT));
        push_message(MsgRole::Ai, response);
    } else {
        push_message(MsgRole::Ai, b"Translation failed. AI engine may be offline.");
    }
}

unsafe fn handle_system_status() {
    let online = folk_net_has_ip();
    let drops = folk_fw_drops();
    let uptime_metric = folk_os_metric(2);
    let suspicious = folk_os_metric(3);

    let mut buf = [0u8; 200];
    let len = {
        let mut m = Msg::new(&mut buf);
        m.s(b"System Status:\n");
        m.s(b"  Net: ");
        m.s(if online == 1 { b"ONLINE" } else { b"OFFLINE" });
        m.s(b" | FW drops: ");
        m.i32(drops);
        m.s(b" | Suspicious: ");
        m.i32(suspicious);
        m.s(b"\n  Uptime metric: ");
        m.i32(uptime_metric);
        m.len()
    };
    push_message(MsgRole::System, &buf[..len]);
}

unsafe fn handle_file_search(query: &[u8]) {
    let result_ptr = core::ptr::addr_of_mut!(QUERY_BUF) as *mut u8;
    let bytes = folk_query_files(
        query.as_ptr() as i32, query.len() as i32,
        result_ptr as i32, 400,
    );

    if bytes <= 0 {
        push_message(MsgRole::System, b"No files found matching that query.");
        return;
    }

    let mut buf = [0u8; MAX_MSG_TEXT];
    let len = {
        let mut m = Msg::new(&mut buf);
        m.s(b"Found files:\n");
        let result = core::slice::from_raw_parts(result_ptr, (bytes as usize).min(300));
        m.s(result);
        m.len()
    };
    push_message(MsgRole::System, &buf[..len]);
}

unsafe fn handle_ai_query(input: &[u8]) {
    let ai_ptr = core::ptr::addr_of_mut!(AI_BUF) as *mut u8;
    let mut prompt = [0u8; 600];
    let prompt_len = {
        let mut m = Msg::new(&mut prompt);
        m.s(b"You are Folk, the AI consciousness of Folkering OS (bare-metal Rust). ");
        m.s(b"Answer concisely about the OS, its architecture, or general topics. ");
        m.s(b"User asks: ");
        m.s(input);
        m.len()
    };

    let resp = folk_slm_generate(
        prompt.as_ptr() as i32, prompt_len as i32,
        ai_ptr as i32, 600,
    );

    if resp > 0 {
        let response = core::slice::from_raw_parts(ai_ptr, (resp as usize).min(MAX_MSG_TEXT));
        push_message(MsgRole::Ai, response);
    } else {
        push_message(MsgRole::Ai, b"Could not generate a response. The AI engine may be offline.");
    }
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
        let inp = core::ptr::addr_of_mut!(INPUT) as *mut u8;

        match key {
            // Enter — send message
            0x0D => {
                if INPUT_LEN > 0 && !PROCESSING {
                    let input = core::slice::from_raw_parts(
                        core::ptr::addr_of!(INPUT) as *const u8, INPUT_LEN);
                    // Add user message
                    push_message(MsgRole::User, input);
                    // Log telemetry
                    folk_log_telemetry(3, INPUT_LEN as i32, 0); // UiInteraction
                    // Route intent
                    PROCESSING = true;
                    route_intent(input);
                    PROCESSING = false;
                    // Clear input
                    INPUT_LEN = 0;
                }
            }
            // Backspace
            0x08 => { if INPUT_LEN > 0 { INPUT_LEN -= 1; } }
            // Printable
            0x20..=0x7E => {
                if INPUT_LEN < MAX_INPUT - 1 {
                    *inp.add(INPUT_LEN) = key;
                    INPUT_LEN += 1;
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

    folk_fill_screen(BG);

    // ── Header ──
    folk_draw_rect(0, 0, sw, HEADER_H, PANEL_BG);
    draw(MARGIN, 6, b"PolyglotChat", ACCENT);
    draw(120, 6, b"OS Consciousness Interface", TEXT_DIM);

    if PROCESSING {
        draw(sw - 140, 6, b"Processing...", WARN);
    }

    // ── Chat area ──
    let chat_y = HEADER_H + 2;
    let chat_h = sh - HEADER_H - INPUT_H - 4;
    let max_visible_lines = (chat_h / (MSG_H + MSG_PAD)) as usize;

    let msgs = core::ptr::addr_of!(MESSAGES) as *const ChatMessage;

    // Calculate which messages to show (auto-scroll to bottom)
    let start = if MSG_COUNT > max_visible_lines {
        MSG_COUNT - max_visible_lines
    } else { 0 };

    let mut y = chat_y + 4;
    for i in start..MSG_COUNT {
        let msg = &*msgs.add(i);
        if msg.text_len == 0 { continue; }

        // Count lines in this message
        let text = &msg.text[..msg.text_len];
        let max_chars = ((sw - MARGIN * 4) / FONT_W) as usize;
        let line_count = count_lines(text, max_chars);
        let msg_height = (line_count as i32) * MSG_H + MSG_PAD * 2;

        if y + msg_height > sh - INPUT_H - 4 { break; }

        // Background and role indicator
        let (bg, text_color, label) = match msg.role {
            MsgRole::User => (USER_BG, USER_COLOR, b"You" as &[u8]),
            MsgRole::System => (SYSTEM_BG, SYSTEM_COLOR, b"Sys" as &[u8]),
            MsgRole::Ai => (AI_BG, AI_COLOR, b"AI " as &[u8]),
        };

        folk_draw_rect(MARGIN, y, sw - MARGIN * 2, msg_height, bg);

        // Role badge
        folk_draw_rect(MARGIN + 2, y + 2, 28, FONT_H + 2, BORDER);
        draw(MARGIN + 4, y + 3, label, text_color);

        // Message text (word-wrapped)
        let text_x = MARGIN + 36;
        let mut tx = text_x;
        let mut ty = y + MSG_PAD;
        let text_max_x = sw - MARGIN * 2;

        for &b in text {
            if b == b'\n' {
                ty += MSG_H;
                tx = text_x;
                continue;
            }
            if tx + FONT_W > text_max_x {
                ty += MSG_H;
                tx = text_x;
            }
            if b >= 0x20 && b < 0x7F {
                folk_draw_text(tx, ty, &b as *const u8 as i32, 1, text_color);
                tx += FONT_W;
            }
        }

        y += msg_height + 2;
    }

    // ── Input bar ──
    let input_y = sh - INPUT_H;
    folk_draw_rect(0, input_y, sw, INPUT_H, PANEL_BG);
    folk_draw_rect(MARGIN, input_y + 6, sw - MARGIN * 2 - 60, INPUT_H - 12, INPUT_BG);

    // Prompt indicator
    draw(MARGIN + 4, input_y + 12, b">", USER_COLOR);

    // Input text
    if INPUT_LEN > 0 {
        let inp = core::ptr::addr_of!(INPUT) as *const u8;
        let show = INPUT_LEN.min(((sw - 80) / FONT_W) as usize);
        folk_draw_text(MARGIN + 16, input_y + 12, inp as i32, show as i32, USER_COLOR);
    } else {
        draw(MARGIN + 16, input_y + 12, b"Ask about network, AutoDream, translate code...", TEXT_DIM);
    }

    // Cursor
    let cursor_x = MARGIN + 16 + (INPUT_LEN as i32) * FONT_W;
    folk_draw_rect(cursor_x, input_y + 10, 2, FONT_H, CURSOR);

    // Send button
    let btn_x = sw - MARGIN - 50;
    folk_draw_rect(btn_x, input_y + 6, 44, INPUT_H - 12, 0x238636);
    draw(btn_x + 6, input_y + 12, b"Send", 0xFFFFFF);
}

fn count_lines(text: &[u8], max_chars: usize) -> usize {
    let mut lines = 1;
    let mut col = 0;
    for &b in text {
        if b == b'\n' { lines += 1; col = 0; }
        else { col += 1; if col >= max_chars { col = 0; lines += 1; } }
    }
    lines
}

// ── Main ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            push_message(MsgRole::Ai,
                b"Welcome to PolyglotChat. I am Folk, the consciousness of Folkering OS.\n\
                  Ask me about network status, AutoDream insights, or paste code to translate.");
            push_message(MsgRole::System,
                b"Intents: 'network' 'firewall' 'autodream' 'status' 'translate' 'find <query>'");
            folk_log_telemetry(0, 0, 0); // AppOpened
            INITIALIZED = true;
        }
        handle_input();
        render();
    }
}
