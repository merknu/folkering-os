//! Window compositing + spatial port pipelining (in-window tick-tock).

extern crate alloc;

use super::RenderContext;

/// Stage 4 — Composite all visible windows + run spatial port pipelines
/// (in-window tick-tock between connected WASM apps).
///
/// Returns true if any spatial pipeline did work.
pub(super) fn render_windows(ctx: &mut RenderContext) -> bool {
    let mut did_work = false;

    ctx.wm.composite(&mut *ctx.fb);

    // Spatial Pipelining: in-window tick-tock between connected apps
    for conn_idx in 0..ctx.wasm.node_connections.len() {
        let src_id = ctx.wasm.node_connections[conn_idx].source_win_id;
        let dst_id = ctx.wasm.node_connections[conn_idx].dest_win_id;
        let config = compositor::wasm_runtime::WasmConfig {
            screen_width: 400,
            screen_height: 300,
            uptime_ms: libfolk::sys::uptime() as u32,
        };

        // TICK: run upstream app → collect stream_data + render to its window
        let stream_data = if let Some(up_app) = ctx.wasm.window_apps.get_mut(&src_id) {
            let (_, output) = up_app.run_frame(config.clone());
            if let Some(w) = ctx.wm.get_window(src_id) {
                let cx = w.x as usize + 2 + 6;
                let cy = w.y as usize + 2 + 26 + 4;
                if let Some(color) = output.fill_screen {
                    ctx.fb.fill_rect(
                        cx, cy,
                        w.width as usize - 12, w.height as usize - 8,
                        ctx.fb.color_from_rgb24(color),
                    );
                }
                for cmd in &output.draw_commands {
                    let rx = cx + cmd.x as usize;
                    let ry = cy + cmd.y as usize;
                    ctx.fb.fill_rect(rx, ry, cmd.w as usize, cmd.h as usize,
                        ctx.fb.color_from_rgb24(cmd.color));
                }
                for tc in &output.text_commands {
                    let tx = cx + tc.x as usize;
                    let ty = cy + tc.y as usize;
                    ctx.fb.draw_string(tx, ty, &tc.text,
                        ctx.fb.color_from_rgb24(tc.color),
                        ctx.fb.color_from_rgb24(0));
                }
            }
            output.stream_data
        } else {
            alloc::vec::Vec::new()
        };

        // TOCK: inject stream data into downstream + render
        if let Some(down_app) = ctx.wasm.window_apps.get_mut(&dst_id) {
            down_app.inject_stream_data(&stream_data);
            let (_, output) = down_app.run_frame(config);
            if let Some(w) = ctx.wm.get_window(dst_id) {
                let cx = w.x as usize + 2 + 6;
                let cy = w.y as usize + 2 + 26 + 4;
                if let Some(color) = output.fill_screen {
                    ctx.fb.fill_rect(
                        cx, cy,
                        w.width as usize - 12, w.height as usize - 8,
                        ctx.fb.color_from_rgb24(color),
                    );
                }
                for cmd in &output.draw_commands {
                    let rx = cx + cmd.x as usize;
                    let ry = cy + cmd.y as usize;
                    ctx.fb.fill_rect(rx, ry, cmd.w as usize, cmd.h as usize,
                        ctx.fb.color_from_rgb24(cmd.color));
                }
                for tc in &output.text_commands {
                    let tx = cx + tc.x as usize;
                    let ty = cy + tc.y as usize;
                    ctx.fb.draw_string(tx, ty, &tc.text,
                        ctx.fb.color_from_rgb24(tc.color),
                        ctx.fb.color_from_rgb24(0));
                }
                for cc in &output.circle_commands {
                    let c = ctx.fb.color_from_rgb24(cc.color);
                    compositor::graphics::draw_circle(
                        &mut *ctx.fb, cx as i32 + cc.cx, cy as i32 + cc.cy, cc.r, c,
                    );
                }
                for lc in &output.line_commands {
                    let c = ctx.fb.color_from_rgb24(lc.color);
                    compositor::graphics::draw_line(
                        &mut *ctx.fb,
                        cx as i32 + lc.x1, cy as i32 + lc.y1,
                        cx as i32 + lc.x2, cy as i32 + lc.y2,
                        c,
                    );
                }
            }
        }

        did_work = true;
    }

    // Render I/O port circles + connection lines
    render_spatial_ports(ctx);
    render_connection_lines(ctx);

    did_work
}

fn render_spatial_ports(ctx: &mut RenderContext) {
    for win in &ctx.wm.windows {
        if !win.visible {
            continue;
        }
        let mid_y = win.y + win.total_h() as i32 / 2;
        if win.output_port {
            let px = win.x + win.total_w() as i32;
            let raw = if compositor::spatial::is_source(&ctx.wasm.node_connections, win.id) {
                compositor::spatial::PORT_COLOR_CONNECTED
            } else {
                compositor::spatial::PORT_COLOR_IDLE
            };
            let c = ctx.fb.color_from_rgb24(raw);
            compositor::graphics::draw_circle(
                &mut *ctx.fb, px, mid_y, compositor::spatial::PORT_RADIUS, c,
            );
        }
        if win.input_port {
            let px = win.x;
            let raw = if compositor::spatial::is_dest(&ctx.wasm.node_connections, win.id) {
                compositor::spatial::PORT_COLOR_CONNECTED
            } else {
                compositor::spatial::PORT_COLOR_IDLE
            };
            let c = ctx.fb.color_from_rgb24(raw);
            compositor::graphics::draw_circle(
                &mut *ctx.fb, px, mid_y, compositor::spatial::PORT_RADIUS, c,
            );
        }
    }
}

fn render_connection_lines(ctx: &mut RenderContext) {
    for conn in &ctx.wasm.node_connections {
        let (sx, sy) = if let Some(w) = ctx.wm.get_window(conn.source_win_id) {
            (w.x + w.total_w() as i32, w.y + w.total_h() as i32 / 2)
        } else {
            continue;
        };
        let (dx, dy) = if let Some(w) = ctx.wm.get_window(conn.dest_win_id) {
            (w.x, w.y + w.total_h() as i32 / 2)
        } else {
            continue;
        };
        let c = ctx.fb.color_from_rgb24(compositor::spatial::CONNECTION_COLOR);
        compositor::graphics::draw_line(&mut *ctx.fb, sx, sy, dx, dy, c);
    }
    // Draw active drag cable
    if let Some(ref drag) = ctx.wasm.connection_drag {
        if let Some(w) = ctx.wm.get_window(drag.source_win_id) {
            let sx = w.x + w.total_w() as i32;
            let sy = w.y + w.total_h() as i32 / 2;
            let c = ctx.fb.color_from_rgb24(compositor::spatial::PORT_COLOR_DRAG);
            compositor::graphics::draw_line(&mut *ctx.fb, sx, sy, drag.current_x, drag.current_y, c);
        }
    }
}
