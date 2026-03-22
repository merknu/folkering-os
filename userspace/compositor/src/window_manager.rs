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
    /// ULTRA 45: Safe UTF-8 conversion — returns empty string for partial sequences
    /// rather than panicking or producing garbage.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

// ===== UI Widget System (Milestone 4: Native UI Schema) =====

/// Widget types for AI-generated app UIs.
/// Apps describe their UI declaratively; the compositor renders it.
#[derive(Clone)]
pub enum UiWidget {
    /// Text label — static display text
    Label {
        text: [u8; 64],
        text_len: usize,
        color: u32,      // RGB24
    },
    /// Clickable button with label and action ID
    Button {
        label: [u8; 32],
        label_len: usize,
        action_id: u32,   // sent back to app on click
        bg_color: u32,
        fg_color: u32,
    },
    /// Vertical stack — children laid out top-to-bottom
    VStack {
        children: Vec<UiWidget>,
        spacing: u16,
    },
    /// Horizontal stack — children laid out left-to-right
    HStack {
        children: Vec<UiWidget>,
        spacing: u16,
    },
    /// Spacer — flexible empty space
    Spacer {
        height: u16,
    },
    /// Text input field — editable text with placeholder
    TextInput {
        placeholder: [u8; 64],
        placeholder_len: usize,
        action_id: u32,
        max_len: u8,
        // Mutable — compositor owns editing state
        value: [u8; 64],
        value_len: usize,
        cursor_pos: usize,
    },
}

impl UiWidget {
    pub fn label(text: &str, color: u32) -> Self {
        let mut buf = [0u8; 64];
        let len = text.len().min(63);
        buf[..len].copy_from_slice(&text.as_bytes()[..len]);
        UiWidget::Label { text: buf, text_len: len, color }
    }

    pub fn button(label: &str, action_id: u32, bg: u32, fg: u32) -> Self {
        let mut buf = [0u8; 32];
        let len = label.len().min(31);
        buf[..len].copy_from_slice(&label.as_bytes()[..len]);
        UiWidget::Button { label: buf, label_len: len, action_id, bg_color: bg, fg_color: fg }
    }

    pub fn text_input(placeholder: &str, action_id: u32, max_len: u8) -> Self {
        let mut buf = [0u8; 64];
        let len = placeholder.len().min(63);
        buf[..len].copy_from_slice(&placeholder.as_bytes()[..len]);
        UiWidget::TextInput {
            placeholder: buf,
            placeholder_len: len,
            action_id,
            max_len: max_len.min(63),
            value: [0u8; 64],
            value_len: 0,
            cursor_pos: 0,
        }
    }
}

// ===== Focusable Widget System (Milestone 16: TextInput) =====

#[derive(Clone, Copy, PartialEq)]
pub enum FocusableKind {
    Button { action_id: u32 },
    TextInput { action_id: u32 },
}

// ===== Window =====

pub enum WindowKind {
    /// Text-only terminal-style window
    Terminal,
    /// Widget-based UI window (for AI-generated apps)
    App,
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
    pub widgets: Vec<UiWidget>,   // Milestone 4: declarative UI widgets
    pub kind: WindowKind,
    pub visible: bool,
    pub dragging: bool,
    pub drag_offset_x: i32,
    pub drag_offset_y: i32,
    // Owner task ID (for sending events back via IPC)
    pub owner_task: u32,
    // Interactive terminal input
    pub interactive: bool,
    pub input_buf: [u8; 128],
    pub input_len: usize,
    pub input_cursor: usize,
    // Keyboard focus: index among all buttons (flattened), None = no focus
    pub focused_widget: Option<usize>,
    /// ULTRA 38: Dirty flag — set when content changes, cleared after redraw
    pub dirty: bool,
    /// True while AI is streaming tokens to this window (shows typing cursor)
    pub typing: bool,
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
        self.dirty = true;
    }

    /// Append raw bytes to window text, handling newlines and line wrapping.
    /// ULTRA 44: Scrolling buffer — remove oldest line when MAX_LINES exceeded.
    /// ULTRA 45: Caller must ensure data is valid UTF-8 (inference server validates).
    pub fn append_text(&mut self, data: &[u8]) {
        const MAX_LINES: usize = 30;
        for &byte in data {
            if byte == b'\n' {
                self.push_line("");
                continue;
            }
            // Ensure we have a line to append to
            if self.lines.is_empty() {
                self.push_line("");
            }
            let last_idx = self.lines.len() - 1;
            let last = &mut self.lines[last_idx];
            if last.len >= 127 {
                // ULTRA 44: Line full → auto-wrap
                self.push_line("");
                let last_idx2 = self.lines.len() - 1;
                let last2 = &mut self.lines[last_idx2];
                last2.buf[last2.len] = byte;
                last2.len += 1;
            } else {
                last.buf[last.len] = byte;
                last.len += 1;
            }
        }
        self.dirty = true;
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
            widgets: Vec::with_capacity(32),
            kind: WindowKind::Terminal,
            owner_task: 0,
            visible: true,
            dragging: false,
            drag_offset_x: 0,
            drag_offset_y: 0,
            interactive: false,
            input_buf: [0u8; 128],
            input_len: 0,
            input_cursor: 0,
            focused_widget: None,
            dirty: true,
            typing: false,
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

    pub fn get_window(&self, id: u32) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
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

    /// Cycle focus to next window (Alt+Tab). Returns (title_buf, title_len) of newly focused window.
    pub fn cycle_next_window(&mut self) -> Option<([u8; 32], usize)> {
        if self.windows.len() < 2 { return None; }
        // Move topmost visible window to the bottom of the stack
        let last = self.windows.len() - 1;
        if self.windows[last].visible {
            let win = self.windows.remove(last);
            self.windows.insert(0, win);
        }
        // Focus the new topmost visible window
        for w in self.windows.iter().rev() {
            if w.visible {
                self.focused_id = Some(w.id);
                return Some((w.title, w.title_len));
            }
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

    // Drop shadow: dark semi-transparent band below and right of window
    // Focused windows get a larger, more visible shadow for depth
    let shadow_offset: usize = if focused { 6 } else { 3 };
    let shadow_alpha: u8 = if focused { 120 } else { 60 };
    // Bottom shadow
    fb.fill_rect_alpha(
        wx + shadow_offset,
        wy + total_h,
        total_w,
        shadow_offset,
        0x000000,
        shadow_alpha,
    );
    // Right shadow
    fb.fill_rect_alpha(
        wx + total_w,
        wy + shadow_offset,
        shadow_offset,
        total_h,
        0x000000,
        shadow_alpha,
    );

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

    // Typewriter cursor: solid block at end of last line while AI is streaming
    if win.typing {
        let visible_lines = win.lines.len().saturating_sub(skip);
        let cursor_col = win.lines.last().map(|l| l.len).unwrap_or(0);
        let cursor_x = text_x + cursor_col * 8;
        let cursor_y = content_y + 6 + visible_lines.saturating_sub(1) * line_h;
        if cursor_x + 8 <= content_x + content_w && cursor_y + 8 <= content_y + content_h {
            fb.fill_rect(cursor_x, cursor_y, 8, 8, text_fg);
        }
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

    // Draw widgets for App windows (Milestone 4)
    if !win.widgets.is_empty() {
        let mut wy_cursor = if win.lines.is_empty() { content_y + 6 } else { text_y + 4 };
        let mut focus_counter = 0;
        draw_widgets(fb, &win.widgets, content_x + 6, wy_cursor, content_w.saturating_sub(12), win_bg, win.focused_widget, &mut focus_counter);
    }
}

/// Focus ring color (bright cyan)
const FOCUS_RING_COLOR: u32 = 0x00CCFF;

/// Recursively render UI widgets with optional keyboard focus highlight
fn draw_widgets(fb: &mut FramebufferView, widgets: &[UiWidget], x: usize, mut y: usize, max_w: usize, bg: u32, focus_idx: Option<usize>, focus_counter: &mut usize) {
    for widget in widgets {
        match widget {
            UiWidget::Label { text, text_len, color } => {
                let s = unsafe { core::str::from_utf8_unchecked(&text[..*text_len]) };
                draw_str_small(fb, x, y, s, rgb(fb, *color), bg);
                y += 12;
            }
            UiWidget::Button { label, label_len, bg_color, fg_color, .. } => {
                let s = unsafe { core::str::from_utf8_unchecked(&label[..*label_len]) };
                let btn_w = (*label_len * 8 + 16).min(max_w);
                let btn_h = 16;
                let is_focused = focus_idx == Some(*focus_counter);
                *focus_counter += 1;
                let btn_bg = rgb(fb, *bg_color);
                let btn_fg = rgb(fb, *fg_color);
                if is_focused {
                    fb.fill_rect(x, y, btn_w, btn_h, btn_fg);
                    draw_str_small(fb, x + 8, y + 4, s, btn_bg, btn_fg);
                    let focus_col = rgb(fb, FOCUS_RING_COLOR);
                    fb.draw_rect(x.saturating_sub(1), y.saturating_sub(1), btn_w + 2, btn_h + 2, focus_col);
                } else {
                    fb.fill_rect(x, y, btn_w, btn_h, btn_bg);
                    fb.draw_rect(x, y, btn_w, btn_h, btn_fg);
                    draw_str_small(fb, x + 8, y + 4, s, btn_fg, btn_bg);
                }
                y += btn_h + 4;
            }
            UiWidget::TextInput { placeholder, placeholder_len, value, value_len, cursor_pos, .. } => {
                let input_w = max_w.min(200);
                let input_h = 16;
                let is_focused = focus_idx == Some(*focus_counter);
                *focus_counter += 1;

                let border_col = if is_focused { rgb(fb, FOCUS_RING_COLOR) } else { rgb(fb, 0x556677) };
                let field_bg = rgb(fb, 0x0a1520);

                fb.fill_rect(x, y, input_w, input_h, field_bg);
                fb.draw_rect(x, y, input_w, input_h, border_col);

                if *value_len > 0 {
                    let s = unsafe { core::str::from_utf8_unchecked(&value[..*value_len]) };
                    draw_str_small(fb, x + 4, y + 4, s, rgb(fb, 0xFFFFFF), field_bg);
                } else {
                    let s = unsafe { core::str::from_utf8_unchecked(&placeholder[..*placeholder_len]) };
                    draw_str_small(fb, x + 4, y + 4, s, rgb(fb, 0x667788), field_bg);
                }

                // Cursor (thin line when focused)
                if is_focused {
                    let cx = x + 4 + *cursor_pos * 8;
                    if cx + 2 <= x + input_w {
                        fb.fill_rect(cx, y + 2, 2, input_h - 4, rgb(fb, 0xFFFFFF));
                    }
                }
                y += input_h + 4;
            }
            UiWidget::VStack { children, spacing } => {
                for child in children {
                    draw_widgets(fb, core::slice::from_ref(child), x, y, max_w, bg, focus_idx, focus_counter);
                    y += widget_height(child) + *spacing as usize;
                }
            }
            UiWidget::HStack { children, spacing } => {
                let mut hx = x;
                for child in children {
                    draw_widgets(fb, core::slice::from_ref(child), hx, y, max_w, bg, focus_idx, focus_counter);
                    hx += widget_width(child) + *spacing as usize;
                }
                y += children.iter().map(widget_height).max().unwrap_or(12);
            }
            UiWidget::Spacer { height } => {
                y += *height as usize;
            }
        }
    }
}

/// Estimate widget height for layout
fn widget_height(w: &UiWidget) -> usize {
    match w {
        UiWidget::Label { .. } => 12,
        UiWidget::Button { .. } => 20,
        UiWidget::TextInput { .. } => 20,
        UiWidget::Spacer { height } => *height as usize,
        UiWidget::VStack { children, spacing } => {
            children.iter().map(|c| widget_height(c) + *spacing as usize).sum::<usize>()
        }
        UiWidget::HStack { children, .. } => {
            children.iter().map(widget_height).max().unwrap_or(0)
        }
    }
}

/// Estimate widget width for layout
fn widget_width(w: &UiWidget) -> usize {
    match w {
        UiWidget::Label { text_len, .. } => *text_len * 8,
        UiWidget::Button { label_len, .. } => *label_len * 8 + 16,
        UiWidget::TextInput { .. } => 200,
        UiWidget::Spacer { .. } => 0,
        _ => 100, // default
    }
}

/// Count the number of Button widgets in the tree (recursively)
pub fn count_buttons(widgets: &[UiWidget]) -> usize {
    count_focusable(widgets)
}

/// Find action_id for the N-th button (0-indexed, depth-first order)
pub fn nth_button_action_id(widgets: &[UiWidget], n: usize) -> Option<u32> {
    match nth_focusable(widgets, n) {
        Some(FocusableKind::Button { action_id }) => Some(action_id),
        Some(FocusableKind::TextInput { action_id }) => Some(action_id),
        None => None,
    }
}

/// Count the number of focusable widgets (Button + TextInput) in the tree
pub fn count_focusable(widgets: &[UiWidget]) -> usize {
    let mut count = 0;
    for w in widgets {
        match w {
            UiWidget::Button { .. } | UiWidget::TextInput { .. } => { count += 1; }
            UiWidget::VStack { children, .. } | UiWidget::HStack { children, .. } => {
                count += count_focusable(children);
            }
            _ => {}
        }
    }
    count
}

/// Find the N-th focusable widget's kind (0-indexed, depth-first order)
pub fn nth_focusable(widgets: &[UiWidget], n: usize) -> Option<FocusableKind> {
    fn inner(widgets: &[UiWidget], target: usize, counter: &mut usize) -> Option<FocusableKind> {
        for w in widgets {
            match w {
                UiWidget::Button { action_id, .. } => {
                    if *counter == target { return Some(FocusableKind::Button { action_id: *action_id }); }
                    *counter += 1;
                }
                UiWidget::TextInput { action_id, .. } => {
                    if *counter == target { return Some(FocusableKind::TextInput { action_id: *action_id }); }
                    *counter += 1;
                }
                UiWidget::VStack { children, .. } | UiWidget::HStack { children, .. } => {
                    if let Some(kind) = inner(children, target, counter) { return Some(kind); }
                }
                _ => {}
            }
        }
        None
    }
    let mut counter = 0;
    inner(widgets, n, &mut counter)
}

/// Get a mutable reference to the N-th focusable widget (for text editing)
pub fn nth_focusable_mut(widgets: &mut [UiWidget], n: usize) -> Option<&mut UiWidget> {
    // Use raw pointer to work around nested borrow issues
    fn inner(widgets: &mut [UiWidget], target: usize, counter: &mut usize) -> Option<*mut UiWidget> {
        for w in widgets.iter_mut() {
            match w {
                UiWidget::Button { .. } | UiWidget::TextInput { .. } => {
                    if *counter == target {
                        return Some(w as *mut UiWidget);
                    }
                    *counter += 1;
                }
                UiWidget::VStack { children, .. } | UiWidget::HStack { children, .. } => {
                    if let Some(ptr) = inner(children, target, counter) { return Some(ptr); }
                }
                _ => {}
            }
        }
        None
    }
    let mut counter = 0;
    let ptr = inner(widgets, n, &mut counter)?;
    Some(unsafe { &mut *ptr })
}

/// Hit-test widgets to find focusable widget index at click position
pub fn hit_test_focusable_index(widgets: &[UiWidget], x: usize, mut y: usize, click_x: usize, click_y: usize) -> Option<usize> {
    fn inner(widgets: &[UiWidget], x: usize, y: &mut usize, click_x: usize, click_y: usize, counter: &mut usize) -> Option<usize> {
        for widget in widgets {
            match widget {
                UiWidget::Label { .. } => { *y += 12; }
                UiWidget::Button { label_len, .. } => {
                    let btn_w = *label_len * 8 + 16;
                    let btn_h = 16;
                    if click_x >= x && click_x < x + btn_w && click_y >= *y && click_y < *y + btn_h {
                        return Some(*counter);
                    }
                    *counter += 1;
                    *y += btn_h + 4;
                }
                UiWidget::TextInput { .. } => {
                    let input_w = 200;
                    let input_h = 16;
                    if click_x >= x && click_x < x + input_w && click_y >= *y && click_y < *y + input_h {
                        return Some(*counter);
                    }
                    *counter += 1;
                    *y += input_h + 4;
                }
                UiWidget::VStack { children, spacing } => {
                    for child in children {
                        if let Some(idx) = inner(core::slice::from_ref(child), x, y, click_x, click_y, counter) {
                            return Some(idx);
                        }
                        *y += widget_height(child) + *spacing as usize;
                    }
                }
                UiWidget::HStack { children, spacing } => {
                    let mut hx = x;
                    let save_y = *y;
                    for child in children {
                        *y = save_y;
                        if let Some(idx) = inner(core::slice::from_ref(child), hx, y, click_x, click_y, counter) {
                            return Some(idx);
                        }
                        hx += widget_width(child) + *spacing as usize;
                    }
                    *y = save_y + children.iter().map(widget_height).max().unwrap_or(0);
                }
                UiWidget::Spacer { height } => { *y += *height as usize; }
            }
        }
        None
    }
    let mut counter = 0;
    inner(widgets, x, &mut y, click_x, click_y, &mut counter)
}

/// Get text content from the N-th focusable widget (0-indexed, depth-first).
/// TextInput → value, Button → label. Returns (buf, len).
pub fn nth_focusable_text(widgets: &[UiWidget], n: usize) -> Option<([u8; 64], usize)> {
    fn inner(widgets: &[UiWidget], target: usize, counter: &mut usize) -> Option<([u8; 64], usize)> {
        for w in widgets {
            match w {
                UiWidget::TextInput { value, value_len, .. } => {
                    if *counter == target {
                        let mut buf = [0u8; 64];
                        buf[..*value_len].copy_from_slice(&value[..*value_len]);
                        return Some((buf, *value_len));
                    }
                    *counter += 1;
                }
                UiWidget::Button { label, label_len, .. } => {
                    if *counter == target {
                        let mut buf = [0u8; 64];
                        let len = (*label_len).min(64);
                        buf[..len].copy_from_slice(&label[..len]);
                        return Some((buf, len));
                    }
                    *counter += 1;
                }
                UiWidget::VStack { children, .. } | UiWidget::HStack { children, .. } => {
                    if let Some(result) = inner(children, target, counter) { return Some(result); }
                }
                _ => {}
            }
        }
        None
    }
    let mut counter = 0;
    inner(widgets, n, &mut counter)
}

/// Find the first Label widget's text content (depth-first).
/// Used as fallback for copy when no widget is focused (e.g. Calc display).
pub fn first_label_text(widgets: &[UiWidget]) -> Option<([u8; 64], usize)> {
    for w in widgets {
        match w {
            UiWidget::Label { text, text_len, .. } => {
                let mut buf = [0u8; 64];
                buf[..*text_len].copy_from_slice(&text[..*text_len]);
                return Some((buf, *text_len));
            }
            UiWidget::VStack { children, .. } | UiWidget::HStack { children, .. } => {
                if let Some(result) = first_label_text(children) { return Some(result); }
            }
            _ => {}
        }
    }
    None
}

/// Hit-test widgets to find clicked focusable widget's kind.
/// Returns Some(FocusableKind) if a Button or TextInput was clicked.
/// Coordinates are absolute screen coordinates.
pub fn hit_test_widgets(widgets: &[UiWidget], x: usize, mut y: usize, click_x: usize, click_y: usize) -> Option<FocusableKind> {
    for widget in widgets {
        match widget {
            UiWidget::Label { .. } => {
                y += 12;
            }
            UiWidget::Button { label_len, action_id, .. } => {
                let btn_w = *label_len * 8 + 16;
                let btn_h = 16;
                if click_x >= x && click_x < x + btn_w
                    && click_y >= y && click_y < y + btn_h {
                    return Some(FocusableKind::Button { action_id: *action_id });
                }
                y += btn_h + 4;
            }
            UiWidget::TextInput { action_id, .. } => {
                let input_w = 200;
                let input_h = 16;
                if click_x >= x && click_x < x + input_w
                    && click_y >= y && click_y < y + input_h {
                    return Some(FocusableKind::TextInput { action_id: *action_id });
                }
                y += input_h + 4;
            }
            UiWidget::VStack { children, spacing } => {
                for child in children {
                    if let Some(kind) = hit_test_widgets(core::slice::from_ref(child), x, y, click_x, click_y) {
                        return Some(kind);
                    }
                    y += widget_height(child) + *spacing as usize;
                }
            }
            UiWidget::HStack { children, spacing } => {
                let mut hx = x;
                for child in children {
                    if let Some(kind) = hit_test_widgets(core::slice::from_ref(child), hx, y, click_x, click_y) {
                        return Some(kind);
                    }
                    hx += widget_width(child) + *spacing as usize;
                }
                y += children.iter().map(widget_height).max().unwrap_or(0);
            }
            UiWidget::Spacer { height } => {
                y += *height as usize;
            }
        }
    }
    None
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
