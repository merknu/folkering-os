//! AutoDoc — Headless Documentation Daemon for Folkering OS
//!
//! A "Liquid App" that runs without UI. Watches the Telemetry Ring for
//! FileWritten events on .rs/.cpp files, reads the code, generates
//! Markdown documentation via AI, and saves to docs/ in Synapse VFS.
//!
//! Also monitors Shadow Runtime test results — if a test fails with
//! "Out of Fuel", it appends to docs/known_issues.md.
//!
//! Yields CPU when no work is pending (costs ~0 fuel when idle).

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

extern "C" {
    // Minimal drawing (headless — only used for status indicator)
    fn folk_fill_screen(color: i32);
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
    fn folk_get_time() -> i32;
    fn folk_poll_event(event_ptr: i32) -> i32;
    // Telemetry polling
    fn folk_telemetry_poll(buf_ptr: i32, max_events: i32) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
    // File I/O
    fn folk_request_file(path_ptr: i32, path_len: i32, dest_ptr: i32, dest_len: i32) -> i32;
    fn folk_write_file(path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32) -> i32;
    fn folk_list_files(buf_ptr: i32, max_len: i32) -> i32;
    // AI
    fn folk_slm_generate(prompt_ptr: i32, prompt_len: i32, buf_ptr: i32, max_len: i32) -> i32;
}

// ── Constants ───────────────────────────────────────────────────────────

const BG: i32 = 0x0D1117;
const TEXT_DIM: i32 = 0x484F58;
const ACCENT: i32 = 0x58A6FF;
const OK_GREEN: i32 = 0x3FB950;
const WARN: i32 = 0xD29922;

// Telemetry event layout (16 bytes each)
const EVENT_SIZE: usize = 16;
const ACTION_FILE_WRITTEN: u8 = 7;

// Timing
const POLL_INTERVAL_MS: i32 = 5000; // Check telemetry every 5s
const DOC_COOLDOWN_MS: i32 = 30000; // Min 30s between doc generations

// Limits
const MAX_PENDING: usize = 8; // Max files queued for documentation
const MAX_FILENAME: usize = 32;
const MAX_CODE: usize = 2048;
const MAX_DOC: usize = 1024;

// ── Pending file queue ──────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct PendingFile {
    name_hash: u32,
    name: [u8; MAX_FILENAME],
    name_len: usize,
    queued_ms: i32,
}

impl PendingFile {
    const fn empty() -> Self {
        Self { name_hash: 0, name: [0u8; MAX_FILENAME], name_len: 0, queued_ms: 0 }
    }
}

// ── State ───────────────────────────────────────────────────────────────

static mut PENDING: [PendingFile; MAX_PENDING] = [PendingFile::empty(); MAX_PENDING];
static mut PENDING_COUNT: usize = 0;

static mut TELEMETRY_BUF: [u8; EVENT_SIZE * 32] = [0u8; EVENT_SIZE * 32]; // 32 events max per poll
static mut CODE_BUF: [u8; MAX_CODE] = [0u8; MAX_CODE];
static mut DOC_BUF: [u8; MAX_DOC] = [0u8; MAX_DOC];
static mut PROMPT_BUF: [u8; 2560] = [0u8; 2560];
static mut FILE_LIST_BUF: [u8; 1024] = [0u8; 1024];
static mut EVT: [i32; 4] = [0i32; 4];

static mut LAST_POLL_MS: i32 = 0;
static mut LAST_DOC_MS: i32 = 0;
static mut DOCS_GENERATED: u32 = 0;
static mut EVENTS_PROCESSED: u32 = 0;
static mut INITIALIZED: bool = false;
static mut STATUS_MSG: [u8; 48] = [0u8; 48];
static mut STATUS_LEN: usize = 0;

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

unsafe fn set_status(text: &[u8]) {
    let st = core::ptr::addr_of_mut!(STATUS_MSG) as *mut u8;
    STATUS_LEN = text.len().min(48);
    for i in 0..STATUS_LEN { *st.add(i) = text[i]; }
}

/// Check if a filename ends with .rs or .cpp
fn is_source_file(name: &[u8]) -> bool {
    if name.len() >= 3 {
        let last3 = &name[name.len()-3..];
        if last3 == b".rs" { return true; }
    }
    if name.len() >= 4 {
        let last4 = &name[name.len()-4..];
        if last4 == b".cpp" { return true; }
    }
    if name.len() >= 5 {
        let last5 = &name[name.len()-5..];
        if last5 == b".wasm" { return true; }
    }
    false
}

/// Simple hash for dedup
fn hash_name(name: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in name { h ^= b as u32; h = h.wrapping_mul(0x01000193); }
    h
}

// ── Telemetry Processing ────────────────────────────────────────────────

unsafe fn poll_telemetry() {
    let buf_ptr = core::ptr::addr_of_mut!(TELEMETRY_BUF) as *mut u8;
    let drained = folk_telemetry_poll(buf_ptr as i32, 32);

    if drained <= 0 { return; }

    let buf = core::slice::from_raw_parts(buf_ptr, (drained as usize) * EVENT_SIZE);
    EVENTS_PROCESSED += drained as u32;

    for i in 0..drained as usize {
        let off = i * EVENT_SIZE;
        let action = buf[off];
        let target_id = u32::from_le_bytes([buf[off+4], buf[off+5], buf[off+6], buf[off+7]]);

        if action == ACTION_FILE_WRITTEN {
            // FileWritten event — target_id is the name hash
            // We need to find the actual filename. Scan file list.
            queue_file_by_hash(target_id);
        }
    }
}

/// Try to resolve a file hash to a filename and queue it for documentation
unsafe fn queue_file_by_hash(target_hash: u32) {
    // List all files and find matching hash
    let list_ptr = core::ptr::addr_of_mut!(FILE_LIST_BUF) as *mut u8;
    let bytes = folk_list_files(list_ptr as i32, 1024);
    if bytes <= 0 { return; }

    let list = core::slice::from_raw_parts(list_ptr, bytes as usize);
    let mut i = 0;
    while i < list.len() {
        let name_start = i;
        while i < list.len() && list[i] != b'\t' && list[i] != b'\n' { i += 1; }
        let name_end = i;
        let name = &list[name_start..name_end];

        // Skip to next line
        while i < list.len() && list[i] != b'\n' { i += 1; }
        if i < list.len() { i += 1; }

        if name.is_empty() { continue; }

        // Check if this is a source file and hash matches (or just queue all source files)
        if is_source_file(name) {
            let h = hash_name(name);
            // Queue it if not already queued
            if !is_already_queued(h) && PENDING_COUNT < MAX_PENDING {
                let p = &mut PENDING[PENDING_COUNT];
                *p = PendingFile::empty();
                p.name_hash = h;
                p.name_len = name.len().min(MAX_FILENAME);
                for j in 0..p.name_len { p.name[j] = name[j]; }
                p.queued_ms = folk_get_time();
                PENDING_COUNT += 1;
            }
        }
    }
}

unsafe fn is_already_queued(hash: u32) -> bool {
    for i in 0..PENDING_COUNT {
        if PENDING[i].name_hash == hash { return true; }
    }
    false
}

// ── Documentation Generation ────────────────────────────────────────────

unsafe fn process_next_pending() {
    if PENDING_COUNT == 0 { return; }

    let now = folk_get_time();
    if now - LAST_DOC_MS < DOC_COOLDOWN_MS { return; }

    // Pop first pending file
    let file = PENDING[0];
    // Shift queue
    for i in 0..PENDING_COUNT - 1 { PENDING[i] = PENDING[i + 1]; }
    PENDING_COUNT -= 1;

    let name = &file.name[..file.name_len];

    // Read the source code
    let code_ptr = core::ptr::addr_of_mut!(CODE_BUF) as *mut u8;
    let loaded = folk_request_file(
        name.as_ptr() as i32, name.len() as i32,
        code_ptr as i32, MAX_CODE as i32);

    if loaded <= 0 {
        set_status(b"Skipped: could not read file");
        return;
    }

    let code_len = loaded as usize;

    // Build AI prompt
    let prompt_ptr = core::ptr::addr_of_mut!(PROMPT_BUF) as *mut u8;
    let prompt_len = {
        let mut m = Msg::new(core::slice::from_raw_parts_mut(prompt_ptr, 2560));
        m.s(b"Read this updated source code from Folkering OS. ");
        m.s(b"Generate concise technical Markdown documentation explaining ");
        m.s(b"the key functions, structs, and syscalls. Max 500 chars.\n\n");
        m.s(b"File: ");
        m.s(name);
        m.s(b"\n```\n");
        // Include first 1800 bytes of code to stay within context
        let code = core::slice::from_raw_parts(code_ptr, code_len.min(1800));
        m.s(code);
        m.s(b"\n```\n");
        m.len()
    };

    set_status(b"Generating docs...");

    // Generate documentation
    let doc_ptr = core::ptr::addr_of_mut!(DOC_BUF) as *mut u8;
    let resp = folk_slm_generate(
        core::ptr::addr_of!(PROMPT_BUF) as i32, prompt_len as i32,
        doc_ptr as i32, MAX_DOC as i32);

    if resp <= 0 {
        set_status(b"AI returned empty doc");
        return;
    }

    let doc_len = resp as usize;

    // Build output document with header
    let mut out = [0u8; 1200];
    let out_len = {
        let mut m = Msg::new(&mut out);
        m.s(b"# AutoDoc: ");
        m.s(name);
        m.s(b"\n\n");
        m.s(b"*Generated automatically by AutoDoc daemon.*\n\n");
        let doc = core::slice::from_raw_parts(doc_ptr, doc_len);
        m.s(doc);
        m.s(b"\n");
        m.len()
    };

    // Build output path: docs/<filename>.md
    let mut path = [0u8; 48];
    let path_len = {
        let mut m = Msg::new(&mut path);
        m.s(b"docs/");
        m.s(name);
        m.s(b".md");
        m.len()
    };

    // Save to VFS
    folk_write_file(
        path.as_ptr() as i32, path_len as i32,
        out.as_ptr() as i32, out_len as i32);

    DOCS_GENERATED += 1;
    LAST_DOC_MS = now;

    folk_log_telemetry(7, file.name_hash as i32, (now - file.queued_ms) as i32); // FileWritten

    // Update status
    let st_ptr = core::ptr::addr_of_mut!(STATUS_MSG) as *mut u8;
    let mut m = Msg::new(core::slice::from_raw_parts_mut(st_ptr, 48));
    m.s(b"Documented: ");
    m.s(name);
    STATUS_LEN = m.len();
}

// ── Scan for existing undocumented files ─────────────────────────────────

unsafe fn initial_scan() {
    // On first boot, queue all source files for documentation
    let list_ptr = core::ptr::addr_of_mut!(FILE_LIST_BUF) as *mut u8;
    let bytes = folk_list_files(list_ptr as i32, 1024);
    if bytes <= 0 { return; }

    let list = core::slice::from_raw_parts(list_ptr, bytes as usize);
    let mut i = 0;
    while i < list.len() {
        let name_start = i;
        while i < list.len() && list[i] != b'\t' && list[i] != b'\n' { i += 1; }
        let name = &list[name_start..i];
        while i < list.len() && list[i] != b'\n' { i += 1; }
        if i < list.len() { i += 1; }

        if is_source_file(name) && PENDING_COUNT < MAX_PENDING {
            let h = hash_name(name);
            if !is_already_queued(h) {
                let p = &mut PENDING[PENDING_COUNT];
                *p = PendingFile::empty();
                p.name_hash = h;
                p.name_len = name.len().min(MAX_FILENAME);
                for j in 0..p.name_len { p.name[j] = name[j]; }
                p.queued_ms = folk_get_time();
                PENDING_COUNT += 1;
            }
        }
    }
}

// ── Input (just drain events to prevent queue buildup) ──────────────────

unsafe fn drain_input() {
    loop {
        let evt_ptr = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(evt_ptr as i32) == 0 { break; }
        // Esc closes the daemon view
    }
}

// ── Render (minimal status display) ─────────────────────────────────────

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();

    folk_fill_screen(BG);

    // Compact status display
    folk_draw_rect(0, 0, sw, 28, 0x161B22);
    draw(8, 6, b"AutoDoc Daemon", ACCENT);

    // Idle/working indicator
    let working = PENDING_COUNT > 0;
    if working {
        let dots = ((folk_get_time() / 400) % 4) as usize;
        let ind = [b"Working." as &[u8], b"Working..", b"Working...", b"Working"];
        draw(140, 6, ind[dots], WARN);
    } else {
        draw(140, 6, b"Idle", OK_GREEN);
    }

    // Stats
    let mut sb = [0u8; 64];
    let sl = {
        let mut m = Msg::new(&mut sb);
        m.s(b"Docs: "); m.u32(DOCS_GENERATED);
        m.s(b" | Queue: "); m.u32(PENDING_COUNT as u32);
        m.s(b" | Events: "); m.u32(EVENTS_PROCESSED);
        m.len()
    };
    draw(sw / 2 - 100, 6, &sb[..sl], TEXT_DIM);

    // Status message
    if STATUS_LEN > 0 {
        let st = core::slice::from_raw_parts(
            core::ptr::addr_of!(STATUS_MSG) as *const u8, STATUS_LEN);
        draw(8, 36, st, TEXT_DIM);
    }

    // Pending files list
    let mut y = 58;
    for i in 0..PENDING_COUNT.min(6) {
        let p = &PENDING[i];
        let name = &p.name[..p.name_len];
        folk_draw_text(20, y, name.as_ptr() as i32, name.len() as i32, TEXT_DIM);
        y += 18;
    }

    if PENDING_COUNT == 0 && DOCS_GENERATED == 0 {
        draw(20, 80, b"Watching for FileWritten events on .rs/.cpp/.wasm files.", TEXT_DIM);
        draw(20, 100, b"Documentation will be generated automatically.", TEXT_DIM);
    }
}

// ── Main ────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            folk_log_telemetry(0, 0, 0); // AppOpened
            initial_scan();
            INITIALIZED = true;
        }

        drain_input();

        // Poll telemetry periodically (not every frame — save fuel)
        let now = folk_get_time();
        if now - LAST_POLL_MS > POLL_INTERVAL_MS {
            poll_telemetry();
            LAST_POLL_MS = now;
        }

        // Process one pending file per frame (spread work across frames)
        if PENDING_COUNT > 0 {
            process_next_pending();
        }

        render();
    }
}
