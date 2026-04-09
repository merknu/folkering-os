//! PromptLaboratory — LLM Logit Analysis & A/B Testing for Folkering OS
//!
//! Three-panel UI:
//!   Left:   Prompt editor with system instruction
//!   Right:  Generated text with per-token confidence heatmap
//!   Bottom: Token inspector showing alternative tokens
//!
//! Uses folk_slm_generate_with_logits() which returns PLAB-formatted
//! results: generated text + per-token probabilities + top-3 alternatives.

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
    fn folk_slm_generate_with_logits(prompt_ptr: i32, prompt_len: i32, out_ptr: i32, max_len: i32) -> i32;
    fn folk_write_file(path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32) -> i32;
    fn folk_list_files(buf_ptr: i32, max_len: i32) -> i32;
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
const CURSOR_COLOR: i32 = 0xF5C2E7;
const BTN_BG: i32 = 0x238636;
const BTN_TEXT: i32 = 0xFFFFFF;

// Confidence heatmap colors
const CONF_HIGH: i32 = 0x3FB950;   // >80% green
const CONF_MED: i32 = 0xD29922;    // 50-80% yellow
const CONF_LOW: i32 = 0xF85149;    // <50% red

// Layout
const DIVIDER_X: i32 = 460;  // vertical split
const INSPECT_Y: i32 = 560;  // bottom panel top
const HEADER_H: i32 = 36;
const HELP_H: i32 = 18;
const MARGIN: i32 = 8;
const FONT_H: i32 = 16;
const FONT_W: i32 = 8;
const LINE_H: i32 = 18;

// Limits
const MAX_PROMPT: usize = 1024;
const MAX_RESULT: usize = 8192;
const MAX_TOKENS: usize = 128;
const MAX_FILES_BUF: usize = 1024;

// ── PLAB wire format ────────────────────────────────────────────────────

fn read_u16_le(buf: &[u8], off: usize) -> u16 {
    if off + 2 > buf.len() { return 0; }
    (buf[off] as u16) | ((buf[off + 1] as u16) << 8)
}
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    if off + 4 > buf.len() { return 0; }
    (buf[off] as u32) | ((buf[off+1] as u32)<<8) | ((buf[off+2] as u32)<<16) | ((buf[off+3] as u32)<<24)
}
fn read_f32_le(buf: &[u8], off: usize) -> f32 {
    f32::from_bits(read_u32_le(buf, off))
}

// ── Token data (parsed from PLAB) ──────────────────────────────────────

#[derive(Clone, Copy)]
struct TokenInfo {
    start: u16,
    len: u16,
    prob: f32,
    alt1: f32,
    alt2: f32,
    alt3: f32,
}

// ── Persistent state ────────────────────────────────────────────────────

// Prompt editor
static mut PROMPT: [u8; MAX_PROMPT] = [0u8; MAX_PROMPT];
static mut PROMPT_LEN: usize = 0;
static mut CURSOR_POS: usize = 0;

// Result from inference
static mut RESULT_BUF: [u8; MAX_RESULT] = [0u8; MAX_RESULT];
static mut RESULT_LEN: usize = 0;
static mut HAS_RESULT: bool = false;

// Parsed tokens
static mut TOKENS: [TokenInfo; MAX_TOKENS] = [TokenInfo {
    start: 0, len: 0, prob: 0.0, alt1: 0.0, alt2: 0.0, alt3: 0.0,
}; MAX_TOKENS];
static mut TOKEN_COUNT: usize = 0;
static mut SELECTED_TOKEN: usize = 0;
static mut HAS_REAL_LOGITS: bool = false;

// Generated text (extracted from PLAB)
static mut GEN_TEXT: [u8; 2048] = [0u8; 2048];
static mut GEN_TEXT_LEN: usize = 0;

// UI state
static mut RUNNING: bool = false;
static mut INITIALIZED: bool = false;
static mut EVT: [i32; 4] = [0i32; 4];
static mut EDIT_MODE: bool = true; // true=editing prompt, false=inspecting output

// Files list
static mut FILES_BUF: [u8; MAX_FILES_BUF] = [0u8; MAX_FILES_BUF];

// ── Helpers ─────────────────────────────────────────────────────────────

struct Msg<'a> { buf: &'a mut [u8], pos: usize }
impl<'a> Msg<'a> {
    fn new(buf: &'a mut [u8]) -> Self { Self { buf, pos: 0 } }
    fn s(&mut self, text: &[u8]) {
        for &b in text {
            if self.pos < self.buf.len() { self.buf[self.pos] = b; self.pos += 1; }
        }
    }
    fn u32(&mut self, mut val: u32) {
        if val == 0 { self.s(b"0"); return; }
        let mut tmp = [0u8; 10]; let mut i = 0;
        while val > 0 { tmp[i] = b'0' + (val % 10) as u8; val /= 10; i += 1; }
        while i > 0 { i -= 1; if self.pos < self.buf.len() { self.buf[self.pos] = tmp[i]; self.pos += 1; } }
    }
    fn pct(&mut self, v: f32) {
        let p = (v * 100.0) as u32;
        self.u32(p.min(100));
        self.s(b"%");
    }
    fn len(&self) -> usize { self.pos }
}

unsafe fn draw(x: i32, y: i32, text: &[u8], color: i32) {
    folk_draw_text(x, y, text.as_ptr() as i32, text.len() as i32, color);
}

fn conf_color(prob: f32) -> i32 {
    if prob >= 0.80 { CONF_HIGH }
    else if prob >= 0.50 { CONF_MED }
    else { CONF_LOW }
}

fn conf_bg(prob: f32) -> i32 {
    // Darker version for background
    if prob >= 0.80 { 0x1A3D1A }
    else if prob >= 0.50 { 0x3D3100 }
    else { 0x3D1A1A }
}

// ── Initialize default prompt ───────────────────────────────────────────

unsafe fn init_default_prompt() {
    let default = b"Explain how a transformer model processes text, step by step.";
    let len = default.len().min(MAX_PROMPT);
    let p = core::ptr::addr_of_mut!(PROMPT) as *mut u8;
    for i in 0..len { *p.add(i) = default[i]; }
    PROMPT_LEN = len;
    CURSOR_POS = len;
}

// ── Run inference ───────────────────────────────────────────────────────

unsafe fn run_inference() {
    RUNNING = true;

    let prompt_ptr = core::ptr::addr_of!(PROMPT) as i32;
    let result_ptr = core::ptr::addr_of_mut!(RESULT_BUF) as *mut u8;

    let bytes = folk_slm_generate_with_logits(
        prompt_ptr,
        PROMPT_LEN as i32,
        result_ptr as i32,
        MAX_RESULT as i32,
    );

    RUNNING = false;

    if bytes <= 0 {
        HAS_RESULT = false;
        return;
    }

    RESULT_LEN = bytes as usize;

    // Parse PLAB header
    let buf = core::slice::from_raw_parts(result_ptr, RESULT_LEN);
    if buf.len() < 16 { HAS_RESULT = false; return; }
    if buf[0] != b'P' || buf[1] != b'L' || buf[2] != b'A' || buf[3] != b'B' {
        HAS_RESULT = false;
        return;
    }

    let text_len = read_u32_le(buf, 4) as usize;
    let token_count = read_u32_le(buf, 8) as usize;
    let flags = read_u32_le(buf, 12);
    HAS_REAL_LOGITS = (flags & 1) != 0;

    // Copy generated text
    let text_start = 16;
    let copy = text_len.min(2048);
    let gt = core::ptr::addr_of_mut!(GEN_TEXT) as *mut u8;
    for i in 0..copy { *gt.add(i) = buf[text_start + i]; }
    GEN_TEXT_LEN = copy;

    // Parse token entries
    let text_padded = (text_len + 3) & !3;
    let entries_start = 16 + text_padded;
    let count = token_count.min(MAX_TOKENS);
    TOKEN_COUNT = count;

    let tok = core::ptr::addr_of_mut!(TOKENS) as *mut TokenInfo;
    for i in 0..count {
        let off = entries_start + i * 24;
        if off + 24 > buf.len() { TOKEN_COUNT = i; break; }
        *tok.add(i) = TokenInfo {
            start: read_u16_le(buf, off),
            len: read_u16_le(buf, off + 2),
            prob: read_f32_le(buf, off + 4),
            alt1: read_f32_le(buf, off + 8),
            alt2: read_f32_le(buf, off + 12),
            alt3: read_f32_le(buf, off + 16),
        };
    }

    SELECTED_TOKEN = 0;
    HAS_RESULT = true;
    EDIT_MODE = false; // Switch to inspect mode
}

// ── Save prompt to VFS ──────────────────────────────────────────────────

unsafe fn save_prompt() {
    let path = b"prompts/prompt_v1.txt";
    folk_write_file(
        path.as_ptr() as i32, path.len() as i32,
        core::ptr::addr_of!(PROMPT) as i32, PROMPT_LEN as i32,
    );
}

// ── Input handling ──────────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let evt_ptr = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(evt_ptr as i32) == 0 { break; }
        let event_type = *evt_ptr.add(0);
        let data = *evt_ptr.add(3);

        if event_type != 3 { continue; } // key_down only

        let key = data as u8;
        match key {
            // F5 — Run inference
            0x74 => {
                if !RUNNING { run_inference(); }
            }
            // Tab — Toggle edit/inspect mode
            0x09 => {
                if HAS_RESULT { EDIT_MODE = !EDIT_MODE; }
            }
            // Ctrl+S — Save prompt
            0x13 => { save_prompt(); }
            // Escape — Back to edit mode
            0x1B => { EDIT_MODE = true; }
            _ => {
                if EDIT_MODE {
                    handle_editor_key(key);
                } else {
                    handle_inspector_key(key);
                }
            }
        }
    }
}

unsafe fn handle_editor_key(key: u8) {
    let p = core::ptr::addr_of_mut!(PROMPT) as *mut u8;
    match key {
        // Backspace
        0x08 => {
            if CURSOR_POS > 0 && PROMPT_LEN > 0 {
                // Shift bytes left
                let mut i = CURSOR_POS - 1;
                while i < PROMPT_LEN - 1 {
                    *p.add(i) = *p.add(i + 1);
                    i += 1;
                }
                PROMPT_LEN -= 1;
                CURSOR_POS -= 1;
            }
        }
        // Left arrow
        0x25 => { if CURSOR_POS > 0 { CURSOR_POS -= 1; } }
        // Right arrow
        0x27 => { if CURSOR_POS < PROMPT_LEN { CURSOR_POS += 1; } }
        // Home
        0x24 => { CURSOR_POS = 0; }
        // End
        0x23 => { CURSOR_POS = PROMPT_LEN; }
        // Enter → newline
        0x0D => {
            if PROMPT_LEN < MAX_PROMPT - 1 {
                // Shift right from cursor
                let mut i = PROMPT_LEN;
                while i > CURSOR_POS {
                    *p.add(i) = *p.add(i - 1);
                    i -= 1;
                }
                *p.add(CURSOR_POS) = b'\n';
                PROMPT_LEN += 1;
                CURSOR_POS += 1;
            }
        }
        // Printable ASCII
        0x20..=0x7E => {
            if PROMPT_LEN < MAX_PROMPT - 1 {
                // Shift right from cursor
                let mut i = PROMPT_LEN;
                while i > CURSOR_POS {
                    *p.add(i) = *p.add(i - 1);
                    i -= 1;
                }
                *p.add(CURSOR_POS) = key;
                PROMPT_LEN += 1;
                CURSOR_POS += 1;
            }
        }
        _ => {}
    }
}

unsafe fn handle_inspector_key(key: u8) {
    match key {
        // Left arrow — previous token
        0x25 => { if SELECTED_TOKEN > 0 { SELECTED_TOKEN -= 1; } }
        // Right arrow — next token
        0x27 => { if SELECTED_TOKEN + 1 < TOKEN_COUNT { SELECTED_TOKEN += 1; } }
        // Home
        0x24 => { SELECTED_TOKEN = 0; }
        // End
        0x23 => { if TOKEN_COUNT > 0 { SELECTED_TOKEN = TOKEN_COUNT - 1; } }
        _ => {}
    }
}

// ── Rendering ───────────────────────────────────────────────────────────

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();

    folk_fill_screen(BG);

    // ── Header ──
    folk_draw_rect(0, 0, sw, HEADER_H, PANEL_BG);
    draw(MARGIN, 10, b"PromptLab", TEXT_ACCENT);

    // Mode indicator
    let mode = if EDIT_MODE { b"EDIT" as &[u8] } else { b"INSPECT" };
    let mode_color = if EDIT_MODE { TEXT_GREEN } else { TEXT_YELLOW };
    draw(120, 10, mode, mode_color);

    // Token count
    if HAS_RESULT {
        let mut buf = [0u8; 32];
        let len = { let mut m = Msg::new(&mut buf); m.s(b"Tokens: "); m.u32(TOKEN_COUNT as u32); m.len() };
        draw(220, 10, &buf[..len], TEXT_DIM);

        if HAS_REAL_LOGITS {
            draw(350, 10, b"[TDMP]", TEXT_GREEN);
        }
    }

    if RUNNING {
        draw(sw / 2 - 60, 10, b"Running inference...", TEXT_YELLOW);
    }

    // ── Vertical divider ──
    folk_draw_line(DIVIDER_X, HEADER_H, DIVIDER_X, INSPECT_Y, BORDER);
    // ── Horizontal divider (inspector) ──
    folk_draw_line(0, INSPECT_Y, sw, INSPECT_Y, BORDER);

    // ── Left panel: Prompt Editor ──
    render_editor(sw, sh);

    // ── Right panel: Output Heatmap ──
    render_output(sw, sh);

    // ── Bottom panel: Token Inspector ──
    render_inspector(sw, sh);

    // ── Help bar ──
    folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
    draw(MARGIN, sh - HELP_H + 1,
        b"[F5] Run  [Tab] Edit/Inspect  [</>] Select token  [Esc] Edit  [Ctrl+S] Save",
        TEXT_DIM);
}

unsafe fn render_editor(_sw: i32, _sh: i32) {
    let x0 = MARGIN;
    let y0 = HEADER_H + 4;
    let panel_w = DIVIDER_X - MARGIN * 2;

    // Title
    draw(x0, y0, b"Prompt:", TEXT_DIM);

    // "Run (F5)" button
    folk_draw_rect(DIVIDER_X - 80, y0 - 2, 70, 18, BTN_BG);
    draw(DIVIDER_X - 74, y0, b"Run F5", BTN_TEXT);

    // Text area background
    let text_y = y0 + 22;
    let text_h = INSPECT_Y - text_y - 8;
    folk_draw_rect(x0, text_y, panel_w, text_h, 0x0F1318);

    // Render prompt text with cursor
    let p = core::ptr::addr_of!(PROMPT) as *const u8;
    let mut line = 0i32;
    let mut col = 0i32;
    let max_cols = ((panel_w - 8) / FONT_W) as usize;
    let max_lines = ((text_h - 4) / LINE_H) as usize;

    let mut i = 0usize;
    while i <= PROMPT_LEN && (line as usize) < max_lines {
        // Draw cursor
        if i == CURSOR_POS && EDIT_MODE {
            let cx = x0 + 4 + col * FONT_W;
            let cy = text_y + 2 + line * LINE_H;
            folk_draw_rect(cx, cy, 2, FONT_H, CURSOR_COLOR);
        }

        if i >= PROMPT_LEN { break; }

        let ch = *p.add(i);
        if ch == b'\n' {
            line += 1;
            col = 0;
        } else {
            if (col as usize) < max_cols {
                let cx = x0 + 4 + col * FONT_W;
                let cy = text_y + 2 + line * LINE_H;
                folk_draw_text(cx, cy, p.add(i) as i32, 1, TEXT);
            }
            col += 1;
            if (col as usize) >= max_cols {
                col = 0;
                line += 1;
            }
        }
        i += 1;
    }

    // Character count
    let mut buf = [0u8; 16];
    let len = { let mut m = Msg::new(&mut buf); m.u32(PROMPT_LEN as u32); m.s(b"/"); m.u32(MAX_PROMPT as u32); m.len() };
    draw(x0, INSPECT_Y - 18, &buf[..len], TEXT_DIM);
}

unsafe fn render_output(sw: i32, _sh: i32) {
    let x0 = DIVIDER_X + MARGIN;
    let y0 = HEADER_H + 4;
    let panel_w = sw - DIVIDER_X - MARGIN * 2;

    draw(x0, y0, b"Output (confidence heatmap):", TEXT_DIM);

    if !HAS_RESULT {
        draw(x0 + 20, y0 + 40, b"Press F5 to run inference", TEXT_DIM);
        return;
    }

    // Render tokens with confidence-colored backgrounds
    let text_y = y0 + 22;
    let max_cols = ((panel_w - 8) / FONT_W) as usize;
    let max_lines = ((INSPECT_Y - text_y - 8) / LINE_H) as usize;

    let gt = core::ptr::addr_of!(GEN_TEXT) as *const u8;
    let tok = core::ptr::addr_of!(TOKENS) as *const TokenInfo;

    let mut line = 0i32;
    let mut col = 0i32;

    for ti in 0..TOKEN_COUNT {
        let t = &*tok.add(ti);
        let word_start = t.start as usize;
        let word_len = t.len as usize;

        if word_start + word_len > GEN_TEXT_LEN { break; }
        if (line as usize) >= max_lines { break; }

        // Add space before word (except first)
        if ti > 0 && col > 0 {
            col += 1;
            if (col as usize) >= max_cols { col = 0; line += 1; }
        }

        // Check if word fits on current line
        if (col as usize) + word_len > max_cols && col > 0 {
            col = 0;
            line += 1;
        }
        if (line as usize) >= max_lines { break; }

        let wx = x0 + 4 + col * FONT_W;
        let wy = text_y + 2 + line * LINE_H;

        // Background color based on confidence
        let bg = conf_bg(t.prob);
        folk_draw_rect(wx - 1, wy - 1, (word_len as i32) * FONT_W + 2, FONT_H + 2, bg);

        // Selected token highlight
        if ti == SELECTED_TOKEN && !EDIT_MODE {
            folk_draw_rect(wx - 2, wy - 2, (word_len as i32) * FONT_W + 4, FONT_H + 4, BORDER);
            folk_draw_rect(wx - 1, wy + FONT_H, (word_len as i32) * FONT_W + 2, 2, CURSOR_COLOR);
        }

        // Draw word text
        let text_color = conf_color(t.prob);
        folk_draw_text(wx, wy, gt.add(word_start) as i32, word_len as i32, text_color);

        col += word_len as i32;
    }
}

unsafe fn render_inspector(sw: i32, sh: i32) {
    let y0 = INSPECT_Y + 4;
    let panel_h = sh - INSPECT_Y - HELP_H - 8;

    draw(MARGIN, y0, b"Token Inspector:", TEXT_DIM);

    if !HAS_RESULT || TOKEN_COUNT == 0 {
        draw(MARGIN + 20, y0 + 24, b"No tokens to inspect", TEXT_DIM);
        return;
    }

    let tok = core::ptr::addr_of!(TOKENS) as *const TokenInfo;
    let gt = core::ptr::addr_of!(GEN_TEXT) as *const u8;

    let t = &*tok.add(SELECTED_TOKEN);
    let word_start = t.start as usize;
    let word_len = t.len as usize;

    // Selected word display
    let sel_y = y0 + 20;
    draw(MARGIN, sel_y, b"Selected:", TEXT_DIM);
    if word_start + word_len <= GEN_TEXT_LEN {
        folk_draw_rect(MARGIN + 80, sel_y - 2, (word_len as i32) * FONT_W + 8, FONT_H + 4, conf_bg(t.prob));
        folk_draw_text(MARGIN + 84, sel_y, gt.add(word_start) as i32, word_len as i32, conf_color(t.prob));
    }

    // Token index
    let mut idx_buf = [0u8; 24];
    let idx_len = {
        let mut m = Msg::new(&mut idx_buf);
        m.s(b"[");
        m.u32((SELECTED_TOKEN + 1) as u32);
        m.s(b"/");
        m.u32(TOKEN_COUNT as u32);
        m.s(b"]");
        m.len()
    };
    draw(MARGIN + 80 + (word_len as i32 + 2) * FONT_W, sel_y, &idx_buf[..idx_len], TEXT_DIM);

    // Probability bars
    let bar_y = sel_y + 24;
    let bar_w = 300i32;

    // Chosen token probability
    let mut p_buf = [0u8; 32];
    let p_len = { let mut m = Msg::new(&mut p_buf); m.s(b"Chosen: "); m.pct(t.prob); m.len() };
    draw(MARGIN, bar_y, &p_buf[..p_len], conf_color(t.prob));
    let bar_fill = ((t.prob * bar_w as f32) as i32).max(1).min(bar_w);
    folk_draw_rect(MARGIN + 120, bar_y, bar_w, 14, 0x0F1318);
    folk_draw_rect(MARGIN + 120, bar_y, bar_fill, 14, conf_color(t.prob));

    // Alternative 1
    let a1_y = bar_y + 20;
    let mut a1_buf = [0u8; 32];
    let a1_len = { let mut m = Msg::new(&mut a1_buf); m.s(b"Alt 1:  "); m.pct(t.alt1); m.len() };
    draw(MARGIN, a1_y, &a1_buf[..a1_len], TEXT_DIM);
    let a1_fill = ((t.alt1 * bar_w as f32) as i32).max(1).min(bar_w);
    folk_draw_rect(MARGIN + 120, a1_y, bar_w, 14, 0x0F1318);
    folk_draw_rect(MARGIN + 120, a1_y, a1_fill, 14, TEXT_DIM);

    // Alternative 2
    let a2_y = a1_y + 20;
    let mut a2_buf = [0u8; 32];
    let a2_len = { let mut m = Msg::new(&mut a2_buf); m.s(b"Alt 2:  "); m.pct(t.alt2); m.len() };
    draw(MARGIN, a2_y, &a2_buf[..a2_len], TEXT_DIM);
    let a2_fill = ((t.alt2 * bar_w as f32) as i32).max(1).min(bar_w);
    folk_draw_rect(MARGIN + 120, a2_y, bar_w, 14, 0x0F1318);
    folk_draw_rect(MARGIN + 120, a2_y, a2_fill, 14, TEXT_DIM);

    // Alternative 3
    let a3_y = a2_y + 20;
    let mut a3_buf = [0u8; 32];
    let a3_len = { let mut m = Msg::new(&mut a3_buf); m.s(b"Alt 3:  "); m.pct(t.alt3); m.len() };
    draw(MARGIN, a3_y, &a3_buf[..a3_len], TEXT_DIM);
    let a3_fill = ((t.alt3 * bar_w as f32) as i32).max(1).min(bar_w);
    folk_draw_rect(MARGIN + 120, a3_y, bar_w, 14, 0x0F1318);
    folk_draw_rect(MARGIN + 120, a3_y, a3_fill, 14, TEXT_DIM);

    // Right side: confidence legend + overall stats
    let legend_x = sw / 2 + 40;
    draw(legend_x, y0, b"Confidence Legend:", TEXT_DIM);
    folk_draw_rect(legend_x, y0 + 18, 14, 14, 0x1A3D1A);
    draw(legend_x + 20, y0 + 19, b">80% High confidence", TEXT_GREEN);
    folk_draw_rect(legend_x, y0 + 36, 14, 14, 0x3D3100);
    draw(legend_x + 20, y0 + 37, b"50-80% Medium", TEXT_YELLOW);
    folk_draw_rect(legend_x, y0 + 54, 14, 14, 0x3D1A1A);
    draw(legend_x + 20, y0 + 55, b"<50% Low confidence", TEXT_RED);

    // Overall stats
    if TOKEN_COUNT > 0 {
        let mut sum = 0.0f32;
        let mut min_p = 1.0f32;
        for i in 0..TOKEN_COUNT {
            let ti = &*tok.add(i);
            sum += ti.prob;
            if ti.prob < min_p { min_p = ti.prob; }
        }
        let avg = sum / TOKEN_COUNT as f32;

        let stats_y = y0 + 80;
        let mut avg_buf = [0u8; 32];
        let avg_len = { let mut m = Msg::new(&mut avg_buf); m.s(b"Avg conf: "); m.pct(avg); m.len() };
        draw(legend_x, stats_y, &avg_buf[..avg_len], TEXT);

        let mut min_buf = [0u8; 32];
        let min_len = { let mut m = Msg::new(&mut min_buf); m.s(b"Min conf: "); m.pct(min_p); m.len() };
        draw(legend_x, stats_y + 18, &min_buf[..min_len], conf_color(min_p));
    }
}

// ── Entry point ─────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            init_default_prompt();
            INITIALIZED = true;
        }
        handle_input();
        render();
    }
}
