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
    /// Pointer to framebuffer memory (Write-Combining — writes fast, reads unreliable)
    buffer: *mut u8,
    /// Pointer to shadow buffer in normal RAM (reads are reliable)
    /// Used for cursor save/restore to avoid WC read artifacts
    shadow: *mut u8,
    /// Width in pixels
    pub width: usize,
    /// Height in pixels
    pub height: usize,
    /// Bytes per scanline (pitch)
    pub pitch: usize,
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
        // Allocate shadow buffer in normal RAM via mmap
        let fb_size = (config.pitch as usize) * (config.height as usize);
        let shadow = match libfolk::sys::mmap(fb_size, libfolk::sys::PROT_READ | libfolk::sys::PROT_WRITE) {
            Ok(ptr) => {
                libfolk::sys::io::write_str("[COMPOSITOR] Shadow buffer allocated (");
                // Print size in KB
                let kb = fb_size / 1024;
                if kb >= 1000 { libfolk::sys::io::write_char(b'0' + ((kb / 1000) % 10) as u8); }
                if kb >= 100 { libfolk::sys::io::write_char(b'0' + ((kb / 100) % 10) as u8); }
                if kb >= 10 { libfolk::sys::io::write_char(b'0' + ((kb / 10) % 10) as u8); }
                libfolk::sys::io::write_char(b'0' + (kb % 10) as u8);
                libfolk::sys::io::write_str("KB)\n");
                ptr as *mut u8
            }
            Err(_) => {
                libfolk::sys::io::write_str("[COMPOSITOR] WARNING: Shadow buffer alloc FAILED\n");
                core::ptr::null_mut()
            }
        };

        Self {
            buffer,
            shadow,
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
    /// Note: QEMU VGA uses XRGB format (R at byte 2, G at byte 1, B at byte 0)
    /// which means shifts should be R=16, G=8, B=0 and we write 0x00RRGGBB
    #[inline]
    pub fn rgb(&self, r: u8, g: u8, b: u8) -> u32 {
        // Use the reported shifts from bootloader
        ((r as u32) << self.red_shift)
            | ((g as u32) << self.green_shift)
            | ((b as u32) << self.blue_shift)
    }

    /// Create a color from a packed 0xRRGGBB value.
    #[inline]
    pub fn color_from_rgb24(&self, rgb: u32) -> u32 {
        // Just pass through - the input is already 0x00RRGGBB format
        // which matches QEMU's XRGB framebuffer format
        rgb
    }

    /// Get pointer to a pixel at (x, y).
    #[inline]
    pub fn pixel_ptr(&self, x: usize, y: usize) -> *mut u32 {
        debug_assert!(x < self.width && y < self.height);
        unsafe {
            self.buffer.add(y * self.pitch + x * self.bpp) as *mut u32
        }
    }

    /// Get raw shadow buffer base pointer (for VGA mirror bulk copy).
    #[inline]
    pub fn shadow_ptr_raw(&self) -> *const u8 {
        self.shadow as *const u8
    }

    /// Get shadow buffer pixel pointer (normal RAM — reliable reads)
    #[inline]
    fn shadow_ptr(&self, x: usize, y: usize) -> *mut u32 {
        debug_assert!(x < self.width && y < self.height);
        unsafe {
            self.shadow.add(y * self.pitch + x * self.bpp) as *mut u32
        }
    }

    /// Set a single pixel. Writes to shadow buffer (cached RAM) only.
    /// Call present_region() after rendering to copy shadow→FB in bulk.
    #[inline]
    pub fn set_pixel(&mut self, x: usize, y: usize, color: u32) {
        if x < self.width && y < self.height {
            unsafe {
                if !self.shadow.is_null() {
                    core::ptr::write(self.shadow_ptr(x, y), color);
                } else {
                    core::ptr::write_volatile(self.pixel_ptr(x, y), color);
                }
            }
        }
    }

    /// Set a pixel in framebuffer ONLY (not shadow).
    /// Use for cursor overlay — cursor should not be saved in shadow buffer.
    #[inline]
    fn set_pixel_overlay(&mut self, x: usize, y: usize, color: u32) {
        if x < self.width && y < self.height {
            unsafe {
                core::ptr::write_volatile(self.pixel_ptr(x, y), color);
            }
        }
    }

    /// Fill a rectangle with a solid color.
    /// Writes to shadow buffer only. Call present_region() to copy to FB.
    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let end_x = (x + w).min(self.width);
        let end_y = (y + h).min(self.height);
        let start_x = x.min(self.width);
        let start_y = y.min(self.height);
        let pixels = end_x - start_x;

        if !self.shadow.is_null() {
            for row in start_y..end_y {
                let offset = row * self.pitch + start_x * self.bpp;
                let shadow_row = unsafe { self.shadow.add(offset) };
                for i in 0..pixels {
                    unsafe {
                        core::ptr::write(shadow_row.add(i * self.bpp) as *mut u32, color);
                    }
                }
            }
        } else {
            for row in start_y..end_y {
                let offset = row * self.pitch + start_x * self.bpp;
                let row_start = unsafe { self.buffer.add(offset) };
                for i in 0..pixels {
                    unsafe {
                        core::ptr::write_volatile(row_start.add(i * self.bpp) as *mut u32, color);
                    }
                }
            }
        }
    }

    /// Fill the entire screen with a color.
    pub fn clear(&mut self, color: u32) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Copy a region from shadow buffer to device framebuffer in bulk.
    /// Call this AFTER all rendering is done, BEFORE gpu_flush.
    /// Uses row-wise memcpy — much faster than per-pixel write_volatile.
    pub fn present_region(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if self.shadow.is_null() { return; }
        let x = (x as usize).min(self.width);
        let y = (y as usize).min(self.height);
        let w = (w as usize).min(self.width - x);
        let h = (h as usize).min(self.height - y);
        let bytes_per_row = w * self.bpp;

        for row in y..y + h {
            let offset = row * self.pitch + x * self.bpp;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.shadow.add(offset),
                    self.buffer.add(offset),
                    bytes_per_row,
                );
            }
        }
    }

    /// Copy entire shadow buffer to device framebuffer.
    pub fn present_full(&mut self) {
        if self.shadow.is_null() { return; }
        let total = self.pitch * self.height;
        unsafe {
            core::ptr::copy_nonoverlapping(self.shadow, self.buffer, total);
        }
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

        let ch_idx = unicode_to_cp437(ch);
        let glyph = if ch_idx < 256 {
            &FONT_8X16[ch_idx]
        } else {
            &FONT_8X16[0]
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
    /// Cursor dimensions (must match CURSOR_BITMAP below)
    pub const CURSOR_W: usize = 16;
    pub const CURSOR_H: usize = 24;

    /// Draw a tall triangle mouse cursor at (x, y).
    /// 1 = outline (black), 2 = fill (color), 0 = transparent.
    /// The hot-spot is at (0, 0) — top-left corner of the bitmap.
    pub fn draw_cursor(&mut self, x: usize, y: usize, color: u32, outline: u32) {
        // Tall triangle cursor (16 wide × 24 tall)
        // Designed to be clearly visible on any background
        #[rustfmt::skip]
        const CURSOR_BITMAP: [[u8; 16]; 24] = [
            [1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
            [1,1,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
            [1,2,1,0,0,0,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,1,0,0,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,1,0,0,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,1,0,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,1,0,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,2,1,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,2,1,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,2,2,1,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,2,2,1,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,2,2,2,1,0,0,0,0,0,0,0],
            [1,2,2,2,2,2,2,2,1,0,0,0,0,0,0,0],
            [1,2,2,2,2,2,2,2,2,1,0,0,0,0,0,0],
            [1,2,2,2,2,2,2,2,2,1,0,0,0,0,0,0],
            [1,2,2,2,2,2,2,2,2,2,1,0,0,0,0,0],
            [1,2,2,2,2,1,1,1,1,1,1,0,0,0,0,0],
            [1,2,2,2,1,0,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,1,0,0,0,0,0,0,0,0,0,0,0,0],
            [1,2,1,0,0,0,0,0,0,0,0,0,0,0,0,0],
            [1,1,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
            [1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
        ];

        for (dy, row) in CURSOR_BITMAP.iter().enumerate() {
            for (dx, &pixel) in row.iter().enumerate() {
                if pixel == 0 { continue; }
                let px = x.wrapping_add(dx);
                let py = y.wrapping_add(dy);
                if px < self.width && py < self.height {
                    // Use overlay-only write — cursor must NOT go into shadow buffer
                    match pixel {
                        1 => self.set_pixel_overlay(px, py, outline),
                        _ => self.set_pixel_overlay(px, py, color),
                    }
                }
            }
        }
    }

    /// Alpha-blend a pixel onto the scene.
    ///
    /// Reads the existing background from the shadow buffer (reliable RAM),
    /// blends `color` on top with the given `alpha` (0=transparent, 255=opaque),
    /// and writes the result to BOTH framebuffer and shadow buffer.
    ///
    /// Math per channel: out = (fg * alpha + bg * (255 - alpha)) / 255
    /// Uses the fast approximation: (x + 1 + (x >> 8)) >> 8 ≈ x / 255
    #[inline]
    pub fn blend_pixel(&mut self, x: usize, y: usize, color: u32, alpha: u8) {
        if x >= self.width || y >= self.height { return; }
        if alpha == 255 {
            self.set_pixel(x, y, color);
            return;
        }
        if alpha == 0 { return; }

        // Read background from shadow buffer (reliable)
        let bg = if !self.shadow.is_null() {
            unsafe { core::ptr::read(self.shadow_ptr(x, y)) }
        } else {
            unsafe { core::ptr::read_volatile(self.pixel_ptr(x, y)) }
        };

        let a = alpha as u32;
        let inv_a = 255 - a;

        // Extract channels (assumes 0x00RRGGBB / XRGB layout)
        let fg_r = (color >> 16) & 0xFF;
        let fg_g = (color >> 8) & 0xFF;
        let fg_b = color & 0xFF;

        let bg_r = (bg >> 16) & 0xFF;
        let bg_g = (bg >> 8) & 0xFF;
        let bg_b = bg & 0xFF;

        // Blend: (fg * a + bg * inv_a + 128) / 255
        // Fast: (x + 1 + (x >> 8)) >> 8
        let r = {
            let tmp = fg_r * a + bg_r * inv_a + 128;
            (tmp + (tmp >> 8)) >> 8
        };
        let g = {
            let tmp = fg_g * a + bg_g * inv_a + 128;
            (tmp + (tmp >> 8)) >> 8
        };
        let b = {
            let tmp = fg_b * a + bg_b * inv_a + 128;
            (tmp + (tmp >> 8)) >> 8
        };

        let blended = (r << 16) | (g << 8) | b;
        self.set_pixel(x, y, blended);
    }

    /// Fill a rectangle with alpha blending.
    ///
    /// Like `fill_rect` but blends `color` at `alpha` opacity over the existing scene.
    pub fn fill_rect_alpha(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32, alpha: u8) {
        if alpha == 255 {
            self.fill_rect(x, y, w, h, color);
            return;
        }
        if alpha == 0 { return; }

        let end_x = (x + w).min(self.width);
        let end_y = (y + h).min(self.height);
        let start_x = x.min(self.width);
        let start_y = y.min(self.height);

        for row in start_y..end_y {
            for col in start_x..end_x {
                self.blend_pixel(col, row, color, alpha);
            }
        }
    }

    /// Draw an 8x16 character with alpha blending for the background.
    ///
    /// Foreground pixels are drawn opaque; background pixels are blended
    /// at `bg_alpha` so the scene behind shows through.
    pub fn draw_char_alpha(&mut self, x: usize, y: usize, ch: char, fg: u32, bg: u32, bg_alpha: u8) {
        use super::font::FONT_8X16;

        let ch_idx = unicode_to_cp437(ch);
        let glyph = if ch_idx < 256 {
            &FONT_8X16[ch_idx]
        } else {
            &FONT_8X16[0]
        };

        for row in 0..16 {
            let bits = glyph[row];
            for col in 0..8 {
                if (bits >> (7 - col)) & 1 != 0 {
                    self.set_pixel(x + col, y + row, fg);
                } else {
                    self.blend_pixel(x + col, y + row, bg, bg_alpha);
                }
            }
        }
    }

    /// Draw a string with alpha-blended background.
    pub fn draw_string_alpha(&mut self, mut x: usize, mut y: usize, s: &str, fg: u32, bg: u32, bg_alpha: u8) {
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
            self.draw_char_alpha(x, y, ch, fg, bg, bg_alpha);
            x += 8;
        }
    }

    /// Read a single pixel value from shadow buffer (reliable).
    /// Falls back to WC framebuffer read if no shadow buffer.
    #[inline]
    pub fn get_pixel(&self, x: usize, y: usize) -> u32 {
        if x < self.width && y < self.height {
            unsafe {
                if !self.shadow.is_null() {
                    core::ptr::read(self.shadow_ptr(x, y))
                } else {
                    core::ptr::read_volatile(self.pixel_ptr(x, y))
                }
            }
        } else {
            0
        }
    }

    /// Save a rectangular region of pixels to a buffer.
    /// Reads from shadow buffer (normal RAM) for reliable cursor save/restore.
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
    /// Writes to FB only — shadow buffer already has the correct scene content.
    pub fn restore_rect(&mut self, x: usize, y: usize, w: usize, h: usize, buf: &[u32]) {
        let mut idx = 0;
        for dy in 0..h {
            for dx in 0..w {
                let px = x.wrapping_add(dx);
                let py = y.wrapping_add(dy);
                if px < self.width && py < self.height {
                    self.set_pixel_overlay(px, py, buf[idx]);
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

/// Map a Unicode code point to a CP437 byte index for the bitmap font.
/// CP437 (IBM PC code page) covers ASCII + many Latin/Greek/box-drawing chars.
/// This handles common European characters (Norwegian, German, Spanish, French).
pub fn unicode_to_cp437(ch: char) -> usize {
    let c = ch as u32;
    // ASCII range — direct mapping
    if c < 0x80 { return c as usize; }

    // Common Latin-1 / European characters mapped to CP437 positions
    match ch {
        // Norwegian / Danish
        'Æ' => 0x92, 'æ' => 0x91,
        'Ø' => 0x9D, 'ø' => 0xED, // CP437 has ø as 0xED (lowercase phi, close enough)
        'Å' => 0x8F, 'å' => 0x86,
        // Swedish / German
        'Ä' => 0x8E, 'ä' => 0x84,
        'Ö' => 0x99, 'ö' => 0x94,
        'Ü' => 0x9A, 'ü' => 0x81,
        'ß' => 0xE1,
        // Spanish
        'Ñ' => 0xA5, 'ñ' => 0xA4,
        'á' => 0xA0, 'é' => 0x82, 'í' => 0xA1, 'ó' => 0xA2, 'ú' => 0xA3,
        'À' => 0x41, 'Á' => 0x41, 'É' => 0x90, 'Í' => 0x49, 'Ó' => 0x4F, 'Ú' => 0x55,
        // French
        'à' => 0x85, 'â' => 0x83, 'ç' => 0x87, 'Ç' => 0x80,
        'è' => 0x8A, 'ê' => 0x88, 'ë' => 0x89,
        'î' => 0x8C, 'ï' => 0x8B,
        'ô' => 0x93, 'œ' => 0x6F, 'Œ' => 0x4F,
        'ù' => 0x97, 'û' => 0x96, 'ÿ' => 0x98,
        // Common punctuation / symbols
        '€' => 0xEE, '£' => 0x9C, '¥' => 0x9D,
        '«' => 0xAE, '»' => 0xAF,
        '°' => 0xF8, '±' => 0xF1, '²' => 0xFD,
        '·' => 0xFA, '½' => 0xAB, '¼' => 0xAC,
        '¿' => 0xA8, '¡' => 0xAD,
        '–' => 0x2D, '—' => 0x2D, // em/en dash → minus
        '\u{2018}' => 0x27, '\u{2019}' => 0x27, // smart quotes → apostrophe
        '\u{201C}' => 0x22, '\u{201D}' => 0x22, // smart double quotes → "
        '\u{2026}' => 0x2E, // ellipsis → period
        // Unknown — return 256 to trigger fallback to NUL glyph
        _ => 256,
    }
}
