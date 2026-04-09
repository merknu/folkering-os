//! SlabVisualizer — Memory Allocation Heatmap for Folkering OS
//!
//! Shows kernel memory as a defrag-style grid. Each cell = a memory region.
//! Color: dark blue (free) → red (heavily allocated).
//! Polls folk_memory_map() every second for live updates.

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
    fn folk_memory_map(buf_ptr: i32, max_len: i32) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
}

const BG: i32 = 0x0D1117;
const PANEL_BG: i32 = 0x161B22;
const TEXT: i32 = 0xC9D1D9;
const TEXT_DIM: i32 = 0x484F58;
const ACCENT: i32 = 0x58A6FF;
const HEADER_H: i32 = 28;
const HELP_H: i32 = 18;
const MARGIN: i32 = 12;
const FONT_W: i32 = 8;
const FONT_H: i32 = 16;

const GRID_COLS: i32 = 16;
const GRID_ROWS: i32 = 4;
const CELLS: usize = 64;

static mut MAP_BUF: [u8; 80] = [0u8; 80];
static mut TOTAL_MB: u32 = 0;
static mut USED_MB: u32 = 0;
static mut USED_PCT: u32 = 0;
static mut UPTIME_S: u32 = 0;
static mut HEATMAP: [u8; CELLS] = [0u8; CELLS];
static mut HISTORY: [u8; 120] = [0u8; 120]; // usage% over 2 min
static mut HIST_HEAD: usize = 0;
static mut HIST_COUNT: usize = 0;
static mut EVT: [i32; 4] = [0i32; 4];
static mut INITIALIZED: bool = false;
static mut LAST_POLL: i32 = 0;

fn density_color(d: u8) -> i32 {
    let t = d as f32 / 255.0;
    if t < 0.25 { 0x0B1830 }      // dark blue (free)
    else if t < 0.5 { 0x1B3050 }  // blue
    else if t < 0.75 { 0x503020 } // brown/orange
    else { 0x6B1818 }             // red (heavy)
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

unsafe fn draw(x: i32, y: i32, text: &[u8], color: i32) {
    folk_draw_text(x, y, text.as_ptr() as i32, text.len() as i32, color);
}

unsafe fn poll_memory() {
    let buf = core::ptr::addr_of_mut!(MAP_BUF) as *mut u8;
    let bytes = folk_memory_map(buf as i32, 80);
    if bytes < 80 { return; }

    let b = core::slice::from_raw_parts(buf, 80);
    TOTAL_MB = u32::from_le_bytes([b[0],b[1],b[2],b[3]]);
    USED_MB = u32::from_le_bytes([b[4],b[5],b[6],b[7]]);
    USED_PCT = u32::from_le_bytes([b[8],b[9],b[10],b[11]]);
    UPTIME_S = u32::from_le_bytes([b[12],b[13],b[14],b[15]]);

    let hm = core::ptr::addr_of_mut!(HEATMAP) as *mut u8;
    for i in 0..CELLS { *hm.add(i) = b[16 + i]; }

    // Record history
    HISTORY[HIST_HEAD] = USED_PCT.min(100) as u8;
    HIST_HEAD = (HIST_HEAD + 1) % 120;
    if HIST_COUNT < 120 { HIST_COUNT += 1; }
}

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();

    folk_fill_screen(BG);

    // Header
    folk_draw_rect(0, 0, sw, HEADER_H, PANEL_BG);
    draw(MARGIN, 6, b"SlabVisualizer", ACCENT);
    draw(140, 6, b"Memory Allocation Heatmap", TEXT_DIM);

    // Stats
    let mut sb = [0u8; 48];
    let sl = {
        let mut m = Msg::new(&mut sb);
        m.u32(USED_MB); m.s(b"/"); m.u32(TOTAL_MB); m.s(b"MB ("); m.u32(USED_PCT); m.s(b"%)");
        m.len()
    };
    draw(sw - 200, 6, &sb[..sl], TEXT);

    // Grid
    let grid_y = HEADER_H + 8;
    let cell_w = (sw - MARGIN * 2) / GRID_COLS;
    let cell_h = 80;

    draw(MARGIN, grid_y, b"Physical Memory Map (each cell = region):", TEXT_DIM);
    let gy = grid_y + 18;

    let hm = core::ptr::addr_of!(HEATMAP) as *const u8;
    for row in 0..GRID_ROWS {
        for col in 0..GRID_COLS {
            let idx = (row * GRID_COLS + col) as usize;
            if idx >= CELLS { break; }
            let density = *hm.add(idx);
            let cx = MARGIN + col * cell_w;
            let cy = gy + row * cell_h;
            folk_draw_rect(cx + 1, cy + 1, cell_w - 2, cell_h - 2, density_color(density));

            // Density label
            let mut db = [0u8; 4];
            let dl = { let mut m = Msg::new(&mut db); m.u32(density as u32); m.len() };
            folk_draw_text(cx + 4, cy + cell_h / 2 - 8, db.as_ptr() as i32, dl as i32,
                if density > 128 { 0xFFFFFF } else { TEXT_DIM });
        }
    }

    // Region labels
    let label_y = gy + GRID_ROWS * cell_h + 4;
    draw(MARGIN, label_y, b"Kernel", 0xF85149);
    draw(MARGIN + 80, label_y, b"Heap", 0xD29922);
    draw(MARGIN + 140, label_y, b"Active", 0x58A6FF);
    draw(MARGIN + 220, label_y, b"Free", 0x3FB950);

    // Usage history graph
    let graph_y = label_y + 24;
    let graph_h = sh - graph_y - HELP_H - 8;
    let graph_w = sw - MARGIN * 2;

    draw(MARGIN, graph_y, b"Usage History (2 min):", TEXT_DIM);
    let gy2 = graph_y + 18;
    folk_draw_rect(MARGIN, gy2, graph_w, graph_h - 18, 0x0A0E15);

    if HIST_COUNT > 1 {
        let pts = HIST_COUNT.min(graph_w as usize);
        let step = if pts > 1 { graph_w / (pts as i32 - 1) } else { 1 };

        for i in 1..pts {
            let i0 = (HIST_HEAD + 120 - pts + i - 1) % 120;
            let i1 = (HIST_HEAD + 120 - pts + i) % 120;
            let y0 = gy2 + graph_h - 18 - (HISTORY[i0] as i32 * (graph_h - 18) / 100);
            let y1 = gy2 + graph_h - 18 - (HISTORY[i1] as i32 * (graph_h - 18) / 100);
            folk_draw_line(MARGIN + ((i-1) as i32)*step, y0, MARGIN + (i as i32)*step, y1, 0xD29922);
        }
    }

    // Help
    folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
    draw(MARGIN, sh - HELP_H + 1, b"Live polling every 1s  |  Blue=free  Red=heavy", TEXT_DIM);
}

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            poll_memory();
            folk_log_telemetry(0, 0, 0);
            INITIALIZED = true;
        }
        // Drain input
        loop { let e = core::ptr::addr_of_mut!(EVT) as *mut i32; if folk_poll_event(e as i32) == 0 { break; } }
        // Poll every second
        let now = folk_get_time();
        if now - LAST_POLL > 1000 { poll_memory(); LAST_POLL = now; }
        render();
    }
}
