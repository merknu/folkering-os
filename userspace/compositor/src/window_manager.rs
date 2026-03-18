//! Window Manager
//!
//! Manages a stack of windows drawn directly to the framebuffer.
//! No pixel-buffer compositing — windows render directly to the FB
//! during the composite pass.

extern crate alloc;
use alloc::vec::Vec;
use super::framebuffer::FramebufferView;
use super::font::FONT_8X16;

// ===== Constants =====

pub const TITLE_BAR_H: usize = 22;
pub const BORDER_W: usize = 2;
const CLOSE_BTN_W: usize = 16;
const CLOSE_BTN_MARGIN: usize = 4;

// Window chrome colors (RGB24 packed)
const WIN_TITLE_BG_FOCUSED:   u32 = 0x1a3a6e;   // deep blue
const WIN_TITLE_BG_UNFOCUSED: u32 = 0x1a2a3e;
const WIN_TITLE_FG:           u32 = 0xffffff;
const WIN_BORDER_FOCUSED:     u32 = 0xff9900;   // bright orange — visible against dark bg
const WIN_BORDER_UNFOCUSED:   u32 = 0x556677;
const WIN_BG:                 u32 = 0x0d1f30;   // dark navy
const WIN_TEXT_FG:            u32 = 0x99ffcc;   // bright green text
const WIN_CLOSE_BG:           u32 = 0xcc2244;
const WIN_CLOSE_FG:           u32 = 0xffffff;

// ===== Text line storage =====

pub struct TextLine {
    pub buf: [u8; 128],
    pub len: usize,
}

impl TextLine {
    pub const fn empty() -> Self {
        Self { buf: [0; 128], len: 0 }
    }
    pub fn as_str(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }
}

// ===== Window =====

pub enum WindowKind {
    /// Text-only terminal-style window
    Terminal,
}

pub struct Window {
    pub id: u32,
    pub title: [u8; 32],
    pub title_len: usize,
    pub x: i32,
    pub y: i32,
    pub width: u32,   // content width (inside border)
    pub height: u32,  // content height (inside border, below title bar)
    pub lines: Vec<TextLine>,
    pub kind: WindowKind,
    pub visible: bool,
    pub dragging: bool,
    pub drag_offset_x: i32,
    pub drag_offset_y: i32,
    // Interactive terminal input
    pub interactive: bool,
    pub input_buf: [u8; 128],
    pub input_len: usize,
    pub input_cursor: usize,
}

impl Window {
    /// Total pixel width (content + 2 * BORDER_W)
    pub fn total_w(&self) -> usize { self.width as usize + BORDER_W * 2 }
    /// Total pixel height (title bar + content + 2 * BORDER_W)
    pub fn total_h(&self) -> usize { TITLE_BAR_H + self.height as usize + BORDER_W * 2 }

    /// Get the current input as a string
    pub fn input_str(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.input_buf[..self.input_len]) }
    }

    /// Clear input buffer
    pub fn clear_input(&mut self) {
        self.input_len = 0;
        self.input_cursor = 0;
        self.input_buf = [0u8; 128];
    }

    /// Append a text line to this window (drops oldest if full)
    pub fn push_line(&mut self, text: &str) {
        const MAX_LINES: usize = 30;
        let mut line = TextLine::empty();
        let bytes = text.as_bytes();
        let len = bytes.len().min(127);
        line.buf[..len].copy_from_slice(&bytes[..len]);
        line.len = len;
        if self.lines.len() >= MAX_LINES {
            self.lines.remove(0);
        }
        self.lines.push(line);
    }
}

// ===== Window Manager =====

pub struct WindowManager {
    pub windows: Vec<Window>,
    next_id: u32,
    pub focused_id: Option<u32>,
}

impl WindowManager {
    pub fn new() -> Self {
        Self {
            windows: Vec::new(),
            next_id: 1,
            focused_id: None,
        }
    }

    /// Create a new terminal window, returns its ID.
    pub fn create_terminal(
        &mut self,
        title: &str,
        x: i32, y: i32,
        width: u32, height: u32,
    ) -> u32 {
        let id = self.next_id;
        self.next_id += 1;

        let mut title_arr = [0u8; 32];
        let tlen = title.len().min(31);
        title_arr[..tlen].copy_from_slice(&title.as_bytes()[..tlen]);

        let win = Window {
            id,
            title: title_arr,
            title_len: tlen,
            x, y,
            width, height,
            lines: Vec::new(),
            kind: WindowKind::Terminal,
            visible: true,
            dragging: false,
            drag_offset_x: 0,
            drag_offset_y: 0,
            interactive: false,
            input_buf: [0u8; 128],
            input_len: 0,
            input_cursor: 0,
        };
        self.windows.push(win);
        self.focused_id = Some(id);
        id
    }

    pub fn close_window(&mut self, id: u32) {
        self.windows.retain(|w| w.id != id);
        if self.focused_id == Some(id) {
            self.focused_id = self.windows.last().map(|w| w.id);
        }
    }

    pub fn get_window_mut(&mut self, id: u32) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    /// Hit-test: returns (window_id, HitZone) for given screen coordinate.
    pub fn hit_test(&self, cx: i32, cy: i32) -> Option<(u32, HitZone)> {
        // Test in reverse (topmost window first)
        for win in self.windows.iter().rev() {
            if !win.visible { continue; }
            let wx = win.x;
            let wy = win.y;
            let ww = win.total_w() as i32;
            let wh = win.total_h() as i32;
            if cx < wx || cx >= wx + ww || cy < wy || cy >= wy + wh {
                continue;
            }
            // Inside this window — check sub-zones
            let rel_x = cx - wx;
            let rel_y = cy - wy;
            // Close button (top-right corner of title bar)
            let close_x = ww - (CLOSE_BTN_W + CLOSE_BTN_MARGIN) as i32;
            if rel_y >= BORDER_W as i32 && rel_y < (BORDER_W + TITLE_BAR_H) as i32
                && rel_x >= close_x
            {
                return Some((win.id, HitZone::CloseButton));
            }
            // Title bar (for dragging)
            if rel_y >= BORDER_W as i32 && rel_y < (BORDER_W + TITLE_BAR_H) as i32 {
                return Some((win.id, HitZone::TitleBar));
            }
            // Content
            return Some((win.id, HitZone::Content));
        }
        None
    }

    pub fn focus(&mut self, id: u32) {
        self.focused_id = Some(id);
        // Bring focused window to top of stack
        if let Some(pos) = self.windows.iter().position(|w| w.id == id) {
            let win = self.windows.remove(pos);
            self.windows.push(win);
        }
    }

    /// Composite all visible windows to the framebuffer.
    pub fn composite(&self, fb: &mut FramebufferView) {
        for win in &self.windows {
            if win.visible {
                let focused = self.focused_id == Some(win.id);
                draw_window(fb, win, focused);
            }
        }
    }

    /// Returns true if any window is visible.
    pub fn has_visible(&self) -> bool {
        self.windows.iter().any(|w| w.visible)
    }
}

// ===== Hit zone =====

#[derive(Copy, Clone, PartialEq)]
pub enum HitZone {
    TitleBar,
    CloseButton,
    Content,
}

// ===== Drawing helpers =====

fn rgb(fb: &FramebufferView, rgb24: u32) -> u32 {
    fb.color_from_rgb24(rgb24)
}

/// Draw a complete window (frame + content) to the framebuffer.
fn draw_window(fb: &mut FramebufferView, win: &Window, focused: bool) {
    let wx = win.x;
    let wy = win.y;
    let total_w = win.total_w();
    let total_h = win.total_h();
    let content_w = win.width as usize;
    let content_h = win.height as usize;

    // Clip to framebuffer
    if wx < 0 || wy < 0 { return; }
    let wx = wx as usize;
    let wy = wy as usize;
    if wx + total_w > fb.width || wy + total_h > fb.height { return; }

    // Border color
    let border_col = rgb(fb, if focused { WIN_BORDER_FOCUSED } else { WIN_BORDER_UNFOCUSED });
    let title_bg   = rgb(fb, if focused { WIN_TITLE_BG_FOCUSED } else { WIN_TITLE_BG_UNFOCUSED });
    let title_fg   = rgb(fb, WIN_TITLE_FG);
    let win_bg     = rgb(fb, WIN_BG);

    // Draw outer border
    fb.draw_rect(wx, wy, total_w, total_h, border_col);
    if BORDER_W > 1 {
        fb.draw_rect(wx + 1, wy + 1, total_w - 2, total_h - 2, border_col);
    }

    // Title bar background
    fb.fill_rect(
        wx + BORDER_W,
        wy + BORDER_W,
        total_w - BORDER_W * 2,
        TITLE_BAR_H,
        title_bg,
    );

    // Title text (centered vertically in title bar)
    let title_str = unsafe {
        core::str::from_utf8_unchecked(&win.title[..win.title_len])
    };
    let ty = wy + BORDER_W + (TITLE_BAR_H - 8) / 2;  // 8px tall (small font), center
    draw_str_small(fb, wx + BORDER_W + 6, ty, title_str, title_fg, title_bg);

    // Close button
    let close_x = wx + total_w - CLOSE_BTN_W - CLOSE_BTN_MARGIN - BORDER_W;
    let close_y = wy + BORDER_W + (TITLE_BAR_H - CLOSE_BTN_W) / 2;
    let close_bg = rgb(fb, WIN_CLOSE_BG);
    let close_fg = rgb(fb, WIN_CLOSE_FG);
    fb.fill_rect(close_x, close_y, CLOSE_BTN_W, CLOSE_BTN_W, close_bg);
    // Draw X in close button using small font
    let x_char_x = close_x + (CLOSE_BTN_W - 8) / 2;
    let x_char_y = close_y + (CLOSE_BTN_W - 8) / 2;
    draw_char_small(fb, x_char_x, x_char_y, 'x', close_fg, close_bg);

    // Content area background
    let content_x = wx + BORDER_W;
    let content_y = wy + BORDER_W + TITLE_BAR_H;
    fb.fill_rect(content_x, content_y, content_w, content_h, win_bg);

    // Draw text lines
    let text_fg = rgb(fb, WIN_TEXT_FG);
    let line_h = 10usize;  // 8px char + 2px gap
    let text_x = content_x + 6;
    let mut text_y = content_y + 6;
    let max_lines = (content_h as usize).saturating_sub(12) / line_h;

    let skip = if win.lines.len() > max_lines {
        win.lines.len() - max_lines
    } else {
        0
    };

    // Reserve space for input prompt in interactive windows
    let reserved_lines = if win.interactive { 2 } else { 0 };
    let display_max = max_lines.saturating_sub(reserved_lines);

    let skip = if win.lines.len() > display_max {
        win.lines.len() - display_max
    } else {
        0
    };

    for line in win.lines.iter().skip(skip) {
        if text_y + 8 > content_y + content_h.saturating_sub(if win.interactive { 20 } else { 0 }) {
            break;
        }
        draw_str_small(fb, text_x, text_y, line.as_str(), text_fg, win_bg);
        text_y += line_h;
    }

    // Draw input prompt for interactive windows
    if win.interactive {
        let prompt_y = content_y + content_h.saturating_sub(14);
        let prompt_fg = rgb(fb, 0x00CCFF); // cyan prompt
        let cursor_fg = rgb(fb, 0xFFFFFF);

        // Draw separator line
        let sep_y = prompt_y.saturating_sub(3);
        fb.fill_rect(content_x + 2, sep_y, content_w.saturating_sub(4), 1, rgb(fb, 0x334455));

        // Draw "folk> " prompt + input text
        draw_str_small(fb, text_x, prompt_y, "folk>", prompt_fg, win_bg);
        let input_str = win.input_str();
        draw_str_small(fb, text_x + 48, prompt_y, input_str, cursor_fg, win_bg);

        // Draw cursor
        let cursor_x = text_x + 48 + win.input_cursor * 8;
        if cursor_x + 8 <= content_x + content_w && focused {
            fb.fill_rect(cursor_x, prompt_y, 8, 8, cursor_fg);
        }
    }
}

/// Draw a string using the 8×8 upper half of the 8×16 font (compact for window text).
fn draw_str_small(fb: &mut FramebufferView, mut x: usize, y: usize, s: &str, fg: u32, bg: u32) {
    for ch in s.chars() {
        if x + 8 > fb.width { break; }
        draw_char_small(fb, x, y, ch, fg, bg);
        x += 8;
    }
}

/// Draw a single character using the top 8 rows of the 8×16 font glyph.
fn draw_char_small(fb: &mut FramebufferView, x: usize, y: usize, ch: char, fg: u32, bg: u32) {
    let idx = ch as usize;
    let glyph = if idx < 256 { &FONT_8X16[idx] } else { &FONT_8X16[0] };
    for row in 0..8usize {
        let bits = glyph[row];
        for col in 0..8usize {
            if x + col >= fb.width || y + row >= fb.height { continue; }
            let color = if (bits >> (7 - col)) & 1 != 0 { fg } else { bg };
            fb.set_pixel(x + col, y + row, color);
        }
    }
}
