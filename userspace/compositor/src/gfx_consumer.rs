//! Display-list consumer side.
//!
//! Walks a `&[u8]` slice — typically produced by `IpcGraphicsRing::pop_into`
//! into a scratch buffer — and yields a typed `Command` per record. The
//! parser is forward-only and tolerant of unknown opcodes: it advances by
//! the header's `payload_len` so a future opcode landed by a newer producer
//! doesn't desync the rest of the frame.
//!
//! What this PR does NOT do: it does not draw anything. The dispatch from
//! `Command` into the existing `blend.rs` / `font.rs` / window-compositor
//! primitives is a separate PR. Splitting the work this way keeps the
//! parser's wire-format tests independent from the rendering path's
//! integration tests.

extern crate alloc;
use alloc::vec::Vec;

use libfolk::gfx::{
    CommandOpCode,
    DrawRectCmd, DrawTextureCmd, SetClipRectCmd,
};

/// One decoded record from the display list. `DrawText` carries a
/// borrowed slice of the original bytes for its UTF-8 payload — we
/// avoid allocating a `String` in the parser's hot path.
#[derive(Debug, Clone)]
pub enum Command<'a> {
    Sync,
    SetClipRect(SetClipRectCmd),
    DrawRect(DrawRectCmd),
    DrawText {
        x: i32,
        y: i32,
        color_rgba: u32,
        font_size: u16,
        text: &'a [u8],
    },
    DrawTexture(DrawTextureCmd),
    /// An opcode we didn't recognize. The parser still advances past
    /// the payload so subsequent commands stay framed; we surface the
    /// opcode + length so the caller can log it.
    Unknown { opcode: u8, payload_len: u16 },
}

/// What went wrong while parsing. All errors leave the parser at the
/// faulty byte so the caller can include the offset in a serial log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Input ran out mid-header. Either the producer wrote a partial
    /// list or `pop_into` returned a truncated buffer.
    ShortHeader { offset: usize },
    /// Header claimed a payload that overruns the buffer.
    TruncatedPayload { offset: usize, claimed: usize, available: usize },
    /// `DrawText` payload prefix doesn't fit before the text bytes.
    BadDrawTextHeader { offset: usize },
    /// Fixed-size opcode payload doesn't match the struct size.
    PayloadSizeMismatch { offset: usize, opcode: u8, expected: usize, got: usize },
}

/// Fixed prefix of a `DrawText` payload — must mirror
/// `libfolk::gfx::DrawTextHeader` exactly. We don't import the libfolk
/// type because it's `pub(crate)`-ish (only re-exported as a marker
/// type via the builder); duplicating it here keeps the parser
/// independent of producer-side internals.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DrawTextWirePrefix {
    x: i32,
    y: i32,
    color_rgba: u32,
    font_size: u16,
    bytes_len: u16,
}

const DRAW_TEXT_PREFIX_SIZE: usize = core::mem::size_of::<DrawTextWirePrefix>();

/// Parse a complete display list into a `Vec<Command>`. Stops at the
/// first `Sync` and includes that `Sync` as the last command — the
/// caller treats receiving one as "frame complete, commit graph".
///
/// Returns the number of bytes consumed alongside the commands so the
/// caller can advance the ring if it `peek`-ed rather than `pop`-ed.
pub fn parse_display_list(bytes: &[u8]) -> Result<(Vec<Command<'_>>, usize), ParseError> {
    let mut out: Vec<Command> = Vec::new();
    let mut walker = Walker::new(bytes);
    while let Some(cmd) = walker.next_command()? {
        let is_sync = matches!(cmd, Command::Sync);
        out.push(cmd);
        if is_sync { break; }
    }
    Ok((out, walker.consumed()))
}

/// Stateful single-pass walker. Use this when you want to dispatch
/// commands as they're decoded (allocation-free per-command path)
/// instead of materializing a Vec.
pub struct Walker<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Walker<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub fn consumed(&self) -> usize { self.pos }
    pub fn remaining(&self) -> usize { self.bytes.len().saturating_sub(self.pos) }

    pub fn next_command(&mut self) -> Result<Option<Command<'a>>, ParseError> {
        if self.pos >= self.bytes.len() {
            return Ok(None);
        }
        // Header is 3 bytes: opcode (u8) + payload_len (u16 LE).
        if self.remaining() < 3 {
            return Err(ParseError::ShortHeader { offset: self.pos });
        }
        let opcode = self.bytes[self.pos];
        let payload_len = u16::from_le_bytes([self.bytes[self.pos + 1], self.bytes[self.pos + 2]]) as usize;
        let payload_off = self.pos + 3;
        if self.bytes.len() < payload_off + payload_len {
            return Err(ParseError::TruncatedPayload {
                offset: self.pos,
                claimed: payload_len,
                available: self.bytes.len() - payload_off,
            });
        }

        let payload = &self.bytes[payload_off..payload_off + payload_len];
        let advance = 3 + payload_len;
        let here = self.pos;
        let cmd = match CommandOpCode::from_u8(opcode) {
            Some(CommandOpCode::Sync) => Command::Sync,
            Some(CommandOpCode::SetClipRect) => {
                let s = core::mem::size_of::<SetClipRectCmd>();
                if payload.len() != s {
                    return Err(ParseError::PayloadSizeMismatch { offset: here, opcode, expected: s, got: payload.len() });
                }
                // SAFETY: SetClipRectCmd is repr(C, packed) with no
                // padding and only POD fields; reading from a byte
                // slice of the right length is sound.
                let v = unsafe { core::ptr::read_unaligned(payload.as_ptr() as *const SetClipRectCmd) };
                Command::SetClipRect(v)
            }
            Some(CommandOpCode::DrawRect) => {
                let s = core::mem::size_of::<DrawRectCmd>();
                if payload.len() != s {
                    return Err(ParseError::PayloadSizeMismatch { offset: here, opcode, expected: s, got: payload.len() });
                }
                let v = unsafe { core::ptr::read_unaligned(payload.as_ptr() as *const DrawRectCmd) };
                Command::DrawRect(v)
            }
            Some(CommandOpCode::DrawText) => {
                if payload.len() < DRAW_TEXT_PREFIX_SIZE {
                    return Err(ParseError::BadDrawTextHeader { offset: here });
                }
                let prefix = unsafe { core::ptr::read_unaligned(payload.as_ptr() as *const DrawTextWirePrefix) };
                let text_off = DRAW_TEXT_PREFIX_SIZE;
                let bytes_len = prefix.bytes_len as usize;
                if payload.len() < text_off + bytes_len {
                    return Err(ParseError::BadDrawTextHeader { offset: here });
                }
                let text = &payload[text_off..text_off + bytes_len];
                Command::DrawText {
                    x: prefix.x,
                    y: prefix.y,
                    color_rgba: prefix.color_rgba,
                    font_size: prefix.font_size,
                    text,
                }
            }
            Some(CommandOpCode::DrawTexture) => {
                let s = core::mem::size_of::<DrawTextureCmd>();
                if payload.len() != s {
                    return Err(ParseError::PayloadSizeMismatch { offset: here, opcode, expected: s, got: payload.len() });
                }
                let v = unsafe { core::ptr::read_unaligned(payload.as_ptr() as *const DrawTextureCmd) };
                Command::DrawTexture(v)
            }
            None => Command::Unknown { opcode, payload_len: payload_len as u16 },
        };
        self.pos += advance;
        Ok(Some(cmd))
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use libfolk::gfx::DisplayListBuilder;

    #[test]
    fn round_trip_one_rect() {
        let mut b = DisplayListBuilder::new();
        b.draw_rect(DrawRectCmd {
            x: 10, y: 20, width: 100, height: 50,
            color_rgba: 0xAA_BB_CC_DD, corner_radius: 4,
        });
        b.end_frame();
        let (cmds, consumed) = parse_display_list(b.as_slice()).unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(consumed, b.as_slice().len());
        match &cmds[0] {
            Command::DrawRect(r) => {
                let (x, w, c, cr) = (r.x, r.width, r.color_rgba, r.corner_radius);
                assert_eq!(x, 10);
                assert_eq!(w, 100);
                assert_eq!(c, 0xAA_BB_CC_DD);
                assert_eq!(cr, 4);
            }
            other => panic!("expected DrawRect, got {:?}", other),
        }
        assert!(matches!(cmds[1], Command::Sync));
    }

    #[test]
    fn round_trip_text_payload() {
        let mut b = DisplayListBuilder::new();
        b.draw_text(50, 60, 0xFFFFFFFF, 14, "Hi there");
        b.end_frame();
        let (cmds, _) = parse_display_list(b.as_slice()).unwrap();
        match &cmds[0] {
            Command::DrawText { x, y, font_size, text, .. } => {
                assert_eq!(*x, 50);
                assert_eq!(*y, 60);
                assert_eq!(*font_size, 14);
                assert_eq!(*text, b"Hi there".as_slice());
            }
            other => panic!("expected DrawText, got {:?}", other),
        }
    }

    #[test]
    fn unknown_opcode_is_skipped_not_fatal() {
        // Hand-craft: opcode 0xFE, payload 4 bytes, then a real DrawRect.
        let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        buf.push(0xFE);
        buf.extend_from_slice(&4u16.to_le_bytes());
        buf.extend_from_slice(&[0, 1, 2, 3]);
        // DrawRect right after.
        let mut b = DisplayListBuilder::new();
        b.draw_rect(DrawRectCmd {
            x: 1, y: 2, width: 3, height: 4, color_rgba: 5, corner_radius: 0,
        });
        b.end_frame();
        buf.extend_from_slice(b.as_slice());

        let (cmds, _) = parse_display_list(&buf).unwrap();
        assert!(matches!(cmds[0], Command::Unknown { opcode: 0xFE, payload_len: 4 }));
        assert!(matches!(cmds[1], Command::DrawRect(_)));
        assert!(matches!(cmds[2], Command::Sync));
    }

    #[test]
    fn truncated_header_is_an_error() {
        let buf = [0x02u8, 0x00];
        let err = parse_display_list(&buf).unwrap_err();
        assert!(matches!(err, ParseError::ShortHeader { .. }));
    }

    #[test]
    fn truncated_payload_is_an_error() {
        // DrawRect opcode but payload_len declares more than we provide.
        let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        buf.push(CommandOpCode::DrawRect as u8);
        buf.extend_from_slice(&100u16.to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]); // only 4 of the claimed 100
        let err = parse_display_list(&buf).unwrap_err();
        assert!(matches!(err, ParseError::TruncatedPayload { .. }));
    }
}
