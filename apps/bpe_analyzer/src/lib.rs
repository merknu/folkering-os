//! BPE_Analyzer — Tokenizer Visualization for Folkering OS
//!
//! Shows how the AI engine's tokenizer splits text into subword tokens.
//! Top: input text editor. Bottom: colorized token view where each token
//! gets a distinct background color. Token boundaries and IDs visible.

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
    fn folk_tokenize(text_ptr: i32, text_len: i32, out_ptr: i32, max_len: i32) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
}

const BG: i32 = 0x0D1117;
const PANEL_BG: i32 = 0x161B22;
const TEXT: i32 = 0xC9D1D9;
const TEXT_DIM: i32 = 0x484F58;
const ACCENT: i32 = 0x58A6FF;
const CURSOR: i32 = 0xF5C2E7;
const BORDER: i32 = 0x30363D;
const HEADER_H: i32 = 28;
const HELP_H: i32 = 18;
const MARGIN: i32 = 12;
const FONT_W: i32 = 8;
const FONT_H: i32 = 16;
const LINE_H: i32 = 20;
const DIVIDER_Y: i32 = 300;

const MAX_TEXT: usize = 512;
const MAX_TOKENS: usize = 256;

// 12 distinct token colors (cycle through)
const TOKEN_COLORS: [i32; 12] = [
    0x1A3A5C, 0x3A1A5C, 0x1A5C3A, 0x5C3A1A,
    0x1A5C5C, 0x5C1A5C, 0x5C5C1A, 0x2A4A6A,
    0x4A2A6A, 0x2A6A4A, 0x6A4A2A, 0x2A6A6A,
];
const TOKEN_TEXT_COLORS: [i32; 12] = [
    0x58A6FF, 0xBC8CFF, 0x3FB950, 0xD29922,
    0x79C0FF, 0xF778BA, 0xE3B341, 0x58A6FF,
    0xBC8CFF, 0x3FB950, 0xD29922, 0x79C0FF,
];

static mut DOC: [u8; MAX_TEXT] = [0u8; MAX_TEXT];
static mut DOC_LEN: usize = 0;
static mut CURSOR_POS: usize = 0;

static mut TOKEN_BUF: [u8; MAX_TOKENS * 4 + 4] = [0u8; MAX_TOKENS * 4 + 4];
static mut TOKEN_STARTS: [u16; MAX_TOKENS] = [0; MAX_TOKENS];
static mut TOKEN_LENS: [u16; MAX_TOKENS] = [0; MAX_TOKENS];
static mut TOKEN_COUNT: usize = 0;

static mut EVT: [i32; 4] = [0i32; 4];
static mut INITIALIZED: bool = false;
static mut DIRTY: bool = true;

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

unsafe fn draw(x: i32, y: i32, text: &[u8], color: i32) {
    folk_draw_text(x, y, text.as_ptr() as i32, text.len() as i32, color);
}

unsafe fn retokenize() {
    if DOC_LEN == 0 { TOKEN_COUNT = 0; return; }
    let out = core::ptr::addr_of_mut!(TOKEN_BUF) as *mut u8;
    let bytes = folk_tokenize(
        core::ptr::addr_of!(DOC) as i32, DOC_LEN as i32,
        out as i32, (MAX_TOKENS * 4 + 4) as i32);

    if bytes < 4 { TOKEN_COUNT = 0; return; }

    let b = core::slice::from_raw_parts(out, bytes as usize);
    let count = u32::from_le_bytes([b[0],b[1],b[2],b[3]]) as usize;
    TOKEN_COUNT = count.min(MAX_TOKENS);

    let starts = core::ptr::addr_of_mut!(TOKEN_STARTS) as *mut u16;
    let lens = core::ptr::addr_of_mut!(TOKEN_LENS) as *mut u16;

    for i in 0..TOKEN_COUNT {
        let off = 4 + i * 4;
        if off + 4 > bytes as usize { TOKEN_COUNT = i; break; }
        *starts.add(i) = u16::from_le_bytes([b[off], b[off+1]]);
        *lens.add(i) = u16::from_le_bytes([b[off+2], b[off+3]]);
    }
    DIRTY = false;
}

unsafe fn handle_input() {
    loop {
        let e = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(e as i32) == 0 { break; }
        if *e.add(0) != 3 { continue; }
        let key = *e.add(3) as u8;
        let d = core::ptr::addr_of_mut!(DOC) as *mut u8;

        match key {
            0x08 => { if CURSOR_POS > 0 && DOC_LEN > 0 {
                let mut i = CURSOR_POS - 1;
                while i < DOC_LEN - 1 { *d.add(i) = *d.add(i+1); i += 1; }
                DOC_LEN -= 1; CURSOR_POS -= 1; DIRTY = true;
            }}
            0x82 => { if CURSOR_POS > 0 { CURSOR_POS -= 1; } }
            0x83 => { if CURSOR_POS < DOC_LEN { CURSOR_POS += 1; } }
            0x24 => { CURSOR_POS = 0; }
            0x23 => { CURSOR_POS = DOC_LEN; }
            0x20..=0x7E => {
                if DOC_LEN < MAX_TEXT - 1 {
                    let mut i = DOC_LEN;
                    while i > CURSOR_POS { *d.add(i) = *d.add(i-1); i -= 1; }
                    *d.add(CURSOR_POS) = key;
                    DOC_LEN += 1; CURSOR_POS += 1; DIRTY = true;
                }
            }
            _ => {}
        }
    }
}

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();

    folk_fill_screen(BG);

    // Header
    folk_draw_rect(0, 0, sw, HEADER_H, PANEL_BG);
    draw(MARGIN, 6, b"BPE Analyzer", ACCENT);
    let mut tb = [0u8; 24];
    let tl = { let mut m = Msg::new(&mut tb); m.u32(TOKEN_COUNT as u32); m.s(b" tokens"); m.len() };
    draw(sw - 120, 6, &tb[..tl], TEXT_DIM);

    // Top: Raw text editor
    draw(MARGIN, HEADER_H + 4, b"Input Text:", TEXT_DIM);
    folk_draw_rect(MARGIN, HEADER_H + 22, sw - MARGIN*2, DIVIDER_Y - HEADER_H - 30, 0x0F1318);

    let d = core::ptr::addr_of!(DOC) as *const u8;
    let max_cols = ((sw - MARGIN * 2 - 8) / FONT_W) as usize;
    let mut line = 0i32;
    let mut col = 0i32;
    let ty = HEADER_H + 26;

    for i in 0..=DOC_LEN {
        if i == CURSOR_POS {
            folk_draw_rect(MARGIN + 4 + col * FONT_W, ty + line * LINE_H, 2, FONT_H, CURSOR);
        }
        if i >= DOC_LEN { break; }
        let ch = *d.add(i);
        if ch == b'\n' { line += 1; col = 0; continue; }
        if (col as usize) < max_cols {
            folk_draw_text(MARGIN + 4 + col * FONT_W, ty + line * LINE_H, d.add(i) as i32, 1, TEXT);
        }
        col += 1;
        if (col as usize) >= max_cols { col = 0; line += 1; }
    }

    // Divider
    folk_draw_line(0, DIVIDER_Y, sw, DIVIDER_Y, BORDER);
    draw(MARGIN, DIVIDER_Y + 4, b"Token View (each color = one BPE token):", TEXT_DIM);

    // Bottom: Colorized token view
    let tok_y = DIVIDER_Y + 24;
    let mut wx = MARGIN;
    let mut wy = tok_y;
    let max_x = sw - MARGIN;

    let starts = core::ptr::addr_of!(TOKEN_STARTS) as *const u16;
    let lens = core::ptr::addr_of!(TOKEN_LENS) as *const u16;

    for ti in 0..TOKEN_COUNT {
        let ts = *starts.add(ti) as usize;
        let tl = *lens.add(ti) as usize;
        if ts + tl > DOC_LEN { break; }

        let word_px = (tl as i32) * FONT_W + 4;
        let color_idx = ti % 12;

        // Wrap
        if wx + word_px > max_x && wx > MARGIN { wx = MARGIN; wy += LINE_H + 6; }
        if wy + LINE_H > sh - HELP_H { break; }

        // Token background
        folk_draw_rect(wx, wy, word_px, FONT_H + 4, TOKEN_COLORS[color_idx]);

        // Token text
        folk_draw_text(wx + 2, wy + 2, d.add(ts) as i32, tl as i32, TOKEN_TEXT_COLORS[color_idx]);

        // Token index (tiny, below)
        let mut ib = [0u8; 4];
        let il = { let mut m = Msg::new(&mut ib); m.u32(ti as u32); m.len() };
        folk_draw_text(wx + 2, wy + FONT_H + 2, ib.as_ptr() as i32, il as i32, TEXT_DIM);

        wx += word_px + 3;
    }

    // Color legend
    let leg_y = sh - HELP_H - 24;
    draw(MARGIN, leg_y, b"Each colored block = one token. Subwords split at case/length/punct boundaries.", TEXT_DIM);

    // Help
    folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
    draw(MARGIN, sh - HELP_H + 1, b"Type text to see live tokenization  |  BPE-style subword splitting", TEXT_DIM);
}

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            let def = b"Folkering OS uses a bare-metal transformer for on-device inference.";
            let d = core::ptr::addr_of_mut!(DOC) as *mut u8;
            for i in 0..def.len() { *d.add(i) = def[i]; }
            DOC_LEN = def.len(); CURSOR_POS = DOC_LEN;
            folk_log_telemetry(0, 0, 0);
            INITIALIZED = true;
            DIRTY = true;
        }
        handle_input();
        if DIRTY { retokenize(); }
        render();
    }
}
