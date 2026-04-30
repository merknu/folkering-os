//! Mouse input processing — extracted from main.rs
//!
//! Handles mouse event accumulation, cursor movement, drag operations,
//! spatial connection cables, folder/tile hit-testing, and WASM app routing.

extern crate alloc;

use compositor::damage::DamageTracker;
use compositor::draug::DraugDaemon;
use compositor::framebuffer::FramebufferView;
use compositor::state::{CursorState, InputState, RenderState, StreamState, WasmState, Category};
use compositor::window_manager::{WindowManager, HitZone, BORDER_W, TITLE_BAR_H};
use libfolk::sys::io::write_str;
use libfolk::sys::read_mouse;

/// Layout/color constants needed for mouse processing.
/// Computed once in main() and passed in to avoid recomputation.
pub struct MouseLayout {
    pub folk_dark: u32,
    pub cursor_white: u32,
    pub cursor_red: u32,
    pub cursor_blue: u32,
    pub cursor_magenta: u32,
    pub cursor_outline: u32,
    pub text_box_x: usize,
    pub text_box_y: usize,
    pub text_box_w: usize,
    pub text_box_h: usize,
    pub results_x: usize,
    pub results_y: usize,
    pub results_w: usize,
    pub results_h: usize,
    pub max_categories: usize,
    pub folder_w: usize,
    pub folder_h: usize,
    pub folder_gap: usize,
    pub app_tile_w: usize,
    pub app_tile_h: usize,
    pub app_tile_gap: usize,
    pub app_tile_cols: usize,
    pub cursor_w: usize,
    pub cursor_h: usize,
}

pub struct MouseResult {
    pub did_work: bool,
    pub need_redraw: bool,
    pub had_events: bool,
}

/// Inline rdtsc — duplicated from main.rs since it's a bare #[inline] fn
#[inline(always)]
fn rdtsc() -> u64 {
    let lo: u32; let hi: u32;
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)); }
    ((hi as u64) << 32) | lo as u64
}

/// Process all pending mouse events, update cursor, hit-test UI.
///
/// This is a verbatim extraction of the "Process mouse input" block from main.rs
/// (lines 1796-2255). Logic is unchanged; only variable prefixes adapted.
pub fn process_mouse(
    cursor: &mut CursorState,
    wm: &mut WindowManager,
    wasm: &mut WasmState,
    input: &mut InputState,
    render: &mut RenderState,
    stream: &mut StreamState,
    draug: &mut DraugDaemon,
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    cursor_drawn: &mut bool,
    last_buttons: &mut u8,
    cursor_bg: &mut [u32],
    tsc_per_us: u64,
    categories: &[Category],
    layout: &MouseLayout,
) -> MouseResult {
    let mut did_work = false;
    let mut need_redraw = false;

    // ===== Process mouse input =====
    // Accumulate all pending mouse events, then draw cursor ONCE
    let mut accumulated_dx: i32 = 0;
    let mut accumulated_dy: i32 = 0;
    let mut latest_buttons: u8 = *last_buttons;
    let mut had_mouse_events = false;

    // Drain capped at 1024 events per call (Issue #56). PS/2 mouse
    // generates 3 bytes per event, kernel ring is small, so flood needs
    // a lot to hit 1024. Above that we yield back to the main loop and
    // pick up the rest next tick.
    let mut events_processed = 0u32;
    while let Some(event) = read_mouse() {
        events_processed += 1;
        if events_processed > 1024 { break; }
        did_work = true;
        if !had_mouse_events {
            // Log first mouse event per batch to serial
            write_str("[M]\n");
        }
        had_mouse_events = true;
        accumulated_dx += event.dx as i32;
        accumulated_dy -= event.dy as i32; // Invert Y (mouse up = negative dy in PS/2)
        latest_buttons = event.buttons;
    }

    if had_mouse_events {
        // Tell Draug the user is actively interacting
        let input_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
        draug.on_user_input(input_ms);
        // Hover detection for folder preview (home view)
        if render.open_folder < 0 && wasm.active_app.is_none() {
            let old_hover = render.hover_folder;
            render.hover_folder = -1;
            let mut vi = 0usize;
            for ci in 0..layout.max_categories {
                if categories[ci].count == 0 { continue; }
                let cols = { let mut c = 0; for j in 0..layout.max_categories { if categories[j].count > 0 { c += 1; } } c.min(3) };
                let gw = cols * (layout.folder_w + layout.folder_gap) - layout.folder_gap;
                let gx = (fb.width.saturating_sub(gw)) / 2;
                let gy: usize = 120;
                let col = vi % 3;
                let row = vi / 3;
                let fx = gx + col * (layout.folder_w + layout.folder_gap);
                let fy = gy + row * (layout.folder_h + layout.folder_gap);
                if cursor.x as usize >= fx && (cursor.x as usize) < fx + layout.folder_w
                    && cursor.y as usize >= fy && (cursor.y as usize) < fy + layout.folder_h {
                    render.hover_folder = ci as i32;
                }
                vi += 1;
            }
            // Hover change: just damage the folder area, don't full-redraw
            if render.hover_folder != old_hover {
                // Damage old and new folder rectangles
                // (folders render will happen in next full redraw; for now just mark cursor.bg_dirty)
                cursor.bg_dirty = true;
                did_work = true;
            }
        }

        // Route mouse events to active WASM app (Phase 2)
        if let Some(app) = &mut wasm.active_app {
            let new_click = (latest_buttons & 1 != 0) && (*last_buttons & 1 == 0);
            // Always send mouse position
            app.push_event(compositor::wasm_runtime::FolkEvent {
                event_type: 1, x: cursor.x, y: cursor.y, data: latest_buttons as i32,
            });
            // Send click event on button press edge
            if new_click {
                write_str("[CLICK->WASM]\n");
                app.push_event(compositor::wasm_runtime::FolkEvent {
                    event_type: 2, x: cursor.x, y: cursor.y, data: 1,
                });

                // Friction Sensor: rage click detection (>5 clicks in 2s)
                let now = libfolk::sys::uptime();
                cursor.click_timestamps[cursor.click_ts_idx] = now;
                cursor.click_ts_idx = (cursor.click_ts_idx + 1) % 8;
                // Count clicks in last 2 seconds
                let mut recent = 0u8;
                for ts in &cursor.click_timestamps {
                    if *ts > 0 && now.saturating_sub(*ts) < 2000 { recent += 1; }
                }
                if recent > 5 {
                    if let Some(ref k) = wasm.active_app_key {
                        let h = compositor::draug::DraugDaemon::key_hash_pub(k);
                        draug.friction.record_signal(h, compositor::draug::FRICTION_RAGE_CLICK);
                        write_str("[Friction] rage_click for '");
                        write_str(&k[..k.len().min(30)]);
                        write_str("'\n");
                    }
                    // Reset to avoid spamming
                    cursor.click_timestamps = [0; 8];
                }
            }
        }

        // Sanity check cursor position
        if cursor.x < 0 || cursor.x >= fb.width as i32 || cursor.y < 0 || cursor.y >= fb.height as i32 {
            cursor.x = (fb.width / 2) as i32;
            cursor.y = (fb.height / 2) as i32;
            cursor.bg_dirty = true;
            *cursor_drawn = false;
        }

        // Determine cursor color based on button state
        let cursor_fill = match (latest_buttons & 1 != 0, latest_buttons & 2 != 0) {
            (true, true) => layout.cursor_magenta,
            (true, false) => layout.cursor_red,
            (false, true) => layout.cursor_blue,
            (false, false) => layout.cursor_white,
        };

        // First mouse event ever: draw cursor at center
        if !*cursor_drawn {
            fb.save_rect(cursor.x as usize, cursor.y as usize, layout.cursor_w, layout.cursor_h, cursor_bg);
            fb.draw_cursor(cursor.x as usize, cursor.y as usize, cursor_fill, layout.cursor_outline);
            *cursor_drawn = true;
            *last_buttons = latest_buttons;
        }

        // Calculate new position from accumulated delta
        let new_x = cursor.x.saturating_add(accumulated_dx);
        let new_y = cursor.y.saturating_add(accumulated_dy);

        // Clamp to screen bounds
        let new_x = if new_x < 0 { 0 } else if new_x >= fb.width as i32 { fb.width as i32 - 1 } else { new_x };
        let new_y = if new_y < 0 { 0 } else if new_y >= fb.height as i32 { fb.height as i32 - 1 } else { new_y };

        // ===== Milestone 1.4 + 2.2: Mouse Click Hit-Testing + Window Dragging =====
        let left_now = latest_buttons & 1 != 0;
        let left_pressed = left_now && !cursor.prev_left_button;  // rising edge
        let left_released = !left_now && cursor.prev_left_button; // falling edge
        cursor.prev_left_button = left_now;

        // Window drag: continue drag if in progress
        if left_now {
            if let Some(drag_id) = cursor.dragging_window_id {
                let dx = new_x - cursor.drag_last_x;
                let dy = new_y - cursor.drag_last_y;
                cursor.drag_last_x = new_x;
                cursor.drag_last_y = new_y;
                if dx != 0 || dy != 0 {
                    if let Some(win) = wm.get_window_mut(drag_id) {
                        win.x = win.x.saturating_add(dx);
                        win.y = win.y.saturating_add(dy);
                        // Clamp to screen
                        if win.x < 0 { win.x = 0; }
                        if win.y < 0 { win.y = 0; }
                    }
                    need_redraw = true;
                    cursor.bg_dirty = true;
                }
            }
        }

        // Release drag
        if left_released {
            cursor.dragging_window_id = None;
            // Cancel connection drag if not on InputPort
            if wasm.connection_drag.is_some() {
                // Check if we're over an InputPort
                if let Some((win_id, HitZone::InputPort)) = wm.hit_test(new_x, new_y) {
                    if let Some(drag) = wasm.connection_drag.take() {
                        if drag.source_win_id != win_id {
                            wasm.node_connections.push(compositor::spatial::NodeConnection {
                                source_win_id: drag.source_win_id,
                                dest_win_id: win_id,
                            });
                            // Instantiate WASM apps for both windows if not already running
                            let config = compositor::wasm_runtime::WasmConfig {
                                screen_width: 400, screen_height: 300,
                                uptime_ms: libfolk::sys::uptime() as u32,
                            };
                            for &wid in &[drag.source_win_id, win_id] {
                                if !wasm.window_apps.contains_key(&wid) {
                                    if let Some(w) = wm.get_window(wid) {
                                        if let Some(ref node_wasm_bytes) = w.node_wasm {
                                            if let Ok(app) = compositor::wasm_runtime::PersistentWasmApp::new(node_wasm_bytes, config.clone()) {
                                                wasm.window_apps.insert(wid, app);
                                            }
                                        }
                                    }
                                }
                            }
                            write_str("[Spatial] Connected + apps instantiated!\n");
                        }
                    }
                } else {
                    wasm.connection_drag = None;
                    write_str("[Spatial] Drag cancelled\n");
                }
                need_redraw = true;
            }
        }

        // Update connection drag position
        if let Some(ref mut drag) = wasm.connection_drag {
            drag.current_x = new_x;
            drag.current_y = new_y;
            need_redraw = true;
        }

        if left_pressed {
            let cx = new_x;
            let cy = new_y;

            // Hit-test windows first (topmost)
            let mut handled = false;
            if let Some((win_id, zone)) = wm.hit_test(cx, cy) {
                match zone {
                    HitZone::CloseButton => {
                        wm.close_window(win_id);
                        if win_id == stream.win_id {
                            stream.win_id = 0;
                        }
                        need_redraw = true;
                        cursor.bg_dirty = true;
                        handled = true;
                        // IQE: window close event
                        libfolk::sys::com3_write(b"IQE,WIN_CLOSE,0\n");
                    }
                    HitZone::TitleBar => {
                        wm.focus(win_id);
                        cursor.dragging_window_id = Some(win_id);
                        cursor.drag_last_x = new_x;
                        cursor.drag_last_y = new_y;
                        need_redraw = true;
                        handled = true;
                        // IQE: window drag start
                        libfolk::sys::com3_write(b"IQE,WIN_DRAG,0\n");
                    }
                    HitZone::OutputPort => {
                        // Start dragging a connection cable from this output port
                        wasm.connection_drag = Some(compositor::spatial::ConnectionDrag {
                            source_win_id: win_id,
                            current_x: cx,
                            current_y: cy,
                        });
                        write_str("[Spatial] Dragging from output port\n");
                        handled = true;
                        need_redraw = true;
                    }
                    HitZone::InputPort => {
                        if let Some(drag) = wasm.connection_drag.take() {
                            if drag.source_win_id != win_id {
                                wasm.node_connections.push(compositor::spatial::NodeConnection {
                                    source_win_id: drag.source_win_id,
                                    dest_win_id: win_id,
                                });
                                // Instantiate apps for connected windows
                                let cfg = compositor::wasm_runtime::WasmConfig {
                                    screen_width: 400, screen_height: 300,
                                    uptime_ms: libfolk::sys::uptime() as u32,
                                };
                                for &wid in &[drag.source_win_id, win_id] {
                                    if !wasm.window_apps.contains_key(&wid) {
                                        if let Some(w) = wm.get_window(wid) {
                                            if let Some(ref node_wasm_bytes) = w.node_wasm {
                                                if let Ok(app) = compositor::wasm_runtime::PersistentWasmApp::new(node_wasm_bytes, cfg.clone()) {
                                                    wasm.window_apps.insert(wid, app);
                                                }
                                            }
                                        }
                                    }
                                }
                                write_str("[Spatial] Connected + apps instantiated!\n");
                            }
                        }
                        handled = true;
                        need_redraw = true;
                    }
                    HitZone::Content => {
                        wm.focus(win_id);
                        // Check if App window widget was clicked
                        let mut btn_info: Option<(u32, u32)> = None; // (action_id, owner)
                        let mut focus_click = false;
                        // Determine what was clicked: Button -> IPC, TextInput -> focus only
                        if let Some(win) = wm.get_window(win_id) {
                            if matches!(win.kind, compositor::window_manager::WindowKind::App)
                                && !win.widgets.is_empty()
                            {
                                let content_x = win.x as usize + BORDER_W + 6;
                                let content_y = win.y as usize + BORDER_W + TITLE_BAR_H + 6;
                                let owner = win.owner_task;

                                match compositor::window_manager::hit_test_widgets(
                                    &win.widgets, content_x, content_y, cx as usize, cy as usize
                                ) {
                                    Some(compositor::window_manager::FocusableKind::Button { action_id }) => {
                                        btn_info = Some((action_id, owner));
                                    }
                                    Some(compositor::window_manager::FocusableKind::TextInput { .. }) => {
                                        focus_click = true;
                                    }
                                    None => {}
                                }
                                // Set focus index via hit_test_focusable_index
                                if focus_click || btn_info.is_some() {
                                    // We'll set focus below after releasing borrow
                                }
                            }
                        }
                        // Set focused_widget for click on any focusable
                        if focus_click || btn_info.is_some() {
                            if let Some(win) = wm.get_window(win_id) {
                                let content_x = win.x as usize + BORDER_W + 6;
                                let content_y = win.y as usize + BORDER_W + TITLE_BAR_H + 6;
                                let idx = compositor::window_manager::hit_test_focusable_index(
                                    &win.widgets, content_x, content_y, cx as usize, cy as usize
                                );
                                if let Some(win) = wm.get_window_mut(win_id) {
                                    win.focused_widget = idx;
                                }
                            }
                        } else {
                            // Click on non-focusable area clears focus
                            if let Some(win) = wm.get_window_mut(win_id) {
                                win.focused_widget = None;
                            }
                        }
                        // Send button IPC outside of borrow
                        if let Some((action_id, owner)) = btn_info {
                            if owner != 0 {
                                let event_payload = 0xAC10_u64
                                    | ((action_id as u64) << 16)
                                    | ((win_id as u64) << 48);
                                let reply = unsafe {
                                    libfolk::syscall::syscall3(
                                        libfolk::syscall::SYS_IPC_SEND,
                                        owner as u64,
                                        event_payload,
                                        0
                                    )
                                };
                                let reply_magic = (reply >> 48) as u16;
                                if reply_magic == 0x5549 {
                                    let ui_handle = (reply & 0xFFFFFFFF) as u32;
                                    crate::ipc_helpers::update_window_widgets(wm, win_id, ui_handle);
                                }
                            }
                        }
                        need_redraw = true;
                        handled = true;
                    }
                }
            }

            if !handled {
                let cx = cx as usize;
                let cy = cy as usize;

                // Hit-test: RAM% in status bar (toggle graph)
                if cy < 20 && cx > fb.width.saturating_sub(80) {
                    input.show_ram_graph = !input.show_ram_graph;
                    need_redraw = true;
                    damage.damage_full();
                    handled = true;
                }

                // Hit-test: app launcher (folders or app tiles)
                if render.open_folder < 0 {
                    // HOME: check folder clicks
                    let mut vis_count = 0usize;
                    for ci in 0..layout.max_categories { if categories[ci].count > 0 { vis_count += 1; } }
                    if vis_count > 0 {
                        let cols = vis_count.min(3);
                        let gw = cols * (layout.folder_w + layout.folder_gap) - layout.folder_gap;
                        let gx = (fb.width.saturating_sub(gw)) / 2;
                        let gy: usize = 120;
                        let mut vi = 0usize;
                        for ci in 0..layout.max_categories {
                            if categories[ci].count == 0 { continue; }
                            let col = vi % 3;
                            let row = vi / 3;
                            let fx = gx + col * (layout.folder_w + layout.folder_gap);
                            let fy = gy + row * (layout.folder_h + layout.folder_gap);
                            if cx >= fx && cx < fx + layout.folder_w && cy >= fy && cy < fy + layout.folder_h {
                                render.open_folder = ci as i32;
                                handled = true;
                                need_redraw = true;
                                damage.damage_full();
                                break;
                            }
                            vi += 1;
                        }
                    }
                } else {
                    // FOLDER VIEW: check "< Back" button or app tile clicks
                    let header_y: usize = 90;
                    if cy >= header_y && cy < header_y + 30 && cx < 100 {
                        // Back button
                        render.open_folder = -1;
                        handled = true;
                        need_redraw = true;
                        damage.damage_full();
                    } else {
                        // App tile click
                        let cat_idx = render.open_folder as usize;
                        if cat_idx < layout.max_categories {
                            let gw = layout.app_tile_cols * (layout.app_tile_w + layout.app_tile_gap) - layout.app_tile_gap;
                            let gx = (fb.width.saturating_sub(gw)) / 2;
                            let gy: usize = 130;
                            for i in 0..categories[cat_idx].count {
                                let col = i % layout.app_tile_cols;
                                let row = i / layout.app_tile_cols;
                                let ax = gx + col * (layout.app_tile_w + layout.app_tile_gap);
                                let ay = gy + row * (layout.app_tile_h + layout.app_tile_gap);
                                if cx >= ax && cx < ax + layout.app_tile_w && cy >= ay && cy < ay + layout.app_tile_h {
                                    render.tile_clicked = i as i32;
                                    handled = true;
                                    break;
                                }
                            }
                        }
                    }
                }

                // Hit-test: click inside the omnibar
                if cx >= layout.text_box_x && cx < layout.text_box_x + layout.text_box_w
                    && cy >= layout.text_box_y && cy < layout.text_box_y + layout.text_box_h
                {
                    if input.show_results {
                        input.show_results = false;
                        need_redraw = true;
                    }
                }

                // Hit-test: click in results panel items
                if input.show_results
                    && cx >= layout.results_x && cx < layout.results_x + layout.results_w
                    && cy >= layout.results_y && cy < layout.results_y + layout.results_h
                {
                    input.show_results = false;
                    need_redraw = true;
                }
            }
        }

        // Redraw cursor if it moved, button state changed, or background is dirty
        let old_cx = cursor.x;
        let old_cy = cursor.y;
        if new_x != cursor.x || new_y != cursor.y || latest_buttons != *last_buttons || cursor.bg_dirty {
            // Erase old cursor by restoring saved background
            if !cursor.bg_dirty {
                fb.restore_rect(cursor.x as usize, cursor.y as usize, layout.cursor_w, layout.cursor_h, cursor_bg);
            }
            cursor.bg_dirty = false;

            // Update position
            cursor.x = new_x;
            cursor.y = new_y;
            *last_buttons = latest_buttons;

            // Determine cursor color based on button state (recalc for final draw)
            let cursor_fill = match (latest_buttons & 1 != 0, latest_buttons & 2 != 0) {
                (true, true) => layout.cursor_magenta,
                (true, false) => layout.cursor_red,
                (false, true) => layout.cursor_blue,
                (false, false) => layout.cursor_white,
            };

            // Save background at new position, then draw cursor on top
            fb.save_rect(cursor.x as usize, cursor.y as usize, layout.cursor_w, layout.cursor_h, cursor_bg);
            fb.draw_cursor(cursor.x as usize, cursor.y as usize, cursor_fill, layout.cursor_outline);

            // Damage old + new cursor areas for VirtIO-GPU flush
            damage.add_damage(compositor::damage::Rect::new(
                old_cx.max(0) as u32, old_cy.max(0) as u32, layout.cursor_w as u32 + 2, layout.cursor_h as u32 + 2));
            damage.add_damage(compositor::damage::Rect::new(
                cursor.x.max(0) as u32, cursor.y.max(0) as u32, layout.cursor_w as u32 + 2, layout.cursor_h as u32 + 2));
            // Cursor-only movement: DON'T set need_redraw (avoids full desktop re-render).
            // The damage tracker + GPU flush handle the cursor update efficiently.
            did_work = true;
        }
    } // end if had_mouse_events

    MouseResult { did_work, need_redraw, had_events: had_mouse_events }
}
