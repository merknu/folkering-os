//! folk_browser — FBP Proxy Mode thin client.
//!
//! The Great Shave (Phase 5 Plan B): the in-process HTML parser,
//! layout engine, image decoders, semantic view, and form handling
//! have all been removed. This version is a pure consumer of the
//! Folkering Binary Protocol served by the host-side folkering-proxy.
//!
//! Flow:
//!
//!   1. User types / auto-fills a URL (default: `proxy:mock`)
//!   2. `fetch_page()` calls `folk_fbp_recv(url, FBP_BUF)`
//!   3. The compositor's `folk_fbp_recv` host fn invokes
//!      `libfolk::sys::fbp_request` → kernel `sys_fbp_request` →
//!      plain TCP to `10.0.2.2:14711` → proxy → FBP bytes
//!   4. We parse the bytes via `fbp_rs::parse_state_update`
//!      (zero-copy) and emit a display list from SemanticNode
//!      bounds + interned strings.

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
    fn folk_poll_event(event_ptr: i32) -> i32;
    fn folk_submit_display_list(ptr: i32, len: i32) -> i32;
    fn folk_fbp_recv(url_ptr: i32, url_len: i32, buf_ptr: i32, max_len: i32) -> i32;
    fn folk_fbp_send(
        url_ptr: i32, url_len: i32,
        action: i32, node_id: i32,
        buf_ptr: i32, max_len: i32,
    ) -> i32;
    fn folk_log_telemetry(action: i32, target: i32, duration: i32);
}

// ── Colors ──────────────────────────────────────────────────────────

const BG: i32 = 0x0D1117;
const TEXT_COLOR: i32 = 0xC9D1D9;
const LINK_COLOR: i32 = 0x58A6FF;
const URLBAR_BG: i32 = 0x2D333B;
const URLBAR_TEXT: i32 = 0xC9D1D9;
const CURSOR_COLOR: i32 = 0x58A6FF;
const BUTTON_BG: i32 = 0x238636;
const INPUT_BG: i32 = 0xF5F5F5;
const INPUT_TEXT: i32 = 0x0D1117;

// ── Layout constants ────────────────────────────────────────────────

const URLBAR_H: i32 = 32;
const FONT_W: i32 = 8;
const FONT_H: i32 = 16;

// ── Limits ──────────────────────────────────────────────────────────

const MAX_URL: usize = 256;
const MAX_DISPLAY_LIST: usize = 16384;
// 128 KB FBP buffer — fits a full Hacker News snapshot (~44 KB,
// 772 nodes) with plenty of headroom for article pages (~60-120 KB).
// Backing store is u64 so the slice is naturally 8-byte aligned,
// which `fbp_rs::parse_state_update` requires for the zero-copy
// SemanticNode cast. The compositor host fn caps at 256 KB.
const FBP_BUF_WORDS: usize = 16384;
const FBP_BUF_SIZE: usize = FBP_BUF_WORDS * 8;
const HISTORY_SIZE: usize = 10;

// FBP action constants — must match fbp_rs::ACTION_*.
const ACTION_CLICK: i32 = 0x01;

// ── State ───────────────────────────────────────────────────────────

static mut URL: [u8; MAX_URL] = [0u8; MAX_URL];
static mut URL_LEN: usize = 0;
static mut CURSOR_POS: usize = 0;

// Mirror of the last URL we actually fetched. `URL` can be scribbled
// over while the user is typing in the URL bar, but `LOADED_URL` keeps
// the previously-loaded page address around so we can push it onto
// the back stack before navigating somewhere new.
static mut LOADED_URL: [u8; MAX_URL] = [0u8; MAX_URL];
static mut LOADED_URL_LEN: usize = 0;

static mut SCROLL_Y: i32 = 0;
static mut CONTENT_HEIGHT: i32 = 0;
static mut LOADING: bool = false;
static mut EDITING_URL: bool = true;

static mut DISPLAY_LIST: [u8; MAX_DISPLAY_LIST] = [0u8; MAX_DISPLAY_LIST];
static mut DL_LEN: usize = 0;

// FBP payload buffer — u64 backing gives natural 8-byte alignment so
// `fbp_rs::parse_state_update` can zero-copy slice-cast safely.
static mut FBP_BUF_WORDS_STORE: [u64; FBP_BUF_WORDS] = [0u64; FBP_BUF_WORDS];
static mut FBP_BYTES: usize = 0;

static mut HISTORY: [[u8; MAX_URL]; HISTORY_SIZE] = [[0u8; MAX_URL]; HISTORY_SIZE];
static mut HISTORY_LENS: [usize; HISTORY_SIZE] = [0; HISTORY_SIZE];
static mut HISTORY_COUNT: usize = 0;

static mut EVT: [i32; 4] = [0i32; 4];
static mut INITIALIZED: bool = false;

#[inline]
unsafe fn fbp_buf_ptr_mut() -> *mut u8 {
    core::ptr::addr_of_mut!(FBP_BUF_WORDS_STORE) as *mut u8
}

#[inline]
unsafe fn fbp_buf_ptr() -> *const u8 {
    core::ptr::addr_of!(FBP_BUF_WORDS_STORE) as *const u8
}

// ── History ─────────────────────────────────────────────────────────

/// Push the current URL onto the back stack. FIFO-evicts the oldest
/// entry when the stack is full so the most recent `HISTORY_SIZE - 1`
/// frames stay reachable.
unsafe fn history_push(url: &[u8]) {
    if url.is_empty() { return; }
    if HISTORY_COUNT == HISTORY_SIZE {
        let h = core::ptr::addr_of_mut!(HISTORY) as *mut [u8; MAX_URL];
        for i in 0..HISTORY_SIZE - 1 {
            let src = *h.add(i + 1);
            *h.add(i) = src;
            HISTORY_LENS[i] = HISTORY_LENS[i + 1];
        }
        HISTORY_COUNT = HISTORY_SIZE - 1;
    }
    let len = url.len().min(MAX_URL);
    let h = core::ptr::addr_of_mut!(HISTORY) as *mut [u8; MAX_URL];
    let entry = &mut *h.add(HISTORY_COUNT);
    for i in 0..len { entry[i] = url[i]; }
    HISTORY_LENS[HISTORY_COUNT] = len;
    HISTORY_COUNT += 1;
}

unsafe fn history_back() -> bool {
    if HISTORY_COUNT == 0 { return false; }
    HISTORY_COUNT -= 1;
    let h = core::ptr::addr_of!(HISTORY) as *const [u8; MAX_URL];
    let entry = &*h.add(HISTORY_COUNT);
    let len = HISTORY_LENS[HISTORY_COUNT];
    let u = core::ptr::addr_of_mut!(URL) as *mut u8;
    for i in 0..len { *u.add(i) = entry[i]; }
    URL_LEN = len;
    CURSOR_POS = len;
    true
}

// ── Fetch ───────────────────────────────────────────────────────────

#[inline]
unsafe fn inner_url_slice() -> &'static [u8] {
    let url_bytes = core::slice::from_raw_parts(
        core::ptr::addr_of!(URL) as *const u8, URL_LEN);
    // Strip optional "proxy:" prefix — the proxy doesn't need it.
    if url_bytes.len() >= 6 && &url_bytes[..6] == b"proxy:" {
        &url_bytes[6..]
    } else {
        url_bytes
    }
}

unsafe fn refresh_content_height() {
    let slice = core::slice::from_raw_parts(fbp_buf_ptr(), FBP_BYTES);
    if let Ok(view) = fbp_rs::parse_state_update(slice) {
        let mut max_y: i32 = 0;
        for node in view.nodes {
            let bottom = node.bounds_y.saturating_add(node.bounds_h as i32);
            if bottom > max_y { max_y = bottom; }
        }
        CONTENT_HEIGHT = max_y + 16;
    }
}

/// Phase 10: pull the current page URL out of the sentinel root
/// node emitted by `dom_extract.js` and copy it into `URL`.
///
/// The proxy stamps `tag="__url__"` and `text=window.location.href`
/// on the first node whenever it walks the DOM. We look for that
/// exact tag on any of the first few nodes and copy the text into
/// the URL bar, so clicking a link updates the address in place.
unsafe fn sync_url_from_fbp() {
    if FBP_BYTES == 0 { return; }
    let slice = core::slice::from_raw_parts(fbp_buf_ptr(), FBP_BYTES);
    let view = match fbp_rs::parse_state_update(slice) {
        Ok(v) => v,
        Err(_) => return,
    };
    // Only scan the first 4 nodes — the sentinel is always at idx 0
    // or 1 if present.
    let limit = view.nodes.len().min(4);
    for i in 0..limit {
        let node = &view.nodes[i];
        let tag = view.tag(node);
        if tag == b"__url__" {
            let url = view.text(node);
            let n = url.len().min(MAX_URL - 1);
            let u = core::ptr::addr_of_mut!(URL) as *mut u8;
            for j in 0..n { *u.add(j) = url[j]; }
            URL_LEN = n;
            CURSOR_POS = n;
            return;
        }
    }
}

unsafe fn fetch_page() {
    LOADING = true;
    SCROLL_Y = 0;
    FBP_BYTES = 0;

    let inner = inner_url_slice();
    let buf_ptr = fbp_buf_ptr_mut();
    let n = folk_fbp_recv(
        inner.as_ptr() as i32,
        inner.len() as i32,
        buf_ptr as i32,
        FBP_BUF_SIZE as i32,
    );

    if n > 0 {
        FBP_BYTES = n as usize;
        folk_log_telemetry(12, n, 0);
        refresh_content_height();
        // Echo back the proxy's view of the current URL (Phase 10
        // tracks redirects and post-click navigation here).
        sync_url_from_fbp();
        remember_loaded_url();
    }

    LOADING = false;
    EDITING_URL = false;
}

/// Snapshot the current `URL` into `LOADED_URL` so later navigations
/// can push it onto the back stack, even after the user scribbles over
/// the URL bar.
unsafe fn remember_loaded_url() {
    let src = core::ptr::addr_of!(URL) as *const u8;
    let dst = core::ptr::addr_of_mut!(LOADED_URL) as *mut u8;
    for i in 0..URL_LEN { *dst.add(i) = *src.add(i); }
    LOADED_URL_LEN = URL_LEN;
}

/// Send an FBP INTERACTION_EVENT to the proxy on the current URL and
/// load the returned post-interaction DOM into FBP_BUF.
unsafe fn fbp_interact(action: i32, node_id: u32) {
    LOADING = true;
    let inner = inner_url_slice();
    let buf_ptr = fbp_buf_ptr_mut();
    let n = folk_fbp_send(
        inner.as_ptr() as i32,
        inner.len() as i32,
        action,
        node_id as i32,
        buf_ptr as i32,
        FBP_BUF_SIZE as i32,
    );
    if n > 0 {
        FBP_BYTES = n as usize;
        SCROLL_Y = 0;
        folk_log_telemetry(13, n, node_id as i32);
        refresh_content_height();
        // Phase 10: the URL bar follows the click.
        sync_url_from_fbp();
        remember_loaded_url();
    }
    LOADING = false;
}

/// TEMP: return an "interesting" interactable node ID for fallback
/// click targets. Skips the first 30 interactables (typically
/// masthead nav links that point back to the current page) and
/// picks one with non-empty text content, so the click demonstrably
/// navigates to a fresh URL.
unsafe fn first_interactable() -> u32 {
    use fbp_rs::NodeFlags;
    if FBP_BYTES == 0 { return 0; }
    let slice = core::slice::from_raw_parts(fbp_buf_ptr(), FBP_BYTES);
    let view = match fbp_rs::parse_state_update(slice) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let mut idx: u32 = 0;
    let mut skipped: u32 = 0;
    for node in view.nodes {
        idx += 1;
        let is_link = node.flags.contains(NodeFlags::IS_LINK);
        let is_button = node.flags.contains(NodeFlags::IS_BUTTON);
        if !(is_link || is_button) { continue; }
        let text = view.text(node);
        if text.is_empty() { continue; }
        skipped += 1;
        if skipped > 30 {
            return idx;
        }
    }
    0
}

/// Hit-test a mouse click against the current FBP node tree.
///
/// Returns the 1-based node_id of the smallest interactable node
/// (IS_LINK or IS_BUTTON) whose on-screen bounds contain (mx, my),
/// or 0 if no interactable node was hit. "Smallest" so nested links
/// inside a wrapper div get the direct click instead of the wrapper.
unsafe fn hit_test(mx: i32, my: i32) -> u32 {
    use fbp_rs::NodeFlags;
    if FBP_BYTES == 0 { return 0; }
    let slice = core::slice::from_raw_parts(fbp_buf_ptr(), FBP_BYTES);
    let view = match fbp_rs::parse_state_update(slice) {
        Ok(v) => v,
        Err(_) => return 0,
    };

    let scroll = SCROLL_Y;
    let y_offset: i32 = URLBAR_H;

    let mut best_id: u32 = 0;
    let mut best_area: u64 = u64::MAX;

    let mut idx: u32 = 0;
    for node in view.nodes {
        idx += 1;
        // Only interactable nodes are clickable.
        let is_link = node.flags.contains(NodeFlags::IS_LINK);
        let is_button = node.flags.contains(NodeFlags::IS_BUTTON);
        if !is_link && !is_button { continue; }

        let screen_x = node.bounds_x;
        let screen_y = node.bounds_y - scroll + y_offset;
        let w = node.bounds_w as i32;
        let h = node.bounds_h as i32;
        if w <= 0 || h <= 0 { continue; }

        if mx >= screen_x && mx < screen_x + w
            && my >= screen_y && my < screen_y + h
        {
            let area = (w as u64) * (h as u64);
            if area < best_area {
                best_area = area;
                best_id = idx;
            }
        }
    }
    best_id
}

// ── Display list builder ────────────────────────────────────────────

unsafe fn emit_rect(
    dl: *mut u8, pos: &mut usize,
    x: i32, y: i32, w: i32, h: i32, color: i32,
) {
    if *pos + 13 > MAX_DISPLAY_LIST { return; }
    *dl.add(*pos) = 0x01; *pos += 1;
    let vals: [u16; 4] = [x as u16, y as u16, w as u16, h as u16];
    for v in &vals {
        let b = v.to_le_bytes();
        *dl.add(*pos) = b[0]; *dl.add(*pos + 1) = b[1]; *pos += 2;
    }
    let cb = (color as u32).to_le_bytes();
    for j in 0..4 { *dl.add(*pos + j) = cb[j]; }
    *pos += 4;
}

unsafe fn emit_text_raw(
    dl: *mut u8, pos: &mut usize,
    x: i32, y: i32,
    text_ptr: u32, text_len: u16,
    color: i32,
) {
    if text_len == 0 || *pos + 15 > MAX_DISPLAY_LIST { return; }
    if y < -100 || y > 800 { return; }
    *dl.add(*pos) = 0x02; *pos += 1;
    let xb = (x as u16).to_le_bytes();
    *dl.add(*pos) = xb[0]; *dl.add(*pos + 1) = xb[1]; *pos += 2;
    let yb = (y as u16).to_le_bytes();
    *dl.add(*pos) = yb[0]; *dl.add(*pos + 1) = yb[1]; *pos += 2;
    let pb = text_ptr.to_le_bytes();
    for j in 0..4 { *dl.add(*pos + j) = pb[j]; }
    *pos += 4;
    let lb = text_len.to_le_bytes();
    *dl.add(*pos) = lb[0]; *dl.add(*pos + 1) = lb[1]; *pos += 2;
    let cb = (color as u32).to_le_bytes();
    for j in 0..4 { *dl.add(*pos + j) = cb[j]; }
    *pos += 4;
}

/// Greedy word-wrap. Emits one text opcode per wrapped line and
/// returns the number of lines written. All lines share the same
/// `color` and start at `x`; vertical position steps by `FONT_H`
/// between lines.
///
/// The wrap breaks at the last space that fits in `max_chars`. A word
/// longer than `max_chars` is hard-broken mid-word (avoids infinite
/// loops on pathological inputs like long URLs with no spaces).
unsafe fn emit_wrapped_text(
    dl: *mut u8,
    pos: &mut usize,
    x: i32,
    y: i32,
    text_base: *const u8,
    text_base_offset: usize,
    text_len: usize,
    max_chars: usize,
    color: i32,
) -> i32 {
    if text_len == 0 || max_chars == 0 { return 0; }
    let text = core::slice::from_raw_parts(text_base.add(text_base_offset), text_len);
    let base_ptr = text_base as u32 + text_base_offset as u32;

    let mut lines_emitted: i32 = 0;
    let mut i: usize = 0;
    while i < text.len() {
        // Skip leading spaces so wrapped lines don't start with blanks.
        while i < text.len() && text[i] == b' ' { i += 1; }
        if i >= text.len() { break; }

        // Find the longest prefix starting at `i` that fits in
        // `max_chars`. Prefer breaking at a space; hard-break if the
        // first word is itself too long.
        let mut end = i;
        let mut last_space = 0usize;
        let line_start = i;
        while end < text.len() && (end - line_start) < max_chars {
            if text[end] == b' ' { last_space = end; }
            end += 1;
        }

        let line_end = if end == text.len() {
            end
        } else if last_space > line_start {
            last_space
        } else {
            end
        };

        let run_len = line_end - line_start;
        if run_len == 0 { break; }

        let row_y = y + lines_emitted * FONT_H;
        // Don't emit off-screen lines but keep counting so content
        // height stays accurate.
        if row_y >= -FONT_H && row_y < 800 {
            emit_text_raw(
                dl,
                pos,
                x,
                row_y,
                base_ptr + line_start as u32,
                run_len.min(u16::MAX as usize) as u16,
                color,
            );
        }

        lines_emitted += 1;
        i = line_end;
    }

    lines_emitted
}

unsafe fn build_display_list(viewport_h: i32) {
    use fbp_rs::NodeFlags;

    let dl = core::ptr::addr_of_mut!(DISPLAY_LIST) as *mut u8;
    let mut pos: usize = 0;
    let scroll = SCROLL_Y;

    // Background fill (opcode 0x04)
    if pos + 5 <= MAX_DISPLAY_LIST {
        *dl.add(pos) = 0x04; pos += 1;
        let bg_bytes = (BG as u32).to_le_bytes();
        for j in 0..4 { *dl.add(pos + j) = bg_bytes[j]; }
        pos += 4;
    }

    if FBP_BYTES == 0 {
        DL_LEN = pos;
        return;
    }

    let slice = core::slice::from_raw_parts(fbp_buf_ptr(), FBP_BYTES);
    let view = match fbp_rs::parse_state_update(slice) {
        Ok(v) => v,
        Err(_) => {
            DL_LEN = pos;
            return;
        }
    };

    // URL bar covers y = 0..URLBAR_H, shift content down by it
    let y_offset: i32 = URLBAR_H;
    // Track the deepest y coordinate any emitted text/line reached,
    // so CONTENT_HEIGHT (and therefore the scroll clamp) reflects the
    // post-wrap rendered height rather than the proxy's raw bounds.
    let mut max_emitted_y: i32 = 0;

    for node in view.nodes {
        // Skip the Phase 10 sentinel node that carries the current URL.
        let tag = view.tag(node);
        if tag == b"__url__" { continue; }

        let screen_y = node.bounds_y - scroll + y_offset;
        // Cull fully off-screen nodes early — but still emit when even
        // a single wrapped line might land in view.
        if screen_y + (node.bounds_h as i32) < URLBAR_H - FONT_H * 4 { continue; }
        if screen_y > viewport_h + URLBAR_H + FONT_H * 2 { continue; }

        let is_link = node.flags.contains(NodeFlags::IS_LINK);
        let is_button = node.flags.contains(NodeFlags::IS_BUTTON);
        let is_input = node.flags.contains(NodeFlags::HAS_TEXT_INPUT);

        if is_button {
            emit_rect(
                dl, &mut pos,
                node.bounds_x, screen_y,
                node.bounds_w as i32, node.bounds_h as i32,
                BUTTON_BG,
            );
        } else if is_input {
            emit_rect(
                dl, &mut pos,
                node.bounds_x, screen_y,
                node.bounds_w as i32, node.bounds_h as i32,
                INPUT_BG,
            );
        }

        let text = view.text(node);
        if !text.is_empty() {
            let color = if is_button {
                0xFFFFFF_i32
            } else if is_input {
                INPUT_TEXT
            } else if is_link {
                LINK_COLOR
            } else {
                TEXT_COLOR
            };

            let (tx, ty) = if is_button || is_input {
                (node.bounds_x + 8, screen_y + 6)
            } else {
                (node.bounds_x, screen_y)
            };

            // Buttons / input placeholders render as a single line —
            // the bounding box is fixed and clipping is visually OK.
            // Body text uses the wrap path so long paragraphs spread
            // vertically instead of overflowing their x-bounds.
            let lines;
            if is_button || is_input {
                emit_text_raw(
                    dl,
                    &mut pos,
                    tx,
                    ty,
                    text.as_ptr() as u32,
                    text.len().min(u16::MAX as usize) as u16,
                    color,
                );
                lines = 1;
            } else {
                let wrap_width = (node.bounds_w as i32).max(FONT_W);
                let max_chars = ((wrap_width / FONT_W) as usize).max(4);
                let text_base_ptr = view.string_pool.as_ptr();
                let text_offset = (text.as_ptr() as usize)
                    .wrapping_sub(text_base_ptr as usize);
                lines = emit_wrapped_text(
                    dl,
                    &mut pos,
                    tx,
                    ty,
                    text_base_ptr,
                    text_offset,
                    text.len(),
                    max_chars,
                    color,
                );
            }

            // Update content-height tracker from the post-wrap y.
            let doc_y_bottom = (node.bounds_y as i32) + lines * FONT_H + 4;
            if doc_y_bottom > max_emitted_y {
                max_emitted_y = doc_y_bottom;
            }
        } else {
            let doc_bottom = node.bounds_y.saturating_add(node.bounds_h as i32);
            if doc_bottom > max_emitted_y {
                max_emitted_y = doc_bottom;
            }
        }
    }

    // Expose the accurate rendered height so scroll clamping works
    // after text wrapping shoved content further down the page.
    if max_emitted_y > 0 {
        CONTENT_HEIGHT = max_emitted_y + FONT_H;
    }

    DL_LEN = pos;
}

// ── Rendering ───────────────────────────────────────────────────────

unsafe fn draw(x: i32, y: i32, text: &[u8], color: i32) {
    folk_draw_text(x, y, text.as_ptr() as i32, text.len() as i32, color);
}

unsafe fn render() {
    let sw = folk_screen_width();
    let sh = folk_screen_height();
    let usable_w = sw.min(1024);

    build_display_list(sh - URLBAR_H);
    if DL_LEN > 0 {
        folk_submit_display_list(
            core::ptr::addr_of!(DISPLAY_LIST) as i32,
            DL_LEN as i32,
        );
    } else {
        folk_fill_screen(BG);
    }

    // URL bar overlay
    folk_draw_rect(0, 0, usable_w, URLBAR_H, URLBAR_BG);

    // BACK button (24×16) — dimmed when history is empty
    let back_active = HISTORY_COUNT > 0;
    let (back_bg, back_fg) = if back_active {
        (0x1B2838_i32, 0xC9D1D9_i32)
    } else {
        (0x161B22_i32, 0x484F58_i32)
    };
    folk_draw_rect(4, 4, 24, 16, back_bg);
    draw(12, 8, b"<", back_fg);

    // URL text + cursor
    if URL_LEN > 0 {
        let url = core::slice::from_raw_parts(
            core::ptr::addr_of!(URL) as *const u8, URL_LEN);
        let show = URL_LEN.min(((usable_w - 80) / FONT_W) as usize);
        folk_draw_text(36, 8, url.as_ptr() as i32, show as i32, URLBAR_TEXT);
    } else {
        draw(36, 8, b"Type proxy:<url> and press Enter", 0x484F58);
    }

    if EDITING_URL {
        let cx = 36 + (CURSOR_POS as i32) * FONT_W;
        folk_draw_rect(cx, 6, 2, FONT_H, CURSOR_COLOR);
    }

    if LOADING {
        draw(usable_w - 80, 8, b"Loading...", 0xD29922);
    }
}

// ── Input handling ──────────────────────────────────────────────────

unsafe fn handle_input() {
    loop {
        let e = core::ptr::addr_of_mut!(EVT) as *mut i32;
        if folk_poll_event(e as i32) == 0 { break; }
        let event_type = *e.add(0);

        // Mouse click
        if event_type == 2 {
            let mx = *e.add(1);
            let my = *e.add(2);

            // BACK button (top-left).
            if mx >= 4 && mx < 28 && my >= 4 && my < 20 {
                if history_back() {
                    fetch_page();
                    return;
                }
                continue;
            }

            // Otherwise hit-test the DOM tree for a link/button.
            if my >= URLBAR_H {
                let mut node_id = hit_test(mx, my);
                // TEMP DEBUG: if no direct hit, fall back to the
                // first interactable node so we can exercise the
                // full interact pipeline while the mouse driver
                // issues are sorted out.
                if node_id == 0 {
                    node_id = first_interactable();
                }
                if node_id > 0 {
                    let loaded = core::slice::from_raw_parts(
                        core::ptr::addr_of!(LOADED_URL) as *const u8, LOADED_URL_LEN);
                    history_push(loaded);
                    fbp_interact(ACTION_CLICK, node_id);
                    return;
                }
            }
            continue;
        }

        if event_type != 3 { continue; }
        let key = *e.add(3) as u8;

        match key {
            0x0D => { // Enter
                if EDITING_URL && URL_LEN > 0 {
                    let loaded = core::slice::from_raw_parts(
                        core::ptr::addr_of!(LOADED_URL) as *const u8,
                        LOADED_URL_LEN);
                    history_push(loaded);
                    fetch_page();
                }
            }
            0x1B => { // Esc — toggle URL bar editing
                EDITING_URL = !EDITING_URL;
            }
            0x08 => { // Backspace
                if EDITING_URL && CURSOR_POS > 0 {
                    let u = core::ptr::addr_of_mut!(URL) as *mut u8;
                    let mut i = CURSOR_POS - 1;
                    while i + 1 < URL_LEN { *u.add(i) = *u.add(i + 1); i += 1; }
                    URL_LEN -= 1; CURSOR_POS -= 1;
                } else if !EDITING_URL && history_back() {
                    fetch_page();
                    return;
                }
            }
            0x80 => { // Up — scroll up
                if !EDITING_URL {
                    SCROLL_Y = (SCROLL_Y - FONT_H * 3).max(0);
                }
            }
            0x81 => { // Down — scroll down
                if !EDITING_URL {
                    let viewport_h = folk_screen_height() - URLBAR_H;
                    let max_scroll = (CONTENT_HEIGHT - viewport_h).max(0);
                    SCROLL_Y = (SCROLL_Y + FONT_H * 3).min(max_scroll);
                }
            }
            0x82 => { // Page Up
                if !EDITING_URL {
                    let viewport_h = folk_screen_height() - URLBAR_H;
                    SCROLL_Y = (SCROLL_Y - viewport_h + FONT_H * 2).max(0);
                }
            }
            0x83 => { // Page Down
                if !EDITING_URL {
                    let viewport_h = folk_screen_height() - URLBAR_H;
                    let max_scroll = (CONTENT_HEIGHT - viewport_h).max(0);
                    SCROLL_Y = (SCROLL_Y + viewport_h - FONT_H * 2).min(max_scroll);
                }
            }
            0x84 => { // Home — top of page
                if !EDITING_URL { SCROLL_Y = 0; }
            }
            0x85 => { // End — bottom of page
                if !EDITING_URL {
                    let viewport_h = folk_screen_height() - URLBAR_H;
                    SCROLL_Y = (CONTENT_HEIGHT - viewport_h).max(0);
                }
            }
            0x20..=0x7E => {
                if EDITING_URL && URL_LEN < MAX_URL - 1 {
                    let u = core::ptr::addr_of_mut!(URL) as *mut u8;
                    let mut i = URL_LEN;
                    while i > CURSOR_POS { *u.add(i) = *u.add(i - 1); i -= 1; }
                    *u.add(CURSOR_POS) = key;
                    URL_LEN += 1;
                    CURSOR_POS += 1;
                }
            }
            _ => {}
        }
    }
}

// ── Entry point ─────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        if !INITIALIZED {
            // Auto-navigate to Hacker News on first launch — Phase 6
            // defaults us through the live Chromium proxy path.
            let default = b"https://news.ycombinator.com";
            let u = core::ptr::addr_of_mut!(URL) as *mut u8;
            for i in 0..default.len() { *u.add(i) = default[i]; }
            URL_LEN = default.len();
            CURSOR_POS = URL_LEN;
            EDITING_URL = false;
            folk_log_telemetry(0, 0, 0);
            fetch_page();
            INITIALIZED = true;
        }
        handle_input();
        render();
    }
}
