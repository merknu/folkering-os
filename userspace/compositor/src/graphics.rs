//! Graphics primitives — Bresenham line and midpoint circle
//!
//! All functions bounds-check every pixel before drawing to prevent
//! out-of-bounds panics from off-screen coordinates (i32 → negative values).

use super::framebuffer::FramebufferView;

/// Draw a line using Bresenham's algorithm. Coordinates are i32 to handle
/// off-screen values safely. Each pixel is bounds-checked before drawing.
pub fn draw_line(fb: &mut FramebufferView, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    let mut x0 = x0;
    let mut y0 = y0;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx: i32 = if x0 < x1 { 1 } else { -1 };
    let sy: i32 = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let w = fb.width as i32;
    let h = fb.height as i32;

    loop {
        if x0 >= 0 && x0 < w && y0 >= 0 && y0 < h {
            fb.set_pixel(x0 as usize, y0 as usize, color);
        }
        if x0 == x1 && y0 == y1 { break; }
        let e2 = 2 * err;
        if e2 >= dy { err += dy; x0 += sx; }
        if e2 <= dx { err += dx; y0 += sy; }
    }
}

/// Draw a circle outline using the midpoint circle algorithm.
/// Coordinates are i32 to handle off-screen centers. Each pixel is bounds-checked.
pub fn draw_circle(fb: &mut FramebufferView, cx: i32, cy: i32, r: i32, color: u32) {
    if r <= 0 { return; }
    let w = fb.width as i32;
    let h = fb.height as i32;
    let mut x = r;
    let mut y: i32 = 0;
    let mut d: i32 = 1 - x;

    while x >= y {
        // Draw 8 symmetric octant points
        let points = [
            (cx + x, cy + y), (cx - x, cy + y),
            (cx + x, cy - y), (cx - x, cy - y),
            (cx + y, cy + x), (cx - y, cy + x),
            (cx + y, cy - x), (cx - y, cy - x),
        ];
        for &(px, py) in &points {
            if px >= 0 && px < w && py >= 0 && py < h {
                fb.set_pixel(px as usize, py as usize, color);
            }
        }
        y += 1;
        if d <= 0 {
            d += 2 * y + 1;
        } else {
            x -= 1;
            d += 2 * (y - x) + 1;
        }
    }
}
