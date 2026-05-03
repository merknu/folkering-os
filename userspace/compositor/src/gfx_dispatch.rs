//! Display-list → render-primitive dispatcher.
//!
//! `gfx_consumer::Walker` decodes bytes into typed `Command`s. This
//! module walks those commands and calls into the compositor's
//! existing render primitives (`FramebufferView::fill_rect` /
//! `draw_string`) so display-list bytes actually paint pixels.
//!
//! The dispatcher owns a small clip stack that mirrors the rapport's
//! `SetClipRect` semantics: a `width=0,height=0` clip resets, anything
//! else pushes a new scissor that intersects with the previous top.
//! Render primitives don't natively scissor, so we clamp coordinates
//! against the current clip before forwarding — sufficient for the
//! axis-aligned shapes we draw today.
//!
//! Out-of-scope this PR (deliberate):
//! - Rounded corners — `DrawRect::corner_radius` is honored only for
//!   `radius == 0`. Non-zero radii get a square fill plus an inline
//!   diagnostic. Real radius rendering needs a SDF or 4-corner-arc
//!   primitive that doesn't exist yet.
//! - `DrawTexture` — no texture/atlas system in compositor yet, so
//!   these are counted and skipped.
//! - Color-space conversion — display-list colors are RGBA (R high
//!   byte → A low byte). Framebuffer is BGRX. We reorder per pixel.
//! - Hooking into `render_frame()` — library-only this round, same
//!   pattern as #112/#113/#116. Migration of the imperative pipeline
//!   to drive a graph + dispatch is a follow-up.

extern crate alloc;
use alloc::vec::Vec;

use crate::framebuffer::FramebufferView;
use crate::render_graph::Rect as ClipRect;

use crate::gfx_consumer::{Command, ParseError, Walker};

/// Convert RGBA8888 (R high byte) to the XRGB8888 / BGRX format the
/// framebuffer expects. The shadow buffer ignores the alpha byte for
/// non-alpha primitives, so we drop it here. (For alpha-aware paths
/// the caller should switch to `fill_rect_alpha` and feed the alpha
/// byte separately — that's a TODO once the compiler emits opacity.)
#[inline]
fn rgba_to_fb(rgba: u32) -> u32 {
    let r = ((rgba >> 24) & 0xFF) as u8;
    let g = ((rgba >> 16) & 0xFF) as u8;
    let b = ((rgba >> 8) & 0xFF) as u8;
    // BGRX (B is low byte) — matches what `fill_rect` writes when
    // VirtIO-GPU's pixel format is `B8G8R8X8_UNORM`.
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Counters reported back to callers so render_frame() can log how
/// much display-list traffic flowed this frame.
#[derive(Default, Clone, Copy, Debug)]
pub struct DispatchStats {
    pub draw_rects: u32,
    pub draw_texts: u32,
    pub set_clips: u32,
    pub draw_textures_skipped: u32,
    pub unknown_skipped: u32,
}

/// Dispatch every command in `bytes` against `fb`. Stops at the first
/// `Sync` (frame boundary) and returns the consumed-byte count plus
/// the per-opcode counters.
pub fn dispatch_display_list(
    bytes: &[u8],
    fb: &mut FramebufferView,
) -> Result<(usize, DispatchStats), ParseError> {
    let mut stats = DispatchStats::default();
    let mut clips: Vec<ClipRect> = Vec::new();
    let fb_w = fb.width as i64;
    let fb_h = fb.height as i64;

    let mut walker = Walker::new(bytes);
    while let Some(cmd) = walker.next_command()? {
        match cmd {
            Command::Sync => break,

            Command::SetClipRect(c) => {
                stats.set_clips += 1;
                if c.width == 0 && c.height == 0 {
                    // Convention from the rapport: zero-size clip pops
                    // the clip stack. Empty stack means "no clip".
                    clips.pop();
                } else {
                    let new_clip = ClipRect::new(c.x, c.y, c.width, c.height);
                    let pushed = match clips.last() {
                        Some(top) => top.intersection(&new_clip).unwrap_or(*top),
                        None => new_clip,
                    };
                    clips.push(pushed);
                }
            }

            Command::DrawRect(r) => {
                stats.draw_rects += 1;
                // `r` is `repr(C, packed)`, so reading fields by ref
                // is unaligned UB. Copy each field via a local first.
                let (rx, ry, rw, rh, rcolor) = (r.x, r.y, r.width, r.height, r.color_rgba);
                let target = ClipRect::new(rx, ry, rw, rh);
                let clipped = match clips.last() {
                    Some(top) => match target.intersection(top) {
                        Some(c) => c,
                        None => continue, // entirely scissored out
                    },
                    None => target,
                };
                // Final clamp against the framebuffer bounds.
                let fx = clipped.x.max(0) as i64;
                let fy = clipped.y.max(0) as i64;
                let fr = (clipped.x as i64 + clipped.w as i64).min(fb_w);
                let fbottom = (clipped.y as i64 + clipped.h as i64).min(fb_h);
                if fr <= fx || fbottom <= fy { continue; }
                fb.fill_rect(
                    fx as usize,
                    fy as usize,
                    (fr - fx) as usize,
                    (fbottom - fy) as usize,
                    rgba_to_fb(rcolor),
                );
                // corner_radius != 0 currently silently rounds to 0 —
                // see module-level comment for why.
            }

            Command::DrawText { x, y, color_rgba, font_size: _, text } => {
                stats.draw_texts += 1;
                // Walk the UTF-8 byte slice without allocating a String.
                // Each codepoint goes through `draw_char_alpha` with
                // `bg_alpha = 0` so only foreground pixels land on the
                // shadow buffer; the background underneath the glyph
                // (whatever the parent's DrawRect painted) shows
                // through unchanged. The previous code passed
                // `fg == bg` as a "transparent BG" sentinel, but
                // `draw_char` writes bg pixels unconditionally so
                // glyphs rendered as solid fg-coloured rectangles.
                let mut cursor_x = x.max(0) as usize;
                let cursor_y = y.max(0) as usize;
                let fg = rgba_to_fb(color_rgba);
                let s = match core::str::from_utf8(text) {
                    Ok(s) => s,
                    Err(_) => continue, // skip malformed run; producer bug
                };
                // Manual char-by-char so we can scissor to the current
                // clip on each glyph rather than relying on
                // draw_string's wrap-at-edge behaviour.
                for ch in s.chars() {
                    if cursor_x + 8 > fb.width { break; }
                    if let Some(top) = clips.last() {
                        let glyph_rect = ClipRect::new(cursor_x as i32, cursor_y as i32, 8, 16);
                        if top.intersection(&glyph_rect).is_none() {
                            cursor_x += 8;
                            continue;
                        }
                    }
                    if cursor_y + 16 > fb.height { break; }
                    // bg colour is a don't-care when bg_alpha == 0;
                    // pass 0 (black) for stable diagnostics in case
                    // someone later flips the alpha to non-zero.
                    fb.draw_char_alpha(cursor_x, cursor_y, ch, fg, 0, 0);
                    cursor_x += 8;
                }
            }

            Command::DrawTexture(_) => {
                // No texture system yet — count and skip.
                stats.draw_textures_skipped += 1;
            }

            Command::Unknown { .. } => {
                stats.unknown_skipped += 1;
            }
        }
    }

    Ok((walker.consumed(), stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfolk::gfx::{DisplayListBuilder, DrawRectCmd, SetClipRectCmd};

    /// A tiny shadow-buffer-like target for tests. We can't construct
    /// a real `FramebufferView` because it needs Limine boot-info
    /// pointers, so the dispatch tests focus on `Walker` integration
    /// and stat counting; pixel correctness is covered by the
    /// FramebufferView's own tests upstream.
    fn build_clipping_list() -> alloc::vec::Vec<u8> {
        let mut b = DisplayListBuilder::new();
        b.set_clip_rect(SetClipRectCmd { x: 100, y: 100, width: 200, height: 200 });
        b.draw_rect(DrawRectCmd {
            x: 50, y: 50, width: 300, height: 300,
            color_rgba: 0xFF_00_00_FF, corner_radius: 0,
        });
        // Pop clip.
        b.set_clip_rect(SetClipRectCmd { x: 0, y: 0, width: 0, height: 0 });
        b.end_frame();
        b.as_slice().to_vec()
    }

    #[test]
    fn rgba_to_fb_drops_alpha_and_reorders() {
        // Red in RGBA = 0xFF0000FF (R=0xFF, A=0xFF); in BGRX it's 0xFF0000.
        assert_eq!(rgba_to_fb(0xFF_00_00_FF), 0x00FF_0000);
        // Green = 0x00FF00FF → BGRX 0x00FF00.
        assert_eq!(rgba_to_fb(0x00_FF_00_FF), 0x0000_FF00);
        // Blue = 0x0000FFFF → BGRX 0xFF.
        assert_eq!(rgba_to_fb(0x00_00_FF_FF), 0x0000_00FF);
    }

    #[test]
    fn walker_round_trips_set_clip_then_draw_rect() {
        // We test the parser side here (no FramebufferView available
        // in cfg(test)). The intent: confirm the bytes the dispatcher
        // would walk really do contain the SetClipRect → DrawRect →
        // SetClipRect(0,0) → Sync sequence.
        use crate::gfx_consumer::Command;
        use crate::gfx_consumer::parse_display_list;

        let bytes = build_clipping_list();
        let (cmds, _) = parse_display_list(&bytes).unwrap();
        assert_eq!(cmds.len(), 4);
        assert!(matches!(cmds[0], Command::SetClipRect(_)));
        assert!(matches!(cmds[1], Command::DrawRect(_)));
        assert!(matches!(cmds[2], Command::SetClipRect(_)));
        assert!(matches!(cmds[3], Command::Sync));
    }
}
