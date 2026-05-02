//! Display-list opcodes and packed wire structures.
//!
//! Each command is `[CommandHeader | payload]`. Header is fixed at 3 bytes
//! (1B opcode + 2B payload length, little-endian implicit on x86_64). The
//! payload is whatever `repr(C, packed)` struct corresponds to the opcode.
//! `payload_len` is redundant for fixed-size opcodes but keeps the format
//! self-framing — the consumer can skip an unknown opcode by jumping
//! `header.payload_len` bytes ahead.
//!
//! All multi-byte fields are little-endian. We're x86_64-only, so this is
//! free; the format becomes a problem if the OS is ever ported to a
//! big-endian target, but that's deliberately out of scope.

extern crate alloc;
use alloc::vec::Vec;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOpCode {
    /// End-of-frame marker. Tells the compositor "this list is complete,
    /// commit it to the render graph and present". Zero-length payload.
    Sync = 0x00,
    /// Push a scissor rect onto the clip stack (or pop if width=0,height=0).
    SetClipRect = 0x01,
    /// Solid filled rectangle, optionally with rounded corners.
    DrawRect = 0x02,
    /// UTF-8 text run rendered against a pre-uploaded font atlas. Variable
    /// payload — see `display_list.rs::DrawTextHeader` for the fixed prefix.
    DrawText = 0x03,
    /// Hardware blit from a texture resource (sprite atlas).
    DrawTexture = 0x04,
}

impl CommandOpCode {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0x00 => Some(Self::Sync),
            0x01 => Some(Self::SetClipRect),
            0x02 => Some(Self::DrawRect),
            0x03 => Some(Self::DrawText),
            0x04 => Some(Self::DrawTexture),
            _ => None,
        }
    }
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct CommandHeader {
    pub opcode: u8,
    pub payload_len: u16,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct DrawRectCmd {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub color_rgba: u32,
    pub corner_radius: u16,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct DrawTextureCmd {
    pub texture_id: u32,
    pub dest_x: i32,
    pub dest_y: i32,
    pub dest_width: u32,
    pub dest_height: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub opacity: u8,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct SetClipRectCmd {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Fixed prefix of a `DrawText` payload. The variable tail is the UTF-8
/// bytes; total length is `core::mem::size_of::<DrawTextHeader>() + bytes_len`,
/// which is what `payload_len` in the outer `CommandHeader` reports.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct DrawTextHeader {
    pub x: i32,
    pub y: i32,
    pub color_rgba: u32,
    pub font_size: u16,
    pub bytes_len: u16,
}

// ── Builder ────────────────────────────────────────────────────────────

/// Builder that accumulates display-list bytes into a heap buffer, ready to
/// be `push`-ed onto an `IpcGraphicsRing` in one shot. Heap-backed because
/// (1) we don't know the final size up front, and (2) keeping it heap-side
/// means the producer doesn't have to interleave atomics into its render
/// loop. One memcpy at the end is the price.
pub struct DisplayListBuilder {
    buf: Vec<u8>,
}

impl Default for DisplayListBuilder {
    fn default() -> Self { Self::new() }
}

impl DisplayListBuilder {
    pub fn new() -> Self {
        // 2 KiB is enough for ~30 typical commands; grows if needed.
        Self { buf: Vec::with_capacity(2048) }
    }

    pub fn with_capacity(n: usize) -> Self {
        Self { buf: Vec::with_capacity(n) }
    }

    /// Bytes pending serialization.
    pub fn len(&self) -> usize { self.buf.len() }
    pub fn is_empty(&self) -> bool { self.buf.is_empty() }
    pub fn as_slice(&self) -> &[u8] { &self.buf }

    fn write_header(&mut self, opcode: CommandOpCode, payload_len: u16) {
        self.buf.push(opcode as u8);
        self.buf.extend_from_slice(&payload_len.to_le_bytes());
    }

    fn write_struct<T: Copy>(&mut self, value: &T) {
        // SAFETY: `T: Copy` guarantees no destructor, and we treat the bytes
        // as opaque. All consumers of the wire format must use the same
        // `repr(C, packed)` structs.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                value as *const T as *const u8,
                core::mem::size_of::<T>(),
            )
        };
        self.buf.extend_from_slice(bytes);
    }

    pub fn draw_rect(&mut self, cmd: DrawRectCmd) -> &mut Self {
        self.write_header(CommandOpCode::DrawRect, core::mem::size_of::<DrawRectCmd>() as u16);
        self.write_struct(&cmd);
        self
    }

    pub fn set_clip_rect(&mut self, cmd: SetClipRectCmd) -> &mut Self {
        self.write_header(CommandOpCode::SetClipRect, core::mem::size_of::<SetClipRectCmd>() as u16);
        self.write_struct(&cmd);
        self
    }

    pub fn draw_texture(&mut self, cmd: DrawTextureCmd) -> &mut Self {
        self.write_header(CommandOpCode::DrawTexture, core::mem::size_of::<DrawTextureCmd>() as u16);
        self.write_struct(&cmd);
        self
    }

    pub fn draw_text(&mut self, x: i32, y: i32, color_rgba: u32, font_size: u16, text: &str) -> &mut Self {
        let header = DrawTextHeader {
            x, y, color_rgba, font_size,
            bytes_len: text.len() as u16,
        };
        let payload = core::mem::size_of::<DrawTextHeader>() + text.len();
        self.write_header(CommandOpCode::DrawText, payload as u16);
        self.write_struct(&header);
        self.buf.extend_from_slice(text.as_bytes());
        self
    }

    /// Finalize with a `Sync` marker. The compositor uses this as the
    /// frame boundary — everything before it is in this frame, everything
    /// after starts the next frame.
    pub fn end_frame(&mut self) -> &mut Self {
        self.write_header(CommandOpCode::Sync, 0);
        self
    }

    /// Reset the builder to empty without freeing the backing buffer.
    /// Useful when the same builder is reused frame-to-frame.
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_size_is_three() {
        // Important: the wire format encodes header as 3 bytes (1+2). If
        // someone adds a field, the parser breaks silently. This is a
        // tripwire.
        assert_eq!(core::mem::size_of::<CommandHeader>(), 3);
    }

    #[test]
    fn build_and_walk_one_rect() {
        let mut b = DisplayListBuilder::new();
        b.draw_rect(DrawRectCmd {
            x: 10, y: 20, width: 100, height: 50,
            color_rgba: 0xFF_AA_22_11,
            corner_radius: 4,
        });
        b.end_frame();

        let bytes = b.as_slice();
        assert_eq!(bytes[0], CommandOpCode::DrawRect as u8);
        let payload_len = u16::from_le_bytes([bytes[1], bytes[2]]);
        assert_eq!(payload_len as usize, core::mem::size_of::<DrawRectCmd>());

        // Last 3 bytes should be the Sync header.
        let n = bytes.len();
        assert_eq!(bytes[n - 3], CommandOpCode::Sync as u8);
        assert_eq!(bytes[n - 2], 0);
        assert_eq!(bytes[n - 1], 0);
    }

    #[test]
    fn variable_text_payload() {
        let mut b = DisplayListBuilder::new();
        b.draw_text(0, 0, 0xFFFFFFFF, 14, "hi");
        let bytes = b.as_slice();
        assert_eq!(bytes[0], CommandOpCode::DrawText as u8);
        let payload_len = u16::from_le_bytes([bytes[1], bytes[2]]) as usize;
        assert_eq!(payload_len, core::mem::size_of::<DrawTextHeader>() + 2);
    }
}
