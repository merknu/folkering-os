//! DriverStudio — PCI & IRQ Dashboard for Folkering OS
//!
//! Left panel:  PCI device list with vendor:device, class, BDF, IRQ
//! Right panel: Live IRQ/stat graph scrolling left every second
//! Bottom:      Selected device details

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
    fn folk_pci_list(buf_ptr: i32, max_len: i32) -> i32;
    fn folk_irq_stats(buf_ptr: i32, max_len: i32) -> i32;
    fn folk_os_metric(metric_id: i32) -> i32;
    fn folk_net_has_ip() -> i32;
    fn folk_fw_drops() -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
}

// ── Colors ──────────────────────────────────────────────────────────────

const BG: i32 = 0x0D1117;
const PANEL_BG: i32 = 0x161B22;
const BORDER: i32 = 0x30363D;
const TEXT: i32 = 0xC9D1D9;
const TEXT_DIM: i32 = 0x484F58;
const ACCENT: i32 = 0x58A6FF;
const GRAPH_NET: i32 = 0x3FB950;  // green for network
const GRAPH_FW: i32 = 0xF85149;   // red for firewall drops
const GRAPH_MEM: i32 = 0xD29922;  // yellow for memory
const SELECTED_BG: i32 = 0x1A2332;
const GRID_COLOR: i32 = 0x1A1F28;

// Layout
const HEADER_H: i32 = 28;
const HELP_H: i32 = 18;
const LIST_W: i32 = 420;
const MARGIN: i32 = 8;
const FONT_H: i32 = 16;
const FONT_W: i32 = 8;
const LINE_H: i32 = 20;

// Graph
const GRAPH_HISTORY: usize = 120; // 2 minutes at 1 sample/sec
const GRAPH_H: i32 = 250;

// Limits
const MAX_PCI_BUF: usize = 1024;
const MAX_STATS_BUF: usize = 256;
const MAX_DEVICES: usize = 16;
const MAX_DEV_LINE: usize = 48;

// ── PCI Device entry ────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct PciEntry {
    line: [u8; MAX_DEV_LINE],
    line_len: usize,
}

impl PciEntry {
    const fn empty() -> Self {
        Self { line: [0u8; MAX_DEV_LINE], line_len: 0 }
    }
}

// ── State ───────────────────────────────────────────────────────────────

static mut DEVICES: [PciEntry; MAX_DEVICES] = [PciEntry::empty(); MAX_DEVICES];
static mut DEVICE_COUNT: usize = 0;
static mut SELECTED: usize = 0;

// Graph data: ring buffers for metrics
static mut NET_HISTORY: [u32; GRAPH_HISTORY] = [0; GRAPH_HISTORY];
static mut FW_HISTORY: [u32; GRAPH_HISTORY] = [0; GRAPH_HISTORY];
static mut MEM_HISTORY: [u8; GRAPH_HISTORY] = [0; GRAPH_HISTORY];
static mut HIST_HEAD: usize = 0;
static mut HIST_COUNT: usize = 0;

// Previous values for delta computation
static mut PREV_FW_DROPS: i32 = 0;
static mut PREV_NET: i32 = 0;

// Stats text
static mut STATS_BUF: [u8; MAX_STATS_BUF] = [0u8; MAX_STATS_BUF];
static mut STATS_LEN: usize = 0;
static mut PCI_BUF: [u8; MAX_PCI_BUF] = [0u8; MAX_PCI_BUF];
static mut EVT: [i32; 4] = [0i32; 4];

static mut INITIALIZED: bool = false;
static mut LAST_SAMPLE_MS: i32 = 0;

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

// ── Data loading ────────────────────────────────────────────────────────

unsafe fn load_pci_devices() {
    let buf = core::ptr::addr_of_mut!(PCI_BUF) as *mut u8;
    let bytes = folk_pci_list(buf as i32, MAX_PCI_BUF as i32);
    if bytes <= 0 { DEVICE_COUNT = 0; return; }

    let data = core::slice::from_raw_parts(buf, bytes as usize);
    let devs = core::ptr::addr_of_mut!(DEVICES) as *mut PciEntry;
    let mut count = 0;
    let mut i = 0;

    while i < data.len() && count < MAX_DEVICES {
        let line_start = i;
        while i < data.len() && data[i] != b'\n' { i += 1; }
        let line_len = (i - line_start).min(MAX_DEV_LINE);

        if line_len > 0 {
            let d = &mut *devs.add(count);
            *d = PciEntry::empty();
            for j in 0..line_len { d.line[j] = data[line_start + j]; }
            d.line_len = line_len;
            count += 1;
        }
        if i < data.len() { i += 1; }
    }
    DEVICE_COUNT = count;
}

unsafe fn sample_metrics() {
    let fw = folk_fw_drops();
    let net = folk_os_metric(0);
    let (_, _, mem_pct) = {
        let stats_ptr = core::ptr::addr_of_mut!(STATS_BUF) as *mut u8;
        let bytes = folk_irq_stats(stats_ptr as i32, MAX_STATS_BUF as i32);
        STATS_LEN = if bytes > 0 { bytes as usize } else { 0 };
        // Parse mem% from stats string
        let stats = core::slice::from_raw_parts(stats_ptr, STATS_LEN);
        let mut mp = 0u32;
        if let Some(pos) = stats.windows(4).position(|w| w == b"mem:") {
            let mut j = pos + 4;
            while j < stats.len() && stats[j] >= b'0' && stats[j] <= b'9' {
                mp = mp * 10 + (stats[j] - b'0') as u32;
                j += 1;
            }
        }
        (0u32, 0u32, mp)
    };

    let fw_delta = (fw - PREV_FW_DROPS).max(0) as u32;
    let net_delta = ((net - PREV_NET).max(0) as u32).min(1000);
    PREV_FW_DROPS = fw;
    PREV_NET = net;

    let idx = HIST_HEAD;
    NET_HISTORY[idx] = net_delta;
    FW_HISTORY[idx] = fw_delta;
    MEM_HISTORY[idx] = mem_pct.min(100) as u8;

    HIST_HEAD = (HIST_HEAD + 1) % GRAPH_HISTORY;
    if HIST_COUNT < GRAPH_HISTORY { HIST_COUNT += 1; }
}

// ── Input ───────────────────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let evt_ptr = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(evt_ptr as i32) == 0 { break; }
        if *evt_ptr.add(0) != 3 { continue; }
        let key = *evt_ptr.add(3) as u8;

        match key {
            0x26 | 0x6B => { if SELECTED > 0 { SELECTED -= 1; } }
            0x28 | 0x6A => { if SELECTED + 1 < DEVICE_COUNT { SELECTED += 1; } }
            0x72 => { load_pci_devices(); } // R — refresh
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
    draw(MARGIN, 6, b"DriverStudio", ACCENT);
    draw(120, 6, b"PCI & Hardware Dashboard", TEXT_DIM);

    let mut db = [0u8; 16];
    let dl = { let mut m = Msg::new(&mut db); m.u32(DEVICE_COUNT as u32); m.s(b" devices"); m.len() };
    draw(sw - 100, 6, &db[..dl], TEXT_DIM);

    // Divider
    folk_draw_line(LIST_W, HEADER_H, LIST_W, sh - HELP_H, BORDER);

    // ── Left: PCI Device List ──
    let lx = MARGIN;
    let ly = HEADER_H + 4;

    draw(lx, ly, b"PCI Devices:", TEXT_DIM);
    draw(lx, ly + 16, b"VID:DID  CL:SC  B:D.F  IRQ", TEXT_DIM);

    let devs = core::ptr::addr_of!(DEVICES) as *const PciEntry;
    let list_y = ly + 36;

    for i in 0..DEVICE_COUNT {
        let d = &*devs.add(i);
        let y = list_y + (i as i32) * LINE_H;
        if y + LINE_H > sh - HELP_H { break; }

        if i == SELECTED {
            folk_draw_rect(0, y - 1, LIST_W - 1, LINE_H, SELECTED_BG);
            folk_draw_rect(0, y - 1, 3, LINE_H, ACCENT);
        }

        // Color-code by device type
        let color = if d.line_len > 5 {
            let vid_hi = d.line[0..2].iter().all(|&b| b == b'1' || b == b'A' || b == b'F');
            if d.line[0] == b'1' && d.line[1] == b'A' && d.line[2] == b'F' && d.line[3] == b'4' {
                0x3FB950 // VirtIO — green
            } else {
                TEXT
            }
        } else { TEXT };

        folk_draw_text(lx, y, d.line.as_ptr() as i32, d.line_len as i32,
            if i == SELECTED { TEXT } else { color });
    }

    // Device name labels (heuristic)
    for i in 0..DEVICE_COUNT {
        let d = &*devs.add(i);
        let y = list_y + (i as i32) * LINE_H;
        if y + LINE_H > sh - HELP_H { break; }

        // Try to identify the device
        let line = &d.line[..d.line_len];
        let label: &[u8] = if line.len() >= 9 {
            let vid_did = &line[..9];
            if vid_did.starts_with(b"1AF4:1000") { b"Net" }
            else if vid_did.starts_with(b"1AF4:1001") { b"Blk" }
            else if vid_did.starts_with(b"1AF4:1050") { b"GPU" }
            else if line.len() > 10 && line[10] == b'0' && line[11] == b'6' { b"Brg" }
            else if line.len() > 10 && line[10] == b'0' && line[11] == b'1' { b"IDE" }
            else if line.len() > 10 && line[10] == b'0' && line[11] == b'3' { b"VGA" }
            else { b"" }
        } else { b"" };

        if !label.is_empty() {
            draw(LIST_W - 40, y, label, ACCENT);
        }
    }

    // ── Right: Live Graph ──
    let gx = LIST_W + MARGIN;
    let gy = HEADER_H + 4;
    let gw = sw - LIST_W - MARGIN * 2;

    draw(gx, gy, b"Live System Metrics (2 min window):", TEXT_DIM);

    // Graph area
    let graph_y = gy + 20;
    folk_draw_rect(gx, graph_y, gw, GRAPH_H, 0x0A0E15);

    // Grid lines
    for i in 1..5 {
        let y = graph_y + (i * GRAPH_H / 5);
        folk_draw_line(gx, y, gx + gw, y, GRID_COLOR);
    }

    // Plot data
    if HIST_COUNT > 1 {
        let points = HIST_COUNT.min(gw as usize);
        let x_step = if points > 1 { gw / (points as i32 - 1) } else { 1 };

        // Find max for scaling
        let mut max_net = 1u32;
        let mut max_fw = 1u32;
        for i in 0..HIST_COUNT {
            if NET_HISTORY[i] > max_net { max_net = NET_HISTORY[i]; }
            if FW_HISTORY[i] > max_fw { max_fw = FW_HISTORY[i]; }
        }

        // Draw lines: network (green), firewall (red), memory (yellow)
        for i in 1..points {
            let idx0 = (HIST_HEAD + GRAPH_HISTORY - points + i - 1) % GRAPH_HISTORY;
            let idx1 = (HIST_HEAD + GRAPH_HISTORY - points + i) % GRAPH_HISTORY;

            let x0 = gx + ((i - 1) as i32) * x_step;
            let x1 = gx + (i as i32) * x_step;

            // Memory (yellow, 0-100%)
            let my0 = graph_y + GRAPH_H - (MEM_HISTORY[idx0] as i32 * GRAPH_H / 100);
            let my1 = graph_y + GRAPH_H - (MEM_HISTORY[idx1] as i32 * GRAPH_H / 100);
            folk_draw_line(x0, my0, x1, my1, GRAPH_MEM);

            // Network (green)
            if max_net > 0 {
                let ny0 = graph_y + GRAPH_H - (NET_HISTORY[idx0] as i32 * GRAPH_H / max_net as i32).min(GRAPH_H);
                let ny1 = graph_y + GRAPH_H - (NET_HISTORY[idx1] as i32 * GRAPH_H / max_net as i32).min(GRAPH_H);
                folk_draw_line(x0, ny0, x1, ny1, GRAPH_NET);
            }

            // Firewall drops (red)
            if max_fw > 0 {
                let fy0 = graph_y + GRAPH_H - (FW_HISTORY[idx0] as i32 * GRAPH_H / max_fw as i32).min(GRAPH_H);
                let fy1 = graph_y + GRAPH_H - (FW_HISTORY[idx1] as i32 * GRAPH_H / max_fw as i32).min(GRAPH_H);
                folk_draw_line(x0, fy0, x1, fy1, GRAPH_FW);
            }
        }
    }

    // Legend
    let leg_y = graph_y + GRAPH_H + 8;
    folk_draw_rect(gx, leg_y, 12, 12, GRAPH_MEM);
    draw(gx + 16, leg_y - 1, b"Memory%", TEXT_DIM);
    folk_draw_rect(gx + 90, leg_y, 12, 12, GRAPH_NET);
    draw(gx + 106, leg_y - 1, b"Net", TEXT_DIM);
    folk_draw_rect(gx + 160, leg_y, 12, 12, GRAPH_FW);
    draw(gx + 176, leg_y - 1, b"FW Drops", TEXT_DIM);

    // Stats text (raw from folk_irq_stats)
    if STATS_LEN > 0 {
        let stats = core::slice::from_raw_parts(
            core::ptr::addr_of!(STATS_BUF) as *const u8, STATS_LEN);
        let show = STATS_LEN.min(((gw - 8) / FONT_W) as usize);
        folk_draw_text(gx, leg_y + 20, stats.as_ptr() as i32, show as i32, TEXT_DIM);
    }

    // Help
    folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
    draw(MARGIN, sh - HELP_H + 1,
        b"[Up/Dn] Select device  [R] Refresh PCI  |  Live sampling every 1s",
        TEXT_DIM);
}

// ── Main ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            load_pci_devices();
            folk_log_telemetry(0, 0, 0);
            INITIALIZED = true;
        }

        handle_input();

        // Sample metrics every second
        let now = folk_get_time();
        if now - LAST_SAMPLE_MS > 1000 {
            sample_metrics();
            LAST_SAMPLE_MS = now;
        }

        render();
    }
}
