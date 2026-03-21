//! Native UI Schema — binary serialization for declarative widget trees
//!
//! Apps describe their UI as a tree of widgets. The tree is serialized
//! into a shmem buffer and sent to the Compositor via IPC for rendering.
//!
//! # Wire Format
//! ```text
//! Header: [magic:4="FKUI"][ver:1][title_len:1][width:2][height:2][title:N]
//! Widget: [tag:1][type-specific...][children recursively]
//! ```
//!
//! No alloc required — serialization writes directly to a byte buffer,
//! deserialization reads from a byte slice.

// ===== Magic & Tags =====

pub const UI_MAGIC: [u8; 4] = *b"FKUI";
pub const UI_VERSION: u8 = 1;

pub const TAG_LABEL: u8 = 0x01;
pub const TAG_BUTTON: u8 = 0x02;
pub const TAG_VSTACK: u8 = 0x03;
pub const TAG_HSTACK: u8 = 0x04;
pub const TAG_SPACER: u8 = 0x05;
pub const TAG_TEXT_INPUT: u8 = 0x06;

// ===== Serialization (no alloc) =====

/// Writer that appends bytes to a fixed buffer
pub struct UiWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> UiWriter<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Write the UI window header
    pub fn header(&mut self, title: &str, width: u16, height: u16) {
        self.bytes(&UI_MAGIC);
        self.byte(UI_VERSION);
        let tlen = title.len().min(63);
        self.byte(tlen as u8);
        self.u16(width);
        self.u16(height);
        self.bytes(&title.as_bytes()[..tlen]);
    }

    /// Write a Label widget
    pub fn label(&mut self, text: &str, color: u32) {
        let tlen = text.len().min(63);
        self.byte(TAG_LABEL);
        self.byte(tlen as u8);
        self.u32(color);
        self.bytes(&text.as_bytes()[..tlen]);
    }

    /// Write a Button widget
    pub fn button(&mut self, label: &str, action_id: u32, bg: u32, fg: u32) {
        let llen = label.len().min(31);
        self.byte(TAG_BUTTON);
        self.byte(llen as u8);
        self.u32(action_id);
        self.u32(bg);
        self.u32(fg);
        self.bytes(&label.as_bytes()[..llen]);
    }

    /// Begin a VStack (call children, then nothing — count is fixed at begin)
    pub fn vstack_begin(&mut self, spacing: u16, child_count: u8) {
        self.byte(TAG_VSTACK);
        self.u16(spacing);
        self.byte(child_count);
    }

    /// Begin an HStack
    pub fn hstack_begin(&mut self, spacing: u16, child_count: u8) {
        self.byte(TAG_HSTACK);
        self.u16(spacing);
        self.byte(child_count);
    }

    /// Write a Spacer
    pub fn spacer(&mut self, height: u16) {
        self.byte(TAG_SPACER);
        self.u16(height);
    }

    /// Write a TextInput widget
    pub fn text_input(&mut self, placeholder: &str, action_id: u32, max_len: u8) {
        let plen = placeholder.len().min(63);
        self.byte(TAG_TEXT_INPUT);
        self.byte(plen as u8);
        self.u32(action_id);
        self.byte(max_len.min(63));
        self.bytes(&placeholder.as_bytes()[..plen]);
    }

    /// Get the number of bytes written
    pub fn len(&self) -> usize {
        self.pos
    }

    // Internal helpers
    fn byte(&mut self, v: u8) {
        if self.pos < self.buf.len() {
            self.buf[self.pos] = v;
            self.pos += 1;
        }
    }

    fn bytes(&mut self, data: &[u8]) {
        let end = (self.pos + data.len()).min(self.buf.len());
        let len = end - self.pos;
        self.buf[self.pos..end].copy_from_slice(&data[..len]);
        self.pos = end;
    }

    fn u16(&mut self, v: u16) {
        let b = v.to_le_bytes();
        self.byte(b[0]);
        self.byte(b[1]);
    }

    fn u32(&mut self, v: u32) {
        let b = v.to_le_bytes();
        self.bytes(&b);
    }
}

// ===== Deserialization (no alloc) =====

/// Parsed UI window header
pub struct UiHeader<'a> {
    pub title: &'a str,
    pub width: u16,
    pub height: u16,
    pub widget_data: &'a [u8],
}

/// Parse UI header from buffer
pub fn parse_header(buf: &[u8]) -> Option<UiHeader<'_>> {
    if buf.len() < 10 { return None; }
    if &buf[0..4] != &UI_MAGIC { return None; }
    if buf[4] != UI_VERSION { return None; }

    let tlen = buf[5] as usize;
    let width = u16::from_le_bytes([buf[6], buf[7]]);
    let height = u16::from_le_bytes([buf[8], buf[9]]);

    if buf.len() < 10 + tlen { return None; }
    let title = core::str::from_utf8(&buf[10..10+tlen]).ok()?;
    let widget_data = &buf[10+tlen..];

    Some(UiHeader { title, width, height, widget_data })
}

/// Parsed widget — returned one at a time during iteration
pub enum ParsedWidget<'a> {
    Label { text: &'a str, color: u32 },
    Button { label: &'a str, action_id: u32, bg: u32, fg: u32 },
    VStackBegin { spacing: u16, child_count: u8 },
    HStackBegin { spacing: u16, child_count: u8 },
    Spacer { height: u16 },
    TextInput { placeholder: &'a str, action_id: u32, max_len: u8 },
}

/// Parse a single widget from bytes. Returns (widget, bytes_consumed).
pub fn parse_widget(buf: &[u8]) -> Option<(ParsedWidget<'_>, usize)> {
    if buf.is_empty() { return None; }

    let tag = buf[0];
    let mut pos = 1;

    match tag {
        TAG_LABEL => {
            if buf.len() < pos + 5 { return None; }
            let tlen = buf[pos] as usize; pos += 1;
            let color = u32::from_le_bytes([buf[pos], buf[pos+1], buf[pos+2], buf[pos+3]]); pos += 4;
            if buf.len() < pos + tlen { return None; }
            let text = core::str::from_utf8(&buf[pos..pos+tlen]).ok()?; pos += tlen;
            Some((ParsedWidget::Label { text, color }, pos))
        }
        TAG_BUTTON => {
            if buf.len() < pos + 13 { return None; }
            let llen = buf[pos] as usize; pos += 1;
            let action_id = u32::from_le_bytes([buf[pos], buf[pos+1], buf[pos+2], buf[pos+3]]); pos += 4;
            let bg = u32::from_le_bytes([buf[pos], buf[pos+1], buf[pos+2], buf[pos+3]]); pos += 4;
            let fg = u32::from_le_bytes([buf[pos], buf[pos+1], buf[pos+2], buf[pos+3]]); pos += 4;
            if buf.len() < pos + llen { return None; }
            let label = core::str::from_utf8(&buf[pos..pos+llen]).ok()?; pos += llen;
            Some((ParsedWidget::Button { label, action_id, bg, fg }, pos))
        }
        TAG_VSTACK => {
            if buf.len() < pos + 3 { return None; }
            let spacing = u16::from_le_bytes([buf[pos], buf[pos+1]]); pos += 2;
            let child_count = buf[pos]; pos += 1;
            Some((ParsedWidget::VStackBegin { spacing, child_count }, pos))
        }
        TAG_HSTACK => {
            if buf.len() < pos + 3 { return None; }
            let spacing = u16::from_le_bytes([buf[pos], buf[pos+1]]); pos += 2;
            let child_count = buf[pos]; pos += 1;
            Some((ParsedWidget::HStackBegin { spacing, child_count }, pos))
        }
        TAG_SPACER => {
            if buf.len() < pos + 2 { return None; }
            let height = u16::from_le_bytes([buf[pos], buf[pos+1]]); pos += 2;
            Some((ParsedWidget::Spacer { height }, pos))
        }
        TAG_TEXT_INPUT => {
            if buf.len() < pos + 6 { return None; }
            let plen = buf[pos] as usize; pos += 1;
            let action_id = u32::from_le_bytes([buf[pos], buf[pos+1], buf[pos+2], buf[pos+3]]); pos += 4;
            let max_len = buf[pos]; pos += 1;
            if buf.len() < pos + plen { return None; }
            let placeholder = core::str::from_utf8(&buf[pos..pos+plen]).ok()?; pos += plen;
            Some((ParsedWidget::TextInput { placeholder, action_id, max_len }, pos))
        }
        _ => None,
    }
}
