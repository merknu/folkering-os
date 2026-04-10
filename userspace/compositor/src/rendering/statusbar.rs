//! Always-on-top overlays: status bar (clock, RAM, IQE), Alt+Tab HUD,
//! RAM history graph + targeted damage tracking + cursor save.

extern crate alloc;

use crate::ipc_helpers::fmt_u64_into;

use super::RenderContext;

/// Stage 5a — Alt+Tab window switcher HUD overlay.
pub(super) fn render_alt_tab_hud(ctx: &mut RenderContext) {
    if ctx.render.hud_show_until == 0 || ctx.render.hud_title_len == 0 {
        return;
    }
    let hud_text = unsafe {
        core::str::from_utf8_unchecked(&ctx.render.hud_title[..ctx.render.hud_title_len])
    };
    let hud_w = ctx.render.hud_title_len * 8 + 24;
    let hud_x = (ctx.fb.width.saturating_sub(hud_w)) / 2;
    let hud_y = ctx.fb.height.saturating_sub(40);
    ctx.fb.fill_rect_alpha(hud_x, hud_y, hud_w, 24, 0x1a1a2e, 200);
    ctx.fb.draw_rect(hud_x, hud_y, hud_w, 24, ctx.layout.folk_accent);
    ctx.fb.draw_string(hud_x + 12, hud_y + 8, hud_text, ctx.layout.white, ctx.layout.folk_dark);
}

/// Stage 5b — Top status bar (clock, date, RAM%, IQE latency dot).
/// Always rendered on top of windows + WASM apps.
pub(super) fn render_statusbar(ctx: &mut RenderContext) {
    let dt = libfolk::sys::get_rtc();
    let mut total_minutes = dt.hour as i32 * 60 + dt.minute as i32 + ctx.mcp.tz_offset_minutes;
    let mut day = dt.day as i32;
    let mut month = dt.month;
    let mut year = dt.year;
    if total_minutes >= 24 * 60 {
        total_minutes -= 24 * 60;
        day += 1;
        let dim = match month { 2 => 28, 4 | 6 | 9 | 11 => 30, _ => 31 };
        if day > dim {
            day = 1;
            month += 1;
            if month > 12 { month = 1; year += 1; }
        }
    } else if total_minutes < 0 {
        total_minutes += 24 * 60;
        day -= 1;
        if day < 1 {
            month -= 1;
            if month < 1 { month = 12; year -= 1; }
            day = 28;
        }
    }
    let lh = (total_minutes / 60) as u8;
    let lm = (total_minutes % 60) as u8;
    let ls = dt.second;

    let mut t = [0u8; 8];
    t[0] = b'0' + lh / 10; t[1] = b'0' + lh % 10; t[2] = b':';
    t[3] = b'0' + lm / 10; t[4] = b'0' + lm % 10; t[5] = b':';
    t[6] = b'0' + ls / 10; t[7] = b'0' + ls % 10;
    let time_str = unsafe { core::str::from_utf8_unchecked(&t) };

    // Status bar background
    let bar_h = 20usize;
    ctx.fb.fill_rect_alpha(0, 0, ctx.fb.width, bar_h, 0x000000, 140);

    // Clock centered
    let time_x = (ctx.fb.width.saturating_sub(8 * 8)) / 2;
    let bar_bg = ctx.fb.color_from_rgb24(0x0a0a0a);
    ctx.fb.draw_string(time_x, 2, time_str, ctx.layout.white, bar_bg);

    // Date on the left
    let mut d = [0u8; 10];
    d[0] = b'0' + ((year / 1000) % 10) as u8;
    d[1] = b'0' + ((year / 100) % 10) as u8;
    d[2] = b'0' + ((year / 10) % 10) as u8;
    d[3] = b'0' + (year % 10) as u8;
    d[4] = b'-';
    d[5] = b'0' + month / 10; d[6] = b'0' + month % 10;
    d[7] = b'-';
    d[8] = b'0' + day as u8 / 10; d[9] = b'0' + day as u8 % 10;
    let date_str = unsafe { core::str::from_utf8_unchecked(&d) };
    ctx.fb.draw_string(8, 2, date_str, ctx.layout.gray, bar_bg);

    // RAM usage on the right
    let (_total_mb, _used_mb, mem_pct) = libfolk::sys::memory_stats();
    let mut rbuf = [0u8; 8];
    let mut ri = 0usize;
    rbuf[ri] = b'R'; ri += 1;
    rbuf[ri] = b'A'; ri += 1;
    rbuf[ri] = b'M'; ri += 1;
    rbuf[ri] = b' '; ri += 1;
    if mem_pct >= 100 {
        rbuf[ri] = b'1'; ri += 1;
        rbuf[ri] = b'0'; ri += 1;
        rbuf[ri] = b'0'; ri += 1;
    } else {
        if mem_pct >= 10 {
            rbuf[ri] = b'0' + (mem_pct / 10) as u8; ri += 1;
        }
        rbuf[ri] = b'0' + (mem_pct % 10) as u8; ri += 1;
    }
    rbuf[ri] = b'%'; ri += 1;
    let ram_str = unsafe { core::str::from_utf8_unchecked(&rbuf[..ri]) };
    let ram_col = if mem_pct > 80 {
        ctx.fb.color_from_rgb24(0xFF4444)
    } else if mem_pct > 50 {
        ctx.fb.color_from_rgb24(0xFFAA00)
    } else {
        ctx.fb.color_from_rgb24(0x44FF44)
    };
    let ram_x = ctx.fb.width.saturating_sub(ri * 8 + 8);
    ctx.fb.draw_string(ram_x, 2, ram_str, ram_col, bar_bg);

    // IQE latency display + colored dot
    if ctx.iqe.ewma_kbd_us > 0 || ctx.iqe.ewma_mou_us > 0 {
        let mut lbuf = [0u8; 48];
        let mut li = 0usize;
        lbuf[li] = b'K'; li += 1;
        lbuf[li] = b':'; li += 1;
        li += fmt_u64_into(&mut lbuf[li..], ctx.iqe.ewma_kbd_us);
        if ctx.iqe.ewma_kbd_wake > 0 {
            lbuf[li] = b'('; li += 1;
            li += fmt_u64_into(&mut lbuf[li..], ctx.iqe.ewma_kbd_wake);
            lbuf[li] = b'+'; li += 1;
            li += fmt_u64_into(&mut lbuf[li..], ctx.iqe.ewma_kbd_rend);
            lbuf[li] = b')'; li += 1;
        }
        if li < 44 {
            lbuf[li] = b' '; li += 1;
            lbuf[li] = b'M'; li += 1;
            lbuf[li] = b':'; li += 1;
            li += fmt_u64_into(&mut lbuf[li..], ctx.iqe.ewma_mou_us);
        }
        let s = unsafe { core::str::from_utf8_unchecked(&lbuf[..li.min(48)]) };
        ctx.fb.draw_string(90, 2, s, ctx.fb.color_from_rgb24(0x88AACC), bar_bg);

        let worst = ctx.iqe.ewma_kbd_us.max(ctx.iqe.ewma_mou_us);
        let dot = if worst < 5000 { 0x44FF44 }
                  else if worst < 16000 { 0xFFAA00 }
                  else { 0xFF4444 };
        ctx.fb.fill_rect(ram_x.saturating_sub(14), 5, 8, 8, ctx.fb.color_from_rgb24(dot));
    }
}

/// Stage 5c — RAM history graph (popup when input.show_ram_graph is set).
pub(super) fn render_ram_graph(ctx: &mut RenderContext) {
    let graph_w: usize = 240;
    let graph_h: usize = 100;
    let graph_x = ctx.fb.width.saturating_sub(graph_w + 8);
    let graph_y: usize = 24;
    let graph_bg = ctx.fb.color_from_rgb24(0x0a0a1e);
    let graph_border = ctx.fb.color_from_rgb24(0x334466);
    let graph_grid = ctx.fb.color_from_rgb24(0x1a1a3a);

    ctx.fb.fill_rect(graph_x, graph_y, graph_w, graph_h, graph_bg);
    ctx.fb.draw_rect(graph_x, graph_y, graph_w, graph_h, graph_border);

    // Grid lines at 25%, 50%, 75%
    for pct in [25usize, 50, 75] {
        let gy = graph_y + graph_h - (pct * graph_h / 100);
        for gx in (graph_x + 1..graph_x + graph_w - 1).step_by(4) {
            ctx.fb.set_pixel(gx, gy, graph_grid);
        }
    }

    ctx.fb.draw_string(
        graph_x + 4, graph_y + 2,
        "RAM % (2min)", ctx.fb.color_from_rgb24(0x6688AA), graph_bg,
    );
    ctx.fb.draw_string(
        graph_x + graph_w - 28, graph_y + graph_h - 14,
        "0%", ctx.fb.color_from_rgb24(0x445566), graph_bg,
    );
    ctx.fb.draw_string(
        graph_x + graph_w - 36, graph_y + 16,
        "100%", ctx.fb.color_from_rgb24(0x445566), graph_bg,
    );

    let ram_hist_len = ctx.ram_history.len();
    let samples = ctx.ram_history_count.min(graph_w - 4);
    let bar_w = 1usize.max((graph_w - 4) / samples.max(1));

    for i in 0..samples {
        let hist_idx = if ctx.ram_history_count >= ram_hist_len {
            (ctx.ram_history_idx + ram_hist_len - samples + i) % ram_hist_len
        } else {
            i
        };
        let pct_val = ctx.ram_history[hist_idx] as usize;
        let bar_height = pct_val * (graph_h - 20) / 100;
        let bx = graph_x + 2 + i * bar_w;
        let by = graph_y + graph_h - 2 - bar_height;

        let bar_color = if pct_val > 80 {
            ctx.fb.color_from_rgb24(0xFF4444)
        } else if pct_val > 50 {
            ctx.fb.color_from_rgb24(0xFFAA00)
        } else {
            ctx.fb.color_from_rgb24(0x44FF44)
        };

        if bx + bar_w < graph_x + graph_w - 1 {
            ctx.fb.fill_rect(bx, by, bar_w, bar_height, bar_color);
        }
    }
}

/// Stage 6a — Add per-element damage rects (coalesced) for partial redraw.
pub(super) fn add_targeted_damage(ctx: &mut RenderContext, wasm_fullscreen: bool) {
    if !wasm_fullscreen {
        ctx.damage.add_damage(compositor::damage::Rect::new(0, 0, ctx.fb.width as u32, 22));
        if ctx.input.omnibar_visible {
            ctx.damage.add_damage(compositor::damage::Rect::new(
                ctx.layout.text_box_x.saturating_sub(4) as u32,
                ctx.layout.text_box_y.saturating_sub(4) as u32,
                (ctx.layout.text_box_w + 8) as u32,
                (ctx.layout.text_box_h + 60) as u32,
            ));
        }
        for w in ctx.wm.windows.iter() {
            ctx.damage.add_damage(compositor::damage::Rect::new(
                w.x.max(0) as u32,
                w.y.max(0) as u32,
                (w.width + 20) as u32,
                (w.height + 40) as u32,
            ));
        }
    } else {
        ctx.damage.damage_full();
    }
}

/// Stage 6b — After full redraw, save the scene under the cursor so the
/// next mouse-move frame can restore the background pixels.
pub(super) fn save_cursor_bg(ctx: &mut RenderContext) {
    ctx.fb.save_rect(
        ctx.cursor.x as usize,
        ctx.cursor.y as usize,
        ctx.layout.cursor_w,
        ctx.layout.cursor_h,
        ctx.cursor_bg,
    );
    ctx.damage.add_damage(compositor::damage::Rect::new(
        ctx.cursor.x.max(0) as u32,
        ctx.cursor.y.max(0) as u32,
        ctx.layout.cursor_w as u32 + 2,
        ctx.layout.cursor_h as u32 + 2,
    ));
}
