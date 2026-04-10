//! Desktop UI: glass omnibar + results panel + folder/app launcher grid.

extern crate alloc;

use compositor::state::MAX_CATEGORIES;

use crate::util::format_usize;

use super::{RenderContext, TEXT_PADDING};

/// Stage 3a — Render the omnibar (visible or hidden state) and the
/// preview results panel.
pub(super) fn render_omnibar(ctx: &mut RenderContext) {
    if ctx.input.omnibar_visible {
        render_omnibar_visible(ctx);
    } else {
        render_omnibar_hidden(ctx);
    }
}

fn render_omnibar_visible(ctx: &mut RenderContext) {
    let layout = ctx.layout;
    let omnibar_alpha: u8 = 180;

    // Outer glow
    ctx.fb.fill_rect_alpha(
        layout.text_box_x.saturating_sub(2),
        layout.text_box_y.saturating_sub(2),
        layout.text_box_w + 4,
        layout.text_box_h + 4,
        0x333333,
        omnibar_alpha / 2,
    );
    // Main glass box
    ctx.fb.fill_rect_alpha(
        layout.text_box_x,
        layout.text_box_y,
        layout.text_box_w,
        layout.text_box_h,
        0x1a1a2e,
        omnibar_alpha,
    );
    ctx.fb.draw_rect(
        layout.text_box_x,
        layout.text_box_y,
        layout.text_box_w,
        layout.text_box_h,
        layout.omnibar_border,
    );

    // Draw user input text
    if ctx.input.text_len > 0 {
        let display_len = if ctx.input.text_len > layout.chars_per_line {
            layout.chars_per_line
        } else {
            ctx.input.text_len
        };
        if let Ok(display_str) = core::str::from_utf8(&ctx.input.text_buffer[..display_len]) {
            ctx.fb.draw_string_alpha(
                layout.text_box_x + TEXT_PADDING,
                layout.text_box_y + 12,
                display_str,
                layout.white,
                0x1a1a2e,
                omnibar_alpha,
            );
        }
    } else {
        ctx.fb.draw_string_alpha(
            layout.text_box_x + TEXT_PADDING,
            layout.text_box_y + 12,
            "Ask anything...",
            layout.gray,
            0x1a1a2e,
            omnibar_alpha,
        );
    }

    // Blinking caret
    let caret_x_pos = layout.text_box_x
        + TEXT_PADDING
        + (ctx.input.cursor_pos.min(layout.chars_per_line) * 8);
    if caret_x_pos < layout.text_box_x + layout.text_box_w - 30 {
        let caret_char = if ctx.input.caret_visible { "|" } else { " " };
        ctx.fb.draw_string_alpha(
            caret_x_pos,
            layout.text_box_y + 10,
            caret_char,
            layout.folk_accent,
            0x1a1a2e,
            omnibar_alpha,
        );
    }

    // ">" icon on right
    ctx.fb.draw_string_alpha(
        layout.text_box_x + layout.text_box_w - 24,
        layout.text_box_y + 12,
        ">",
        layout.folk_accent,
        0x1a1a2e,
        omnibar_alpha,
    );

    // Context hints below omnibar
    let hint = "Type <query> | open calc | gemini <prompt> | help";
    let hint_x = (ctx.fb.width.saturating_sub(hint.len() * 8)) / 2;
    ctx.fb.draw_string(
        hint_x,
        layout.text_box_y + layout.text_box_h + 16,
        hint,
        layout.dark_gray,
        layout.folk_dark,
    );

    // Results panel
    if ctx.input.show_results && ctx.input.text_len > 0 {
        render_results_panel(ctx);
    } else {
        ctx.fb.fill_rect(
            layout.results_x,
            layout.results_y,
            layout.results_w,
            layout.results_h,
            layout.folk_dark,
        );
    }
}

fn render_results_panel(ctx: &mut RenderContext) {
    let layout = ctx.layout;
    let results_bg = ctx.fb.color_from_rgb24(0x252540);

    ctx.fb.fill_rect(layout.results_x, layout.results_y, layout.results_w, layout.results_h, results_bg);
    ctx.fb.draw_rect(layout.results_x, layout.results_y, layout.results_w, layout.results_h, layout.folk_accent);

    let cmd_str = match core::str::from_utf8(&ctx.input.text_buffer[..ctx.input.text_len]) {
        Ok(s) => s,
        Err(_) => return,
    };

    ctx.fb.draw_string(
        layout.results_x + 12, layout.results_y + 12,
        "Results:", layout.folk_accent, results_bg,
    );

    let (label, hint) = if cmd_str == "ls" || cmd_str == "files" {
        ("List files in ramdisk", Some("Press Enter to run"))
    } else if cmd_str == "ps" || cmd_str == "tasks" {
        ("Show running tasks", Some("Press Enter to run"))
    } else if cmd_str == "uptime" {
        ("System uptime", Some("Press Enter to run"))
    } else if cmd_str.starts_with("calc ") {
        ("Calculator:", Some("(math evaluation coming soon)"))
    } else if cmd_str.starts_with("find ") || cmd_str.starts_with("search ") {
        ("Search Synapse", Some("Press Enter to search"))
    } else if cmd_str.starts_with("open ") {
        ("Open app:", Some("Press Enter to launch"))
    } else if cmd_str == "help" {
        ("Available commands:", Some("ls, cat, ps, uptime, find, calc, open"))
    } else {
        ("Command:", Some("Press Enter to run"))
    };

    ctx.fb.draw_string(
        layout.results_x + 12, layout.results_y + 36,
        label, ctx.layout.white, results_bg,
    );
    if cmd_str.starts_with("open ") || cmd_str.starts_with("calc ") {
        let detail = if cmd_str.starts_with("open ") { &cmd_str[5..] } else { cmd_str };
        ctx.fb.draw_string(
            layout.results_x + 12, layout.results_y + 56,
            detail, ctx.layout.folk_accent, results_bg,
        );
    } else if cmd_str.len() > 0 && !["ls", "files", "ps", "tasks", "uptime", "help"].contains(&cmd_str) {
        ctx.fb.draw_string(
            layout.results_x + 12, layout.results_y + 56,
            cmd_str, ctx.layout.folk_accent, results_bg,
        );
    }
    if let Some(h) = hint {
        ctx.fb.draw_string(
            layout.results_x + 12, layout.results_y + 80,
            h, ctx.layout.dark_gray, results_bg,
        );
    }
}

fn render_omnibar_hidden(ctx: &mut RenderContext) {
    let layout = ctx.layout;
    ctx.fb.fill_rect(
        layout.text_box_x - 2,
        layout.text_box_y - 2,
        layout.text_box_w + 4,
        layout.text_box_h + 4,
        layout.folk_dark,
    );
    ctx.fb.fill_rect(
        layout.results_x, layout.results_y, layout.results_w, layout.results_h, layout.folk_dark,
    );
    ctx.fb.fill_rect(
        0,
        layout.text_box_y + layout.text_box_h + 8,
        ctx.fb.width, 24, layout.folk_dark,
    );

    let hint = "Press Windows/Super key to open Omnibar";
    let hint_x = (ctx.fb.width.saturating_sub(hint.len() * 8)) / 2;
    ctx.fb.draw_string(hint_x, ctx.fb.height - 50, hint, layout.dark_gray, layout.folk_dark);
}

/// Stage 3b — Folder grid (home view) or app grid (folder view).
pub(super) fn render_app_launcher(ctx: &mut RenderContext) {
    let layout = ctx.layout;
    let tile_text = ctx.fb.color_from_rgb24(0xDDDDDD);
    let tile_bg = ctx.fb.color_from_rgb24(0x222244);
    let tile_border = ctx.fb.color_from_rgb24(0x444477);

    if ctx.render.open_folder < 0 {
        // HOME VIEW: show category folders
        let mut visible: [(usize, usize); MAX_CATEGORIES] = [(0, 0); MAX_CATEGORIES];
        let mut vis_count = 0;
        for i in 0..MAX_CATEGORIES {
            if ctx.categories[i].count > 0 {
                visible[vis_count] = (i, vis_count);
                vis_count += 1;
            }
        }

        if vis_count > 0 {
            let cols = vis_count.min(3);
            let grid_w = cols * (layout.folder_w + layout.folder_gap) - layout.folder_gap;
            let grid_x = (ctx.fb.width.saturating_sub(grid_w)) / 2;
            let grid_y = 120;

            for v in 0..vis_count {
                let (cat_idx, _) = visible[v];
                let col = v % 3;
                let row = v / 3;
                let fx = grid_x + col * (layout.folder_w + layout.folder_gap);
                let fy = grid_y + row * (layout.folder_h + layout.folder_gap);

                let cat = &ctx.categories[cat_idx];
                let c = ctx.fb.color_from_rgb24(cat.color);

                ctx.fb.fill_rect(fx, fy, layout.folder_w, layout.folder_h, tile_bg);
                ctx.fb.draw_rect(fx, fy, layout.folder_w, layout.folder_h, c);
                ctx.fb.draw_rect(fx + 1, fy + 1, layout.folder_w - 2, layout.folder_h - 2, tile_border);

                let preview_count = cat.count.min(4);
                for p in 0..preview_count {
                    let px = fx + 15 + (p % 2) * 35;
                    let py = fy + 10 + (p / 2) * 25;
                    ctx.fb.fill_rect(px, py, 28, 20, c);
                }

                let label = unsafe { core::str::from_utf8_unchecked(cat.label) };
                let lbl_len = label.trim_end_matches('\0').len();
                let lbl_trimmed = &label[..lbl_len];
                let lx = fx + (layout.folder_w.saturating_sub(lbl_len * 8)) / 2;
                ctx.fb.draw_string(lx, fy + layout.folder_h - 20, lbl_trimmed, tile_text, tile_bg);

                let mut nbuf = [0u8; 16];
                let ns = format_usize(cat.count, &mut nbuf);
                ctx.fb.draw_string(fx + layout.folder_w - 16, fy + 4, ns, c, tile_bg);

                if ctx.render.hover_folder == cat_idx as i32 {
                    let hover_bg = ctx.fb.color_from_rgb24(0x2a2a5a);
                    let prev_x = fx;
                    let prev_y = fy + layout.folder_h + 4;
                    let prev_w = layout.folder_w + 60;
                    let prev_h = 20 + cat.count.min(5) * 18;
                    ctx.fb.fill_rect(prev_x, prev_y, prev_w, prev_h, hover_bg);
                    ctx.fb.draw_rect(prev_x, prev_y, prev_w, prev_h, c);
                    for ai in 0..cat.count.min(5) {
                        let entry = &cat.apps[ai];
                        if entry.name_len > 0 {
                            let name = unsafe {
                                core::str::from_utf8_unchecked(&entry.name[..entry.name_len])
                            };
                            ctx.fb.draw_string(
                                prev_x + 8, prev_y + 4 + ai * 18,
                                &name[..name.len().min(16)], tile_text, hover_bg,
                            );
                        }
                    }
                    if cat.count > 5 {
                        ctx.fb.draw_string(prev_x + 8, prev_y + 4 + 5 * 18, "...", tile_text, hover_bg);
                    }
                }
            }
        }
    } else {
        // FOLDER VIEW: show apps inside the selected category
        let cat_idx = ctx.render.open_folder as usize;
        if cat_idx < MAX_CATEGORIES {
            let cat = &ctx.categories[cat_idx];
            let label = unsafe { core::str::from_utf8_unchecked(cat.label) };
            let c = ctx.fb.color_from_rgb24(cat.color);

            let header_y = 90;
            let hdr_bg = ctx.fb.color_from_rgb24(0x1a1a3a);
            ctx.fb.fill_rect(0, header_y, ctx.fb.width, 30, hdr_bg);
            ctx.fb.draw_string(16, header_y + 7, "< Back", tile_text, hdr_bg);
            let title_x = (ctx.fb.width.saturating_sub(label.trim_end_matches('\0').len() * 8)) / 2;
            ctx.fb.draw_string(title_x, header_y + 7, label.trim_end_matches('\0'), c, hdr_bg);

            let grid_w = layout.app_tile_cols * (layout.app_tile_w + layout.app_tile_gap)
                - layout.app_tile_gap;
            let grid_x = (ctx.fb.width.saturating_sub(grid_w)) / 2;
            let grid_y = 130;

            for i in 0..cat.count {
                let col = i % layout.app_tile_cols;
                let row = i / layout.app_tile_cols;
                let ax = grid_x + col * (layout.app_tile_w + layout.app_tile_gap);
                let ay = grid_y + row * (layout.app_tile_h + layout.app_tile_gap);

                ctx.fb.fill_rect(ax, ay, layout.app_tile_w, layout.app_tile_h, tile_bg);
                ctx.fb.draw_rect(ax, ay, layout.app_tile_w, layout.app_tile_h, tile_border);
                ctx.fb.fill_rect(ax + 16, ay + 8, 40, 36, c);

                let entry = &cat.apps[i];
                if entry.name_len > 0 {
                    let name = unsafe {
                        core::str::from_utf8_unchecked(&entry.name[..entry.name_len])
                    };
                    let nx = ax + (layout.app_tile_w.saturating_sub(entry.name_len.min(9) * 8)) / 2;
                    ctx.fb.draw_string(
                        nx, ay + layout.app_tile_h - 20,
                        &name[..name.len().min(9)], tile_text, tile_bg,
                    );
                }
            }
        }
    }
}
