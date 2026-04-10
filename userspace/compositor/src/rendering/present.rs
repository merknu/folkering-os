//! Present + flush — copy shadow → framebuffer for damaged regions,
//! redraw cursor, GPU flush, and VGA mirror for QMP screendump support.

use compositor::damage::DamageTracker;
use compositor::framebuffer::FramebufferView;

/// Cursor color palette, pre-resolved through `fb.color_from_rgb24`.
pub struct CursorColors {
    pub white: u32,
    pub red: u32,
    pub blue: u32,
    pub magenta: u32,
    pub outline: u32,
}

/// Present shadow buffer to framebuffer, redraw cursor, GPU flush,
/// VGA mirror dirty regions, and clear damage tracker.
pub fn present_and_flush(
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    cursor_x: i32,
    cursor_y: i32,
    cursor_drawn: bool,
    last_buttons: u8,
    cursor_fill_colors: &CursorColors,
    need_redraw: bool,
    had_mouse_events: bool,
    use_gpu: bool,
    vga_mirror_ptr: *mut u8,
    vga_mirror_pitch: usize,
    vga_mirror_w: usize,
    vga_mirror_h: usize,
) {
    if damage.has_damage() {
        if need_redraw {
            // Full redraw: present everything then redraw cursor on top
            for r in damage.regions() {
                fb.present_region(r.x, r.y, r.w, r.h);
            }
            if cursor_drawn {
                let cursor_fill = pick_cursor_color(last_buttons, cursor_fill_colors);
                fb.draw_cursor(
                    cursor_x as usize, cursor_y as usize,
                    cursor_fill, cursor_fill_colors.outline,
                );
            }
        } else if !had_mouse_events {
            // Non-mouse damage (clock tick, Draug, etc.): present shadow→FB
            for r in damage.regions() {
                fb.present_region(r.x, r.y, r.w, r.h);
            }
            // Redraw cursor if it overlaps the presented region
            if cursor_drawn && cursor_y < 22 {
                let cursor_fill = pick_cursor_color(last_buttons, cursor_fill_colors);
                fb.draw_cursor(
                    cursor_x as usize, cursor_y as usize,
                    cursor_fill, cursor_fill_colors.outline,
                );
            }
        }
        // else: cursor-only movement — FB already has correct pixels
    }

    if use_gpu && damage.has_damage() {
        let regions = damage.regions();
        if regions.len() == 1 {
            let r = &regions[0];
            libfolk::sys::gpu_flush(r.x, r.y, r.w, r.h);
        } else {
            let mut batch = [[0u32; 4]; 4];
            let n = regions.len().min(4);
            for i in 0..n {
                batch[i] = [regions[i].x, regions[i].y, regions[i].w, regions[i].h];
            }
            libfolk::sys::gpu_flush_batch(&batch[..n]);
        }

        // VGA Mirror: copy dirty regions from shadow → Limine VGA FB so
        // QMP screendump and VNC show the current frame even on TCG
        // (where VirtIO-GPU scanout isn't capturable).
        if !vga_mirror_ptr.is_null() {
            let shadow_ptr = fb.shadow_ptr_raw();
            if !shadow_ptr.is_null() {
                let gpu_pitch = fb.pitch;
                for r in regions {
                    let rx = r.x as usize;
                    let ry = r.y as usize;
                    let rw = (r.w as usize).min(vga_mirror_w.saturating_sub(rx));
                    let rh = (r.h as usize).min(vga_mirror_h.saturating_sub(ry));
                    if rw == 0 || rh == 0 {
                        continue;
                    }
                    let bytes_per_row = rw * 4;
                    for row in ry..ry + rh {
                        let src_off = row * gpu_pitch + rx * 4;
                        let dst_off = row * vga_mirror_pitch + rx * 4;
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                shadow_ptr.add(src_off),
                                vga_mirror_ptr.add(dst_off),
                                bytes_per_row,
                            );
                        }
                    }
                }
            }
        }

        damage.clear();
    } else {
        damage.clear();
    }
}

#[inline]
fn pick_cursor_color(buttons: u8, colors: &CursorColors) -> u32 {
    match (buttons & 1 != 0, buttons & 2 != 0) {
        (true, true) => colors.magenta,
        (true, false) => colors.red,
        (false, true) => colors.blue,
        _ => colors.white,
    }
}
