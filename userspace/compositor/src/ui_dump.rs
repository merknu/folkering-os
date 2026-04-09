//! UI state serialization for MCP and debugging.
//! Emits JSON-formatted UI dumps between @@UI_DUMP@@ markers.

extern crate alloc;
use alloc::string::String;

use compositor::window_manager::{WindowManager, UiWidget, WindowKind};
use libfolk::sys::io::write_str;

/// Emit UI state dump to serial as minified JSON between markers.
/// Format: @@UI_DUMP@@{json}@@END_UI_DUMP@@
/// All on one line to avoid kernel log interleaving breaking the JSON.
pub fn emit_ui_dump(wm: &WindowManager, omnibar_visible: bool, text_buffer: &[u8], text_len: usize, cursor_pos: usize) {
    // Use a stack-allocated buffer for the JSON string (4KB should be plenty)
    let mut buf = [0u8; 4096];
    let mut pos = 0;

    buf_write(&mut buf, &mut pos, "{\"omnibar\":{\"visible\":");
    if omnibar_visible { buf_write(&mut buf, &mut pos, "true"); } else { buf_write(&mut buf, &mut pos, "false"); }
    if omnibar_visible && text_len > 0 {
        buf_write(&mut buf, &mut pos, ",\"text\":\"");
        buf_write_escaped(&mut buf, &mut pos, &text_buffer[..text_len]);
        buf_write(&mut buf, &mut pos, "\",\"cursor\":");
        buf_write_num(&mut buf, &mut pos, cursor_pos as u32);
    }
    buf_write(&mut buf, &mut pos, "},\"windows\":[");

    let mut first_win = true;
    for window in &wm.windows {
        if !window.visible { continue; }
        if !first_win { buf_write(&mut buf, &mut pos, ","); }
        first_win = false;

        buf_write(&mut buf, &mut pos, "{\"id\":");
        buf_write_num(&mut buf, &mut pos, window.id);
        buf_write(&mut buf, &mut pos, ",\"title\":\"");
        if window.title_len > 0 {
            buf_write_escaped(&mut buf, &mut pos, &window.title[..window.title_len]);
        }
        buf_write(&mut buf, &mut pos, "\"");

        // focused?
        if wm.focused_id == Some(window.id) {
            buf_write(&mut buf, &mut pos, ",\"focused\":true");
        }

        // kind
        buf_write(&mut buf, &mut pos, ",\"kind\":\"");
        match window.kind {
            WindowKind::Terminal => buf_write(&mut buf, &mut pos, "terminal"),
            WindowKind::App => buf_write(&mut buf, &mut pos, "app"),
        }
        buf_write(&mut buf, &mut pos, "\"");

        // Interactive terminal input
        if window.interactive && window.input_len > 0 {
            buf_write(&mut buf, &mut pos, ",\"input\":\"");
            buf_write_escaped(&mut buf, &mut pos, &window.input_buf[..window.input_len]);
            buf_write(&mut buf, &mut pos, "\"");
        }

        // Widgets
        if !window.widgets.is_empty() {
            buf_write(&mut buf, &mut pos, ",\"widgets\":[");
            let mut first_w = true;
            emit_widgets(&window.widgets, &mut buf, &mut pos, &mut first_w, window.focused_widget);
            buf_write(&mut buf, &mut pos, "]");
        }

        // Terminal lines (last few)
        if !window.lines.is_empty() {
            buf_write(&mut buf, &mut pos, ",\"lines\":[");
            // Only include last 5 lines to save space
            let start = if window.lines.len() > 5 { window.lines.len() - 5 } else { 0 };
            for (i, line) in window.lines[start..].iter().enumerate() {
                if i > 0 { buf_write(&mut buf, &mut pos, ","); }
                buf_write(&mut buf, &mut pos, "\"");
                let line_len = line.len.min(line.buf.len());
                buf_write_escaped(&mut buf, &mut pos, &line.buf[..line_len]);
                buf_write(&mut buf, &mut pos, "\"");
            }
            buf_write(&mut buf, &mut pos, "]");
        }

        buf_write(&mut buf, &mut pos, "}");
    }

    buf_write(&mut buf, &mut pos, "],\"focused_id\":");
    if let Some(fid) = wm.focused_id {
        buf_write_num(&mut buf, &mut pos, fid);
    } else {
        buf_write(&mut buf, &mut pos, "null");
    }
    buf_write(&mut buf, &mut pos, "}");

    // Write the complete dump atomically (as much as possible via write_str)
    write_str("@@UI_DUMP@@");
    // Write the JSON portion from buf
    if let Ok(json_str) = core::str::from_utf8(&buf[..pos]) {
        write_str(json_str);
    }
    write_str("@@END_UI_DUMP@@\n");
}

/// Write a string into a buffer at the given position, advancing pos
pub fn buf_write(buf: &mut [u8], pos: &mut usize, s: &str) {
    let bytes = s.as_bytes();
    let end = (*pos + bytes.len()).min(buf.len());
    let copy_len = end - *pos;
    buf[*pos..*pos + copy_len].copy_from_slice(&bytes[..copy_len]);
    *pos += copy_len;
}

/// Write a u32 as decimal into buffer
pub fn buf_write_num(buf: &mut [u8], pos: &mut usize, n: u32) {
    if n == 0 {
        if *pos < buf.len() { buf[*pos] = b'0'; *pos += 1; }
        return;
    }
    let mut digits = [0u8; 10];
    let mut d = 0usize;
    let mut val = n;
    while val > 0 && d < 10 {
        digits[9 - d] = b'0' + (val % 10) as u8;
        val /= 10;
        d += 1;
    }
    for j in (10 - d)..10 {
        if *pos < buf.len() { buf[*pos] = digits[j]; *pos += 1; }
    }
}

/// Write a byte slice as JSON-escaped string content into buffer
pub fn buf_write_escaped(buf: &mut [u8], pos: &mut usize, data: &[u8]) {
    for &b in data {
        if b == b'"' || b == b'\\' {
            if *pos < buf.len() { buf[*pos] = b'\\'; *pos += 1; }
        }
        if *pos < buf.len() { buf[*pos] = b; *pos += 1; }
    }
}

/// Recursively emit widget JSON into buffer
pub fn emit_widgets(widgets: &[UiWidget], buf: &mut [u8], pos: &mut usize, first: &mut bool, focused_idx: Option<usize>) {
    for widget in widgets {
        if !*first { buf_write(buf, pos, ","); }
        *first = false;

        match widget {
            UiWidget::Label { text, text_len, .. } => {
                buf_write(buf, pos, "{\"type\":\"label\",\"text\":\"");
                let len = (*text_len).min(text.len());
                buf_write_escaped(buf, pos, &text[..len]);
                buf_write(buf, pos, "\"}");
            }
            UiWidget::Button { label, label_len, action_id, .. } => {
                buf_write(buf, pos, "{\"type\":\"button\",\"label\":\"");
                let len = (*label_len).min(label.len());
                buf_write_escaped(buf, pos, &label[..len]);
                buf_write(buf, pos, "\",\"action_id\":");
                buf_write_num(buf, pos, *action_id);
                buf_write(buf, pos, "}");
            }
            UiWidget::TextInput { placeholder, placeholder_len, value, value_len, cursor_pos, action_id, .. } => {
                buf_write(buf, pos, "{\"type\":\"textinput\",\"placeholder\":\"");
                let plen = (*placeholder_len).min(placeholder.len());
                buf_write_escaped(buf, pos, &placeholder[..plen]);
                buf_write(buf, pos, "\",\"value\":\"");
                let vlen = (*value_len).min(value.len());
                buf_write_escaped(buf, pos, &value[..vlen]);
                buf_write(buf, pos, "\",\"cursor\":");
                buf_write_num(buf, pos, *cursor_pos as u32);
                buf_write(buf, pos, ",\"action_id\":");
                buf_write_num(buf, pos, *action_id);
                buf_write(buf, pos, "}");
            }
            UiWidget::VStack { children, spacing } => {
                buf_write(buf, pos, "{\"type\":\"vstack\",\"spacing\":");
                buf_write_num(buf, pos, *spacing as u32);
                buf_write(buf, pos, ",\"children\":[");
                let mut child_first = true;
                emit_widgets(children, buf, pos, &mut child_first, focused_idx);
                buf_write(buf, pos, "]}");
            }
            UiWidget::HStack { children, spacing } => {
                buf_write(buf, pos, "{\"type\":\"hstack\",\"spacing\":");
                buf_write_num(buf, pos, *spacing as u32);
                buf_write(buf, pos, ",\"children\":[");
                let mut child_first = true;
                emit_widgets(children, buf, pos, &mut child_first, focused_idx);
                buf_write(buf, pos, "]}");
            }
            UiWidget::Spacer { height } => {
                buf_write(buf, pos, "{\"type\":\"spacer\",\"height\":");
                buf_write_num(buf, pos, *height as u32);
                buf_write(buf, pos, "}");
            }
        }
    }
}

/// Format uptime in ms as "Xm Ys" or "Xs" string
pub fn format_uptime(ms: u64, buf: &mut [u8; 32]) -> &str {
    let secs = ms / 1000;
    let mins = secs / 60;
    let remaining_secs = secs % 60;

    let mut i = 0;

    if mins > 0 {
        // Format minutes
        let mut m = mins;
        let mut digits = [0u8; 10];
        let mut d = 0;
        while m > 0 && d < 10 {
            digits[9 - d] = b'0' + (m % 10) as u8;
            m /= 10;
            d += 1;
        }
        for j in (10 - d)..10 {
            buf[i] = digits[j];
            i += 1;
        }
        buf[i] = b'm';
        i += 1;
        buf[i] = b' ';
        i += 1;
    }

    // Format seconds
    let mut s = remaining_secs;
    let mut digits = [0u8; 10];
    let mut d = 0;
    if s == 0 {
        buf[i] = b'0';
        i += 1;
    } else {
        while s > 0 && d < 10 {
            digits[9 - d] = b'0' + (s % 10) as u8;
            s /= 10;
            d += 1;
        }
        for j in (10 - d)..10 {
            buf[i] = digits[j];
            i += 1;
        }
    }
    buf[i] = b's';
    i += 1;

    unsafe { core::str::from_utf8_unchecked(&buf[..i]) }
}
