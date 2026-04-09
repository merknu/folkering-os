//! WasmForge — WASM Assembler IDE for Folkering OS
//!
//! AutoDream Phase 2 bridge: takes FolkScript source, assembles to
//! raw WASM binary, and shadow-tests it via folk_shadow_test().
//!
//! FolkScript is a minimal instruction set:
//!   fill <color>        → folk_fill_screen(color)
//!   rect <x> <y> <w> <h> <color>  → folk_draw_rect(...)
//!   text <x> <y> "msg" <color>    → folk_draw_text(...)
//!   nop                 → no operation
//!
//! The assembler emits valid WASM binary with imported folk_* functions,
//! a run() export, and the instruction body compiled from FolkScript.
//!
//! F5 = assemble + shadow test. Results shown on right panel.

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
    fn folk_shadow_test(wasm_ptr: i32, wasm_len: i32, result_ptr: i32, max_len: i32) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
}

const BG: i32 = 0x0D1117;
const PANEL_BG: i32 = 0x161B22;
const BORDER: i32 = 0x30363D;
const TEXT: i32 = 0xC9D1D9;
const TEXT_DIM: i32 = 0x484F58;
const ACCENT: i32 = 0x58A6FF;
const OK: i32 = 0x3FB950;
const ERR: i32 = 0xF85149;
const WARN: i32 = 0xD29922;
const CURSOR: i32 = 0xF5C2E7;
const HEADER_H: i32 = 28;
const HELP_H: i32 = 18;
const MARGIN: i32 = 8;
const FONT_W: i32 = 8;
const FONT_H: i32 = 16;
const LINE_H: i32 = 18;
const DIVIDER_X: i32 = 560;

const MAX_SRC: usize = 2048;
const MAX_WASM: usize = 4096;
const MAX_RESULT: usize = 512;
const MAX_ERR_MSG: usize = 80;

// ── State ───────────────────────────────────────────────────────────────

static mut SRC: [u8; MAX_SRC] = [0u8; MAX_SRC];
static mut SRC_LEN: usize = 0;
static mut CURSOR_POS: usize = 0;

static mut WASM_OUT: [u8; MAX_WASM] = [0u8; MAX_WASM];
static mut WASM_LEN: usize = 0;

static mut RESULT_BUF: [u8; MAX_RESULT] = [0u8; MAX_RESULT];
static mut RESULT_LEN: usize = 0;

static mut BUILD_OK: bool = false;
static mut BUILD_MSG: [u8; MAX_ERR_MSG] = [0u8; MAX_ERR_MSG];
static mut BUILD_MSG_LEN: usize = 0;
static mut BUILD_COUNT: u32 = 0;

static mut EVT: [i32; 4] = [0i32; 4];
static mut INITIALIZED: bool = false;

// ── WASM Binary Emitter ─────────────────────────────────────────────────

struct WasmEmitter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> WasmEmitter<'a> {
    fn new(buf: &'a mut [u8]) -> Self { Self { buf, pos: 0 } }

    fn emit(&mut self, b: u8) {
        if self.pos < self.buf.len() { self.buf[self.pos] = b; self.pos += 1; }
    }
    fn emit_bytes(&mut self, bs: &[u8]) { for &b in bs { self.emit(b); } }

    fn emit_leb128_u32(&mut self, mut val: u32) {
        loop {
            let mut byte = (val & 0x7F) as u8;
            val >>= 7;
            if val != 0 { byte |= 0x80; }
            self.emit(byte);
            if val == 0 { break; }
        }
    }

    fn emit_leb128_i32(&mut self, val: i32) {
        let mut val = val;
        loop {
            let mut byte = (val & 0x7F) as u8;
            val >>= 7;
            let done = (val == 0 && byte & 0x40 == 0) || (val == -1 && byte & 0x40 != 0);
            if !done { byte |= 0x80; }
            self.emit(byte);
            if done { break; }
        }
    }

    // WASM instruction helpers
    fn i32_const(&mut self, val: i32) { self.emit(0x41); self.emit_leb128_i32(val); }
    fn call(&mut self, func_idx: u32) { self.emit(0x10); self.emit_leb128_u32(func_idx); }
    fn end(&mut self) { self.emit(0x0B); }

    fn len(&self) -> usize { self.pos }
}

/// Emit a WASM section: [id] [size as LEB128] [content]
/// Note: content must be a separate buffer (not borrowing from out).
fn emit_section_to(buf: &mut [u8], pos: &mut usize, id: u8, content: &[u8]) {
    if *pos < buf.len() { buf[*pos] = id; *pos += 1; }
    // LEB128 encode length
    let mut len = content.len() as u32;
    loop {
        let mut byte = (len & 0x7F) as u8;
        len >>= 7;
        if len != 0 { byte |= 0x80; }
        if *pos < buf.len() { buf[*pos] = byte; *pos += 1; }
        if len == 0 { break; }
    }
    for &b in content {
        if *pos < buf.len() { buf[*pos] = b; *pos += 1; }
    }
}

// ── FolkScript Assembler ────────────────────────────────────────────────

/// Imported function indices:
/// 0 = folk_fill_screen(color: i32)
/// 1 = folk_draw_rect(x,y,w,h,color: i32×5)
/// 2 = folk_draw_text(x,y,ptr,len,color: i32×5)
const IMPORT_FILL: u32 = 0;
const IMPORT_RECT: u32 = 1;
const IMPORT_TEXT: u32 = 2;

/// Assemble FolkScript source into WASM binary.
/// Returns (wasm_bytes_len, error_message) — 0 len on error.
unsafe fn assemble(src: &[u8], wasm_buf: &mut [u8]) -> (usize, &'static [u8]) {
    // First: build the function body from FolkScript instructions
    let mut body = [0u8; 2048];
    let mut body_len = 0usize;

    // Parse lines
    let mut line_start = 0;
    let mut line_num = 0u32;

    while line_start < src.len() {
        // Find end of line
        let mut line_end = line_start;
        while line_end < src.len() && src[line_end] != b'\n' { line_end += 1; }
        let line = &src[line_start..line_end];
        let trimmed = trim(line);
        line_start = line_end + 1;
        line_num += 1;

        if trimmed.is_empty() || trimmed[0] == b'#' || trimmed[0] == b';' {
            continue; // Skip empty lines and comments
        }

        // Parse instruction
        let (cmd, rest) = split_first_word(trimmed);

        let mut be = WasmEmitter::new(&mut body[body_len..]);

        match cmd {
            b"fill" => {
                // fill <color>
                let color = parse_int(rest);
                be.i32_const(color);
                be.call(IMPORT_FILL);
            }
            b"rect" => {
                // rect <x> <y> <w> <h> <color>
                let args = parse_ints(rest, 5);
                for i in 0..5 { be.i32_const(args[i]); }
                be.call(IMPORT_RECT);
            }
            b"nop" => {
                be.emit(0x01); // WASM nop
            }
            _ => {
                return (0, b"Unknown instruction");
            }
        }

        body_len += be.len();
    }

    if body_len == 0 {
        return (0, b"Empty program");
    }

    // Build complete WASM module
    let mut out = WasmEmitter::new(wasm_buf);

    // Magic + version
    out.emit_bytes(&[0x00, 0x61, 0x73, 0x6D]); // \0asm
    out.emit_bytes(&[0x01, 0x00, 0x00, 0x00]); // version 1

    // Type section (section 1): function types
    // Type 0: (i32) -> ()           [fill_screen]
    // Type 1: (i32,i32,i32,i32,i32) -> ()  [draw_rect, draw_text]
    // Type 2: () -> ()              [run]
    {
        let mut ts = [0u8; 32];
        let mut t = WasmEmitter::new(&mut ts);
        t.emit_leb128_u32(3); // 3 types
        // Type 0: (i32) -> ()
        t.emit(0x60); t.emit_leb128_u32(1); t.emit(0x7F); t.emit_leb128_u32(0);
        // Type 1: (i32×5) -> ()
        t.emit(0x60); t.emit_leb128_u32(5);
        for _ in 0..5 { t.emit(0x7F); }
        t.emit_leb128_u32(0);
        // Type 2: () -> ()
        t.emit(0x60); t.emit_leb128_u32(0); t.emit_leb128_u32(0);
        let tl = t.len();
        emit_section_to(out.buf, &mut out.pos, 1, &ts[..tl]);
    }

    // Import section (section 2): folk_fill_screen, folk_draw_rect, folk_draw_text
    {
        let mut is = [0u8; 128];
        let mut im = WasmEmitter::new(&mut is);
        im.emit_leb128_u32(3); // 3 imports
        // Import 0: env.folk_fill_screen (type 0)
        im.emit_leb128_u32(3); im.emit_bytes(b"env");
        im.emit_leb128_u32(16); im.emit_bytes(b"folk_fill_screen");
        im.emit(0x00); im.emit_leb128_u32(0);
        // Import 1: env.folk_draw_rect (type 1)
        im.emit_leb128_u32(3); im.emit_bytes(b"env");
        im.emit_leb128_u32(14); im.emit_bytes(b"folk_draw_rect");
        im.emit(0x00); im.emit_leb128_u32(1);
        // Import 2: env.folk_draw_text (type 1)
        im.emit_leb128_u32(3); im.emit_bytes(b"env");
        im.emit_leb128_u32(14); im.emit_bytes(b"folk_draw_text");
        im.emit(0x00); im.emit_leb128_u32(1);
        let il = im.len();
        emit_section_to(out.buf, &mut out.pos, 2, &is[..il]);
    }

    // Function section (section 3): 1 function (run) of type 2
    {
        let fs = [1u8, 2]; // count=1, type_idx=2
        emit_section_to(out.buf, &mut out.pos, 3, &fs);
    }

    // Memory section (section 5): 1 memory, min 1 page
    {
        let ms = [1u8, 0x00, 1]; // count=1, limits(min=1)
        emit_section_to(out.buf, &mut out.pos, 5, &ms);
    }

    // Export section (section 7): export "run" as function 3 (idx=3, after 3 imports)
    {
        let mut es = [0u8; 16];
        let mut e = WasmEmitter::new(&mut es);
        e.emit_leb128_u32(2); // 2 exports
        // Export "run" → func 3
        e.emit_leb128_u32(3); e.emit_bytes(b"run");
        e.emit(0x00); e.emit_leb128_u32(3);
        // Export "memory" → memory 0
        e.emit_leb128_u32(6); e.emit_bytes(b"memory");
        e.emit(0x02); e.emit_leb128_u32(0);
        let el = e.len();
        emit_section_to(out.buf, &mut out.pos, 7, &es[..el]);
    }

    // Code section (section 10): 1 function body
    {
        // Function body: [local_count] [body bytes] [end]
        let mut fb = [0u8; 2100];
        let mut f = WasmEmitter::new(&mut fb);
        // Body size = 1 (local count) + body_len + 1 (end)
        let func_body_size = 1 + body_len + 1;
        f.emit_leb128_u32(1); // 1 function
        f.emit_leb128_u32(func_body_size as u32);
        f.emit_leb128_u32(0); // 0 locals
        f.emit_bytes(&body[..body_len]);
        f.end(); // function end
        let fl = f.len();
        emit_section_to(out.buf, &mut out.pos, 10, &fb[..fl]);
    }

    (out.len(), b"")
}

// ── Parsing helpers ─────────────────────────────────────────────────────

fn trim(s: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = s.len();
    while start < end && (s[start] == b' ' || s[start] == b'\t') { start += 1; }
    while end > start && (s[end-1] == b' ' || s[end-1] == b'\t' || s[end-1] == b'\r') { end -= 1; }
    &s[start..end]
}

fn split_first_word(s: &[u8]) -> (&[u8], &[u8]) {
    let mut i = 0;
    while i < s.len() && s[i] != b' ' && s[i] != b'\t' { i += 1; }
    let cmd = &s[..i];
    while i < s.len() && (s[i] == b' ' || s[i] == b'\t') { i += 1; }
    (cmd, &s[i..])
}

fn parse_int(s: &[u8]) -> i32 {
    let s = trim(s);
    if s.is_empty() { return 0; }

    // Handle 0x prefix
    if s.len() > 2 && s[0] == b'0' && (s[1] == b'x' || s[1] == b'X') {
        let mut val = 0i32;
        for &b in &s[2..] {
            if b == b' ' || b == b'\t' { break; }
            let digit = match b {
                b'0'..=b'9' => (b - b'0') as i32,
                b'a'..=b'f' => (b - b'a' + 10) as i32,
                b'A'..=b'F' => (b - b'A' + 10) as i32,
                _ => break,
            };
            val = (val << 4) | digit;
        }
        return val;
    }

    // Handle negative
    let (neg, start) = if s[0] == b'-' { (true, 1) } else { (false, 0) };
    let mut val = 0i32;
    for &b in &s[start..] {
        if b < b'0' || b > b'9' { break; }
        val = val * 10 + (b - b'0') as i32;
    }
    if neg { -val } else { val }
}

fn parse_ints(s: &[u8], max: usize) -> [i32; 5] {
    let mut result = [0i32; 5];
    let mut idx = 0;
    let mut i = 0;
    while i < s.len() && idx < max {
        while i < s.len() && (s[i] == b' ' || s[i] == b'\t') { i += 1; }
        if i >= s.len() { break; }
        let start = i;
        while i < s.len() && s[i] != b' ' && s[i] != b'\t' { i += 1; }
        result[idx] = parse_int(&s[start..i]);
        idx += 1;
    }
    result
}

// ── Build + Shadow Test ─────────────────────────────────────────────────

unsafe fn build_and_test() {
    let src = core::slice::from_raw_parts(core::ptr::addr_of!(SRC) as *const u8, SRC_LEN);
    let wasm = core::ptr::addr_of_mut!(WASM_OUT) as *mut u8;
    let wasm_slice = core::slice::from_raw_parts_mut(wasm, MAX_WASM);

    let (wasm_len, err) = assemble(src, wasm_slice);
    BUILD_COUNT += 1;

    if wasm_len == 0 {
        BUILD_OK = false;
        let msg = core::ptr::addr_of_mut!(BUILD_MSG) as *mut u8;
        BUILD_MSG_LEN = err.len().min(MAX_ERR_MSG);
        for i in 0..BUILD_MSG_LEN { *msg.add(i) = err[i]; }
        WASM_LEN = 0;
        RESULT_LEN = 0;
        return;
    }

    WASM_LEN = wasm_len;
    BUILD_OK = true;

    let msg = core::ptr::addr_of_mut!(BUILD_MSG) as *mut u8;
    let ok_msg = b"Assembly OK";
    BUILD_MSG_LEN = ok_msg.len();
    for i in 0..BUILD_MSG_LEN { *msg.add(i) = ok_msg[i]; }

    // Shadow test
    let result_ptr = core::ptr::addr_of_mut!(RESULT_BUF) as *mut u8;
    let result_bytes = folk_shadow_test(
        wasm as i32, wasm_len as i32,
        result_ptr as i32, MAX_RESULT as i32);

    RESULT_LEN = if result_bytes > 0 { result_bytes as usize } else { 0 };
    folk_log_telemetry(3, wasm_len as i32, 0); // UiInteraction
}

// ── Input ───────────────────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let e = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(e as i32) == 0 { break; }
        if *e.add(0) != 3 { continue; }
        let key = *e.add(3) as u8;
        let d = core::ptr::addr_of_mut!(SRC) as *mut u8;

        match key {
            0x74 => { if SRC_LEN > 0 { build_and_test(); } } // F5
            0x08 => { if CURSOR_POS > 0 && SRC_LEN > 0 {
                let mut i = CURSOR_POS - 1;
                while i < SRC_LEN - 1 { *d.add(i) = *d.add(i+1); i += 1; }
                SRC_LEN -= 1; CURSOR_POS -= 1;
            }}
            0x25 => { if CURSOR_POS > 0 { CURSOR_POS -= 1; } }
            0x27 => { if CURSOR_POS < SRC_LEN { CURSOR_POS += 1; } }
            0x24 => { CURSOR_POS = 0; }
            0x23 => { CURSOR_POS = SRC_LEN; }
            0x0D => { // Enter
                if SRC_LEN < MAX_SRC - 1 {
                    let mut i = SRC_LEN;
                    while i > CURSOR_POS { *d.add(i) = *d.add(i-1); i -= 1; }
                    *d.add(CURSOR_POS) = b'\n';
                    SRC_LEN += 1; CURSOR_POS += 1;
                }
            }
            0x20..=0x7E => {
                if SRC_LEN < MAX_SRC - 1 {
                    let mut i = SRC_LEN;
                    while i > CURSOR_POS { *d.add(i) = *d.add(i-1); i -= 1; }
                    *d.add(CURSOR_POS) = key;
                    SRC_LEN += 1; CURSOR_POS += 1;
                }
            }
            _ => {}
        }
    }
}

// ── Rendering ───────────────────────────────────────────────────────────

struct MsgBuf<'a> { buf: &'a mut [u8], pos: usize }
impl<'a> MsgBuf<'a> {
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

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();

    folk_fill_screen(BG);

    // Header
    folk_draw_rect(0, 0, sw, HEADER_H, PANEL_BG);
    draw(MARGIN, 6, b"WasmForge", ACCENT);
    draw(100, 6, b"FolkScript Assembler + Shadow Tester", TEXT_DIM);

    let mut bb = [0u8; 16];
    let bl = { let mut m = MsgBuf::new(&mut bb); m.s(b"#"); m.u32(BUILD_COUNT); m.len() };
    draw(sw - 60, 6, &bb[..bl], TEXT_DIM);

    // Divider
    folk_draw_line(DIVIDER_X, HEADER_H, DIVIDER_X, sh - HELP_H, BORDER);

    // ── Left: Code Editor ──
    let ex = MARGIN;
    let ey = HEADER_H + 4;
    draw(ex, ey, b"FolkScript Editor:", TEXT_DIM);
    draw(DIVIDER_X - 80, ey, b"[F5] Build", OK);

    let text_y = ey + 18;
    folk_draw_rect(ex, text_y, DIVIDER_X - MARGIN * 2, sh - text_y - HELP_H - 4, 0x0F1318);

    let d = core::ptr::addr_of!(SRC) as *const u8;
    let max_cols = ((DIVIDER_X - MARGIN * 2 - 8) / FONT_W) as usize;
    let mut line = 0i32;
    let mut col = 0i32;
    let mut line_num = 1u32;

    // Line number for first line
    let mut lnb = [0u8; 4];
    let lnl = { let mut m = MsgBuf::new(&mut lnb); m.u32(line_num); m.len() };
    folk_draw_text(ex + 2, text_y + 2, lnb.as_ptr() as i32, lnl as i32, TEXT_DIM);

    for i in 0..=SRC_LEN {
        if i == CURSOR_POS {
            folk_draw_rect(ex + 28 + col * FONT_W, text_y + 2 + line * LINE_H, 2, FONT_H, CURSOR);
        }
        if i >= SRC_LEN { break; }
        let ch = *d.add(i);
        if ch == b'\n' {
            line += 1; col = 0; line_num += 1;
            let mut lnb2 = [0u8; 4];
            let lnl2 = { let mut m = MsgBuf::new(&mut lnb2); m.u32(line_num); m.len() };
            if text_y + 2 + line * LINE_H < sh - HELP_H {
                folk_draw_text(ex + 2, text_y + 2 + line * LINE_H,
                    lnb2.as_ptr() as i32, lnl2 as i32, TEXT_DIM);
            }
            continue;
        }
        if (col as usize) < max_cols && text_y + 2 + line * LINE_H < sh - HELP_H {
            // Syntax coloring
            let color = if ch == b'#' || ch == b';' { TEXT_DIM }
                else if ch >= b'0' && ch <= b'9' { WARN }
                else { TEXT };
            folk_draw_text(ex + 28 + col * FONT_W, text_y + 2 + line * LINE_H,
                d.add(i) as i32, 1, color);
        }
        col += 1;
    }

    // ── Right: Build Results ──
    let rx = DIVIDER_X + MARGIN;
    let ry = HEADER_H + 4;

    draw(rx, ry, b"Build & Shadow Test Results:", TEXT_DIM);

    // Build status
    let status_y = ry + 22;
    if BUILD_COUNT > 0 {
        let status_color = if BUILD_OK { OK } else { ERR };
        let msg = core::slice::from_raw_parts(
            core::ptr::addr_of!(BUILD_MSG) as *const u8, BUILD_MSG_LEN);
        folk_draw_rect(rx, status_y, sw - rx - MARGIN, 20, if BUILD_OK { 0x0D2818 } else { 0x2D1117 });
        folk_draw_text(rx + 4, status_y + 2, msg.as_ptr() as i32, msg.len() as i32, status_color);

        // WASM size
        if BUILD_OK {
            let mut sb = [0u8; 24];
            let sl = { let mut m = MsgBuf::new(&mut sb); m.s(b"WASM: "); m.u32(WASM_LEN as u32); m.s(b" bytes"); m.len() };
            draw(rx, status_y + 24, &sb[..sl], TEXT);
        }
    }

    // Shadow test results
    let result_y = status_y + 50;
    draw(rx, result_y, b"Shadow Test Report:", TEXT_DIM);

    if RESULT_LEN > 0 {
        let result = core::slice::from_raw_parts(
            core::ptr::addr_of!(RESULT_BUF) as *const u8, RESULT_LEN);

        // Parse key-value pairs from result
        let max_w = ((sw - rx - MARGIN) / FONT_W) as usize;
        let mut ry2 = result_y + 20;
        let mut col2 = 0i32;

        for &b in result {
            if b == b'\n' { ry2 += LINE_H; col2 = 0; continue; }
            if (col2 as usize) < max_w && ry2 < sh - HELP_H {
                let color = if b == b'1' && col2 > 0 { OK }
                    else if b == b'0' && col2 > 0 { ERR }
                    else if b >= b'0' && b <= b'9' { WARN }
                    else { TEXT };
                folk_draw_text(rx + col2 * FONT_W, ry2, &b as *const u8 as i32, 1, color);
                col2 += 1;
            }
        }
    } else if BUILD_COUNT > 0 {
        draw(rx, result_y + 20, b"No shadow test result", TEXT_DIM);
    } else {
        draw(rx, result_y + 20, b"Write FolkScript and press F5", TEXT_DIM);
    }

    // FolkScript reference
    let ref_y = sh - HELP_H - 100;
    draw(rx, ref_y, b"FolkScript Reference:", TEXT_DIM);
    draw(rx, ref_y + 18, b"fill <0xRRGGBB>", ACCENT);
    draw(rx, ref_y + 36, b"rect <x> <y> <w> <h> <color>", ACCENT);
    draw(rx, ref_y + 54, b"nop", ACCENT);
    draw(rx, ref_y + 72, b"# comment", TEXT_DIM);

    // Help
    folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
    draw(MARGIN, sh - HELP_H + 1,
        b"[F5] Assemble + Shadow Test  |  FolkScript: fill/rect/nop instructions",
        TEXT_DIM);
}

// ── Main ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            // Default FolkScript program
            let def = b"# FolkScript Demo\n# Press F5 to assemble + shadow test\n\nfill 0x1a1a2e\nrect 100 100 200 150 0x58A6FF\nrect 120 120 160 110 0x161B22\n";
            let d = core::ptr::addr_of_mut!(SRC) as *mut u8;
            for i in 0..def.len() { *d.add(i) = def[i]; }
            SRC_LEN = def.len(); CURSOR_POS = SRC_LEN;
            folk_log_telemetry(0, 0, 0);
            INITIALIZED = true;
        }
        handle_input();
        render();
    }
}
