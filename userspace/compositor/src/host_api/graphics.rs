//! Graphics host functions for WASM apps
//! Drawing primitives, pixel access, display list batch rendering.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use wasmi::*;
use super::{HostState, DrawCmd, TextCmd, LineCmd, CircleCmd, PixelBlit, SURFACE_OFFSET};

pub fn register(linker: &mut Linker<HostState>) {
    // Drawing
    let _ = linker.func_wrap("env", "folk_draw_rect",
        |mut caller: Caller<HostState>, x: i32, y: i32, w: i32, h: i32, color: i32| {
            caller.data_mut().draw_commands.push(DrawCmd {
                x: x as u32, y: y as u32, w: w as u32, h: h as u32, color: color as u32,
            });
        },
    );

    let _ = linker.func_wrap("env", "folk_draw_text",
        |mut caller: Caller<HostState>, x: i32, y: i32, ptr: i32, len: i32, color: i32| {
            // Bounds check: prevent integer overflow and out-of-bounds read
            if len <= 0 || len > 4096 { return; }
            let ptr_u = ptr as u32;
            let len_u = len as u32;
            let end = match ptr_u.checked_add(len_u) {
                Some(e) => e,
                None => return, // Integer overflow
            };
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return,
            };
            if end as usize > mem.data_size(&caller) { return; }
            let mut buf = alloc::vec![0u8; len as usize];
            if mem.read(&caller, ptr as usize, &mut buf).is_ok() {
                if let Ok(text) = alloc::str::from_utf8(&buf) {
                    caller.data_mut().text_commands.push(TextCmd {
                        x: x as u32, y: y as u32,
                        text: String::from(text),
                        color: color as u32,
                    });
                }
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_draw_line",
        |mut caller: Caller<HostState>, x1: i32, y1: i32, x2: i32, y2: i32, color: i32| {
            caller.data_mut().line_commands.push(LineCmd {
                x1, y1, x2, y2, color: color as u32,
            });
        },
    );

    let _ = linker.func_wrap("env", "folk_draw_circle",
        |mut caller: Caller<HostState>, cx: i32, cy: i32, r: i32, color: i32| {
            caller.data_mut().circle_commands.push(CircleCmd {
                cx, cy, r, color: color as u32,
            });
        },
    );

    let _ = linker.func_wrap("env", "folk_fill_screen",
        |mut caller: Caller<HostState>, color: i32| {
            caller.data_mut().fill_screen = Some(color as u32);
        },
    );

    // Phase 3: Direct pixel access — returns offset in WASM linear memory
    let _ = linker.func_wrap("env", "folk_get_surface",
        |caller: Caller<HostState>| -> i32 {
            // Return surface offset (only if memory is large enough)
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            let mem_size = mem.data_size(&caller);
            let fb_size = (caller.data().config.screen_width as usize)
                * (caller.data().config.screen_height as usize) * 4;
            if SURFACE_OFFSET + fb_size <= mem_size {
                SURFACE_OFFSET as i32
            } else {
                0 // Memory too small
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_surface_pitch",
        |caller: Caller<HostState>| -> i32 {
            (caller.data().config.screen_width * 4) as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_surface_present",
        |mut caller: Caller<HostState>| {
            caller.data_mut().surface_dirty = true;
        },
    );

    // Returns 0 on success, -1 on error.
    let _ = linker.func_wrap("env", "folk_draw_pixels",
        |mut caller: Caller<HostState>, x: i32, y: i32, w: i32, h: i32, pixel_ptr: i32, pixel_len: i32| -> i32 {
            if w <= 0 || h <= 0 || w > 2048 || h > 2048 || pixel_len <= 0 { return -1; }
            let expected = (w * h * 4) as usize; // RGBA = 4 bytes/pixel
            if (pixel_len as usize) < expected { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            // Read pixel data from WASM memory
            let mut pixels = alloc::vec![0u8; expected];
            if mem.read(&caller, pixel_ptr as usize, &mut pixels).is_err() { return -1; }

            // Store as a special draw command for the compositor to blit
            caller.data_mut().draw_commands.push(DrawCmd {
                x: x as u32, y: y as u32, w: w as u32, h: h as u32,
                // Use a sentinel color value to mark this as a pixel blit
                // The pixel data is stored separately
                color: 0xFFFF_FFFF, // sentinel: compositor checks this
            });

            // Store pixel data in a new field (extend HostState)
            caller.data_mut().pending_pixel_blits.push(PixelBlit {
                x: x as u32, y: y as u32, w: w as u32, h: h as u32,
                data: pixels,
            });

            0
        },
    );

    // Returns number of commands processed, or -1 on error.
    let _ = linker.func_wrap("env", "folk_submit_display_list",
        |mut caller: Caller<HostState>, ptr: i32, len: i32| -> i32 {
            if len <= 0 || len > 65536 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            let mut buf = alloc::vec![0u8; len as usize];
            if mem.read(&caller, ptr as usize, &mut buf).is_err() { return -1; }

            // First pass: collect text strings that need WASM memory reads
            // (must be done before taking mutable ref to state)
            let mut text_entries: alloc::vec::Vec<(u32, u32, String, u32)> = alloc::vec::Vec::new();
            {
                let mut scan = 0usize;
                while scan < buf.len() {
                    let op = buf[scan];
                    scan += 1;
                    match op {
                        0x01 => { scan += 12; }
                        0x02 => {
                            if scan + 14 > buf.len() { break; }
                            let x = u16::from_le_bytes([buf[scan], buf[scan+1]]) as u32;
                            let y = u16::from_le_bytes([buf[scan+2], buf[scan+3]]) as u32;
                            let text_ptr = u32::from_le_bytes([buf[scan+4], buf[scan+5], buf[scan+6], buf[scan+7]]);
                            let text_len = u16::from_le_bytes([buf[scan+8], buf[scan+9]]) as usize;
                            let color = u32::from_le_bytes([buf[scan+10], buf[scan+11], buf[scan+12], buf[scan+13]]);
                            scan += 14;
                            if text_len > 0 && text_len < 4096 {
                                let mut tb = alloc::vec![0u8; text_len];
                                if mem.read(&caller, text_ptr as usize, &mut tb).is_ok() {
                                    if let Ok(s) = alloc::str::from_utf8(&tb) {
                                        text_entries.push((x, y, String::from(s), color));
                                    }
                                }
                            }
                        }
                        0x03 => { scan += 12; }
                        0x04 => { scan += 4; }
                        0x05 => { scan += 10; }
                        _ => { break; }
                    }
                }
            }

            // Second pass: emit all draw commands (no more mem reads needed)
            let state = caller.data_mut();
            let mut pos = 0usize;
            let mut cmd_count = 0i32;
            let mut text_idx = 0usize;

            while pos < buf.len() {
                let opcode = buf[pos];
                pos += 1;

                match opcode {
                    0x01 => {
                        if pos + 12 > buf.len() { break; }
                        let x = u16::from_le_bytes([buf[pos], buf[pos+1]]) as u32;
                        let y = u16::from_le_bytes([buf[pos+2], buf[pos+3]]) as u32;
                        let w = u16::from_le_bytes([buf[pos+4], buf[pos+5]]) as u32;
                        let h = u16::from_le_bytes([buf[pos+6], buf[pos+7]]) as u32;
                        let color = u32::from_le_bytes([buf[pos+8], buf[pos+9], buf[pos+10], buf[pos+11]]);
                        pos += 12;
                        state.draw_commands.push(DrawCmd { x, y, w, h, color });
                        cmd_count += 1;
                    }
                    0x02 => {
                        if pos + 14 > buf.len() { break; }
                        pos += 14;
                        if text_idx < text_entries.len() {
                            let (x, y, ref text, color) = text_entries[text_idx];
                            state.text_commands.push(TextCmd { x, y, text: text.clone(), color });
                            text_idx += 1;
                        }
                        cmd_count += 1;
                    }
                    0x03 => {
                        if pos + 12 > buf.len() { break; }
                        let x1 = u16::from_le_bytes([buf[pos], buf[pos+1]]) as i32;
                        let y1 = u16::from_le_bytes([buf[pos+2], buf[pos+3]]) as i32;
                        let x2 = u16::from_le_bytes([buf[pos+4], buf[pos+5]]) as i32;
                        let y2 = u16::from_le_bytes([buf[pos+6], buf[pos+7]]) as i32;
                        let color = u32::from_le_bytes([buf[pos+8], buf[pos+9], buf[pos+10], buf[pos+11]]);
                        pos += 12;
                        state.line_commands.push(LineCmd { x1, y1, x2, y2, color });
                        cmd_count += 1;
                    }
                    0x04 => {
                        if pos + 4 > buf.len() { break; }
                        let color = u32::from_le_bytes([buf[pos], buf[pos+1], buf[pos+2], buf[pos+3]]);
                        pos += 4;
                        state.fill_screen = Some(color);
                        cmd_count += 1;
                    }
                    0x05 => {
                        if pos + 10 > buf.len() { break; }
                        let cx = u16::from_le_bytes([buf[pos], buf[pos+1]]) as i32;
                        let cy = u16::from_le_bytes([buf[pos+2], buf[pos+3]]) as i32;
                        let r = u16::from_le_bytes([buf[pos+4], buf[pos+5]]) as i32;
                        let color = u32::from_le_bytes([buf[pos+6], buf[pos+7], buf[pos+8], buf[pos+9]]);
                        pos += 10;
                        state.circle_commands.push(CircleCmd { cx, cy, r, color });
                        cmd_count += 1;
                    }
                    _ => { break; }
                }
            }

            cmd_count
        },
    );
}
