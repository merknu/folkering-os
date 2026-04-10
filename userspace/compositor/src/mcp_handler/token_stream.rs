//! TokenRing polling, tag-parsing FSM (`<think>`, `<|tool|>`,
//! `<|tool_result|>`), and AI Think overlay rendering.

extern crate alloc;

use libfolk::sys::io::write_str;
use libfolk::sys::ipc::{recv_async, reply_with_token, IpcError};
use libfolk::sys::{shmem_destroy, shmem_map, shmem_unmap};

use compositor::Compositor;
use compositor::damage::DamageTracker;
use compositor::framebuffer::FramebufferView;
use compositor::state::StreamState;
use compositor::window_manager::WindowManager;

use crate::ipc_helpers::{handle_message, parse_widget_tree, execute_tool_call, MSG_CREATE_UI_WINDOW};

use super::{
    AiTickResult, COMPOSITOR_SHMEM_VADDR, RING_HEADER_SIZE, RING_VADDR,
    THINK_BUF_SIZE, THINK_CLOSE, THINK_OPEN, TOOL_CLOSE, TOOL_OPEN,
    RESULT_CLOSE, RESULT_OPEN,
};

pub(super) fn tick(
    wm: &mut WindowManager,
    stream: &mut StreamState,
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    compositor: &mut Compositor,
) -> AiTickResult {
    let mut did_work = false;
    let mut need_redraw = false;

    // ===== Process IPC messages (non-blocking) =====
    match recv_async() {
        Ok(msg) => {
            did_work = true;
            let opcode = msg.payload0 & 0xFF;

            if opcode == MSG_CREATE_UI_WINDOW {
                let shmem_handle = ((msg.payload0 >> 8) & 0xFFFFFFFF) as u32;
                let mut response = u64::MAX;

                if shmem_handle != 0 {
                    if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                        let buf = unsafe {
                            core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, 4096)
                        };

                        if let Some(header) = libfolk::ui::parse_header(buf) {
                            let win_count = wm.windows.len() as i32;
                            let wx = 100 + win_count * 30;
                            let wy = 80 + win_count * 30;
                            let win_id = wm.create_terminal(
                                header.title, wx, wy,
                                header.width as u32, header.height as u32,
                            );

                            if let Some(win) = wm.get_window_mut(win_id) {
                                win.kind = compositor::window_manager::WindowKind::App;
                                win.owner_task = msg.sender;
                                let (root_widget, _) = parse_widget_tree(header.widget_data);
                                if let Some(widget) = root_widget {
                                    win.widgets.push(widget);
                                }
                            }

                            write_str("[WM] Created UI window: ");
                            write_str(header.title);
                            write_str("\n");
                            response = win_id as u64;
                        }

                        let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
                        let _ = shmem_destroy(shmem_handle);
                    }
                }
                let _ = reply_with_token(msg.token, response, 0);
                need_redraw = true;
            } else {
                let response = handle_message(compositor, msg.payload0);
                let _ = reply_with_token(msg.token, response, 0);
            }
        }
        Err(IpcError::WouldBlock) => {}
        Err(_) => {}
    }

    // ===== Token Streaming: Poll TokenRing =====
    if stream.ring_handle != 0 {
        process_token_ring(stream, wm, &mut did_work, &mut need_redraw);
    }

    // ===== AI Think Overlay =====
    if (stream.think_active || stream.think_fade_timer > 0) && stream.think_display_len > 0 {
        render_think_overlay(stream, fb, damage, &mut need_redraw);
    }

    // Decrement fade timer
    if stream.think_fade_timer > 0 {
        stream.think_fade_timer -= 1;
        if stream.think_fade_timer == 0 {
            need_redraw = true;
        }
    }

    AiTickResult { did_work, need_redraw }
}

/// Poll the TokenRing for new bytes, run them through the 3-layer tag FSM
/// (think → result → tool/visible), and append visible text to the window.
fn process_token_ring(
    stream: &mut StreamState,
    wm: &mut WindowManager,
    did_work: &mut bool,
    need_redraw: &mut bool,
) {
    use core::sync::atomic::Ordering;
    let ring_ptr = RING_VADDR as *const u32;
    let write_idx_atomic = unsafe { &*(ring_ptr as *const core::sync::atomic::AtomicU32) };
    let status_atomic = unsafe { &*((ring_ptr as *const core::sync::atomic::AtomicU32).add(1)) };

    let new_write = write_idx_atomic.load(Ordering::Acquire) as usize;
    if new_write > stream.ring_read_idx {
        *did_work = true;
        let data_ptr = unsafe { (RING_VADDR as *const u8).add(RING_HEADER_SIZE) };
        let new_data = unsafe {
            core::slice::from_raw_parts(
                data_ptr.add(stream.ring_read_idx),
                new_write - stream.ring_read_idx,
            )
        };

        let mut visible_buf: [u8; 512] = [0; 512];
        let mut vis_len: usize = 0;

        for &byte in new_data.iter() {
            // ── Layer 1: Think tag filter ──
            if stream.think_state == 0 {
                if byte == THINK_OPEN[stream.think_open_match] {
                    stream.think_pending[stream.think_pending_len] = byte;
                    stream.think_pending_len += 1;
                    stream.think_open_match += 1;
                    if stream.think_open_match == THINK_OPEN.len() {
                        stream.think_state = 1;
                        stream.think_open_match = 0;
                        stream.think_pending_len = 0;
                        stream.think_active = true;
                        stream.think_display_len = 0;
                        stream.think_fade_timer = 0;
                        *need_redraw = true;
                    }
                    continue;
                } else if stream.think_open_match > 0 {
                    let pending_count = stream.think_pending_len;
                    stream.think_open_match = 0;
                    stream.think_pending_len = 0;
                    for j in 0..pending_count {
                        let pb = stream.think_pending[j];
                        process_tool_byte(pb, stream, &mut visible_buf, &mut vis_len);
                    }
                }
            } else {
                // Inside <think> block — scan for </think>
                if byte == THINK_CLOSE[stream.think_close_match] {
                    stream.think_close_match += 1;
                    if stream.think_close_match == THINK_CLOSE.len() {
                        stream.think_state = 0;
                        stream.think_close_match = 0;
                        stream.think_active = false;
                        stream.think_fade_timer = 120;
                        *need_redraw = true;
                    }
                } else {
                    for k in 0..stream.think_close_match {
                        if stream.think_display_len < THINK_BUF_SIZE {
                            stream.think_display[stream.think_display_len] = THINK_CLOSE[k];
                            stream.think_display_len += 1;
                        }
                    }
                    stream.think_close_match = 0;
                    if stream.think_display_len < THINK_BUF_SIZE {
                        stream.think_display[stream.think_display_len] = byte;
                        stream.think_display_len += 1;
                    }
                    *need_redraw = true;
                }
                continue;
            }

            // ── Layer 1.5: Tool result filter ──
            if stream.result_state == 0 {
                if byte == RESULT_OPEN[stream.result_open_match] {
                    stream.result_open_match += 1;
                    if stream.result_open_match == RESULT_OPEN.len() {
                        stream.result_state = 1;
                        stream.result_open_match = 0;
                    }
                    continue;
                } else if stream.result_open_match > 0 {
                    stream.result_open_match = 0;
                }
            } else {
                if byte == RESULT_CLOSE[stream.result_close_match] {
                    stream.result_close_match += 1;
                    if stream.result_close_match == RESULT_CLOSE.len() {
                        stream.result_state = 0;
                        stream.result_close_match = 0;
                    }
                } else {
                    stream.result_close_match = 0;
                }
                continue;
            }

            // ── Layer 2: Tool tag filter + visible output ──
            process_tool_byte(byte, stream, &mut visible_buf, &mut vis_len);
        }

        // Append visible (non-tool) text to window
        if vis_len > 0 {
            if let Some(win) = wm.get_window_mut(stream.win_id) {
                win.append_text(&visible_buf[..vis_len]);
            }
        }

        // Execute completed tool call
        if stream.tool_state == 3 {
            let tool_content = core::str::from_utf8(&stream.tool_buf[..stream.tool_buf_len]).unwrap_or("");
            let ring_va = if stream.ring_handle != 0 { RING_VADDR } else { 0 };
            let ring_write = new_write;
            if let Some(win) = wm.get_window_mut(stream.win_id) {
                execute_tool_call(tool_content, win, ring_va, ring_write);
            }
            stream.tool_state = 0;
            stream.tool_buf_len = 0;
            *need_redraw = true;
        }
        stream.ring_read_idx = new_write;
        *need_redraw = true;
    }

    // Check status (DONE / ERROR)
    let status = status_atomic.load(Ordering::Acquire);
    if status != 0 {
        *did_work = true;
        let _ = shmem_unmap(stream.ring_handle, RING_VADDR);
        let _ = shmem_destroy(stream.ring_handle);
        let _ = shmem_destroy(stream.query_handle);
        stream.ring_handle = 0;
        stream.query_handle = 0;
        if stream.tool_state != 0 {
            stream.tool_state = 0;
            stream.tool_open_match = 0;
            stream.tool_close_match = 0;
            stream.tool_buf_len = 0;
            stream.tool_pending_len = 0;
        }
        if let Some(win) = wm.get_window_mut(stream.win_id) {
            win.typing = false;
            win.push_line("");
            if status == 2 {
                win.push_line("[AI] Error during generation");
            }
        }
        *need_redraw = true;
    }
}

/// Tool-tag FSM: route a single byte to either the tool buffer or the
/// visible output, based on `stream.tool_state`.
fn process_tool_byte(
    byte: u8,
    stream: &mut StreamState,
    visible_buf: &mut [u8; 512],
    vis_len: &mut usize,
) {
    match stream.tool_state {
        0 => {
            if byte == TOOL_OPEN[stream.tool_open_match] {
                stream.tool_pending[stream.tool_pending_len] = byte;
                stream.tool_pending_len += 1;
                stream.tool_open_match += 1;
                if stream.tool_open_match == TOOL_OPEN.len() {
                    stream.tool_state = 1;
                    stream.tool_open_match = 0;
                    stream.tool_pending_len = 0;
                    stream.tool_buf_len = 0;
                }
            } else if stream.tool_open_match > 0 {
                for j in 0..stream.tool_pending_len {
                    if *vis_len < visible_buf.len() {
                        visible_buf[*vis_len] = stream.tool_pending[j];
                        *vis_len += 1;
                    }
                }
                stream.tool_open_match = 0;
                stream.tool_pending_len = 0;
                if *vis_len < visible_buf.len() {
                    visible_buf[*vis_len] = byte;
                    *vis_len += 1;
                }
            } else {
                if *vis_len < visible_buf.len() {
                    visible_buf[*vis_len] = byte;
                    *vis_len += 1;
                }
            }
        }
        1 => {
            if byte == TOOL_CLOSE[stream.tool_close_match] {
                stream.tool_close_match += 1;
                if stream.tool_close_match == TOOL_CLOSE.len() {
                    stream.tool_state = 3;
                    stream.tool_close_match = 0;
                }
            } else {
                for j in 0..stream.tool_close_match {
                    if stream.tool_buf_len < stream.tool_buf.len() {
                        stream.tool_buf[stream.tool_buf_len] = TOOL_CLOSE[j];
                        stream.tool_buf_len += 1;
                    }
                }
                stream.tool_close_match = 0;
                if stream.tool_buf_len < stream.tool_buf.len() {
                    stream.tool_buf[stream.tool_buf_len] = byte;
                    stream.tool_buf_len += 1;
                }
            }
        }
        _ => {
            if *vis_len < visible_buf.len() {
                visible_buf[*vis_len] = byte;
                *vis_len += 1;
            }
        }
    }
}

/// Render the AI Think overlay: semi-transparent panel showing the
/// last 8 lines of `<think>` content in real time.
fn render_think_overlay(
    stream: &mut StreamState,
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    need_redraw: &mut bool,
) {
    let overlay_w = 400usize;
    let overlay_x = fb.width.saturating_sub(overlay_w + 16);
    let overlay_y = 40usize;

    let think_text = unsafe {
        core::str::from_utf8_unchecked(&stream.think_display[..stream.think_display_len])
    };

    let max_lines = 8usize;
    let mut line_starts = [0usize; 9];
    let mut line_count = 0usize;
    let bytes = think_text.as_bytes();
    line_starts[0] = 0;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' && line_count < max_lines {
            line_count += 1;
            line_starts[line_count] = i + 1;
        }
    }
    if line_count == 0 { line_count = 1; }

    let first_line = if line_count > max_lines { line_count - max_lines } else { 0 };
    let display_lines = line_count - first_line;
    let overlay_h = 28 + display_lines * 18;

    let alpha = if stream.think_active { 200u8 } else {
        (stream.think_fade_timer as u16 * 200 / 120).min(200) as u8
    };

    fb.fill_rect_alpha(overlay_x, overlay_y, overlay_w, overlay_h, 0x0a0a1e, alpha);

    let header = if stream.think_active { "AI Thinking..." } else { "AI Thought" };
    let header_color = if stream.think_active { 0x00ccff } else { 0x666688 };
    fb.draw_string(overlay_x + 8, overlay_y + 6, header,
        fb.color_from_rgb24(header_color), fb.color_from_rgb24(0));

    let text_color = fb.color_from_rgb24(if stream.think_active { 0xaaaacc } else { 0x666688 });
    let bg_color = fb.color_from_rgb24(0);
    for li in 0..display_lines {
        let idx = first_line + li;
        let start = line_starts[idx];
        let end = if idx + 1 <= line_count {
            line_starts[idx + 1].min(stream.think_display_len)
        } else {
            stream.think_display_len
        };
        if start < end {
            let line_end = end.min(start + 48);
            let line = unsafe {
                core::str::from_utf8_unchecked(&stream.think_display[start..line_end])
            };
            let line_trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            if !line_trimmed.is_empty() {
                fb.draw_string(overlay_x + 8, overlay_y + 24 + li * 18,
                    line_trimmed, text_color, bg_color);
            }
        }
    }

    let overlay_w_u32 = 400;
    damage.add_damage(compositor::damage::Rect::new(
        overlay_x as u32, overlay_y as u32, overlay_w_u32, overlay_h as u32));
    *need_redraw = true;
}
