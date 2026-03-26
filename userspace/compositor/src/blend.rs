//! SIMD-Optimized Alpha Blending for the Neural Desktop
//!
//! Provides fast pixel compositing using integer-only arithmetic.
//! Fast-paths for fully opaque (alpha=255) and transparent (alpha=0).
//!
//! Formula: result = (src * alpha + dst * (255 - alpha)) >> 8

/// Blend a single ARGB pixel over a background pixel.
/// `src`: 0xAARRGGBB (alpha in bits 24-31)
/// `dst`: 0x00RRGGBB (background, alpha ignored)
/// Returns: composited 0x00RRGGBB
#[inline(always)]
pub fn blend_pixel(src: u32, dst: u32) -> u32 {
    let alpha = src >> 24;

    // Fast-path: fully opaque or transparent
    if alpha >= 255 { return src & 0x00FFFFFF; }
    if alpha == 0 { return dst; }

    let inv_alpha = 255 - alpha;

    // Unpack channels
    let sr = (src >> 16) & 0xFF;
    let sg = (src >> 8) & 0xFF;
    let sb = src & 0xFF;
    let dr = (dst >> 16) & 0xFF;
    let dg = (dst >> 8) & 0xFF;
    let db = dst & 0xFF;

    // Integer blend: (src * alpha + dst * inv_alpha) >> 8
    let r = (sr * alpha + dr * inv_alpha) >> 8;
    let g = (sg * alpha + dg * inv_alpha) >> 8;
    let b = (sb * alpha + db * inv_alpha) >> 8;

    (r << 16) | (g << 8) | b
}

/// Fill a rectangle with an alpha-blended color.
/// `color`: 0xAARRGGBB
pub fn fill_rect_alpha(
    fb: *mut u32,
    fb_pitch_px: u32,
    fb_width: u32,
    fb_height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: u32,
) {
    let alpha = color >> 24;
    if alpha == 0 { return; }

    let opaque = alpha >= 255;
    let solid_color = color & 0x00FFFFFF;

    for row in y..(y + h).min(fb_height) {
        let row_base = (row * fb_pitch_px) as usize;
        for col in x..(x + w).min(fb_width) {
            let idx = row_base + col as usize;
            if opaque {
                unsafe { *fb.add(idx) = solid_color; }
            } else {
                let dst = unsafe { *fb.add(idx) };
                unsafe { *fb.add(idx) = blend_pixel(color, dst); }
            }
        }
    }
}

/// Draw a horizontal line with alpha blending.
pub fn hline_alpha(
    fb: *mut u32,
    fb_pitch_px: u32,
    fb_width: u32,
    y: u32,
    x_start: u32,
    x_end: u32,
    color: u32,
) {
    let alpha = color >> 24;
    if alpha == 0 { return; }

    let row_base = (y * fb_pitch_px) as usize;
    let end = x_end.min(fb_width);

    if alpha >= 255 {
        let solid = color & 0x00FFFFFF;
        for col in x_start..end {
            unsafe { *fb.add(row_base + col as usize) = solid; }
        }
    } else {
        for col in x_start..end {
            let idx = row_base + col as usize;
            let dst = unsafe { *fb.add(idx) };
            unsafe { *fb.add(idx) = blend_pixel(color, dst); }
        }
    }
}
