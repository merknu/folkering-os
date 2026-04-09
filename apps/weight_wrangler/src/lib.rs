//! WeightWrangler — Live Tensor Editing for Folkering OS
//!
//! DANGEROUS: Directly modifies AI model weights in the TDMP mailbox.
//! Split-screen: tensor grid (left) + live benchmark (right).
//! Select a weight, press +/- to adjust, see the effect immediately.
//!
//! Left:  8x16 tensor grid with values from folk_tensor_read
//! Right: Live inference benchmark streamed via WebSocket

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
    fn folk_tensor_read(buf_ptr: i32, buf_len: i32, sector_offset: i32) -> i32;
    fn folk_tensor_write(sector_offset: i32, byte_offset: i32, value_bits: i32) -> i32;
    fn folk_ws_connect(url_ptr: i32, url_len: i32) -> i32;
    fn folk_ws_send(socket_id: i32, data_ptr: i32, data_len: i32) -> i32;
    fn folk_ws_poll_recv(socket_id: i32, buf_ptr: i32, max_len: i32) -> i32;
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
const WARN: i32 = 0xD29922;
const DANGER: i32 = 0xF85149;
const CURSOR_COLOR: i32 = 0xF5C2E7;
const STREAM_COLOR: i32 = 0xD2A8FF;
const MODIFIED: i32 = 0xFF6E40; // orange for modified values

// Layout
const HEADER_H: i32 = 28;
const HELP_H: i32 = 18;
const DIVIDER_X: i32 = 560;
const MARGIN: i32 = 8;
const FONT_H: i32 = 16;
const FONT_W: i32 = 8;
const CELL_W: i32 = 66;
const CELL_H: i32 = 22;
const GRID_COLS: usize = 8;
const GRID_ROWS: usize = 16;
const GRID_SIZE: usize = GRID_COLS * GRID_ROWS; // 128 floats per page

const WS_URL: &[u8] = b"ws://10.0.2.2:8080/stream";
const BENCHMARK_PROMPT: &[u8] = b"Complete this sentence: The meaning of";

// ── State ───────────────────────────────────────────────────────────────

// Tensor data
static mut TENSOR_DATA: [f32; GRID_SIZE] = [0.0; GRID_SIZE];
static mut TENSOR_MODIFIED: [bool; GRID_SIZE] = [false; GRID_SIZE];
static mut DATA_LOADED: bool = false;
static mut DATA_PAGE: usize = 0; // which page of 128 floats

// Grid cursor
static mut CURSOR_ROW: usize = 0;
static mut CURSOR_COL: usize = 0;

// TDMP header info
static mut TDMP_NAME: [u8; 32] = [0u8; 32];
static mut TDMP_NAME_LEN: usize = 0;
static mut TDMP_N_DUMPED: u32 = 0;
static mut TDMP_SEQ: u32 = 0;

// Benchmark output (streamed)
static mut BENCH_OUTPUT: [u8; 1024] = [0u8; 1024];
static mut BENCH_LEN: usize = 0;
static mut WS_SOCKET: i32 = -1;
static mut BENCH_STREAMING: bool = false;
static mut BENCH_START_MS: i32 = 0;
static mut BENCH_COUNT: u32 = 0;

// Buffers
static mut SECTOR_BUF: [u8; 512] = [0u8; 512];
static mut HDR_BUF: [u8; 512] = [0u8; 512];
static mut WS_BUF: [u8; 512] = [0u8; 512];
static mut AI_BUF: [u8; 512] = [0u8; 512];
static mut EVT: [i32; 4] = [0i32; 4];

static mut INITIALIZED: bool = false;
static mut LAST_LOAD_MS: i32 = 0;
static mut FOCUS_LEFT: bool = true; // true=tensor grid, false=benchmark

// ── Helpers ─────────────────────────────────────────────────────────────

fn read_u32_le(b: &[u8], o: usize) -> u32 {
    if o+4 > b.len() { return 0; }
    (b[o] as u32)|((b[o+1] as u32)<<8)|((b[o+2] as u32)<<16)|((b[o+3] as u32)<<24)
}
fn read_f32_le(b: &[u8], o: usize) -> f32 { f32::from_bits(read_u32_le(b, o)) }

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
    fn f32_short(&mut self, v: f32) {
        if v < 0.0 { self.s(b"-"); self.f32_short(-v); return; }
        let int = v as u32;
        self.u32(int);
        self.s(b".");
        let frac = ((v - int as f32) * 100.0) as u32;
        if frac < 10 { self.s(b"0"); }
        self.u32(frac.min(99));
    }
    fn len(&self) -> usize { self.pos }
}

unsafe fn draw(x: i32, y: i32, text: &[u8], color: i32) {
    folk_draw_text(x, y, text.as_ptr() as i32, text.len() as i32, color);
}

/// Map value to cell background color (similar to TensorView inferno)
fn value_color(v: f32, min: f32, max: f32) -> i32 {
    let range = max - min;
    if range <= 0.0 { return 0x161B22; }
    let t = ((v - min) / range).max(0.0).min(1.0);
    if t < 0.33 { 0x0B1A3D }      // dark blue
    else if t < 0.66 { 0x2D1B4E }  // purple
    else { 0x4A1A1A }              // dark red
}

// ── Data loading ────────────────────────────────────────────────────────

unsafe fn load_tensor_page() {
    // Read TDMP header
    let hdr = core::ptr::addr_of_mut!(HDR_BUF) as *mut u8;
    let bytes = folk_tensor_read(hdr as i32, 512, 0);
    if bytes < 512 { DATA_LOADED = false; return; }

    let h = core::slice::from_raw_parts(hdr, 512);
    if h[0] != b'T' || h[1] != b'D' || h[2] != b'M' || h[3] != b'P' {
        DATA_LOADED = false;
        return;
    }

    TDMP_SEQ = read_u32_le(h, 4);
    TDMP_N_DUMPED = read_u32_le(h, 12);

    // Read name
    let name = core::ptr::addr_of_mut!(TDMP_NAME) as *mut u8;
    TDMP_NAME_LEN = 0;
    for i in 0..31 {
        let c = h[48 + i];
        if c == 0 { break; }
        *name.add(i) = c;
        TDMP_NAME_LEN += 1;
    }

    // Read data sectors for this page
    let float_offset = DATA_PAGE * GRID_SIZE;
    let byte_offset = float_offset * 4;
    let sector_in_data = byte_offset / 512;
    let sectors_needed = (GRID_SIZE * 4 + 511) / 512; // 1 sector per 128 floats

    let data = core::ptr::addr_of_mut!(TENSOR_DATA) as *mut f32;
    let sec = core::ptr::addr_of_mut!(SECTOR_BUF) as *mut u8;

    let mut loaded = 0usize;
    for s in 0..sectors_needed {
        let sector_offset = 1 + sector_in_data + s; // +1 for header
        let read = folk_tensor_read(sec as i32, 512, sector_offset as i32);
        if read <= 0 { break; }

        let buf = core::slice::from_raw_parts(sec, 512);
        let floats_in_sector = 128; // 512 / 4
        for f in 0..floats_in_sector {
            let idx = loaded;
            if idx >= GRID_SIZE { break; }
            *data.add(idx) = read_f32_le(buf, f * 4);
            loaded += 1;
        }
    }

    DATA_LOADED = loaded > 0;
    LAST_LOAD_MS = folk_get_time();
}

// ── Write tensor value ──────────────────────────────────────────────────

unsafe fn write_selected_value(delta: f32) {
    let idx = CURSOR_ROW * GRID_COLS + CURSOR_COL;
    if idx >= GRID_SIZE || !DATA_LOADED { return; }

    let data = core::ptr::addr_of_mut!(TENSOR_DATA) as *mut f32;
    let modified = core::ptr::addr_of_mut!(TENSOR_MODIFIED) as *mut bool;

    let new_val = *data.add(idx) + delta;
    *data.add(idx) = new_val;
    *modified.add(idx) = true;

    // Write to TDMP disk
    let float_offset = DATA_PAGE * GRID_SIZE + idx;
    let byte_in_data = float_offset * 4;
    let sector_in_data = byte_in_data / 512;
    let byte_in_sector = byte_in_data % 512;

    let sector_offset = 1 + sector_in_data as i32; // +1 for TDMP header
    let value_bits = new_val.to_bits() as i32;

    folk_tensor_write(sector_offset, byte_in_sector as i32, value_bits);
    folk_log_telemetry(3, idx as i32, 0); // UiInteraction
}

// ── Benchmark ───────────────────────────────────────────────────────────

unsafe fn start_benchmark() {
    BENCH_LEN = 0;
    BENCH_COUNT += 1;

    if WS_SOCKET < 0 {
        WS_SOCKET = folk_ws_connect(WS_URL.as_ptr() as i32, WS_URL.len() as i32);
    }

    if WS_SOCKET >= 0 && folk_ws_send(WS_SOCKET, BENCHMARK_PROMPT.as_ptr() as i32, BENCHMARK_PROMPT.len() as i32) == 0 {
        BENCH_STREAMING = true;
        BENCH_START_MS = folk_get_time();
    } else {
        // Fallback
        WS_SOCKET = -1;
        let ai = core::ptr::addr_of_mut!(AI_BUF) as *mut u8;
        let resp = folk_slm_generate(
            BENCHMARK_PROMPT.as_ptr() as i32, BENCHMARK_PROMPT.len() as i32,
            ai as i32, 512);
        if resp > 0 {
            let out = core::ptr::addr_of_mut!(BENCH_OUTPUT) as *mut u8;
            let n = (resp as usize).min(1024);
            core::ptr::copy_nonoverlapping(ai, out, n);
            BENCH_LEN = n;
        }
    }
}

unsafe fn poll_benchmark() {
    if !BENCH_STREAMING || WS_SOCKET < 0 { return; }

    let buf = core::ptr::addr_of_mut!(WS_BUF) as *mut u8;
    let n = folk_ws_poll_recv(WS_SOCKET, buf as i32, 512);

    if n > 0 {
        let data = core::slice::from_raw_parts(buf, n as usize);
        let out = core::ptr::addr_of_mut!(BENCH_OUTPUT) as *mut u8;
        let copy = (n as usize).min(1024 - BENCH_LEN);
        for i in 0..copy { *out.add(BENCH_LEN + i) = data[i]; }
        BENCH_LEN += copy;
        BENCH_START_MS = folk_get_time();
    } else if n < 0 {
        BENCH_STREAMING = false;
        WS_SOCKET = -1;
    } else if folk_get_time() - BENCH_START_MS > 15000 {
        BENCH_STREAMING = false;
        WS_SOCKET = -1;
    }
}

// ── Input ───────────────────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let evt_ptr = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(evt_ptr as i32) == 0 { break; }
        if *evt_ptr.add(0) != 3 { continue; }
        let key = *evt_ptr.add(3) as u8;

        match key {
            0x09 => { FOCUS_LEFT = !FOCUS_LEFT; } // Tab
            0x84 => { start_benchmark(); } // F5
            0x72 => { load_tensor_page(); } // R — reload
            0x1B => {} // Esc
            _ => {
                if FOCUS_LEFT {
                    match key {
                        0x26 => { if CURSOR_ROW > 0 { CURSOR_ROW -= 1; } } // Up
                        0x28 => { if CURSOR_ROW + 1 < GRID_ROWS { CURSOR_ROW += 1; } } // Down
                        0x25 => { if CURSOR_COL > 0 { CURSOR_COL -= 1; } } // Left
                        0x27 => { if CURSOR_COL + 1 < GRID_COLS { CURSOR_COL += 1; } } // Right
                        0x2B | 0x3D => { write_selected_value(0.01); } // + or =
                        0x2D => { write_selected_value(-0.01); } // -
                        0x5D => { write_selected_value(0.1); } // ]  big +
                        0x5B => { write_selected_value(-0.1); } // [ big -
                        0x21 => { if DATA_PAGE > 0 { DATA_PAGE -= 1; load_tensor_page(); } } // PgUp
                        0x22 => { DATA_PAGE += 1; load_tensor_page(); } // PgDn
                        _ => {}
                    }
                }
            }
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
    draw(MARGIN, 6, b"WeightWrangler", DANGER);
    draw(130, 6, b"DANGEROUS: Live Tensor Editing", WARN);

    // Tensor info
    if DATA_LOADED {
        let mut ib = [0u8; 48];
        let il = {
            let mut m = Msg::new(&mut ib);
            m.s(b"seq="); m.u32(TDMP_SEQ);
            m.s(b" p"); m.u32(DATA_PAGE as u32);
            m.s(b" n="); m.u32(TDMP_N_DUMPED);
            m.len()
        };
        draw(sw - 200, 6, &ib[..il], TEXT_DIM);
    }

    // Divider
    folk_draw_line(DIVIDER_X, HEADER_H, DIVIDER_X, sh - HELP_H, BORDER);

    // ── Left: Tensor Grid ──
    let grid_x = MARGIN;
    let grid_y = HEADER_H + 4;

    // Tensor name
    if TDMP_NAME_LEN > 0 {
        let name = core::slice::from_raw_parts(
            core::ptr::addr_of!(TDMP_NAME) as *const u8, TDMP_NAME_LEN);
        folk_draw_text(grid_x, grid_y, name.as_ptr() as i32, name.len() as i32, ACCENT);
    } else {
        draw(grid_x, grid_y, b"No tensor loaded (press R)", TEXT_DIM);
    }

    if DATA_LOADED {
        let data = core::ptr::addr_of!(TENSOR_DATA) as *const f32;
        let modified = core::ptr::addr_of!(TENSOR_MODIFIED) as *const bool;

        // Find min/max for coloring
        let mut min_v = *data; let mut max_v = *data;
        for i in 1..GRID_SIZE {
            let v = *data.add(i);
            if v < min_v { min_v = v; }
            if v > max_v { max_v = v; }
        }

        let gy = grid_y + 20;
        for row in 0..GRID_ROWS {
            for col in 0..GRID_COLS {
                let idx = row * GRID_COLS + col;
                let v = *data.add(idx);
                let is_mod = *modified.add(idx);
                let cx = grid_x + (col as i32) * CELL_W;
                let cy = gy + (row as i32) * CELL_H;

                // Background
                let bg = if is_mod { MODIFIED } else { value_color(v, min_v, max_v) };
                folk_draw_rect(cx, cy, CELL_W - 2, CELL_H - 2, bg);

                // Selection cursor
                if row == CURSOR_ROW && col == CURSOR_COL && FOCUS_LEFT {
                    folk_draw_rect(cx - 1, cy - 1, CELL_W, CELL_H, CURSOR_COLOR);
                    folk_draw_rect(cx, cy, CELL_W - 2, CELL_H - 2, bg);
                }

                // Value text
                let mut vb = [0u8; 8];
                let vl = { let mut m = Msg::new(&mut vb); m.f32_short(v); m.len() };
                let text_color = if is_mod { 0x000000 } else { TEXT };
                folk_draw_text(cx + 2, cy + 3, vb.as_ptr() as i32, vl as i32, text_color);
            }
        }

        // Selected value info
        let sel_idx = CURSOR_ROW * GRID_COLS + CURSOR_COL;
        let sel_v = *data.add(sel_idx);
        let info_y = gy + (GRID_ROWS as i32) * CELL_H + 4;

        let mut sb = [0u8; 48];
        let sl = {
            let mut m = Msg::new(&mut sb);
            m.s(b"["); m.u32(sel_idx as u32); m.s(b"] = ");
            m.f32_short(sel_v);
            m.s(b"  +/-: adjust  [/]: big");
            m.len()
        };
        draw(grid_x, info_y, &sb[..sl], if FOCUS_LEFT { ACCENT } else { TEXT_DIM });
    }

    // ── Right: Live Benchmark ──
    let rx = DIVIDER_X + MARGIN;
    let ry = HEADER_H + 4;

    draw(rx, ry, b"Live Benchmark [F5]:", TEXT_DIM);

    let mut bb = [0u8; 24];
    let bl = { let mut m = Msg::new(&mut bb); m.s(b"Run #"); m.u32(BENCH_COUNT); m.len() };
    draw(rx + 180, ry, &bb[..bl], TEXT_DIM);

    if BENCH_STREAMING {
        let dots = ((folk_get_time() / 300) % 4) as usize;
        let ind = [b"..." as &[u8], b".  ", b".. ", b"..."];
        draw(rx + 240, ry, ind[dots], STREAM_COLOR);
    }

    // Benchmark prompt
    draw(rx, ry + 20, b"Prompt:", TEXT_DIM);
    draw(rx + 60, ry + 20, BENCHMARK_PROMPT, 0x58A6FF);

    // Output
    draw(rx, ry + 42, b"Output:", TEXT_DIM);
    if BENCH_LEN > 0 {
        let out = core::slice::from_raw_parts(
            core::ptr::addr_of!(BENCH_OUTPUT) as *const u8, BENCH_LEN);
        let max_chars = ((sw - rx - MARGIN) / FONT_W) as usize;
        let mut line = 0i32;
        let mut col = 0i32;
        let oy = ry + 60;
        for &b in out {
            if b == b'\n' { line += 1; col = 0; continue; }
            if (col as usize) >= max_chars { col = 0; line += 1; }
            if oy + line * 18 > sh - HELP_H - 20 { break; }
            if b >= 0x20 && b < 0x7F {
                folk_draw_text(rx + col * FONT_W, oy + line * 18,
                    &b as *const u8 as i32, 1, STREAM_COLOR);
                col += 1;
            }
        }
        // Streaming cursor
        if BENCH_STREAMING {
            let blink = (folk_get_time() / 200) % 2;
            if blink == 0 {
                folk_draw_rect(rx + col * FONT_W, oy + line * 18, FONT_W, FONT_H, STREAM_COLOR);
            }
        }
    } else {
        draw(rx + 20, ry + 60, b"Press F5 to run benchmark", TEXT_DIM);
    }

    // Help
    folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
    draw(MARGIN, sh - HELP_H + 1,
        b"[Arrows] Navigate  [+/-] Adjust 0.01  [[/]] Adjust 0.1  [F5] Benchmark  [R] Reload  [Tab] Focus",
        TEXT_DIM);
}

// ── Main ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            load_tensor_page();
            folk_log_telemetry(0, 0, 0);
            INITIALIZED = true;
        }

        handle_input();
        poll_benchmark();

        // Auto-reload tensor data every 3 seconds (catch inference updates)
        let now = folk_get_time();
        if now - LAST_LOAD_MS > 3000 && !FOCUS_LEFT {
            load_tensor_page();
        }

        render();
    }
}
