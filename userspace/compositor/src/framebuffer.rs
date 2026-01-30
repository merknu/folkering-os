//! Framebuffer Graphics
//!
//! Software rasterizer for direct framebuffer access.
//! Provides basic drawing primitives optimized for Write-Combining memory.
//!
//! # Write-Combining Optimization
//!
//! Write-Combining memory batches sequential writes, so:
//! - Write pixels in row order (left-to-right, top-to-bottom)
//! - Avoid reading back from framebuffer (very slow)
//! - Fill entire scanlines when possible
//!
//! # Color Format
//!
//! Assumes 32-bit XRGB (or BGRX depending on framebuffer config):
//! - Byte 0: Blue (or Red)
//! - Byte 1: Green
//! - Byte 2: Red (or Blue)
//! - Byte 3: Reserved (X)

use libfolk::sys::boot_info::FramebufferConfig;

/// Framebuffer view for drawing operations.
///
/// This struct provides a safe interface to the mapped framebuffer memory.
/// All drawing operations are optimized for Write-Combining access patterns.
pub struct FramebufferView {
    /// Pointer to framebuffer memory
    buffer: *mut u8,
    /// Width in pixels
    pub width: usize,
    /// Height in pixels
    pub height: usize,
    /// Bytes per scanline (pitch)
    pitch: usize,
    /// Bytes per pixel
    bpp: usize,
    /// Red shift (bit position)
    red_shift: u8,
    /// Green shift (bit position)
    green_shift: u8,
    /// Blue shift (bit position)
    blue_shift: u8,
}

impl FramebufferView {
    /// Create a new framebuffer view from mapped memory.
    ///
    /// # Safety
    ///
    /// - `buffer` must be a valid pointer to mapped framebuffer memory
    /// - The memory must remain valid for the lifetime of this struct
    /// - Only one FramebufferView should exist at a time (no aliasing)
    pub unsafe fn new(buffer: *mut u8, config: &FramebufferConfig) -> Self {
        Self {
            buffer,
            width: config.width as usize,
            height: config.height as usize,
            pitch: config.pitch as usize,
            bpp: (config.bpp as usize + 7) / 8,
            red_shift: config.red_mask_shift,
            green_shift: config.green_mask_shift,
            blue_shift: config.blue_mask_shift,
        }
    }

    /// Create a color value from RGB components.
    ///
    /// Returns a 32-bit color in the framebuffer's native format.
    #[inline]
    pub fn rgb(&self, r: u8, g: u8, b: u8) -> u32 {
        ((r as u32) << self.red_shift)
            | ((g as u32) << self.green_shift)
            | ((b as u32) << self.blue_shift)
    }

    /// Create a color from a packed 0xRRGGBB value.
    #[inline]
    pub fn color_from_rgb24(&self, rgb: u32) -> u32 {
        let r = ((rgb >> 16) & 0xFF) as u8;
        let g = ((rgb >> 8) & 0xFF) as u8;
        let b = (rgb & 0xFF) as u8;
        self.rgb(r, g, b)
    }

    /// Get pointer to a pixel at (x, y).
    #[inline]
    fn pixel_ptr(&self, x: usize, y: usize) -> *mut u32 {
        debug_assert!(x < self.width && y < self.height);
        unsafe {
            self.buffer.add(y * self.pitch + x * self.bpp) as *mut u32
        }
    }

    /// Set a single pixel.
    #[inline]
    pub fn set_pixel(&mut self, x: usize, y: usize, color: u32) {
        if x < self.width && y < self.height {
            unsafe {
                core::ptr::write_volatile(self.pixel_ptr(x, y), color);
            }
        }
    }

    /// Fill a rectangle with a solid color.
    ///
    /// Optimized for Write-Combining: writes in row order.
    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let end_x = (x + w).min(self.width);
        let end_y = (y + h).min(self.height);
        let start_x = x.min(self.width);
        let start_y = y.min(self.height);

        for row in start_y..end_y {
            let row_start = unsafe { self.buffer.add(row * self.pitch + start_x * self.bpp) };
            let pixels = (end_x - start_x) as isize;

            for i in 0..pixels {
                unsafe {
                    core::ptr::write_volatile(row_start.add(i as usize * self.bpp) as *mut u32, color);
                }
            }
        }
    }

    /// Fill the entire screen with a color.
    pub fn clear(&mut self, color: u32) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Draw a horizontal line.
    pub fn hline(&mut self, x: usize, y: usize, w: usize, color: u32) {
        self.fill_rect(x, y, w, 1, color);
    }

    /// Draw a vertical line.
    pub fn vline(&mut self, x: usize, y: usize, h: usize, color: u32) {
        self.fill_rect(x, y, 1, h, color);
    }

    /// Draw a rectangle outline.
    pub fn draw_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        self.hline(x, y, w, color);
        self.hline(x, y + h - 1, w, color);
        self.vline(x, y, h, color);
        self.vline(x + w - 1, y, h, color);
    }

    /// Draw an 8x16 character using the built-in font.
    ///
    /// # Arguments
    /// * `x` - X position (pixels)
    /// * `y` - Y position (pixels)
    /// * `ch` - Character to draw
    /// * `fg` - Foreground color
    /// * `bg` - Background color
    pub fn draw_char(&mut self, x: usize, y: usize, ch: char, fg: u32, bg: u32) {
        use super::font::FONT_8X16;

        let ch_idx = ch as usize;
        let glyph = if ch_idx < 256 {
            &FONT_8X16[ch_idx]
        } else {
            &FONT_8X16[0] // Use first glyph for unknown chars
        };

        for row in 0..16 {
            let bits = glyph[row];
            for col in 0..8 {
                let color = if (bits >> (7 - col)) & 1 != 0 { fg } else { bg };
                self.set_pixel(x + col, y + row, color);
            }
        }
    }

    /// Draw a string using the built-in 8x16 font.
    ///
    /// Wraps at screen edge. Newlines move to next line.
    pub fn draw_string(&mut self, mut x: usize, mut y: usize, s: &str, fg: u32, bg: u32) {
        for ch in s.chars() {
            if ch == '\n' {
                x = 0;
                y += 16;
                continue;
            }

            if x + 8 > self.width {
                x = 0;
                y += 16;
            }

            if y + 16 > self.height {
                break;
            }

            self.draw_char(x, y, ch, fg, bg);
            x += 8;
        }
    }

    /// Draw a checkerboard pattern (for testing pitch calculation).
    pub fn draw_checkerboard(&mut self, square_size: usize, color1: u32, color2: u32) {
        for y in 0..self.height {
            for x in 0..self.width {
                let checker = ((x / square_size) + (y / square_size)) % 2;
                let color = if checker == 0 { color1 } else { color2 };
                self.set_pixel(x, y, color);
            }
        }
    }

    /// Draw a mouse cursor at the given position.
    ///
    /// Draws a simple arrow cursor (16x16 pixels).
    /// The hotspot is at the top-left corner (0,0) of the cursor.
    pub fn draw_cursor(&mut self, x: usize, y: usize, color: u32, outline: u32) {
        // Simple arrow cursor bitmap (16 pixels tall)
        // 1 = filled, 2 = outline, 0 = transparent
        const CURSOR: [[u8; 12]; 16] = [
            [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [1, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [1, 2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [1, 2, 2, 2, 1, 0, 0, 0, 0, 0, 0, 0],
            [1, 2, 2, 2, 2, 1, 0, 0, 0, 0, 0, 0],
            [1, 2, 2, 2, 2, 2, 1, 0, 0, 0, 0, 0],
            [1, 2, 2, 2, 2, 2, 2, 1, 0, 0, 0, 0],
            [1, 2, 2, 2, 2, 2, 2, 2, 1, 0, 0, 0],
            [1, 2, 2, 2, 2, 2, 2, 2, 2, 1, 0, 0],
            [1, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 0],
            [1, 2, 2, 1, 2, 2, 1, 0, 0, 0, 0, 0],
            [1, 2, 1, 0, 1, 2, 2, 1, 0, 0, 0, 0],
            [1, 1, 0, 0, 1, 2, 2, 1, 0, 0, 0, 0],
            [1, 0, 0, 0, 0, 1, 2, 2, 1, 0, 0, 0],
            [0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0],
        ];

        for (dy, row) in CURSOR.iter().enumerate() {
            for (dx, &pixel) in row.iter().enumerate() {
                let px = x.wrapping_add(dx);
                let py = y.wrapping_add(dy);
                if px < self.width && py < self.height {
                    match pixel {
                        1 => self.set_pixel(px, py, outline),
                        2 => self.set_pixel(px, py, color),
                        _ => {} // transparent
                    }
                }
            }
        }
    }

    /// Read a single pixel value (for cursor save/restore).
    /// Note: This is slow on Write-Combining memory, use sparingly.
    #[inline]
    pub fn get_pixel(&self, x: usize, y: usize) -> u32 {
        if x < self.width && y < self.height {
            unsafe {
                core::ptr::read_volatile(self.pixel_ptr(x, y))
            }
        } else {
            0
        }
    }

    /// Save a rectangular region of pixels to a buffer.
    ///
    /// Used for cursor background save/restore.
    /// Buffer must be at least w * h elements.
    pub fn save_rect(&self, x: usize, y: usize, w: usize, h: usize, buf: &mut [u32]) {
        let mut idx = 0;
        for dy in 0..h {
            for dx in 0..w {
                let px = x.wrapping_add(dx);
                let py = y.wrapping_add(dy);
                buf[idx] = self.get_pixel(px, py);
                idx += 1;
            }
        }
    }

    /// Restore a rectangular region of pixels from a buffer.
    ///
    /// Used for cursor background save/restore.
    pub fn restore_rect(&mut self, x: usize, y: usize, w: usize, h: usize, buf: &[u32]) {
        let mut idx = 0;
        for dy in 0..h {
            for dx in 0..w {
                let px = x.wrapping_add(dx);
                let py = y.wrapping_add(dy);
                if px < self.width && py < self.height {
                    self.set_pixel(px, py, buf[idx]);
                }
                idx += 1;
            }
        }
    }
}

// Standard colors (in 0xRRGGBB format, converted at runtime)
pub mod colors {
    pub const BLACK: u32 = 0x000000;
    pub const WHITE: u32 = 0xFFFFFF;
    pub const RED: u32 = 0xFF0000;
    pub const GREEN: u32 = 0x00FF00;
    pub const BLUE: u32 = 0x0000FF;
    pub const YELLOW: u32 = 0xFFFF00;
    pub const CYAN: u32 = 0x00FFFF;
    pub const MAGENTA: u32 = 0xFF00FF;
    pub const GRAY: u32 = 0x808080;
    pub const DARK_GRAY: u32 = 0x404040;
    pub const LIGHT_GRAY: u32 = 0xC0C0C0;

    // Folkering OS brand colors
    pub const FOLK_BLUE: u32 = 0x2D5AA0;
    pub const FOLK_DARK: u32 = 0x1A1A2E;
    pub const FOLK_ACCENT: u32 = 0x4ECDC4;
}
