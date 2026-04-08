//! KernelSnoop — Real-time OS Monitor for Folkering OS
//!
//! Continuously polls system metrics (network, firewall, memory, uptime),
//! detects anomalies (firewall bursts, suspicious packets, network drops),
//! and uses the on-device AI (folk_slm_generate) to explain what's happening.
//!
//! UI: Two-column scrolling terminal.
//!   Left  = raw metric data (green/white)
//!   Right = AI-generated explanation (cyan) for anomalous events

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

// ── Host function declarations ──────────────────────────────────────────

extern "C" {
    // Drawing
    fn folk_fill_screen(color: i32);
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);
    fn folk_draw_line(x1: i32, y1: i32, x2: i32, y2: i32, color: i32);

    // System
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
    fn folk_get_time() -> i32; // uptime ms
    fn folk_get_datetime(ptr: i32) -> i32; // writes 6 x i32

    // Metrics
    fn folk_os_metric(metric_id: i32) -> i32;
    fn folk_net_has_ip() -> i32;
    fn folk_fw_drops() -> i32;

    // Input
    fn folk_poll_event(event_ptr: i32) -> i32;

    // AI
    fn folk_slm_generate(prompt_ptr: i32, prompt_len: i32, buf_ptr: i32, max_len: i32) -> i32;
}

// ── Constants ───────────────────────────────────────────────────────────

const BG_COLOR: i32 = 0x0D1117; // GitHub dark
const HEADER_BG: i32 = 0x161B22;
const BORDER_COLOR: i32 = 0x30363D;
const TEXT_NORMAL: i32 = 0xC9D1D9; // light gray
const TEXT_GREEN: i32 = 0x3FB950; // metric values
const TEXT_CYAN: i32 = 0x58A6FF; // AI explanations
const TEXT_RED: i32 = 0xF85149; // anomalies
const TEXT_YELLOW: i32 = 0xD29922; // warnings
const TEXT_DIM: i32 = 0x484F58; // timestamps
const TEXT_HEADER: i32 = 0x58A6FF; // title

const FONT_H: i32 = 16; // folk_draw_text line height
const FONT_W: i32 = 8; // approximate char width
const LINE_SPACING: i32 = 2;
const ROW_H: i32 = FONT_H + LINE_SPACING;

const MAX_LOG_ENTRIES: usize = 40;
const MAX_RAW_LEN: usize = 60; // max chars in raw column
const MAX_AI_LEN: usize = 120; // max chars in AI column
const POLL_INTERVAL_MS: i32 = 1000; // poll metrics every 1s
const AI_COOLDOWN_MS: i32 = 5000; // min time between AI calls

// Anomaly thresholds
const FW_BURST_THRESHOLD: i32 = 3; // >3 drops/sec = anomaly
const _SUSPICIOUS_THRESHOLD: i32 = 1; // any new suspicious = anomaly

// Column layout
const LEFT_MARGIN: i32 = 10;
const DIVIDER_X: i32 = 520; // where the vertical divider sits
const RIGHT_COL_X: i32 = 530; // AI explanation column start

// ── Log entry ───────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
#[repr(u8)]
enum Category {
    Net = 0,
    Fw = 1,
    Sys = 2,
    _Ai = 3,
    Boot = 4,
}

#[derive(Clone, Copy)]
struct LogEntry {
    timestamp_ms: i32,
    category: Category,
    is_anomaly: bool,
    raw: [u8; MAX_RAW_LEN],
    raw_len: usize,
    ai: [u8; MAX_AI_LEN],
    ai_len: usize,
}

impl LogEntry {
    const fn empty() -> Self {
        Self {
            timestamp_ms: 0,
            category: Category::Sys,
            is_anomaly: false,
            raw: [0u8; MAX_RAW_LEN],
            raw_len: 0,
            ai: [0u8; MAX_AI_LEN],
            ai_len: 0,
        }
    }
}

// ── Persistent state ────────────────────────────────────────────────────

static mut LOG: [LogEntry; MAX_LOG_ENTRIES] = [LogEntry::empty(); MAX_LOG_ENTRIES];
static mut LOG_COUNT: usize = 0;
static mut SCROLL_OFFSET: usize = 0;

// Previous metric values for delta detection
static mut PREV_NET_ONLINE: i32 = -1; // -1 = unknown
static mut PREV_FW_DROPS: i32 = 0;
static mut PREV_SUSPICIOUS: i32 = 0;
static mut _PREV_METRIC0: i32 = 0; // network metric

static mut LAST_POLL_MS: i32 = 0;
static mut LAST_AI_MS: i32 = 0;
static mut INITIALIZED: bool = false;
static mut FRAME_COUNT: u32 = 0;

// AI request/response buffers
static mut AI_PROMPT_BUF: [u8; 512] = [0u8; 512];
static mut AI_RESP_BUF: [u8; 512] = [0u8; 512];

// Datetime buffer (6 x i32)
static mut DATETIME_BUF: [i32; 6] = [0i32; 6];

// Event buffer
static mut EVENT_BUF: [i32; 4] = [0i32; 4];

// ── Helpers ─────────────────────────────────────────────────────────────

/// Copy a byte slice into a fixed-size array, return bytes written
fn copy_to_buf(dst: &mut [u8], src: &[u8]) -> usize {
    let len = if src.len() < dst.len() {
        src.len()
    } else {
        dst.len()
    };
    let mut i = 0;
    while i < len {
        dst[i] = src[i];
        i += 1;
    }
    len
}

/// Simple integer to decimal string (no alloc)
fn fmt_u32(val: u32, buf: &mut [u8]) -> usize {
    if val == 0 {
        if !buf.is_empty() {
            buf[0] = b'0';
        }
        return 1;
    }
    // Write digits backwards
    let mut tmp = [0u8; 10];
    let mut n = val;
    let mut i = 0;
    while n > 0 && i < 10 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    // Reverse into buf
    let digits = i;
    let len = if digits < buf.len() { digits } else { buf.len() };
    let mut j = 0;
    while j < len {
        buf[j] = tmp[digits - 1 - j];
        j += 1;
    }
    len
}

/// Format i32 (handles negative)
fn fmt_i32(val: i32, buf: &mut [u8]) -> usize {
    if val < 0 {
        if !buf.is_empty() {
            buf[0] = b'-';
        }
        let written = fmt_u32((-val) as u32, &mut buf[1..]);
        written + 1
    } else {
        fmt_u32(val as u32, buf)
    }
}

/// Build a string by concatenating parts into a buffer
struct MsgBuilder<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> MsgBuilder<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn push_str(&mut self, s: &[u8]) {
        let mut i = 0;
        while i < s.len() && self.pos < self.buf.len() {
            self.buf[self.pos] = s[i];
            self.pos += 1;
            i += 1;
        }
    }

    fn push_i32(&mut self, val: i32) {
        let mut tmp = [0u8; 12];
        let len = fmt_i32(val, &mut tmp);
        self.push_str(&tmp[..len]);
    }

    fn len(&self) -> usize {
        self.pos
    }
}

/// Format timestamp as "HH:MM:SS"
fn fmt_time(hour: i32, minute: i32, second: i32, buf: &mut [u8]) -> usize {
    if buf.len() < 8 {
        return 0;
    }
    buf[0] = b'0' + (hour / 10) as u8;
    buf[1] = b'0' + (hour % 10) as u8;
    buf[2] = b':';
    buf[3] = b'0' + (minute / 10) as u8;
    buf[4] = b'0' + (minute % 10) as u8;
    buf[5] = b':';
    buf[6] = b'0' + (second / 10) as u8;
    buf[7] = b'0' + (second % 10) as u8;
    8
}

// ── Log management ──────────────────────────────────────────────────────

unsafe fn add_log(category: Category, is_anomaly: bool, raw: &[u8]) {
    let now = folk_get_time();
    let idx = if LOG_COUNT < MAX_LOG_ENTRIES {
        let i = LOG_COUNT;
        LOG_COUNT += 1;
        i
    } else {
        // Shift everything up by 1 (drop oldest)
        let mut i = 0;
        while i < MAX_LOG_ENTRIES - 1 {
            LOG[i] = LOG[i + 1];
            i += 1;
        }
        MAX_LOG_ENTRIES - 1
    };

    LOG[idx] = LogEntry::empty();
    LOG[idx].timestamp_ms = now;
    LOG[idx].category = category;
    LOG[idx].is_anomaly = is_anomaly;
    LOG[idx].raw_len = copy_to_buf(&mut LOG[idx].raw, raw);
}

/// Attach AI explanation to the most recent log entry
unsafe fn attach_ai(explanation: &[u8]) {
    if LOG_COUNT > 0 {
        let idx = LOG_COUNT - 1;
        LOG[idx].ai_len = copy_to_buf(&mut LOG[idx].ai, explanation);
    }
}

// ── AI explanation ──────────────────────────────────────────────────────

unsafe fn request_ai_explanation(context: &[u8]) {
    let now = folk_get_time();
    if now - LAST_AI_MS < AI_COOLDOWN_MS {
        return; // Rate limit
    }

    // Build prompt using raw pointers to avoid static mut ref warnings
    let prompt_ptr = core::ptr::addr_of_mut!(AI_PROMPT_BUF) as *mut [u8; 512];
    let mut builder = MsgBuilder::new(&mut *prompt_ptr);
    builder.push_str(b"You are KernelSnoop, an OS monitor for Folkering OS (bare-metal Rust). ");
    builder.push_str(b"Explain this system event in ONE short sentence (max 100 chars). ");
    builder.push_str(b"Be specific and technical. Event: ");
    builder.push_str(context);
    let prompt_len = builder.len();

    let resp_ptr = core::ptr::addr_of_mut!(AI_RESP_BUF) as *mut u8;
    let result = folk_slm_generate(
        core::ptr::addr_of!(AI_PROMPT_BUF) as i32,
        prompt_len as i32,
        resp_ptr as i32,
        512,
    );

    if result > 0 {
        let resp_len = result as usize;
        let use_len = if resp_len < MAX_AI_LEN {
            resp_len
        } else {
            MAX_AI_LEN
        };
        attach_ai(core::slice::from_raw_parts(resp_ptr, use_len));
        LAST_AI_MS = now;
    }
}

// ── Metric polling & anomaly detection ──────────────────────────────────

unsafe fn poll_metrics() {
    let now = folk_get_time();
    if now - LAST_POLL_MS < POLL_INTERVAL_MS && INITIALIZED {
        return;
    }
    LAST_POLL_MS = now;

    // ── Network status ──
    let net_online = folk_net_has_ip();
    if PREV_NET_ONLINE != net_online {
        if PREV_NET_ONLINE == -1 {
            // First read
            if net_online == 1 {
                add_log(Category::Net, false, b"NET  Online - IP acquired via DHCP");
            } else {
                add_log(Category::Net, false, b"NET  Offline - no IP assigned");
            }
        } else if net_online == 1 {
            add_log(Category::Net, false, b"NET  Link UP - IP acquired");
        } else {
            add_log(Category::Net, true, b"NET  Link DOWN - connection lost!");
            // AI explain network loss
            request_ai_explanation(b"Network link went DOWN, IP lost. Possible cable disconnect or DHCP failure.");
        }
        PREV_NET_ONLINE = net_online;
    }

    // ── Firewall drops ──
    let fw_drops = folk_fw_drops();
    let fw_delta = fw_drops - PREV_FW_DROPS;
    if fw_delta > 0 && INITIALIZED {
        let is_burst = fw_delta > FW_BURST_THRESHOLD;

        // Build raw message
        let mut raw_buf = [0u8; MAX_RAW_LEN];
        let mut b = MsgBuilder::new(&mut raw_buf);
        b.push_str(b"FW   Drops: +");
        b.push_i32(fw_delta);
        b.push_str(b" (total: ");
        b.push_i32(fw_drops);
        b.push_str(b")");
        let raw_len = b.len();

        add_log(Category::Fw, is_burst, &raw_buf[..raw_len]);

        if is_burst {
            // AI explain the burst
            let mut ctx = [0u8; 128];
            let mut cb = MsgBuilder::new(&mut ctx);
            cb.push_str(b"Firewall blocked ");
            cb.push_i32(fw_delta);
            cb.push_str(b" packets in 1 second (total ");
            cb.push_i32(fw_drops);
            cb.push_str(b"). Possible port scan or DDoS.");
            let ctx_len = cb.len();
            request_ai_explanation(&ctx[..ctx_len]);
        }
    }
    PREV_FW_DROPS = fw_drops;

    // ── Suspicious packets ──
    let suspicious = folk_os_metric(3); // suspicious_count
    let sus_delta = suspicious - PREV_SUSPICIOUS;
    if sus_delta > 0 && INITIALIZED {
        let mut raw_buf = [0u8; MAX_RAW_LEN];
        let mut b = MsgBuilder::new(&mut raw_buf);
        b.push_str(b"SEC  Suspicious: +");
        b.push_i32(sus_delta);
        b.push_str(b" (total: ");
        b.push_i32(suspicious);
        b.push_str(b")");
        let raw_len = b.len();

        add_log(Category::Fw, true, &raw_buf[..raw_len]);

        let mut ctx = [0u8; 128];
        let mut cb = MsgBuilder::new(&mut ctx);
        cb.push_str(b"Detected ");
        cb.push_i32(sus_delta);
        cb.push_str(b" suspicious packets. Total anomalous: ");
        cb.push_i32(suspicious);
        let ctx_len = cb.len();
        request_ai_explanation(&ctx[..ctx_len]);
    }
    PREV_SUSPICIOUS = suspicious;

    // ── Periodic system status (every 10 polls = ~10s) ──
    if FRAME_COUNT % 600 == 0 && INITIALIZED {
        let _metric_net = folk_os_metric(0);
        let metric_fw = folk_os_metric(1);
        let uptime_ms = folk_os_metric(2);
        let uptime_s = uptime_ms / 1000;
        let minutes = uptime_s / 60;
        let seconds = uptime_s % 60;

        let mut raw_buf = [0u8; MAX_RAW_LEN];
        let mut b = MsgBuilder::new(&mut raw_buf);
        b.push_str(b"SYS  Uptime: ");
        b.push_i32(minutes);
        b.push_str(b"m ");
        b.push_i32(seconds);
        b.push_str(b"s | FW: ");
        b.push_i32(metric_fw);
        let raw_len = b.len();

        add_log(Category::Sys, false, &raw_buf[..raw_len]);
    }

    if !INITIALIZED {
        add_log(Category::Boot, false, b"SYS  KernelSnoop v1.0 started");
        add_log(
            Category::Boot,
            false,
            b"SYS  Monitoring: NET, FW, SEC, SYS",
        );

        let mut raw_buf = [0u8; MAX_RAW_LEN];
        let mut b = MsgBuilder::new(&mut raw_buf);
        b.push_str(b"SYS  FW baseline: ");
        b.push_i32(fw_drops);
        b.push_str(b" drops");
        let raw_len = b.len();
        add_log(Category::Sys, false, &raw_buf[..raw_len]);

        INITIALIZED = true;
    }
}

// ── Input handling ──────────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let evt_ptr = core::ptr::addr_of_mut!(EVENT_BUF) as *mut i32;
        let has_event = folk_poll_event(evt_ptr as i32);
        if has_event == 0 {
            break;
        }
        let event_type = *evt_ptr.add(0);
        let data = *evt_ptr.add(3);

        if event_type == 3 {
            // key_down
            match data as u8 {
                // Page Up (or 'k')
                0x6B | 0x21 => {
                    if SCROLL_OFFSET < LOG_COUNT {
                        SCROLL_OFFSET += 1;
                    }
                }
                // Page Down (or 'j')
                0x6A | 0x22 => {
                    if SCROLL_OFFSET > 0 {
                        SCROLL_OFFSET -= 1;
                    }
                }
                // Home (or 'g')
                0x67 | 0x24 => {
                    // Scroll to oldest
                    if LOG_COUNT > 0 {
                        SCROLL_OFFSET = LOG_COUNT - 1;
                    }
                }
                // End (or 'G')
                0x47 | 0x23 => {
                    // Scroll to newest
                    SCROLL_OFFSET = 0;
                }
                _ => {}
            }
        }
    }
}

// ── Rendering ───────────────────────────────────────────────────────────

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();

    // Background
    folk_fill_screen(BG_COLOR);

    // ── Header bar ──
    folk_draw_rect(0, 0, sw, 36, HEADER_BG);

    let title = b"KernelSnoop";
    folk_draw_text(
        LEFT_MARGIN,
        10,
        title.as_ptr() as i32,
        title.len() as i32,
        TEXT_HEADER,
    );

    // Subtitle
    let sub = b"Real-time OS Monitor";
    folk_draw_text(
        LEFT_MARGIN + 100,
        10,
        sub.as_ptr() as i32,
        sub.len() as i32,
        TEXT_DIM,
    );

    // Clock in header (right side)
    let dt_ptr = core::ptr::addr_of_mut!(DATETIME_BUF) as *mut i32;
    folk_get_datetime(dt_ptr as i32);
    let mut time_str = [0u8; 8];
    let time_len = fmt_time(*dt_ptr.add(3), *dt_ptr.add(4), *dt_ptr.add(5), &mut time_str);
    if time_len > 0 {
        folk_draw_text(
            sw - 80,
            10,
            time_str.as_ptr() as i32,
            time_len as i32,
            TEXT_NORMAL,
        );
    }

    // ── Status bar under header ──
    let status_y = 38;
    folk_draw_rect(0, status_y, sw, 20, 0x0F1318);

    // Network status indicator
    let net_online = folk_net_has_ip();
    if net_online == 1 {
        // Green dot + ONLINE
        folk_draw_rect(LEFT_MARGIN, status_y + 5, 10, 10, TEXT_GREEN);
        let s = b"ONLINE";
        folk_draw_text(
            LEFT_MARGIN + 14,
            status_y + 2,
            s.as_ptr() as i32,
            s.len() as i32,
            TEXT_GREEN,
        );
    } else {
        // Red dot + OFFLINE
        folk_draw_rect(LEFT_MARGIN, status_y + 5, 10, 10, TEXT_RED);
        let s = b"OFFLINE";
        folk_draw_text(
            LEFT_MARGIN + 14,
            status_y + 2,
            s.as_ptr() as i32,
            s.len() as i32,
            TEXT_RED,
        );
    }

    // FW drops counter
    let fw_total = folk_fw_drops();
    let mut fw_str = [0u8; 32];
    let mut fb = MsgBuilder::new(&mut fw_str);
    fb.push_str(b"FW Drops: ");
    fb.push_i32(fw_total);
    let fw_len = fb.len();
    folk_draw_text(
        LEFT_MARGIN + 120,
        status_y + 2,
        fw_str.as_ptr() as i32,
        fw_len as i32,
        TEXT_YELLOW,
    );

    // Suspicious counter
    let sus = folk_os_metric(3);
    let mut sus_str = [0u8; 32];
    let mut sb = MsgBuilder::new(&mut sus_str);
    sb.push_str(b"Suspicious: ");
    sb.push_i32(sus);
    let sus_len = sb.len();
    let sus_color = if sus > 0 { TEXT_RED } else { TEXT_DIM };
    folk_draw_text(
        LEFT_MARGIN + 280,
        status_y + 2,
        sus_str.as_ptr() as i32,
        sus_len as i32,
        sus_color,
    );

    // Log entry count
    let mut cnt_str = [0u8; 24];
    let mut cb = MsgBuilder::new(&mut cnt_str);
    cb.push_str(b"Log: ");
    cb.push_i32(LOG_COUNT as i32);
    let cnt_len = cb.len();
    folk_draw_text(
        LEFT_MARGIN + 440,
        status_y + 2,
        cnt_str.as_ptr() as i32,
        cnt_len as i32,
        TEXT_DIM,
    );

    // ── Column headers ──
    let header_y = 62;
    folk_draw_rect(0, header_y, sw, 18, 0x0F1318);

    let lh = b"TIME     CAT  EVENT";
    folk_draw_text(
        LEFT_MARGIN,
        header_y + 1,
        lh.as_ptr() as i32,
        lh.len() as i32,
        TEXT_DIM,
    );

    let rh = b"AI ANALYSIS";
    folk_draw_text(
        RIGHT_COL_X,
        header_y + 1,
        rh.as_ptr() as i32,
        rh.len() as i32,
        TEXT_DIM,
    );

    // Vertical divider
    folk_draw_line(DIVIDER_X, header_y, DIVIDER_X, sh, BORDER_COLOR);

    // ── Log entries ──
    let log_start_y = 84;
    let visible_rows = ((sh - log_start_y) / ROW_H) as usize;

    if LOG_COUNT == 0 {
        let msg = b"Waiting for events...";
        folk_draw_text(
            LEFT_MARGIN + 40,
            log_start_y + 40,
            msg.as_ptr() as i32,
            msg.len() as i32,
            TEXT_DIM,
        );
        return;
    }

    // Calculate visible range (newest at bottom, scroll goes up)
    let end_idx = if SCROLL_OFFSET >= LOG_COUNT {
        0
    } else {
        LOG_COUNT - SCROLL_OFFSET
    };
    let start_idx = if end_idx > visible_rows {
        end_idx - visible_rows
    } else {
        0
    };

    let mut row = 0i32;
    let mut i = start_idx;
    while i < end_idx {
        let entry = &LOG[i];
        let y = log_start_y + row * ROW_H;

        if y + ROW_H > sh {
            break;
        }

        // Anomaly background highlight
        if entry.is_anomaly {
            folk_draw_rect(0, y - 1, DIVIDER_X - 2, ROW_H, 0x1A0A0A);
        }

        // Timestamp (from uptime)
        let ts_sec = entry.timestamp_ms / 1000;
        let ts_min = ts_sec / 60;
        let ts_s = ts_sec % 60;
        let mut ts_buf = [0u8; 12];
        let mut tb = MsgBuilder::new(&mut ts_buf);
        // Format as MM:SS
        if ts_min < 10 {
            tb.push_str(b"0");
        }
        tb.push_i32(ts_min);
        tb.push_str(b":");
        if ts_s < 10 {
            tb.push_str(b"0");
        }
        tb.push_i32(ts_s);
        let ts_len = tb.len();

        folk_draw_text(
            LEFT_MARGIN,
            y,
            ts_buf.as_ptr() as i32,
            ts_len as i32,
            TEXT_DIM,
        );

        // Category + raw text
        let text_color = match entry.category {
            Category::Net => TEXT_GREEN,
            Category::Fw => {
                if entry.is_anomaly {
                    TEXT_RED
                } else {
                    TEXT_YELLOW
                }
            }
            Category::Sys => TEXT_NORMAL,
            Category::_Ai => TEXT_CYAN,
            Category::Boot => TEXT_DIM,
        };

        if entry.raw_len > 0 {
            folk_draw_text(
                LEFT_MARGIN + 52,
                y,
                entry.raw.as_ptr() as i32,
                entry.raw_len as i32,
                text_color,
            );
        }

        // AI explanation (right column)
        if entry.ai_len > 0 {
            // Cyan bracket indicator
            let indicator = b"> ";
            folk_draw_text(
                RIGHT_COL_X,
                y,
                indicator.as_ptr() as i32,
                indicator.len() as i32,
                TEXT_CYAN,
            );

            // Wrap AI text if needed (simple truncation at column width)
            let max_chars = ((sw - RIGHT_COL_X - 20) / FONT_W) as usize;
            let show_len = if entry.ai_len < max_chars {
                entry.ai_len
            } else {
                max_chars
            };
            folk_draw_text(
                RIGHT_COL_X + 16,
                y,
                entry.ai.as_ptr() as i32,
                show_len as i32,
                TEXT_CYAN,
            );
        }

        row += 1;
        i += 1;
    }

    // ── Scroll indicator (right edge) ──
    if LOG_COUNT > visible_rows {
        let bar_h = sh - log_start_y;
        let thumb_h = (visible_rows as i32 * bar_h) / LOG_COUNT as i32;
        let thumb_h = if thumb_h < 10 { 10 } else { thumb_h };
        let scroll_frac = if LOG_COUNT - visible_rows > 0 {
            (SCROLL_OFFSET as i32 * (bar_h - thumb_h)) / (LOG_COUNT - visible_rows) as i32
        } else {
            0
        };
        let thumb_y = log_start_y + bar_h - thumb_h - scroll_frac;

        // Track
        folk_draw_rect(sw - 4, log_start_y, 3, bar_h, 0x0F1318);
        // Thumb
        folk_draw_rect(sw - 4, thumb_y, 3, thumb_h, BORDER_COLOR);
    }

    // ── Bottom help bar ──
    let help_y = sh - 18;
    folk_draw_rect(0, help_y, sw, 18, HEADER_BG);
    let help = b"[j/k] Scroll  [g/G] Top/Bottom  |  Anomalies trigger AI analysis";
    folk_draw_text(
        LEFT_MARGIN,
        help_y + 1,
        help.as_ptr() as i32,
        help.len() as i32,
        TEXT_DIM,
    );
}

// ── Entry point ─────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        FRAME_COUNT += 1;

        handle_input();
        poll_metrics();
        render();
    }
}
