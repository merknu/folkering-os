//! DreamState — AutoDream Knowledge Graph Visualizer for Folkering OS
//!
//! Reads #AutoDreamInsight files from Synapse VFS and renders them as
//! a visual node graph. Nodes = insights, edges = keyword connections.
//! Visualizes how AutoDream's knowledge evolves over time.

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);
    fn folk_draw_line(x1: i32, y1: i32, x2: i32, y2: i32, color: i32);
    fn folk_draw_circle(cx: i32, cy: i32, r: i32, color: i32);
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
    fn folk_get_time() -> i32;
    fn folk_poll_event(event_ptr: i32) -> i32;
    fn folk_query_files(q_ptr: i32, q_len: i32, r_ptr: i32, r_max: i32) -> i32;
    fn folk_request_file(p_ptr: i32, p_len: i32, d_ptr: i32, d_max: i32) -> i32;
    fn folk_list_files(buf_ptr: i32, max_len: i32) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
}

const BG: i32 = 0x0D1117;
const PANEL_BG: i32 = 0x161B22;
const TEXT: i32 = 0xC9D1D9;
const TEXT_DIM: i32 = 0x484F58;
const ACCENT: i32 = 0x58A6FF;
const NODE_COLOR: i32 = 0xBC8CFF;  // purple for dream nodes
const EDGE_COLOR: i32 = 0x30363D;
const SELECTED_COLOR: i32 = 0xF5C2E7;
const BORDER: i32 = 0x30363D;
const HEADER_H: i32 = 28;
const HELP_H: i32 = 18;
const MARGIN: i32 = 12;
const FONT_W: i32 = 8;
const FONT_H: i32 = 16;

const MAX_NODES: usize = 12;
const MAX_TITLE: usize = 32;
const MAX_CONTENT: usize = 256;
const NODE_R: i32 = 30;

#[derive(Clone, Copy)]
struct DreamNode {
    title: [u8; MAX_TITLE],
    title_len: usize,
    content: [u8; MAX_CONTENT],
    content_len: usize,
    x: i32,
    y: i32,
    // Keyword hash for edge detection
    hash: u32,
}

impl DreamNode {
    const fn empty() -> Self {
        Self {
            title: [0u8; MAX_TITLE], title_len: 0,
            content: [0u8; MAX_CONTENT], content_len: 0,
            x: 0, y: 0, hash: 0,
        }
    }
}

static mut NODES: [DreamNode; MAX_NODES] = [DreamNode::empty(); MAX_NODES];
static mut NODE_COUNT: usize = 0;
static mut SELECTED: usize = 0;
static mut DETAIL_VIEW: bool = false;
static mut EVT: [i32; 4] = [0i32; 4];
static mut QUERY_BUF: [u8; 512] = [0u8; 512];
static mut FILE_BUF: [u8; 512] = [0u8; 512];
static mut LIST_BUF: [u8; 1024] = [0u8; 1024];
static mut INITIALIZED: bool = false;

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

fn hash(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in data { h ^= b as u32; h = h.wrapping_mul(0x01000193); }
    h
}

/// Check if two insights share keywords (simple overlap check)
fn are_related(a: &[u8], b: &[u8]) -> bool {
    if a.len() < 5 || b.len() < 5 { return false; }
    // Check for shared 4-grams
    let check_len = a.len().min(60);
    for i in 0..check_len.saturating_sub(3) {
        let gram = &a[i..i+4];
        if b.windows(4).any(|w| {
            w.iter().zip(gram.iter()).all(|(&a, &b)| {
                let la = if a >= b'A' && a <= b'Z' { a + 32 } else { a };
                let lb = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
                la == lb
            })
        }) {
            return true;
        }
    }
    false
}

unsafe fn load_insights() {
    // Search for AutoDream insight files
    let query = b"autodream";
    let result_ptr = core::ptr::addr_of_mut!(QUERY_BUF) as *mut u8;
    let bytes = folk_query_files(
        query.as_ptr() as i32, query.len() as i32,
        result_ptr as i32, 512);

    // Also try listing all files to find insight-named ones
    let list_ptr = core::ptr::addr_of_mut!(LIST_BUF) as *mut u8;
    let list_bytes = folk_list_files(list_ptr as i32, 1024);

    let nodes = core::ptr::addr_of_mut!(NODES) as *mut DreamNode;
    let mut count = 0usize;

    // Parse file list for insight-like files
    if list_bytes > 0 {
        let list = core::slice::from_raw_parts(list_ptr, list_bytes as usize);
        let mut i = 0;
        while i < list.len() && count < MAX_NODES {
            let name_start = i;
            while i < list.len() && list[i] != b'\t' && list[i] != b'\n' { i += 1; }
            let name = &list[name_start..i];
            while i < list.len() && list[i] != b'\n' { i += 1; }
            if i < list.len() { i += 1; }

            // Check if it's a .wasm or .txt file (potential insight or app)
            if name.is_empty() { continue; }

            // Try to load the file content
            let file_ptr = core::ptr::addr_of_mut!(FILE_BUF) as *mut u8;
            let loaded = folk_request_file(
                name.as_ptr() as i32, name.len() as i32,
                file_ptr as i32, 512);

            let n = &mut *nodes.add(count);
            *n = DreamNode::empty();
            let tl = name.len().min(MAX_TITLE);
            for j in 0..tl { n.title[j] = name[j]; }
            n.title_len = tl;
            n.hash = hash(name);

            if loaded > 0 {
                let cl = (loaded as usize).min(MAX_CONTENT);
                let content = core::slice::from_raw_parts(file_ptr, cl);
                for j in 0..cl { n.content[j] = content[j]; }
                n.content_len = cl;
            }

            // Layout: circular arrangement
            let angle = (count as f32) * 6.2832 / MAX_NODES as f32;
            let cx = 400; // center of graph
            let cy = 350;
            let radius = 200;
            n.x = cx + (cos_approx(angle) * radius as f32) as i32;
            n.y = cy + (sin_approx(angle) * radius as f32) as i32;

            count += 1;
        }
    }
    NODE_COUNT = count;
}

/// Fast sine approximation (Taylor series, good enough for layout)
fn sin_approx(x: f32) -> f32 {
    let mut x = x;
    // Normalize to -PI..PI
    while x > 3.14159 { x -= 6.28318; }
    while x < -3.14159 { x += 6.28318; }
    // Taylor: sin(x) ≈ x - x³/6 + x⁵/120
    let x3 = x * x * x;
    let x5 = x3 * x * x;
    x - x3 / 6.0 + x5 / 120.0
}

fn cos_approx(x: f32) -> f32 {
    sin_approx(x + 1.5708) // cos(x) = sin(x + π/2)
}

unsafe fn handle_input() {
    loop {
        let e = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(e as i32) == 0 { break; }
        if *e.add(0) != 3 { continue; }
        let key = *e.add(3) as u8;

        match key {
            0x82 | 0x80 => { if SELECTED > 0 { SELECTED -= 1; } } // Left/Up
            0x83 | 0x81 => { if SELECTED + 1 < NODE_COUNT { SELECTED += 1; } } // Right/Down
            0x0D => { DETAIL_VIEW = !DETAIL_VIEW; } // Enter
            0x1B => { DETAIL_VIEW = false; } // Esc
            0x72 => { load_insights(); } // R
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
    draw(MARGIN, 6, b"DreamState", NODE_COLOR);
    draw(120, 6, b"AutoDream Knowledge Graph", TEXT_DIM);

    let mut nb = [0u8; 16];
    let nl = { let mut m = Msg::new(&mut nb); m.u32(NODE_COUNT as u32); m.s(b" nodes"); m.len() };
    draw(sw - 100, 6, &nb[..nl], TEXT_DIM);

    if NODE_COUNT == 0 {
        draw(MARGIN + 40, 100, b"No files found in VFS.", TEXT_DIM);
        draw(MARGIN + 40, 120, b"Run the OS idle for AutoDream insights,", TEXT_DIM);
        draw(MARGIN + 40, 140, b"or press [R] to refresh.", TEXT_DIM);

        folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
        draw(MARGIN, sh - HELP_H + 1, b"[R] Refresh  |  Waiting for AutoDream insights...", TEXT_DIM);
        return;
    }

    let nodes = core::ptr::addr_of!(NODES) as *const DreamNode;

    // Draw edges (connections between related nodes)
    for i in 0..NODE_COUNT {
        for j in (i+1)..NODE_COUNT {
            let a = &*nodes.add(i);
            let b = &*nodes.add(j);
            if are_related(&a.content[..a.content_len], &b.content[..b.content_len]) {
                folk_draw_line(a.x, a.y, b.x, b.y, EDGE_COLOR);
            }
        }
    }

    // Draw nodes
    for i in 0..NODE_COUNT {
        let n = &*nodes.add(i);
        let is_selected = i == SELECTED;

        let color = if is_selected { SELECTED_COLOR } else { NODE_COLOR };
        let r = if is_selected { NODE_R + 5 } else { NODE_R };

        // Node circle
        folk_draw_circle(n.x, n.y, r, color);
        // Filled center
        folk_draw_rect(n.x - r/2, n.y - r/2, r, r, PANEL_BG);

        // Node index
        let mut ib = [0u8; 4];
        let il = { let mut m = Msg::new(&mut ib); m.u32(i as u32); m.len() };
        folk_draw_text(n.x - (il as i32 * FONT_W / 2), n.y - 8,
            ib.as_ptr() as i32, il as i32, color);

        // Title (below node)
        let show_title = n.title_len.min(16);
        folk_draw_text(n.x - (show_title as i32 * FONT_W / 2), n.y + r + 4,
            n.title.as_ptr() as i32, show_title as i32,
            if is_selected { TEXT } else { TEXT_DIM });
    }

    // Detail panel (bottom)
    if DETAIL_VIEW && SELECTED < NODE_COUNT {
        let n = &*nodes.add(SELECTED);
        let dy = sh - 180;

        folk_draw_rect(0, dy, sw, 180 - HELP_H, PANEL_BG);
        folk_draw_line(0, dy, sw, dy, BORDER);

        // Title
        folk_draw_text(MARGIN, dy + 8, n.title.as_ptr() as i32, n.title_len as i32, ACCENT);

        // Content
        let max_chars = ((sw - MARGIN * 2) / FONT_W) as usize;
        let mut line = 0i32;
        let mut col = 0i32;
        let cy = dy + 28;
        for ci in 0..n.content_len {
            let b = n.content[ci];
            if b == b'\n' { line += 1; col = 0; continue; }
            if (col as usize) >= max_chars { col = 0; line += 1; }
            if cy + line * 18 > sh - HELP_H { break; }
            if b >= 0x20 && b < 0x7F {
                folk_draw_text(MARGIN + col * FONT_W, cy + line * 18,
                    n.content.as_ptr().add(ci) as i32, 1, TEXT_DIM);
                col += 1;
            }
        }
    }

    // Help
    folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
    draw(MARGIN, sh - HELP_H + 1,
        b"[</>] Select  [Enter] Detail  [Esc] Close  [R] Refresh  |  Edges = shared keywords",
        TEXT_DIM);
}

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            load_insights();
            folk_log_telemetry(0, 0, 0);
            INITIALIZED = true;
        }
        handle_input();
        render();
    }
}
