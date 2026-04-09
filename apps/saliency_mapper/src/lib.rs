//! SaliencyMapper — Attention Visualization for Folkering OS
//!
//! Maps AI attention weights to input tokens in real-time.
//!
//! Layout:
//!   Top:    Input prompt with per-word saliency heatmap (blue→red)
//!   Middle: Divider with selected output token info
//!   Bottom: Streamed AI output via WebSocket
//!
//! When user hovers/selects an output token (arrow keys), the input
//! heatmap freezes to show which input words caused that specific token.
//!
//! Saliency data: reads attention weights from TDMP tensor mailbox
//! after inference, or simulates from token position/length heuristics.

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
    fn folk_ws_connect(url_ptr: i32, url_len: i32) -> i32;
    fn folk_ws_send(socket_id: i32, data_ptr: i32, data_len: i32) -> i32;
    fn folk_ws_poll_recv(socket_id: i32, buf_ptr: i32, max_len: i32) -> i32;
    fn folk_tensor_read(buf_ptr: i32, buf_len: i32, sector_offset: i32) -> i32;
    fn folk_slm_generate(prompt_ptr: i32, prompt_len: i32, buf_ptr: i32, max_len: i32) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
}

// ── Colors ──────────────────────────────────────────────────────────────

const BG: i32 = 0x0D1117;
const PANEL_BG: i32 = 0x161B22;
const BORDER: i32 = 0x30363D;
const TEXT: i32 = 0xC9D1D9;
const TEXT_DIM: i32 = 0x484F58;
const ACCENT: i32 = 0x58A6FF;
const CURSOR_COLOR: i32 = 0xF5C2E7;
const INPUT_BG: i32 = 0x0F1318;
const STREAM_COLOR: i32 = 0xD2A8FF;
const ERR_RED: i32 = 0xF85149;

// Layout
const HEADER_H: i32 = 28;
const PROMPT_AREA_H: i32 = 200; // top half for input prompt heatmap
const DIVIDER_H: i32 = 30;      // info bar between halves
const INPUT_H: i32 = 40;
const MARGIN: i32 = 12;
const FONT_H: i32 = 16;
const FONT_W: i32 = 8;
const LINE_H: i32 = 20;
const WORD_PAD: i32 = 4;

// Limits
const MAX_INPUT: usize = 512;
const MAX_INPUT_WORDS: usize = 64;
const MAX_OUTPUT: usize = 2048;
const MAX_OUTPUT_WORDS: usize = 128;
const MAX_BATCH: usize = 64; // max bytes to process per frame from WS

const WS_URL: &[u8] = b"ws://10.0.2.2:8080/stream";

// ── Saliency color: cold blue → hot red ─────────────────────────────────

/// Map saliency score (0.0-1.0) to background color
fn saliency_bg(score: f32) -> i32 {
    let s = if score < 0.0 { 0.0 } else if score > 1.0 { 1.0 } else { score };
    // Blue(0) → Cyan(0.25) → Green(0.5) → Yellow(0.75) → Red(1.0)
    let (r, g, b) = if s < 0.25 {
        let t = s * 4.0;
        lerp(0x08, 0x20, 0x50, 0x10, 0x40, 0x60, t) // dark blue → teal
    } else if s < 0.5 {
        let t = (s - 0.25) * 4.0;
        lerp(0x10, 0x40, 0x60, 0x20, 0x50, 0x20, t) // teal → green
    } else if s < 0.75 {
        let t = (s - 0.5) * 4.0;
        lerp(0x20, 0x50, 0x20, 0x60, 0x50, 0x10, t) // green → yellow
    } else {
        let t = (s - 0.75) * 4.0;
        lerp(0x60, 0x50, 0x10, 0x80, 0x18, 0x10, t) // yellow → red
    };
    ((r as i32) << 16) | ((g as i32) << 8) | (b as i32)
}

fn saliency_text(score: f32) -> i32 {
    if score > 0.6 { 0xFFFFFF } else { 0xC9D1D9 }
}

fn lerp(r0: u8, g0: u8, b0: u8, r1: u8, g1: u8, b1: u8, t: f32) -> (u8, u8, u8) {
    (
        (r0 as f32 + (r1 as f32 - r0 as f32) * t) as u8,
        (g0 as f32 + (g1 as f32 - g0 as f32) * t) as u8,
        (b0 as f32 + (b1 as f32 - b0 as f32) * t) as u8,
    )
}

// ── Word token ──────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct WordToken {
    start: u16,
    len: u16,
}

// ── Persistent state ────────────────────────────────────────────────────

// Prompt input
static mut PROMPT: [u8; MAX_INPUT] = [0u8; MAX_INPUT];
static mut PROMPT_LEN: usize = 0;
static mut CURSOR_POS: usize = 0;
static mut EDITING: bool = true;

// Prompt words (for saliency mapping)
static mut INPUT_WORDS: [WordToken; MAX_INPUT_WORDS] = [WordToken { start: 0, len: 0 }; MAX_INPUT_WORDS];
static mut INPUT_WORD_COUNT: usize = 0;

// Saliency scores: [output_word][input_word] attention matrix
// Flattened: saliency[out_idx * MAX_INPUT_WORDS + in_idx]
static mut SALIENCY: [f32; MAX_OUTPUT_WORDS * MAX_INPUT_WORDS] = [0.0; MAX_OUTPUT_WORDS * MAX_INPUT_WORDS];

// Output (streamed)
static mut OUTPUT: [u8; MAX_OUTPUT] = [0u8; MAX_OUTPUT];
static mut OUTPUT_LEN: usize = 0;
static mut OUTPUT_WORDS: [WordToken; MAX_OUTPUT_WORDS] = [WordToken { start: 0, len: 0 }; MAX_OUTPUT_WORDS];
static mut OUTPUT_WORD_COUNT: usize = 0;

// Selection
static mut SELECTED_OUT_WORD: usize = 0;
static mut INSPECTING: bool = false; // true = arrow keys select output words

// WebSocket streaming
static mut WS_SOCKET: i32 = -1;
static mut STREAMING: bool = false;
static mut STREAM_START_MS: i32 = 0;
static mut WS_RECV_BUF: [u8; 2048] = [0u8; 2048];

// TDMP tensor buffer
static mut TDMP_HDR: [u8; 512] = [0u8; 512];

// General
static mut EVT: [i32; 4] = [0i32; 4];
static mut INITIALIZED: bool = false;
static mut AI_BUF: [u8; 1024] = [0u8; 1024];

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
    fn pct(&mut self, v: f32) { self.u32(((v * 100.0) as u32).min(100)); self.s(b"%"); }
    fn len(&self) -> usize { self.pos }
}

unsafe fn draw(x: i32, y: i32, text: &[u8], color: i32) {
    folk_draw_text(x, y, text.as_ptr() as i32, text.len() as i32, color);
}

/// Tokenize a byte buffer into space-separated words
fn tokenize_words(text: &[u8], len: usize, words: &mut [WordToken]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < len && count < words.len() {
        while i < len && (text[i] == b' ' || text[i] == b'\n' || text[i] == b'\t') { i += 1; }
        if i >= len { break; }
        let start = i;
        while i < len && text[i] != b' ' && text[i] != b'\n' && text[i] != b'\t' { i += 1; }
        if i > start {
            words[count] = WordToken { start: start as u16, len: (i - start) as u16 };
            count += 1;
        }
    }
    count
}

// ── Saliency computation ────────────────────────────────────────────────

/// Generate saliency scores for output word `out_idx` against all input words.
/// Uses TDMP attention data if available, otherwise heuristic.
unsafe fn compute_saliency_for_output(out_idx: usize) {
    if out_idx >= OUTPUT_WORD_COUNT || INPUT_WORD_COUNT == 0 { return; }

    let sal = core::ptr::addr_of_mut!(SALIENCY) as *mut f32;
    let base = out_idx * MAX_INPUT_WORDS;

    // Try to read TDMP for real attention data
    let hdr_ptr = core::ptr::addr_of_mut!(TDMP_HDR) as *mut u8;
    let hdr_bytes = folk_tensor_read(hdr_ptr as i32, 512, 0);

    let has_tdmp = hdr_bytes >= 512
        && *hdr_ptr.add(0) == b'T' && *hdr_ptr.add(1) == b'D'
        && *hdr_ptr.add(2) == b'M' && *hdr_ptr.add(3) == b'P';

    if has_tdmp {
        // Use summary floats from TDMP header (offset 112, up to 100 values)
        // Map these as attention weights across input words
        let n_summary = INPUT_WORD_COUNT.min(100);
        let mut max_val: f32 = 0.0;
        for j in 0..n_summary {
            let off = 112 + j * 4;
            if off + 4 <= 512 {
                let v = f32::from_le_bytes([
                    *hdr_ptr.add(off), *hdr_ptr.add(off+1),
                    *hdr_ptr.add(off+2), *hdr_ptr.add(off+3),
                ]);
                let abs_v = if v < 0.0 { -v } else { v };
                *sal.add(base + j) = abs_v;
                if abs_v > max_val { max_val = abs_v; }
            }
        }
        // Normalize to 0-1
        if max_val > 0.0 {
            for j in 0..n_summary {
                *sal.add(base + j) /= max_val;
            }
        }
    } else {
        // Heuristic saliency: position + word overlap based
        let out_word = &OUTPUT_WORDS[out_idx];
        let out_text = &OUTPUT[out_word.start as usize..(out_word.start + out_word.len) as usize];

        let prompt = core::ptr::addr_of!(PROMPT) as *const u8;

        for j in 0..INPUT_WORD_COUNT {
            let in_word = &INPUT_WORDS[j];
            let in_text = core::slice::from_raw_parts(
                prompt.add(in_word.start as usize), in_word.len as usize);

            // Score: partial string overlap + position proximity + length similarity
            let mut score: f32 = 0.05; // base attention

            // Substring overlap check (case-insensitive)
            let overlap = byte_overlap(in_text, out_text);
            score += overlap * 0.6;

            // Position: nearby words get more attention (local context)
            let pos_dist = if out_idx > j { out_idx - j } else { j - out_idx };
            let pos_factor = 1.0 / (1.0 + pos_dist as f32 * 0.15);
            score += pos_factor * 0.2;

            // Short common words (the, is, a) get less attention
            if in_word.len <= 3 { score *= 0.5; }

            *sal.add(base + j) = score.min(1.0);
        }

        // Normalize so max = 1.0
        let mut max_s: f32 = 0.0;
        for j in 0..INPUT_WORD_COUNT {
            let v = *sal.add(base + j);
            if v > max_s { max_s = v; }
        }
        if max_s > 0.0 {
            for j in 0..INPUT_WORD_COUNT {
                *sal.add(base + j) /= max_s;
            }
        }
    }
}

/// Compute byte-level overlap score between two words (0.0-1.0)
fn byte_overlap(a: &[u8], b: &[u8]) -> f32 {
    if a.is_empty() || b.is_empty() { return 0.0; }
    let shorter = if a.len() < b.len() { a } else { b };
    let longer = if a.len() < b.len() { b } else { a };

    let mut matches = 0u32;
    for i in 0..shorter.len() {
        let ca = if shorter[i] >= b'A' && shorter[i] <= b'Z' { shorter[i] + 32 } else { shorter[i] };
        for j in 0..longer.len() {
            let cb = if longer[j] >= b'A' && longer[j] <= b'Z' { longer[j] + 32 } else { longer[j] };
            if ca == cb { matches += 1; break; }
        }
    }
    matches as f32 / shorter.len() as f32
}

// ── Run inference ───────────────────────────────────────────────────────

unsafe fn start_inference() {
    // Tokenize input prompt
    let prompt = core::slice::from_raw_parts(core::ptr::addr_of!(PROMPT) as *const u8, PROMPT_LEN);
    INPUT_WORD_COUNT = tokenize_words(prompt, PROMPT_LEN,
        &mut *core::ptr::addr_of_mut!(INPUT_WORDS));

    // Reset output
    OUTPUT_LEN = 0;
    OUTPUT_WORD_COUNT = 0;
    SELECTED_OUT_WORD = 0;
    INSPECTING = false;

    // Try WebSocket first
    if WS_SOCKET < 0 {
        WS_SOCKET = folk_ws_connect(WS_URL.as_ptr() as i32, WS_URL.len() as i32);
    }

    if WS_SOCKET >= 0 {
        if folk_ws_send(WS_SOCKET, core::ptr::addr_of!(PROMPT) as i32, PROMPT_LEN as i32) == 0 {
            STREAMING = true;
            STREAM_START_MS = folk_get_time();
            EDITING = false;
            folk_log_telemetry(4, PROMPT_LEN as i32, 0); // AiInferenceRequested
            return;
        }
        WS_SOCKET = -1;
    }

    // Fallback: blocking generate
    let ai_ptr = core::ptr::addr_of_mut!(AI_BUF) as *mut u8;
    let resp = folk_slm_generate(
        core::ptr::addr_of!(PROMPT) as i32, PROMPT_LEN as i32,
        ai_ptr as i32, 1024);

    if resp > 0 {
        let out = core::ptr::addr_of_mut!(OUTPUT) as *mut u8;
        let n = (resp as usize).min(MAX_OUTPUT);
        core::ptr::copy_nonoverlapping(ai_ptr, out, n);
        OUTPUT_LEN = n;
        retokenize_output();
        // Compute saliency for all output words
        for i in 0..OUTPUT_WORD_COUNT { compute_saliency_for_output(i); }
    }
    EDITING = false;
}

unsafe fn retokenize_output() {
    let out = core::slice::from_raw_parts(core::ptr::addr_of!(OUTPUT) as *const u8, OUTPUT_LEN);
    OUTPUT_WORD_COUNT = tokenize_words(out, OUTPUT_LEN,
        &mut *core::ptr::addr_of_mut!(OUTPUT_WORDS));
}

/// Poll WS for streaming tokens
unsafe fn poll_stream() {
    if !STREAMING || WS_SOCKET < 0 { return; }

    let recv_ptr = core::ptr::addr_of_mut!(WS_RECV_BUF) as *mut u8;
    let n = folk_ws_poll_recv(WS_SOCKET, recv_ptr as i32, MAX_BATCH as i32);

    if n > 0 {
        let data = core::slice::from_raw_parts(recv_ptr, n as usize);
        let out = core::ptr::addr_of_mut!(OUTPUT) as *mut u8;
        let copy = (n as usize).min(MAX_OUTPUT - OUTPUT_LEN);
        for i in 0..copy {
            *out.add(OUTPUT_LEN + i) = data[i];
        }
        OUTPUT_LEN += copy;
        retokenize_output();

        // Compute saliency for newly arrived words
        if OUTPUT_WORD_COUNT > 0 {
            let last = OUTPUT_WORD_COUNT - 1;
            compute_saliency_for_output(last);
        }

        STREAM_START_MS = folk_get_time();
    } else if n < 0 {
        // Connection closed
        STREAMING = false;
        WS_SOCKET = -1;
        // Compute remaining saliency
        for i in 0..OUTPUT_WORD_COUNT { compute_saliency_for_output(i); }
    } else {
        if folk_get_time() - STREAM_START_MS > 30000 {
            STREAMING = false;
            WS_SOCKET = -1;
        }
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
        match key {
            0xB5 => { // F5 — Run
                if EDITING && PROMPT_LEN > 0 && !STREAMING {
                    start_inference();
                }
            }
            0x1B => { // Esc — back to edit
                EDITING = true;
                INSPECTING = false;
                STREAMING = false;
            }
            0x09 => { // Tab — toggle inspect mode
                if !EDITING && OUTPUT_WORD_COUNT > 0 {
                    INSPECTING = !INSPECTING;
                }
            }
            _ => {
                if EDITING {
                    handle_editor_key(key);
                } else if INSPECTING {
                    match key {
                        0x82 => { if SELECTED_OUT_WORD > 0 { SELECTED_OUT_WORD -= 1; } }
                        0x83 => { if SELECTED_OUT_WORD + 1 < OUTPUT_WORD_COUNT { SELECTED_OUT_WORD += 1; } }
                        _ => {}
                    }
                }
            }
        }
    }
}

unsafe fn handle_editor_key(key: u8) {
    let p = core::ptr::addr_of_mut!(PROMPT) as *mut u8;
    match key {
        0x08 => { if CURSOR_POS > 0 && PROMPT_LEN > 0 {
            let mut i = CURSOR_POS - 1;
            while i < PROMPT_LEN - 1 { *p.add(i) = *p.add(i+1); i += 1; }
            PROMPT_LEN -= 1; CURSOR_POS -= 1;
        }}
        0x82 => { if CURSOR_POS > 0 { CURSOR_POS -= 1; } }
        0x83 => { if CURSOR_POS < PROMPT_LEN { CURSOR_POS += 1; } }
        0x24 => { CURSOR_POS = 0; }
        0x23 => { CURSOR_POS = PROMPT_LEN; }
        0x20..=0x7E => {
            if PROMPT_LEN < MAX_INPUT - 1 {
                let mut i = PROMPT_LEN;
                while i > CURSOR_POS { *p.add(i) = *p.add(i-1); i -= 1; }
                *p.add(CURSOR_POS) = key;
                PROMPT_LEN += 1; CURSOR_POS += 1;
            }
        }
        _ => {}
    }
}

// ── Rendering ───────────────────────────────────────────────────────────

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();
    let output_y = HEADER_H + PROMPT_AREA_H + DIVIDER_H;
    let output_h = sh - output_y - INPUT_H;

    folk_fill_screen(BG);

    // ── Header ──
    folk_draw_rect(0, 0, sw, HEADER_H, PANEL_BG);
    draw(MARGIN, 6, b"SaliencyMapper", ACCENT);

    if STREAMING {
        let dots = ((folk_get_time() / 300) % 4) as usize;
        let ind = [b"Streaming." as &[u8], b"Streaming..", b"Streaming...", b"Streaming"];
        draw(140, 6, ind[dots], STREAM_COLOR);
    } else if INSPECTING {
        draw(140, 6, b"INSPECT MODE", 0xD29922);
    } else if EDITING {
        draw(140, 6, b"EDIT", 0x3FB950);
    } else {
        draw(140, 6, b"VIEW", TEXT_DIM);
    }

    // Token counts
    let mut cb = [0u8; 32];
    let cl = {
        let mut m = Msg::new(&mut cb);
        m.s(b"In:"); m.u32(INPUT_WORD_COUNT as u32);
        m.s(b" Out:"); m.u32(OUTPUT_WORD_COUNT as u32);
        m.len()
    };
    draw(sw - 150, 6, &cb[..cl], TEXT_DIM);

    // ── Top: Input prompt with saliency heatmap ──
    folk_draw_rect(0, HEADER_H, sw, PROMPT_AREA_H, 0x0A0E15);
    draw(MARGIN, HEADER_H + 4, b"Input Prompt (saliency heatmap):", TEXT_DIM);

    let prompt = core::ptr::addr_of!(PROMPT) as *const u8;
    let sal = core::ptr::addr_of!(SALIENCY) as *const f32;

    // Which output word drives the heatmap?
    let focus_word = if INSPECTING || !STREAMING { SELECTED_OUT_WORD } else {
        if OUTPUT_WORD_COUNT > 0 { OUTPUT_WORD_COUNT - 1 } else { 0 }
    };

    if EDITING {
        // Just show the prompt text with cursor (no saliency yet)
        let text_y = HEADER_H + 24;
        let max_cols = ((sw - MARGIN * 2) / FONT_W) as usize;
        let mut col = 0i32;
        let mut line = 0i32;
        for i in 0..=PROMPT_LEN {
            if i == CURSOR_POS {
                let cx = MARGIN + col * FONT_W;
                let cy = text_y + line * LINE_H;
                folk_draw_rect(cx, cy, 2, FONT_H, CURSOR_COLOR);
            }
            if i >= PROMPT_LEN { break; }
            let ch = *prompt.add(i);
            if ch == b'\n' { line += 1; col = 0; continue; }
            if (col as usize) < max_cols {
                folk_draw_text(MARGIN + col * FONT_W, text_y + line * LINE_H,
                    prompt.add(i) as i32, 1, TEXT);
            }
            col += 1;
            if (col as usize) >= max_cols { col = 0; line += 1; }
        }
    } else if INPUT_WORD_COUNT > 0 {
        // Show words with saliency background colors
        let text_y = HEADER_H + 24;
        let mut wx = MARGIN;
        let mut wy = text_y;
        let max_x = sw - MARGIN;

        for j in 0..INPUT_WORD_COUNT {
            let w = &INPUT_WORDS[j];
            let word_px = (w.len as i32) * FONT_W + WORD_PAD * 2;

            // Wrap
            if wx + word_px > max_x { wx = MARGIN; wy += LINE_H + 4; }
            if wy + LINE_H > HEADER_H + PROMPT_AREA_H { break; }

            // Saliency score for this input word given the focused output word
            let score = if focus_word < OUTPUT_WORD_COUNT {
                *sal.add(focus_word * MAX_INPUT_WORDS + j)
            } else { 0.05 };

            // Draw background
            folk_draw_rect(wx, wy - 2, word_px, FONT_H + 4, saliency_bg(score));

            // Draw word text
            folk_draw_text(wx + WORD_PAD, wy,
                prompt.add(w.start as usize) as i32, w.len as i32,
                saliency_text(score));

            wx += word_px + 4;
        }
    }

    // ── Divider with info ──
    let div_y = HEADER_H + PROMPT_AREA_H;
    folk_draw_rect(0, div_y, sw, DIVIDER_H, PANEL_BG);
    folk_draw_line(0, div_y, sw, div_y, BORDER);
    folk_draw_line(0, div_y + DIVIDER_H, sw, div_y + DIVIDER_H, BORDER);

    if INSPECTING && SELECTED_OUT_WORD < OUTPUT_WORD_COUNT {
        let w = &OUTPUT_WORDS[SELECTED_OUT_WORD];
        let out = core::ptr::addr_of!(OUTPUT) as *const u8;
        let mut ib = [0u8; 64];
        let il = {
            let mut m = Msg::new(&mut ib);
            m.s(b"Selected ["); m.u32((SELECTED_OUT_WORD + 1) as u32);
            m.s(b"/"); m.u32(OUTPUT_WORD_COUNT as u32); m.s(b"]: \"");
            let ws = core::slice::from_raw_parts(out.add(w.start as usize), (w.len as usize).min(20));
            m.s(ws);
            m.s(b"\"  </>: navigate");
            m.len()
        };
        draw(MARGIN, div_y + 7, &ib[..il], ACCENT);
    } else {
        draw(MARGIN, div_y + 7, b"Generated Output  [Tab] Inspect  [F5] Run  [Esc] Edit", TEXT_DIM);
    }

    // ── Bottom: Output stream ──
    folk_draw_rect(0, output_y, sw, output_h, 0x0A0E15);

    if OUTPUT_LEN == 0 && !STREAMING {
        draw(MARGIN + 20, output_y + 40, b"Press F5 to run inference", TEXT_DIM);
    } else {
        let out = core::ptr::addr_of!(OUTPUT) as *const u8;
        let mut wx = MARGIN;
        let mut wy = output_y + 8;
        let max_x = sw - MARGIN;

        for i in 0..OUTPUT_WORD_COUNT {
            let w = &OUTPUT_WORDS[i];
            let word_px = (w.len as i32) * FONT_W + 2;

            if wx + word_px > max_x { wx = MARGIN; wy += LINE_H; }
            if wy + LINE_H > sh - INPUT_H { break; }

            let selected = INSPECTING && i == SELECTED_OUT_WORD;

            if selected {
                folk_draw_rect(wx - 1, wy - 2, word_px + 2, FONT_H + 4, 0x213352);
                folk_draw_rect(wx - 1, wy + FONT_H + 1, word_px + 2, 2, CURSOR_COLOR);
            }

            let color = if selected { 0xFFFFFF } else { STREAM_COLOR };
            folk_draw_text(wx + 1, wy, out.add(w.start as usize) as i32, w.len as i32, color);

            wx += word_px + 4;
        }

        // Streaming cursor
        if STREAMING {
            let blink = (folk_get_time() / 200) % 2;
            if blink == 0 {
                folk_draw_rect(wx, wy, FONT_W, FONT_H, STREAM_COLOR);
            }
        }
    }

    // ── Color legend (bottom-right) ──
    let leg_x = sw - 200;
    let leg_y = output_y + output_h - 20;
    draw(leg_x, leg_y, b"Low", TEXT_DIM);
    for i in 0..12 {
        let t = i as f32 / 11.0;
        folk_draw_rect(leg_x + 30 + i * 12, leg_y - 1, 12, FONT_H, saliency_bg(t));
    }
    draw(leg_x + 30 + 12 * 12 + 4, leg_y, b"High", TEXT_DIM);

    // ── Help bar ──
    folk_draw_rect(0, sh - INPUT_H + 20, sw, 18, PANEL_BG);
    draw(MARGIN, sh - INPUT_H + 22,
        b"[F5] Run  [Tab] Inspect  [</>] Select token  [Esc] Edit",
        TEXT_DIM);
}

// ── Main ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            let default = b"Explain how a transformer model uses self-attention to process tokens.";
            let p = core::ptr::addr_of_mut!(PROMPT) as *mut u8;
            for i in 0..default.len() { *p.add(i) = default[i]; }
            PROMPT_LEN = default.len();
            CURSOR_POS = PROMPT_LEN;
            folk_log_telemetry(0, 0, 0); // AppOpened
            INITIALIZED = true;
        }

        handle_input();
        poll_stream();
        render();
    }
}
