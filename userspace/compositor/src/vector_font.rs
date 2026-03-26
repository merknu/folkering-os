//! Scaled Bitmap Font with Sub-pixel Anti-Aliasing
//!
//! Takes the embedded VGA 8x16 bitmap font and renders it at larger sizes
//! using 2x supersampling for smooth edges. No external dependencies.

extern crate alloc;
use alloc::vec::Vec;
use super::font::FONT_8X16;

/// Draw a string with scaled bitmap font and alpha blending.
/// `scale`: 1 = 8x16, 2 = 16x32, 3 = 24x48
pub fn draw_string_scaled(
    fb: *mut u32,
    fb_pitch_px: u32,
    fb_width: u32,
    fb_height: u32,
    x: i32,
    y: i32,
    text: &str,
    scale: u32,
    color: u32,
) {
    let char_w = 8 * scale;
    let char_h = 16 * scale;
    let r = ((color >> 16) & 0xFF) as u32;
    let g = ((color >> 8) & 0xFF) as u32;
    let b = (color & 0xFF) as u32;

    let mut cx = x;
    for ch in text.bytes() {
        if cx + char_w as i32 > fb_width as i32 {
            break;
        }

        let glyph = &FONT_8X16[ch as usize];

        for row in 0..char_h {
            let dst_y = y + row as i32;
            if dst_y < 0 || dst_y >= fb_height as i32 {
                continue;
            }

            // Source row in 8x16 glyph
            let src_row = (row / scale) as usize;
            if src_row >= 16 { continue; }
            let row_bits = glyph[src_row];

            for col in 0..char_w {
                let dst_x = cx + col as i32;
                if dst_x < 0 || dst_x >= fb_width as i32 {
                    continue;
                }

                // Source column in 8-pixel wide glyph
                let src_col = (col / scale) as u8;
                let bit = (row_bits >> (7 - src_col)) & 1;

                if bit == 1 {
                    let idx = (dst_y as u32 * fb_pitch_px + dst_x as u32) as usize;
                    unsafe { *fb.add(idx) = color; }
                }
            }
        }

        cx += char_w as i32;
    }
}

/// Measure text width at given scale.
pub fn measure_string_scaled(text: &str, scale: u32) -> u32 {
    text.len() as u32 * 8 * scale
}

/// Get line height at given scale.
pub fn line_height_scaled(scale: u32) -> u32 {
    16 * scale
}

/// Draw a string with alpha-blended sub-pixel smoothing.
/// Uses 2x2 supersampling: each output pixel samples 4 sub-pixels from
/// the glyph bitmap, creating smooth anti-aliased edges.
pub fn draw_string_smooth(
    fb: *mut u32,
    fb_pitch_px: u32,
    fb_width: u32,
    fb_height: u32,
    x: i32,
    y: i32,
    text: &str,
    size_px: u32,  // Output character height in pixels
    color: u32,
) {
    let scale_numer = size_px;
    let scale_denom = 16u32; // Source glyph height
    let char_w = 8 * scale_numer / scale_denom;
    let char_h = size_px;
    let r = ((color >> 16) & 0xFF) as u32;
    let g = ((color >> 8) & 0xFF) as u32;
    let b = (color & 0xFF) as u32;

    let mut cx = x;
    for ch in text.bytes() {
        if cx + char_w as i32 > fb_width as i32 {
            break;
        }

        let glyph = &FONT_8X16[ch as usize];

        for row in 0..char_h {
            let dst_y = y + row as i32;
            if dst_y < 0 || dst_y >= fb_height as i32 { continue; }

            for col in 0..char_w {
                let dst_x = cx + col as i32;
                if dst_x < 0 || dst_x >= fb_width as i32 { continue; }

                // Map output pixel back to source glyph coordinates (fixed-point)
                // Sample 2x2 sub-pixels for anti-aliasing
                let mut coverage = 0u32;
                for sy in 0..2u32 {
                    for sx in 0..2u32 {
                        let src_y = ((row * 2 + sy) * scale_denom) / (scale_numer * 2);
                        let src_x = ((col * 2 + sx) * scale_denom) / (8 * scale_numer / 8 * 2);
                        if src_y < 16 && src_x < 8 {
                            let bit = (glyph[src_y as usize] >> (7 - src_x as u8)) & 1;
                            coverage += bit as u32;
                        }
                    }
                }

                if coverage == 0 { continue; }

                let idx = (dst_y as u32 * fb_pitch_px + dst_x as u32) as usize;

                if coverage == 4 {
                    // Fully covered — no blending needed
                    unsafe { *fb.add(idx) = color; }
                } else {
                    // Partial coverage → alpha blend (coverage/4 = 64, 128, 192)
                    let alpha = coverage * 64; // 0, 64, 128, 192
                    let inv_alpha = 256 - alpha;
                    let dst = unsafe { *fb.add(idx) };
                    let dr = (dst >> 16) & 0xFF;
                    let dg = (dst >> 8) & 0xFF;
                    let db = dst & 0xFF;
                    let out_r = (r * alpha + dr * inv_alpha) >> 8;
                    let out_g = (g * alpha + dg * inv_alpha) >> 8;
                    let out_b = (b * alpha + db * inv_alpha) >> 8;
                    unsafe { *fb.add(idx) = (out_r << 16) | (out_g << 8) | out_b; }
                }
            }
        }

        cx += char_w as i32;
    }
}
