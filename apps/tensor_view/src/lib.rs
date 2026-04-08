//! TensorView — Real-time Tensor Inspection for Folkering OS
//!
//! Reads the TDMP (Tensor DuMP) mailbox written by the inference server
//! to VirtIO-blk sectors 1-257. Visualizes tensor data as:
//!   - 16×16 heatmap with blue→yellow→white color scale
//!   - 20-bin histogram of value distribution
//!   - Statistical summary (min, max, mean, argmax)
//!   - AI health analysis via folk_slm_generate()
//!
//! Navigation: ←/→ step through 256-float pages of the tensor.

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

// ── Host function declarations ──────────────────────────────────────────

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
    fn folk_tensor_read(buf_ptr: i32, buf_len: i32, sector_offset: i32) -> i32;
}

// ── Constants ───────────────────────────────────────────────────────────

// Colors — dark theme
const BG: i32 = 0x0D1117;
const PANEL_BG: i32 = 0x161B22;
const BORDER: i32 = 0x30363D;
const TEXT: i32 = 0xC9D1D9;
const TEXT_DIM: i32 = 0x484F58;
const TEXT_ACCENT: i32 = 0x58A6FF;
const TEXT_WARN: i32 = 0xD29922;
const TEXT_ERR: i32 = 0xF85149;
const TEXT_OK: i32 = 0x3FB950;

// Layout
const HEADER_H: i32 = 40;
const HELP_H: i32 = 20;
const HEATMAP_X: i32 = 20;
const HEATMAP_Y: i32 = 50;
const HEATMAP_CELLS: usize = 16; // 16×16 grid
const CELL_SIZE: i32 = 30; // pixels per cell
const HEATMAP_W: i32 = (HEATMAP_CELLS as i32) * CELL_SIZE; // 480
const HIST_X: i32 = 540;
const HIST_Y: i32 = 50;
const HIST_W: i32 = 700; // histogram panel width
const HIST_BAR_W: i32 = 28;
const HIST_H: i32 = 280; // histogram height
const HIST_BINS: usize = 24;
const STATS_Y: i32 = 360;
const AI_Y: i32 = 480;

// Data
const SECTOR_SIZE: usize = 512;
const FLOATS_PER_PAGE: usize = HEATMAP_CELLS * HEATMAP_CELLS; // 256
const MAX_DATA_FLOATS: usize = 32768; // from mailbox spec
const AI_COOLDOWN_MS: i32 = 8000;

// ── TDMP Header Layout (sector 1, 512 bytes) ───────────────────────────
// 0-3:   magic "TDMP"
// 4-7:   seq (u32)
// 8-11:  n (u32) total floats
// 12-15: n_dumped (u32) floats in mailbox
// 16-19: shape0 (u32)
// 20-23: shape1 (u32)
// 24-27: argmax_idx (u32)
// 32-35: min (f32)
// 36-39: max (f32)
// 40-43: mean (f32)
// 44-47: argmax_val (f32)
// 48-111: name (64 bytes, null-terminated)
// 112+:  summary floats (up to 100 × f32)

// ── Persistent state ────────────────────────────────────────────────────

// TDMP header buffer
static mut HDR_BUF: [u8; SECTOR_SIZE] = [0u8; SECTOR_SIZE];
// Raw float data (read 8 sectors = 1024 floats = enough for 4 pages)
static mut DATA_BUF: [u8; SECTOR_SIZE * 8] = [0u8; SECTOR_SIZE * 8];
static mut DATA_FLOATS_LOADED: usize = 0;
static mut DATA_SECTOR_BASE: i32 = -1; // which data sector range is loaded

// Parsed header fields
static mut TENSOR_SEQ: u32 = 0;
static mut TENSOR_N: u32 = 0;
static mut TENSOR_N_DUMPED: u32 = 0;
static mut TENSOR_SHAPE0: u32 = 0;
static mut TENSOR_SHAPE1: u32 = 0;
static mut TENSOR_ARGMAX_IDX: u32 = 0;
static mut TENSOR_MIN: f32 = 0.0;
static mut TENSOR_MAX: f32 = 0.0;
static mut TENSOR_MEAN: f32 = 0.0;
static mut TENSOR_ARGMAX_VAL: f32 = 0.0;
static mut TENSOR_NAME: [u8; 64] = [0u8; 64];
static mut TENSOR_NAME_LEN: usize = 0;

// Navigation state
static mut CURRENT_PAGE: usize = 0;
static mut TOTAL_PAGES: usize = 0;
static mut LAST_READ_MS: i32 = 0;
static mut PREV_SEQ: u32 = 0;
static mut HAS_DATA: bool = false;

// Histogram bins
static mut HIST_COUNTS: [u32; HIST_BINS] = [0u32; HIST_BINS];
static mut HIST_MAX_COUNT: u32 = 0;

// AI state
static mut AI_PROMPT: [u8; 512] = [0u8; 512];
static mut AI_RESP: [u8; 400] = [0u8; 400];
static mut AI_RESP_LEN: usize = 0;
static mut LAST_AI_MS: i32 = 0;
static mut AI_SEQ: u32 = 0; // seq of last AI analysis

// Event buffer
static mut EVT: [i32; 4] = [0i32; 4];

static mut INITIALIZED: bool = false;

// ── Color scale: Inferno-inspired (dark → blue → magenta → orange → yellow) ──

/// Map a normalized value [0.0, 1.0] to an RGB color.
/// Uses a 5-stop gradient: black → blue → magenta → orange → yellow
fn value_to_color(t: f32) -> i32 {
    let t = if t < 0.0 { 0.0 } else if t > 1.0 { 1.0 } else { t };

    // 5 color stops
    let (r, g, b) = if t < 0.25 {
        // black (0x03,0x05,0x1E) → dark blue (0x1B,0x08,0x7E)
        let f = t * 4.0;
        lerp_rgb(0x03, 0x05, 0x1E, 0x1B, 0x08, 0x7E, f)
    } else if t < 0.5 {
        // dark blue → magenta (0x8C,0x11,0x7A)
        let f = (t - 0.25) * 4.0;
        lerp_rgb(0x1B, 0x08, 0x7E, 0x8C, 0x11, 0x7A, f)
    } else if t < 0.75 {
        // magenta → orange (0xE8,0x6A,0x17)
        let f = (t - 0.5) * 4.0;
        lerp_rgb(0x8C, 0x11, 0x7A, 0xE8, 0x6A, 0x17, f)
    } else {
        // orange → yellow (0xFC, 0xFD, 0xB7)
        let f = (t - 0.75) * 4.0;
        lerp_rgb(0xE8, 0x6A, 0x17, 0xFC, 0xFD, 0xB7, f)
    };
    ((r as i32) << 16) | ((g as i32) << 8) | (b as i32)
}

fn lerp_rgb(
    r0: u8, g0: u8, b0: u8,
    r1: u8, g1: u8, b1: u8,
    t: f32,
) -> (u8, u8, u8) {
    let r = (r0 as f32 + (r1 as f32 - r0 as f32) * t) as u8;
    let g = (g0 as f32 + (g1 as f32 - g0 as f32) * t) as u8;
    let b = (b0 as f32 + (b1 as f32 - b0 as f32) * t) as u8;
    (r, g, b)
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    if off + 4 > buf.len() { return 0; }
    (buf[off] as u32)
        | ((buf[off + 1] as u32) << 8)
        | ((buf[off + 2] as u32) << 16)
        | ((buf[off + 3] as u32) << 24)
}

fn read_f32_le(buf: &[u8], off: usize) -> f32 {
    f32::from_bits(read_u32_le(buf, off))
}

struct Msg<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Msg<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn str(&mut self, s: &[u8]) {
        let mut i = 0;
        while i < s.len() && self.pos < self.buf.len() {
            self.buf[self.pos] = s[i];
            self.pos += 1;
            i += 1;
        }
    }
    fn i32(&mut self, val: i32) {
        if val < 0 {
            self.str(b"-");
            self.u32((-val) as u32);
        } else {
            self.u32(val as u32);
        }
    }
    fn u32(&mut self, val: u32) {
        if val == 0 {
            self.str(b"0");
            return;
        }
        let mut tmp = [0u8; 10];
        let mut n = val;
        let mut i = 0;
        while n > 0 {
            tmp[i] = b'0' + (n % 10) as u8;
            n /= 10;
            i += 1;
        }
        let mut j = i;
        while j > 0 {
            j -= 1;
            if self.pos < self.buf.len() {
                self.buf[self.pos] = tmp[j];
                self.pos += 1;
            }
        }
    }
    /// Format f32 with 4 decimal places (no alloc)
    fn f32(&mut self, val: f32) {
        if val < 0.0 {
            self.str(b"-");
            self.f32_unsigned(-val);
        } else {
            self.f32_unsigned(val);
        }
    }
    fn f32_unsigned(&mut self, val: f32) {
        let int_part = val as u32;
        self.u32(int_part);
        self.str(b".");
        let frac = ((val - int_part as f32) * 10000.0) as u32;
        // Zero-pad to 4 digits
        if frac < 1000 { self.str(b"0"); }
        if frac < 100 { self.str(b"0"); }
        if frac < 10 { self.str(b"0"); }
        self.u32(frac);
    }
    fn len(&self) -> usize {
        self.pos
    }
}

// ── Data loading ────────────────────────────────────────────────────────

unsafe fn load_header() -> bool {
    let hdr_ptr = core::ptr::addr_of_mut!(HDR_BUF) as *mut u8;
    let bytes = folk_tensor_read(hdr_ptr as i32, SECTOR_SIZE as i32, 0);
    if bytes < SECTOR_SIZE as i32 {
        return false;
    }

    let hdr = &*core::ptr::addr_of!(HDR_BUF);

    // Check magic
    if hdr[0] != b'T' || hdr[1] != b'D' || hdr[2] != b'M' || hdr[3] != b'P' {
        return false;
    }

    TENSOR_SEQ = read_u32_le(hdr, 4);
    TENSOR_N = read_u32_le(hdr, 8);
    TENSOR_N_DUMPED = read_u32_le(hdr, 12);
    TENSOR_SHAPE0 = read_u32_le(hdr, 16);
    TENSOR_SHAPE1 = read_u32_le(hdr, 20);
    TENSOR_ARGMAX_IDX = read_u32_le(hdr, 24);
    TENSOR_MIN = read_f32_le(hdr, 32);
    TENSOR_MAX = read_f32_le(hdr, 36);
    TENSOR_MEAN = read_f32_le(hdr, 40);
    TENSOR_ARGMAX_VAL = read_f32_le(hdr, 44);

    // Read name (null-terminated, max 63 chars)
    let name_ptr = core::ptr::addr_of_mut!(TENSOR_NAME) as *mut u8;
    TENSOR_NAME_LEN = 0;
    let mut i = 0;
    while i < 63 {
        let c = hdr[48 + i];
        if c == 0 { break; }
        *name_ptr.add(i) = c;
        i += 1;
    }
    TENSOR_NAME_LEN = i;

    TOTAL_PAGES = if TENSOR_N_DUMPED == 0 {
        0
    } else {
        ((TENSOR_N_DUMPED as usize) + FLOATS_PER_PAGE - 1) / FLOATS_PER_PAGE
    };

    HAS_DATA = true;
    true
}

/// Load float data for the current page from disk sectors
unsafe fn load_page_data() {
    if TENSOR_N_DUMPED == 0 {
        DATA_FLOATS_LOADED = 0;
        return;
    }

    // Each page = 256 floats = 1024 bytes = 2 sectors
    // Data starts at sector_offset=1 (disk sector 2)
    let float_offset = CURRENT_PAGE * FLOATS_PER_PAGE;
    let byte_offset = float_offset * 4;
    let sector_in_data = byte_offset / SECTOR_SIZE; // sector within data area
    let sector_offset = 1 + sector_in_data as i32; // +1 because header is at offset 0

    // Only reload if sector range changed
    if sector_offset != DATA_SECTOR_BASE {
        let data_ptr = core::ptr::addr_of_mut!(DATA_BUF) as *mut u8;
        let read_bytes = folk_tensor_read(data_ptr as i32, (SECTOR_SIZE * 8) as i32, sector_offset);
        if read_bytes <= 0 {
            DATA_FLOATS_LOADED = 0;
            return;
        }
        DATA_SECTOR_BASE = sector_offset;
    }

    // Calculate how many floats are available in this page
    let floats_remaining = TENSOR_N_DUMPED as usize - float_offset;
    DATA_FLOATS_LOADED = if floats_remaining > FLOATS_PER_PAGE {
        FLOATS_PER_PAGE
    } else {
        floats_remaining
    };
}

/// Get a float value from the loaded data buffer (relative to page start)
unsafe fn get_float(idx: usize) -> f32 {
    if idx >= DATA_FLOATS_LOADED { return 0.0; }
    let float_offset = CURRENT_PAGE * FLOATS_PER_PAGE;
    let byte_offset_in_data = float_offset * 4;
    let sector_in_data = byte_offset_in_data / SECTOR_SIZE;
    let local_byte_offset = (float_offset * 4) - (sector_in_data * SECTOR_SIZE) + (idx * 4);
    let buf = &*core::ptr::addr_of!(DATA_BUF);
    if local_byte_offset + 4 > buf.len() { return 0.0; }
    read_f32_le(buf, local_byte_offset)
}

/// Compute histogram bins from current page data
unsafe fn compute_histogram() {
    let hist = core::ptr::addr_of_mut!(HIST_COUNTS) as *mut u32;
    let mut i = 0;
    while i < HIST_BINS {
        *hist.add(i) = 0;
        i += 1;
    }
    HIST_MAX_COUNT = 0;

    let range = TENSOR_MAX - TENSOR_MIN;
    if range <= 0.0 || DATA_FLOATS_LOADED == 0 {
        return;
    }

    i = 0;
    while i < DATA_FLOATS_LOADED {
        let v = get_float(i);
        let norm = (v - TENSOR_MIN) / range;
        let bin = (norm * (HIST_BINS as f32 - 1.0)) as usize;
        let bin = if bin >= HIST_BINS { HIST_BINS - 1 } else { bin };
        let count = (*hist.add(bin)) + 1;
        *hist.add(bin) = count;
        if count > HIST_MAX_COUNT {
            HIST_MAX_COUNT = count;
        }
        i += 1;
    }
}

// ── AI analysis ─────────────────────────────────────────────────────────

unsafe fn request_ai_analysis() {
    let now = folk_get_time();
    if now - LAST_AI_MS < AI_COOLDOWN_MS { return; }
    if TENSOR_SEQ == AI_SEQ { return; } // Already analyzed this dump

    let prompt_ptr = core::ptr::addr_of_mut!(AI_PROMPT) as *mut u8;
    let mut m = Msg::new(core::slice::from_raw_parts_mut(prompt_ptr, 512));
    m.str(b"Analyze this tensor from a neural network inference. ");
    m.str(b"Name: ");
    m.str(core::slice::from_raw_parts(
        core::ptr::addr_of!(TENSOR_NAME) as *const u8,
        TENSOR_NAME_LEN,
    ));
    m.str(b". Shape: [");
    m.u32(TENSOR_SHAPE0);
    m.str(b",");
    m.u32(TENSOR_SHAPE1);
    m.str(b"]. Stats: min=");
    m.f32(TENSOR_MIN);
    m.str(b" max=");
    m.f32(TENSOR_MAX);
    m.str(b" mean=");
    m.f32(TENSOR_MEAN);
    m.str(b" argmax_idx=");
    m.u32(TENSOR_ARGMAX_IDX);
    m.str(b". In 1-2 sentences: are values healthy or show vanishing/exploding gradients or saturation?");
    let prompt_len = m.len();

    let resp_ptr = core::ptr::addr_of_mut!(AI_RESP) as *mut u8;
    let result = folk_slm_generate(
        core::ptr::addr_of!(AI_PROMPT) as i32,
        prompt_len as i32,
        resp_ptr as i32,
        400,
    );

    if result > 0 {
        AI_RESP_LEN = result as usize;
        if AI_RESP_LEN > 400 { AI_RESP_LEN = 400; }
        LAST_AI_MS = now;
        AI_SEQ = TENSOR_SEQ;
    }
}

// ── Input ───────────────────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let evt_ptr = core::ptr::addr_of_mut!(EVT) as *mut i32;
        let has = folk_poll_event(evt_ptr as i32);
        if has == 0 { break; }

        let event_type = *evt_ptr.add(0);
        let data = *evt_ptr.add(3);

        if event_type == 3 {
            // key_down
            match data as u8 {
                // Right arrow or 'l' — next page
                0x27 | 0x6C => {
                    if CURRENT_PAGE + 1 < TOTAL_PAGES {
                        CURRENT_PAGE += 1;
                        load_page_data();
                        compute_histogram();
                    }
                }
                // Left arrow or 'h' — prev page
                0x25 | 0x68 => {
                    if CURRENT_PAGE > 0 {
                        CURRENT_PAGE -= 1;
                        load_page_data();
                        compute_histogram();
                    }
                }
                // Home or 'g' — first page
                0x24 | 0x67 => {
                    CURRENT_PAGE = 0;
                    load_page_data();
                    compute_histogram();
                }
                // End or 'G' — last page
                0x23 | 0x47 => {
                    if TOTAL_PAGES > 0 {
                        CURRENT_PAGE = TOTAL_PAGES - 1;
                        load_page_data();
                        compute_histogram();
                    }
                }
                // 'a' — trigger AI analysis
                0x61 => {
                    AI_SEQ = 0; // Force re-analysis
                }
                // 'r' — force reload
                0x72 => {
                    PREV_SEQ = 0;
                    LAST_READ_MS = 0;
                }
                _ => {}
            }
        }
    }
}

// ── Rendering ───────────────────────────────────────────────────────────

unsafe fn draw_text(x: i32, y: i32, text: &[u8], color: i32) {
    folk_draw_text(x, y, text.as_ptr() as i32, text.len() as i32, color);
}

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();

    folk_fill_screen(BG);

    // ── Header ──
    folk_draw_rect(0, 0, sw, HEADER_H, PANEL_BG);

    let title = b"TensorView";
    draw_text(12, 12, title, TEXT_ACCENT);

    if !HAS_DATA {
        draw_text(12, 60, b"No tensor data in mailbox.", TEXT_DIM);
        draw_text(12, 80, b"Run inference to populate the TDMP mailbox.", TEXT_DIM);
        draw_text(12, 100, b"Waiting for TDMP header at sector 1...", TEXT_DIM);

        // Help
        folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
        draw_text(12, sh - HELP_H + 2, b"[r] Reload  |  Reads VirtIO-blk sectors 1-257", TEXT_DIM);
        return;
    }

    // Tensor name + seq
    let name_ptr = core::ptr::addr_of!(TENSOR_NAME) as *const u8;
    let name_slice = core::slice::from_raw_parts(name_ptr, TENSOR_NAME_LEN);
    draw_text(120, 12, name_slice, TEXT);

    // Seq + shape
    let mut info_buf = [0u8; 80];
    let info_len = {
        let mut m = Msg::new(&mut info_buf);
        m.str(b"seq=");
        m.u32(TENSOR_SEQ);
        m.str(b"  shape=[");
        m.u32(TENSOR_SHAPE0);
        m.str(b",");
        m.u32(TENSOR_SHAPE1);
        m.str(b"]  n=");
        m.u32(TENSOR_N);
        m.str(b"  dumped=");
        m.u32(TENSOR_N_DUMPED);
        m.len()
    };
    draw_text(sw / 3, 12, &info_buf[..info_len], TEXT_DIM);

    // Page indicator
    let mut pg_buf = [0u8; 40];
    let pg_len = {
        let mut pm = Msg::new(&mut pg_buf);
        pm.str(b"Page ");
        pm.u32((CURRENT_PAGE + 1) as u32);
        pm.str(b"/");
        pm.u32(TOTAL_PAGES as u32);
        pm.len()
    };
    draw_text(sw - 120, 12, &pg_buf[..pg_len], TEXT_WARN);

    // ── Heatmap panel ──
    folk_draw_rect(HEATMAP_X - 2, HEATMAP_Y - 2, HEATMAP_W + 4, HEATMAP_W + 4, BORDER);

    let range = TENSOR_MAX - TENSOR_MIN;
    let range_safe = if range > 0.0 { range } else { 1.0 };

    let mut idx = 0usize;
    let mut row = 0;
    while row < HEATMAP_CELLS as i32 {
        let mut col = 0;
        while col < HEATMAP_CELLS as i32 {
            let val = if idx < DATA_FLOATS_LOADED {
                get_float(idx)
            } else {
                0.0
            };
            let norm = (val - TENSOR_MIN) / range_safe;
            let color = value_to_color(norm);

            let cx = HEATMAP_X + col * CELL_SIZE;
            let cy = HEATMAP_Y + row * CELL_SIZE;
            folk_draw_rect(cx, cy, CELL_SIZE - 1, CELL_SIZE - 1, color);

            idx += 1;
            col += 1;
        }
        row += 1;
    }

    // Color scale legend (under heatmap)
    let legend_y = HEATMAP_Y + HEATMAP_W + 12;
    draw_text(HEATMAP_X, legend_y, b"Low", TEXT_DIM);
    let mut li = 0;
    while li < 40 {
        let t = li as f32 / 39.0;
        let c = value_to_color(t);
        folk_draw_rect(HEATMAP_X + 30 + li * 10, legend_y - 2, 10, 16, c);
        li += 1;
    }
    draw_text(HEATMAP_X + 30 + 40 * 10 + 4, legend_y, b"High", TEXT_DIM);

    // Min/max labels
    let mut min_buf = [0u8; 24];
    let min_len = { let mut mm = Msg::new(&mut min_buf); mm.f32(TENSOR_MIN); mm.len() };
    draw_text(HEATMAP_X, legend_y + 20, &min_buf[..min_len], TEXT_DIM);

    let mut max_buf = [0u8; 24];
    let max_len = { let mut mx = Msg::new(&mut max_buf); mx.f32(TENSOR_MAX); mx.len() };
    draw_text(HEATMAP_X + 30 + 40 * 10 - 60, legend_y + 20, &max_buf[..max_len], TEXT_DIM);

    // ── Histogram panel ──
    folk_draw_rect(HIST_X - 2, HIST_Y - 2, HIST_W + 4, HIST_H + 4, BORDER);
    folk_draw_rect(HIST_X, HIST_Y, HIST_W, HIST_H, PANEL_BG);

    draw_text(HIST_X + 4, HIST_Y + 4, b"Distribution", TEXT_DIM);

    if HIST_MAX_COUNT > 0 {
        let hist_ptr = core::ptr::addr_of!(HIST_COUNTS) as *const u32;
        let bar_area_h = HIST_H - 40; // leave room for labels
        let bar_base_y = HIST_Y + HIST_H - 20;

        let mut bi = 0;
        while bi < HIST_BINS {
            let count = *hist_ptr.add(bi);
            let bar_h = if HIST_MAX_COUNT > 0 {
                (count as i32 * bar_area_h) / HIST_MAX_COUNT as i32
            } else {
                0
            };

            let bx = HIST_X + 12 + (bi as i32) * (HIST_BAR_W + 2);
            let by = bar_base_y - bar_h;

            // Color bar based on bin position
            let t = bi as f32 / (HIST_BINS as f32 - 1.0);
            let bar_color = value_to_color(t);
            folk_draw_rect(bx, by, HIST_BAR_W, bar_h, bar_color);

            // Count label on top of tall bars
            if bar_h > 16 {
                let mut cnt_buf = [0u8; 8];
                let cnt_len = { let mut cb = Msg::new(&mut cnt_buf); cb.u32(count); cb.len() };
                draw_text(bx + 2, by + 2, &cnt_buf[..cnt_len], BG);
            }
            bi += 1;
        }

        // Axis line
        folk_draw_line(HIST_X + 10, bar_base_y + 1, HIST_X + HIST_W - 10, bar_base_y + 1, BORDER);
    }

    // ── Stats panel ──
    folk_draw_rect(HIST_X - 2, STATS_Y - 2, HIST_W + 4, 100, BORDER);
    folk_draw_rect(HIST_X, STATS_Y, HIST_W, 96, PANEL_BG);

    draw_text(HIST_X + 4, STATS_Y + 4, b"Statistics", TEXT_DIM);

    // Row 1: min, max
    let mut s1 = [0u8; 60];
    let s1_len = {
        let mut m1 = Msg::new(&mut s1);
        m1.str(b"min=");
        m1.f32(TENSOR_MIN);
        m1.str(b"    max=");
        m1.f32(TENSOR_MAX);
        m1.str(b"    range=");
        m1.f32(TENSOR_MAX - TENSOR_MIN);
        m1.len()
    };
    draw_text(HIST_X + 10, STATS_Y + 24, &s1[..s1_len], TEXT_OK);

    // Row 2: mean, argmax
    let mut s2 = [0u8; 60];
    let s2_len = {
        let mut m2 = Msg::new(&mut s2);
        m2.str(b"mean=");
        m2.f32(TENSOR_MEAN);
        m2.str(b"    argmax[");
        m2.u32(TENSOR_ARGMAX_IDX);
        m2.str(b"]=");
        m2.f32(TENSOR_ARGMAX_VAL);
        m2.len()
    };
    draw_text(HIST_X + 10, STATS_Y + 44, &s2[..s2_len], TEXT);

    // Row 3: health indicator
    let spread = TENSOR_MAX - TENSOR_MIN;
    let mean_ratio = if spread > 0.0 {
        (TENSOR_MEAN - TENSOR_MIN) / spread
    } else {
        0.5
    };
    let (health_text, health_color) = if spread < 0.0001 {
        (b"DEAD (near-zero spread)" as &[u8], TEXT_ERR)
    } else if spread > 100.0 {
        (b"EXPLODING (huge range)" as &[u8], TEXT_ERR)
    } else if mean_ratio < 0.1 || mean_ratio > 0.9 {
        (b"SKEWED (mean near extreme)" as &[u8], TEXT_WARN)
    } else {
        (b"HEALTHY (balanced distribution)" as &[u8], TEXT_OK)
    };

    let mut s3 = [0u8; 48];
    let s3_len = { let mut m3 = Msg::new(&mut s3); m3.str(b"Health: "); m3.str(health_text); m3.len() };
    draw_text(HIST_X + 10, STATS_Y + 64, &s3[..s3_len], health_color);

    // ── AI Analysis panel ──
    folk_draw_rect(HIST_X - 2, AI_Y - 2, HIST_W + 4, sh - AI_Y - HELP_H - 4, BORDER);
    folk_draw_rect(HIST_X, AI_Y, HIST_W, sh - AI_Y - HELP_H - 8, PANEL_BG);

    draw_text(HIST_X + 4, AI_Y + 4, b"AI Analysis", TEXT_DIM);

    if AI_RESP_LEN > 0 {
        let resp = core::slice::from_raw_parts(
            core::ptr::addr_of!(AI_RESP) as *const u8,
            AI_RESP_LEN,
        );
        // Word-wrap AI text into multiple lines
        let max_chars = ((HIST_W - 20) / 8) as usize; // ~85 chars per line
        let mut line_start = 0;
        let mut line_y = AI_Y + 24;
        while line_start < AI_RESP_LEN && line_y < sh - HELP_H - 20 {
            let line_end = if line_start + max_chars > AI_RESP_LEN {
                AI_RESP_LEN
            } else {
                // Find last space within max_chars
                let mut end = line_start + max_chars;
                while end > line_start && resp[end] != b' ' && resp[end] != b'\n' {
                    end -= 1;
                }
                if end == line_start { end = line_start + max_chars; }
                end
            };
            draw_text(
                HIST_X + 10,
                line_y,
                &resp[line_start..line_end],
                TEXT_ACCENT,
            );
            line_y += 18;
            line_start = line_end;
            // Skip space/newline
            if line_start < AI_RESP_LEN
                && (resp[line_start] == b' ' || resp[line_start] == b'\n')
            {
                line_start += 1;
            }
        }
    } else {
        draw_text(HIST_X + 10, AI_Y + 24, b"Press [a] for AI analysis", TEXT_DIM);
    }

    // ── Offset info under heatmap ──
    let offset_y = legend_y + 40;
    let mut ob = [0u8; 60];
    let end_float = (CURRENT_PAGE * FLOATS_PER_PAGE + DATA_FLOATS_LOADED) as u32;
    let ob_len = {
        let mut om = Msg::new(&mut ob);
        om.str(b"Showing floats [");
        om.u32((CURRENT_PAGE * FLOATS_PER_PAGE) as u32);
        om.str(b" .. ");
        om.u32(end_float);
        om.str(b") of ");
        om.u32(TENSOR_N_DUMPED);
        om.len()
    };
    draw_text(HEATMAP_X, offset_y, &ob[..ob_len], TEXT_DIM);

    // ── Help bar ──
    folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
    draw_text(
        12,
        sh - HELP_H + 2,
        b"[</>] Page  [g/G] First/Last  [a] AI Analyze  [r] Reload  |  Reads TDMP mailbox from VirtIO-blk",
        TEXT_DIM,
    );
}

// ── Main loop ───────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        handle_input();

        let now = folk_get_time();

        // Auto-reload header every 2 seconds to catch new tensor dumps
        if now - LAST_READ_MS > 2000 || !INITIALIZED {
            if load_header() {
                if TENSOR_SEQ != PREV_SEQ {
                    // New tensor data available
                    if CURRENT_PAGE >= TOTAL_PAGES && TOTAL_PAGES > 0 {
                        CURRENT_PAGE = 0;
                    }
                    DATA_SECTOR_BASE = -1; // Force data reload
                    load_page_data();
                    compute_histogram();
                    PREV_SEQ = TENSOR_SEQ;
                }
            }
            LAST_READ_MS = now;
            INITIALIZED = true;
        }

        // Auto-trigger AI analysis when new data arrives
        if HAS_DATA && AI_SEQ != TENSOR_SEQ {
            request_ai_analysis();
        }

        render();
    }
}
