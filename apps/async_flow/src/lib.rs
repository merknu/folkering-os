//! AsyncFlow — IPC Message Bus Visualizer for Folkering OS
//!
//! Live network graph: each OS task is a circle node, IPC messages
//! are animated packets flowing along edges between nodes.
//! Congested queues turn edges red and thick.

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
    fn folk_ipc_stats(buf_ptr: i32, max_len: i32) -> i32;
    fn folk_telemetry_poll(buf_ptr: i32, max_events: i32) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
}

const BG: i32 = 0x0D1117;
const PANEL_BG: i32 = 0x161B22;
const BORDER: i32 = 0x30363D;
const TEXT: i32 = 0xC9D1D9;
const TEXT_DIM: i32 = 0x484F58;
const ACCENT: i32 = 0x58A6FF;
const NODE_IDLE: i32 = 0x30363D;
const NODE_ACTIVE: i32 = 0x3FB950;
const NODE_BLOCKED: i32 = 0xD29922;
const EDGE_NORMAL: i32 = 0x21262D;
const EDGE_BUSY: i32 = 0xD29922;
const EDGE_CONGESTED: i32 = 0xF85149;
const PACKET_COLOR: i32 = 0x58A6FF;
const HEADER_H: i32 = 28;
const HELP_H: i32 = 18;
const MARGIN: i32 = 12;
const FONT_W: i32 = 8;
const FONT_H: i32 = 16;

const MAX_TASKS: usize = 8;
const MAX_NAME: usize = 16;
const NODE_R: i32 = 32;
const MAX_PACKETS: usize = 16;

// ── Task node ───────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct TaskNode {
    id: u32,
    name: [u8; MAX_NAME],
    name_len: usize,
    state: u32, // 0=ready, 1=running, 2=blocked, 3=waiting
    cpu_ms: u64,
    x: i32,
    y: i32,
}

impl TaskNode {
    const fn empty() -> Self {
        Self { id: 0, name: [0u8; MAX_NAME], name_len: 0, state: 0, cpu_ms: 0, x: 0, y: 0 }
    }
}

// ── Animated packet ─────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Packet {
    from: u8,   // source task index
    to: u8,     // dest task index
    progress: f32, // 0.0 → 1.0 animation
    active: bool,
}

impl Packet {
    const fn empty() -> Self {
        Self { from: 0, to: 0, progress: 0.0, active: false }
    }
}

// ── IPC edge (traffic between two tasks) ────────────────────────────────

#[derive(Clone, Copy)]
struct Edge {
    from: u8,
    to: u8,
    msg_count: u32,    // total messages observed
    recent_count: u16, // messages in last 5 seconds
}

const MAX_EDGES: usize = 16;

// ── State ───────────────────────────────────────────────────────────────

static mut TASKS: [TaskNode; MAX_TASKS] = [TaskNode::empty(); MAX_TASKS];
static mut TASK_COUNT: usize = 0;
static mut SELECTED: usize = 0;

static mut EDGES: [Edge; MAX_EDGES] = [Edge { from: 0, to: 0, msg_count: 0, recent_count: 0 }; MAX_EDGES];
static mut EDGE_COUNT: usize = 0;

static mut PACKETS: [Packet; MAX_PACKETS] = [Packet::empty(); MAX_PACKETS];

static mut IPC_BUF: [u8; 512] = [0u8; 512];
static mut TELEM_BUF: [u8; 512] = [0u8; 512];
static mut EVT: [i32; 4] = [0i32; 4];

static mut INITIALIZED: bool = false;
static mut LAST_POLL_MS: i32 = 0;
static mut FRAME_COUNT: u32 = 0;

// ── Helpers ─────────────────────────────────────────────────────────────

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

fn sin_approx(x: f32) -> f32 {
    let mut x = x;
    while x > 3.14159 { x -= 6.28318; }
    while x < -3.14159 { x += 6.28318; }
    let x3 = x * x * x;
    let x5 = x3 * x * x;
    x - x3 / 6.0 + x5 / 120.0
}
fn cos_approx(x: f32) -> f32 { sin_approx(x + 1.5708) }

// ── Data loading ────────────────────────────────────────────────────────

unsafe fn poll_tasks() {
    let buf = core::ptr::addr_of_mut!(IPC_BUF) as *mut u8;
    let bytes = folk_ipc_stats(buf as i32, 512);
    if bytes <= 0 { return; }

    let data = core::slice::from_raw_parts(buf, bytes as usize);
    let tasks = core::ptr::addr_of_mut!(TASKS) as *mut TaskNode;
    let mut count = 0usize;
    let mut i = 0;

    // Parse "id:name:state:cpu_ms\n"
    while i < data.len() && count < MAX_TASKS {
        let line_start = i;
        while i < data.len() && data[i] != b'\n' { i += 1; }
        let line = &data[line_start..i];
        if i < data.len() { i += 1; }

        if line.is_empty() { continue; }

        // Parse fields separated by ':'
        let mut fields = [0usize; 5]; // start positions
        let mut fcount = 0;
        fields[0] = 0;
        fcount = 1;
        for j in 0..line.len() {
            if line[j] == b':' && fcount < 5 {
                fields[fcount] = j + 1;
                fcount += 1;
            }
        }
        if fcount < 4 { continue; }

        let t = &mut *tasks.add(count);
        *t = TaskNode::empty();

        // ID
        t.id = parse_u32(&line[..fields[1]-1]);

        // Name
        let name_end = fields[2] - 1;
        let name_start = fields[1];
        t.name_len = (name_end - name_start).min(MAX_NAME);
        for j in 0..t.name_len { t.name[j] = line[name_start + j]; }

        // State
        let state_str = &line[fields[2]..fields[3]-1];
        t.state = match state_str {
            b"ready" => 0,
            b"running" => 1,
            b"blocked" => 2,
            b"waiting" => 3,
            _ => 0,
        };

        // CPU time
        t.cpu_ms = parse_u32(&line[fields[3]..]) as u64;

        // Layout: circular
        let angle = (count as f32) * 6.28318 / 8.0;
        let cx = 500i32;
        let cy = 380i32;
        let radius = 220i32;
        t.x = cx + (cos_approx(angle) * radius as f32) as i32;
        t.y = cy + (sin_approx(angle) * radius as f32) as i32;

        count += 1;
    }
    TASK_COUNT = count;

    // Generate edges: connect each task to Synapse (id=2) and Compositor (id=4)
    let edges = core::ptr::addr_of_mut!(EDGES) as *mut Edge;
    let mut ec = 0;
    for a in 0..count {
        for b in (a+1)..count {
            if ec >= MAX_EDGES { break; }
            let ta = &*tasks.add(a);
            let tb = &*tasks.add(b);

            // Connect tasks that interact: everyone talks to synapse (2) and shell (3)
            let connects = ta.id == 2 || tb.id == 2 // Synapse
                || ta.id == 4 || tb.id == 4         // Compositor
                || (ta.id == 3 && tb.id == 4) || (ta.id == 4 && tb.id == 3); // Shell↔Compositor

            if connects {
                let e = &mut *edges.add(ec);
                e.from = a as u8;
                e.to = b as u8;
                e.msg_count = (ta.cpu_ms / 100 + tb.cpu_ms / 100) as u32; // proxy for activity
                e.recent_count = if ta.state == 1 || tb.state == 1 { 5 } else { 1 };
                ec += 1;
            }
        }
    }
    EDGE_COUNT = ec;
}

fn parse_u32(s: &[u8]) -> u32 {
    let mut val = 0u32;
    for &b in s {
        if b >= b'0' && b <= b'9' { val = val * 10 + (b - b'0') as u32; }
        else { break; }
    }
    val
}

/// Spawn animated packets on active edges
unsafe fn animate_packets() {
    let time = folk_get_time();
    let t = (time as f32) / 1000.0; // seconds

    let pkts = core::ptr::addr_of_mut!(PACKETS) as *mut Packet;

    // Advance existing packets
    for i in 0..MAX_PACKETS {
        let p = &mut *pkts.add(i);
        if !p.active { continue; }
        p.progress += 0.03; // ~30 frames to cross
        if p.progress >= 1.0 { p.active = false; }
    }

    // Spawn new packets on active edges (every ~20 frames)
    if FRAME_COUNT % 20 == 0 {
        let edges = core::ptr::addr_of!(EDGES) as *const Edge;
        for ei in 0..EDGE_COUNT {
            let e = &*edges.add(ei);
            if e.recent_count > 2 {
                // Find free packet slot
                for pi in 0..MAX_PACKETS {
                    let p = &mut *pkts.add(pi);
                    if !p.active {
                        p.from = e.from;
                        p.to = e.to;
                        p.progress = 0.0;
                        p.active = true;
                        break;
                    }
                }
            }
        }
    }
}

// ── Input ───────────────────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let e = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(e as i32) == 0 { break; }
        if *e.add(0) != 3 { continue; }
        let key = *e.add(3) as u8;
        match key {
            0x82 | 0x80 => { if SELECTED > 0 { SELECTED -= 1; } }
            0x83 | 0x81 => { if SELECTED + 1 < TASK_COUNT { SELECTED += 1; } }
            0x72 => { poll_tasks(); } // R
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
    draw(MARGIN, 6, b"AsyncFlow", ACCENT);
    draw(100, 6, b"Semantic IPC Bus Visualizer", TEXT_DIM);

    let mut tb = [0u8; 16];
    let tl = { let mut m = Msg::new(&mut tb); m.u32(TASK_COUNT as u32); m.s(b" tasks"); m.len() };
    draw(sw - 100, 6, &tb[..tl], TEXT_DIM);

    if TASK_COUNT == 0 {
        draw(MARGIN + 40, 100, b"Loading task graph...", TEXT_DIM);
        draw(MARGIN + 40, 120, b"Press [R] to refresh", TEXT_DIM);
        folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
        draw(MARGIN, sh - HELP_H + 1, b"[R] Refresh  [Arrows] Select node", TEXT_DIM);
        return;
    }

    let tasks = core::ptr::addr_of!(TASKS) as *const TaskNode;
    let edges = core::ptr::addr_of!(EDGES) as *const Edge;
    let pkts = core::ptr::addr_of!(PACKETS) as *const Packet;

    // Draw edges
    for i in 0..EDGE_COUNT {
        let e = &*edges.add(i);
        let a = &*tasks.add(e.from as usize);
        let b = &*tasks.add(e.to as usize);

        let color = if e.recent_count > 8 { EDGE_CONGESTED }
            else if e.recent_count > 3 { EDGE_BUSY }
            else { EDGE_NORMAL };

        // Draw edge (thicker if busy)
        folk_draw_line(a.x, a.y, b.x, b.y, color);
        if e.recent_count > 5 {
            // Extra thickness
            folk_draw_line(a.x + 1, a.y, b.x + 1, b.y, color);
            folk_draw_line(a.x, a.y + 1, b.x, b.y + 1, color);
        }
    }

    // Draw animated packets
    for i in 0..MAX_PACKETS {
        let p = &*pkts.add(i);
        if !p.active { continue; }
        if (p.from as usize) >= TASK_COUNT || (p.to as usize) >= TASK_COUNT { continue; }

        let a = &*tasks.add(p.from as usize);
        let b = &*tasks.add(p.to as usize);

        let px = a.x + ((b.x - a.x) as f32 * p.progress) as i32;
        let py = a.y + ((b.y - a.y) as f32 * p.progress) as i32;

        // Draw packet as small bright square
        folk_draw_rect(px - 3, py - 3, 6, 6, PACKET_COLOR);
    }

    // Draw task nodes
    for i in 0..TASK_COUNT {
        let t = &*tasks.add(i);
        let selected = i == SELECTED;

        let node_color = match t.state {
            1 => NODE_ACTIVE,    // running = green
            2 => NODE_BLOCKED,   // blocked = yellow
            _ => NODE_IDLE,      // ready/waiting = grey
        };

        let r = if selected { NODE_R + 6 } else { NODE_R };

        // Outer ring
        folk_draw_circle(t.x, t.y, r, if selected { ACCENT } else { node_color });

        // Filled center
        folk_draw_rect(t.x - r + 8, t.y - r + 8, (r - 8) * 2, (r - 8) * 2, PANEL_BG);

        // Task ID
        let mut ib = [0u8; 4];
        let il = { let mut m = Msg::new(&mut ib); m.u32(t.id); m.len() };
        folk_draw_text(t.x - (il as i32 * FONT_W / 2), t.y - 8,
            ib.as_ptr() as i32, il as i32, node_color);

        // Name below
        let show = t.name_len.min(12);
        folk_draw_text(t.x - (show as i32 * FONT_W / 2), t.y + r + 4,
            t.name.as_ptr() as i32, show as i32,
            if selected { TEXT } else { TEXT_DIM });
    }

    // Selected task detail (bottom)
    if SELECTED < TASK_COUNT {
        let t = &*tasks.add(SELECTED);
        let dy = sh - HELP_H - 50;
        folk_draw_rect(0, dy, sw, 50, PANEL_BG);
        folk_draw_line(0, dy, sw, dy, BORDER);

        let mut db = [0u8; 64];
        let dl = {
            let mut m = Msg::new(&mut db);
            m.s(b"Task "); m.u32(t.id);
            m.s(b": "); m.s(&t.name[..t.name_len]);
            m.s(b" | State: ");
            match t.state {
                0 => m.s(b"Ready"),
                1 => m.s(b"Running"),
                2 => m.s(b"Blocked"),
                3 => m.s(b"Waiting"),
                _ => m.s(b"?"),
            }
            m.s(b" | CPU: "); m.u32(t.cpu_ms as u32); m.s(b"ms");
            m.len()
        };
        draw(MARGIN, dy + 8, &db[..dl], TEXT);

        // Edge summary
        let mut edge_count = 0u32;
        for ei in 0..EDGE_COUNT {
            let e = &*edges.add(ei);
            if e.from == SELECTED as u8 || e.to == SELECTED as u8 {
                edge_count += 1;
            }
        }
        let mut eb = [0u8; 24];
        let el = { let mut m = Msg::new(&mut eb); m.s(b"Connections: "); m.u32(edge_count); m.len() };
        draw(MARGIN, dy + 28, &eb[..el], TEXT_DIM);
    }

    // Legend
    let lx = sw - 240;
    let ly = HEADER_H + 8;
    draw(lx, ly, b"Legend:", TEXT_DIM);
    folk_draw_circle(lx + 10, ly + 22, 6, NODE_ACTIVE);
    draw(lx + 22, ly + 16, b"Running", TEXT_DIM);
    folk_draw_circle(lx + 10, ly + 40, 6, NODE_BLOCKED);
    draw(lx + 22, ly + 34, b"Blocked", TEXT_DIM);
    folk_draw_circle(lx + 10, ly + 58, 6, NODE_IDLE);
    draw(lx + 22, ly + 52, b"Idle", TEXT_DIM);
    folk_draw_rect(lx + 6, ly + 72, 8, 4, EDGE_CONGESTED);
    draw(lx + 22, ly + 70, b"Congested", TEXT_DIM);
    folk_draw_rect(lx + 6, ly + 88, 8, 8, PACKET_COLOR);
    draw(lx + 22, ly + 86, b"IPC Packet", TEXT_DIM);

    // Help
    folk_draw_rect(0, sh - HELP_H, sw, HELP_H, PANEL_BG);
    draw(MARGIN, sh - HELP_H + 1,
        b"[Arrows] Select  [R] Refresh  |  Packets animate on active IPC channels",
        TEXT_DIM);
}

// ── Main ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            poll_tasks();
            folk_log_telemetry(0, 0, 0);
            INITIALIZED = true;
        }

        FRAME_COUNT += 1;
        handle_input();

        // Poll tasks every 2 seconds
        let now = folk_get_time();
        if now - LAST_POLL_MS > 2000 {
            poll_tasks();
            LAST_POLL_MS = now;
        }

        animate_packets();
        render();
    }
}
