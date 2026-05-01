//! Keyboard input processing — extracted from main.rs
//!
//! Handles key routing to WASM apps, interactive windows, app widget navigation,
//! clipboard copy/paste, omnibar text editing, and window management shortcuts.

extern crate alloc;

use compositor::damage::DamageTracker;
use compositor::draug::DraugDaemon;
use compositor::framebuffer::FramebufferView;
use compositor::state::{InputState, RenderState, WasmState};
use compositor::window_manager::WindowManager;
use libfolk::sys::io::write_str;
use libfolk::sys::{read_key, uptime, shmem_create, shmem_map, shmem_unmap, shmem_destroy, shmem_grant};
use crate::ui_dump::emit_ui_dump;
use crate::ipc_helpers::{update_window_widgets, clamp_focus};

/// Layout/color constants needed for keyboard processing.
pub struct KeyboardLayout {
    pub folk_dark: u32,
    pub folk_accent: u32,
    pub gray: u32,
    pub max_text_len: usize,
    pub compositor_shmem_vaddr: usize,
}

pub struct KeyboardResult {
    pub did_work: bool,
    pub need_redraw: bool,
    pub execute_command: bool,
    pub win_execute_command: Option<u32>,
}

/// Inline rdtsc — duplicated from main.rs since it's a bare #[inline] fn
#[inline(always)]
fn rdtsc() -> u64 {
    let lo: u32; let hi: u32;
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)); }
    ((hi as u64) << 32) | lo as u64
}

/// Process all pending keyboard events.
///
/// This is a verbatim extraction of the "Process keyboard input" block from main.rs
/// (lines 2330-2953). Logic is unchanged; only variable prefixes adapted.
pub fn process_keyboard(
    input: &mut InputState,
    wasm: &mut WasmState,
    wm: &mut WindowManager,
    render: &mut RenderState,
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    draug: &mut DraugDaemon,
    tsc_per_us: u64,
    layout: &KeyboardLayout,
) -> KeyboardResult {
    let mut did_work = false;
    let mut need_redraw = false;

    // ===== Process keyboard input =====
    // First, collect all pending keys without redrawing.
    //
    // Drain capped at 1024 keys per call (Issue #56) — practical paste
    // bursts are <500 keys, autorepeat is ~30 keys/sec, so 1024 is well
    // above legitimate use. Defends against a flooded COM1 (read_key
    // falls through to serial::read_byte) pinning the compositor main
    // loop. Use a bounded `for` loop so the 1025th key stays in the
    // kernel ring for the next tick instead of being consumed and
    // dropped (PR #63 Copilot review).
    let mut execute_command = false;
    let mut win_execute_command: Option<u32> = None; // window id to execute from
    for _ in 0..1024 {
        let key = match read_key() {
            Some(k) => k,
            None => break,
        };
        did_work = true;
        let input_ms = if tsc_per_us > 0 { rdtsc() / tsc_per_us / 1000 } else { 0 };
        // Phase A.5: forward to draug-daemon over IPC. Local update
        // stays for the transition window so compositor-side HUD reads
        // (which still target the in-process DraugDaemon) keep
        // returning fresh values; the call goes away in step 2.4 once
        // the local instance is dropped.
        libfolk::sys::draug::send_user_input(input_ms);
        draug.on_user_input(input_ms);

        // Ctrl+G (0x07) or 'G'/'g': toggle RAM graph
        if key == 0x07 || (wasm.active_app.is_none() && (key == b'G' || key == b'g') && !input.omnibar_visible) {
            input.show_ram_graph = !input.show_ram_graph;
            need_redraw = true;
            damage.damage_full(); // RAM graph covers large area
            continue;
        }

        // Route to active WASM app (Phase 2) -- ESC kills the app
        // ESC: close folder view first, then WASM app
        if key == 0x1B && render.open_folder >= 0 && wasm.active_app.is_none() {
            render.open_folder = -1;
            need_redraw = true;
            damage.damage_full(); // folder covers large area, full redraw needed
            continue;
        }
        if let Some(app) = &mut wasm.active_app {
            if key == 0x1B { // ESC
                // Friction Sensor: detect quick close (<3s = frustration)
                if wasm.app_open_since_ms > 0 {
                    let open_duration = libfolk::sys::uptime().saturating_sub(wasm.app_open_since_ms);
                    if open_duration < 3000 {
                        if let Some(ref k) = wasm.active_app_key {
                            let h = compositor::draug::DraugDaemon::key_hash_pub(k);
                            // Phase A.5 Path A.2: forward friction
                            // signal to draug-daemon so its friction
                            // map sees the same input pattern. Local
                            // call stays for autodream's gating
                            // (which still consults compositor's local
                            // DraugDaemon) until autodream migrates.
                            libfolk::sys::draug::send_friction_signal(
                                h, compositor::draug::FRICTION_QUICK_CLOSE);
                            draug.friction.record_signal(h, compositor::draug::FRICTION_QUICK_CLOSE);
                            write_str("[Friction] quick_close for '");
                            write_str(&k[..k.len().min(30)]);
                            write_str("'\n");
                        }
                    }
                }
                wasm.active_app = None;
                wasm.active_app_key = None;
                wasm.app_open_since_ms = 0;
                wasm.fuel_fail_count = 0;
                // Also close streaming pipeline
                wasm.streaming_upstream = None;
                wasm.streaming_downstream = None;
                // Clear WASM residue from framebuffer
                fb.clear(layout.folk_dark);
                // Re-draw desktop title
                let title_x2 = (fb.width.saturating_sub(12 * 8)) / 2;
                fb.draw_string(title_x2, 40, "FOLKERING OS", layout.folk_accent, layout.folk_dark);
                let sub_x2 = (fb.width.saturating_sub(14 * 8)) / 2;
                fb.draw_string(sub_x2, 60, "Neural Desktop", layout.gray, layout.folk_dark);
                need_redraw = true;
                damage.damage_full();
                continue;
            }
            app.push_event(compositor::wasm_runtime::FolkEvent {
                event_type: 3, x: key as i32, y: 0, data: key as i32,
            });
        }

        // Arrow key codes from kernel keyboard driver
        const KEY_ARROW_LEFT: u8 = 0x82;
        const KEY_ARROW_RIGHT: u8 = 0x83;
        const KEY_HOME: u8 = 0x84;
        const KEY_END: u8 = 0x85;
        const KEY_DELETE: u8 = 0x86;
        const KEY_SHIFT_TAB: u8 = 0x87;
        const KEY_ALT_TAB: u8 = 0x88;
        const KEY_CTRL_F12: u8 = 0x89;
        const KEY_CTRL_C: u8 = 0x8A;
        const KEY_CTRL_V: u8 = 0x8B;

        // Ctrl+F12: UI state dump to serial (for MCP automation)
        if key == KEY_CTRL_F12 {
            emit_ui_dump(&wm, input.omnibar_visible, &input.text_buffer, input.text_len, input.cursor_pos);
            continue;
        }

        // ===== Ctrl+C: Copy to clipboard =====
        if key == KEY_CTRL_C {
            let mut copied = false;

            if !input.omnibar_visible {
                if let Some(focused_id) = wm.focused_id {
                    let win_is_interactive = wm.get_window(focused_id).map(|w| w.interactive).unwrap_or(false);
                    let win_is_app = wm.get_window(focused_id)
                        .map(|w| matches!(w.kind, compositor::window_manager::WindowKind::App) && !w.widgets.is_empty())
                        .unwrap_or(false);

                    if win_is_app {
                        if let Some(win) = wm.get_window(focused_id) {
                            // Priority 1 & 2: Focused TextInput or Button
                            if let Some(idx) = win.focused_widget {
                                if let Some((buf, len)) = compositor::window_manager::nth_focusable_text(&win.widgets, idx) {
                                    if len > 0 {
                                        let copy_len = len.min(256);
                                        input.clipboard_buf[..copy_len].copy_from_slice(&buf[..copy_len]);
                                        input.clipboard_len = copy_len;
                                        copied = true;
                                    }
                                }
                            }
                            // Priority 3: First Label (e.g. Calc display)
                            if !copied {
                                if let Some((buf, len)) = compositor::window_manager::first_label_text(&win.widgets) {
                                    if len > 0 {
                                        let copy_len = len.min(256);
                                        input.clipboard_buf[..copy_len].copy_from_slice(&buf[..copy_len]);
                                        input.clipboard_len = copy_len;
                                        copied = true;
                                    }
                                }
                            }
                        }
                    } else if win_is_interactive {
                        // Priority 4: Terminal input_buf
                        if let Some(win) = wm.get_window(focused_id) {
                            if win.input_len > 0 {
                                let copy_len = win.input_len.min(256);
                                input.clipboard_buf[..copy_len].copy_from_slice(&win.input_buf[..copy_len]);
                                input.clipboard_len = copy_len;
                                copied = true;
                            }
                        }
                    }
                }
            }

            // Priority 5: Omnibar text
            if !copied && input.omnibar_visible && input.text_len > 0 {
                let copy_len = input.text_len.min(256);
                input.clipboard_buf[..copy_len].copy_from_slice(&input.text_buffer[..copy_len]);
                input.clipboard_len = copy_len;
                copied = true;
            }

            if copied {
                // Show HUD confirmation
                render.hud_title = [0u8; 32];
                let prefix = b"Copied: ";
                render.hud_title[..prefix.len()].copy_from_slice(prefix);
                let show_len = input.clipboard_len.min(32 - prefix.len());
                render.hud_title[prefix.len()..prefix.len() + show_len].copy_from_slice(&input.clipboard_buf[..show_len]);
                render.hud_title_len = prefix.len() + show_len;
                render.hud_show_until = uptime() + 1000;
                need_redraw = true;
            }
            continue;
        }

        // ===== Ctrl+V: Paste from clipboard =====
        if key == KEY_CTRL_V && input.clipboard_len > 0 {
            let mut pasted = false;

            if !input.omnibar_visible {
                if let Some(focused_id) = wm.focused_id {
                    let win_is_interactive = wm.get_window(focused_id).map(|w| w.interactive).unwrap_or(false);
                    let win_is_app = wm.get_window(focused_id)
                        .map(|w| matches!(w.kind, compositor::window_manager::WindowKind::App) && !w.widgets.is_empty())
                        .unwrap_or(false);

                    if win_is_app {
                        // Priority 1: Focused TextInput
                        let focused_idx = wm.get_window(focused_id).and_then(|w| w.focused_widget);
                        let is_text_input = if let Some(idx) = focused_idx {
                            wm.get_window(focused_id)
                                .and_then(|w| compositor::window_manager::nth_focusable(&w.widgets, idx))
                                .map(|k| matches!(k, compositor::window_manager::FocusableKind::TextInput { .. }))
                                .unwrap_or(false)
                        } else { false };

                        if is_text_input {
                            if let Some(idx) = focused_idx {
                                if let Some(win) = wm.get_window_mut(focused_id) {
                                    if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                        if let compositor::window_manager::UiWidget::TextInput { value, value_len, cursor_pos, max_len, .. } = w {
                                            let available = (*max_len as usize).saturating_sub(*value_len);
                                            let paste_len = input.clipboard_len.min(available);
                                            if paste_len > 0 {
                                                value.copy_within(*cursor_pos..*value_len, *cursor_pos + paste_len);
                                                value[*cursor_pos..*cursor_pos + paste_len].copy_from_slice(&input.clipboard_buf[..paste_len]);
                                                *value_len += paste_len;
                                                *cursor_pos += paste_len;
                                                need_redraw = true;
                                                pasted = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if win_is_interactive {
                        // Priority 2: Terminal input_buf
                        if let Some(win) = wm.get_window_mut(focused_id) {
                            let available = 126usize.saturating_sub(win.input_len);
                            let paste_len = input.clipboard_len.min(available);
                            if paste_len > 0 {
                                // Shift existing text right
                                let mut i = win.input_len;
                                while i > win.input_cursor {
                                    win.input_buf[i + paste_len - 1] = win.input_buf[i - 1];
                                    i -= 1;
                                }
                                win.input_buf[win.input_cursor..win.input_cursor + paste_len].copy_from_slice(&input.clipboard_buf[..paste_len]);
                                win.input_len += paste_len;
                                win.input_cursor += paste_len;
                                need_redraw = true;
                                pasted = true;
                            }
                        }
                    }
                }
            }

            // Priority 3: Omnibar
            if !pasted && input.omnibar_visible {
                let available = (layout.max_text_len - 1).saturating_sub(input.text_len);
                let paste_len = input.clipboard_len.min(available);
                if paste_len > 0 {
                    // Shift existing text right using copy_within
                    input.text_buffer.copy_within(input.cursor_pos..input.text_len, input.cursor_pos + paste_len);
                    input.text_buffer[input.cursor_pos..input.cursor_pos + paste_len].copy_from_slice(&input.clipboard_buf[..paste_len]);
                    input.text_len += paste_len;
                    input.cursor_pos += paste_len;
                    need_redraw = true;
                }
            }
            continue;
        }

        // Alt+Tab: cycle window focus (highest priority, before all other routing)
        if key == KEY_ALT_TAB {
            if let Some((title, tlen)) = wm.cycle_next_window() {
                render.hud_title = title;
                render.hud_title_len = tlen;
                render.hud_show_until = uptime() + 1000;
            }
            input.omnibar_visible = false;
            need_redraw = true;
            continue;
        }

        // Route keys to focused interactive window when omnibar is hidden
        if !input.omnibar_visible {
            let mut key_consumed = false;
            if let Some(focused_id) = wm.focused_id {
                // Check window type first with immutable borrow
                let win_is_interactive = wm.get_window(focused_id).map(|w| w.interactive).unwrap_or(false);
                let win_is_app_with_widgets = wm.get_window(focused_id)
                    .map(|w| matches!(w.kind, compositor::window_manager::WindowKind::App) && !w.widgets.is_empty())
                    .unwrap_or(false);

                if win_is_interactive {
                    if let Some(win) = wm.get_window_mut(focused_id) {
                        match key {
                            0x08 | 0x7F => {
                                if win.input_cursor > 0 {
                                    let mut i = win.input_cursor - 1;
                                    while i < win.input_len - 1 {
                                        win.input_buf[i] = win.input_buf[i + 1];
                                        i += 1;
                                    }
                                    win.input_len -= 1;
                                    win.input_buf[win.input_len] = 0;
                                    win.input_cursor -= 1;
                                    need_redraw = true;
                                }
                            }
                            b'\n' | b'\r' => {
                                if win.input_len > 0 {
                                    win_execute_command = Some(focused_id);
                                    need_redraw = true;
                                }
                            }
                            0x1B => {
                                // Escape: toggle omnibar back
                                input.omnibar_visible = true;
                                need_redraw = true;
                            }
                            0x20..=0x7E => {
                                if win.input_len < 126 {
                                    let mut i = win.input_len;
                                    while i > win.input_cursor {
                                        win.input_buf[i] = win.input_buf[i - 1];
                                        i -= 1;
                                    }
                                    win.input_buf[win.input_cursor] = key;
                                    win.input_len += 1;
                                    win.input_cursor += 1;
                                    need_redraw = true;
                                }
                            }
                            _ => {}
                        }
                        key_consumed = true;
                    }
                } else if win_is_app_with_widgets {
                    // App window keyboard navigation (Tab/Shift+Tab/Enter/Space/Text editing)
                    let mut activate_info: Option<(u32, u32, u32)> = None; // (action_id, owner, win_id)
                    let mut text_submit_info: Option<(u32, u32, u32, [u8; 64], usize)> = None; // (action_id, owner, win_id, text, len)

                    // Determine current focused widget kind
                    let focused_kind = if let Some(win) = wm.get_window(focused_id) {
                        win.focused_widget.and_then(|idx| compositor::window_manager::nth_focusable(&win.widgets, idx))
                    } else { None };

                    if let Some(win) = wm.get_window_mut(focused_id) {
                        match key {
                            b'\t' => {
                                let fc = compositor::window_manager::count_focusable(&win.widgets);
                                if fc > 0 {
                                    let cur = win.focused_widget.unwrap_or(fc.wrapping_sub(1));
                                    win.focused_widget = Some((cur + 1) % fc);
                                    need_redraw = true;
                                }
                                key_consumed = true;
                            }
                            KEY_SHIFT_TAB => {
                                let fc = compositor::window_manager::count_focusable(&win.widgets);
                                if fc > 0 {
                                    let cur = win.focused_widget.unwrap_or(1);
                                    win.focused_widget = Some(cur.checked_sub(1).unwrap_or(fc - 1));
                                    need_redraw = true;
                                }
                                key_consumed = true;
                            }
                            b'\n' | b'\r' => {
                                if let Some(idx) = win.focused_widget {
                                    match focused_kind {
                                        Some(compositor::window_manager::FocusableKind::Button { action_id }) => {
                                            activate_info = Some((action_id, win.owner_task, win.id));
                                        }
                                        Some(compositor::window_manager::FocusableKind::TextInput { action_id }) => {
                                            // Grab text from the widget for IPC submit
                                            if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                                if let compositor::window_manager::UiWidget::TextInput { value, value_len, .. } = w {
                                                    let mut buf = [0u8; 64];
                                                    let len = *value_len;
                                                    buf[..len].copy_from_slice(&value[..len]);
                                                    text_submit_info = Some((action_id, win.owner_task, win.id, buf, len));
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                key_consumed = true;
                            }
                            b' ' => {
                                match focused_kind {
                                    Some(compositor::window_manager::FocusableKind::TextInput { .. }) => {
                                        // Type space into TextInput
                                        if let Some(idx) = win.focused_widget {
                                            if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                                if let compositor::window_manager::UiWidget::TextInput { value, value_len, cursor_pos, max_len, .. } = w {
                                                    if *value_len < (*max_len as usize) {
                                                        // Shift right and insert
                                                        let mut i = *value_len;
                                                        while i > *cursor_pos { value[i] = value[i - 1]; i -= 1; }
                                                        value[*cursor_pos] = b' ';
                                                        *value_len += 1;
                                                        *cursor_pos += 1;
                                                        need_redraw = true;
                                                    }
                                                }
                                            }
                                        }
                                        key_consumed = true;
                                    }
                                    Some(compositor::window_manager::FocusableKind::Button { action_id }) => {
                                        activate_info = Some((action_id, win.owner_task, win.id));
                                        key_consumed = true;
                                    }
                                    _ => { key_consumed = true; }
                                }
                            }
                            0x08 | 0x7F => {
                                // Backspace -- only for TextInput
                                if matches!(focused_kind, Some(compositor::window_manager::FocusableKind::TextInput { .. })) {
                                    if let Some(idx) = win.focused_widget {
                                        if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                            if let compositor::window_manager::UiWidget::TextInput { value, value_len, cursor_pos, .. } = w {
                                                if *cursor_pos > 0 {
                                                    let mut i = *cursor_pos - 1;
                                                    while i < *value_len - 1 { value[i] = value[i + 1]; i += 1; }
                                                    *value_len -= 1;
                                                    value[*value_len] = 0;
                                                    *cursor_pos -= 1;
                                                    need_redraw = true;
                                                }
                                            }
                                        }
                                    }
                                    key_consumed = true;
                                }
                            }
                            KEY_ARROW_LEFT => {
                                if matches!(focused_kind, Some(compositor::window_manager::FocusableKind::TextInput { .. })) {
                                    if let Some(idx) = win.focused_widget {
                                        if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                            if let compositor::window_manager::UiWidget::TextInput { cursor_pos, .. } = w {
                                                if *cursor_pos > 0 { *cursor_pos -= 1; need_redraw = true; }
                                            }
                                        }
                                    }
                                    key_consumed = true;
                                }
                            }
                            KEY_ARROW_RIGHT => {
                                if matches!(focused_kind, Some(compositor::window_manager::FocusableKind::TextInput { .. })) {
                                    if let Some(idx) = win.focused_widget {
                                        if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                            if let compositor::window_manager::UiWidget::TextInput { value_len, cursor_pos, .. } = w {
                                                if *cursor_pos < *value_len { *cursor_pos += 1; need_redraw = true; }
                                            }
                                        }
                                    }
                                    key_consumed = true;
                                }
                            }
                            0x21..=0x7E => {
                                // Printable chars -- type into TextInput if focused
                                if matches!(focused_kind, Some(compositor::window_manager::FocusableKind::TextInput { .. })) {
                                    if let Some(idx) = win.focused_widget {
                                        if let Some(w) = compositor::window_manager::nth_focusable_mut(&mut win.widgets, idx) {
                                            if let compositor::window_manager::UiWidget::TextInput { value, value_len, cursor_pos, max_len, .. } = w {
                                                if *value_len < (*max_len as usize) {
                                                    let mut i = *value_len;
                                                    while i > *cursor_pos { value[i] = value[i - 1]; i -= 1; }
                                                    value[*cursor_pos] = key;
                                                    *value_len += 1;
                                                    *cursor_pos += 1;
                                                    need_redraw = true;
                                                }
                                            }
                                        }
                                    }
                                    key_consumed = true;
                                }
                            }
                            0x1B => {
                                input.omnibar_visible = true;
                                need_redraw = true;
                                key_consumed = true;
                            }
                            _ => {}
                        }
                    }
                    // Send button activation IPC outside of borrow
                    if let Some((action_id, owner, win_id)) = activate_info {
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
                                update_window_widgets(wm, win_id, ui_handle);
                                clamp_focus(wm, win_id);
                            }
                            need_redraw = true;
                        }
                    }
                    // Send text submit IPC (0xAC11) outside of borrow
                    if let Some((action_id, owner, win_id, text_buf, submit_text_len)) = text_submit_info {
                        if owner != 0 && submit_text_len > 0 {
                            if let Ok(handle) = shmem_create(submit_text_len + 2) {
                                let _ = shmem_grant(handle, owner);
                                if shmem_map(handle, layout.compositor_shmem_vaddr).is_ok() {
                                    let dst = unsafe {
                                        core::slice::from_raw_parts_mut(layout.compositor_shmem_vaddr as *mut u8, submit_text_len + 2)
                                    };
                                    dst[0..2].copy_from_slice(&(submit_text_len as u16).to_le_bytes());
                                    dst[2..2+submit_text_len].copy_from_slice(&text_buf[..submit_text_len]);
                                    let _ = shmem_unmap(handle, layout.compositor_shmem_vaddr);
                                }
                                let payload = 0xAC11_u64
                                    | ((action_id as u64) << 16)
                                    | ((handle as u64) << 32)
                                    | ((win_id as u64) << 48);
                                let reply = unsafe {
                                    libfolk::syscall::syscall3(
                                        libfolk::syscall::SYS_IPC_SEND,
                                        owner as u64,
                                        payload,
                                        0
                                    )
                                };
                                let _ = shmem_destroy(handle);
                                let reply_magic = (reply >> 48) as u16;
                                if reply_magic == 0x5549 {
                                    let ui_handle = (reply & 0xFFFFFFFF) as u32;
                                    update_window_widgets(wm, win_id, ui_handle);
                                    clamp_focus(wm, win_id);
                                }
                                need_redraw = true;
                            }
                        }
                    }
                }
            }
            if key_consumed {
                continue;
            }
            // No interactive/app window focused -- Escape reopens omnibar
            if key == 0x1B {
                input.omnibar_visible = true;
                need_redraw = true;
                continue;
            }
        }

        match key {
            // Backspace - delete character before cursor
            0x08 | 0x7F => {
                if input.cursor_pos > 0 {
                    // Shift characters left to fill gap
                    let mut i = input.cursor_pos - 1;
                    while i < input.text_len - 1 {
                        input.text_buffer[i] = input.text_buffer[i + 1];
                        i += 1;
                    }
                    input.text_len -= 1;
                    input.text_buffer[input.text_len] = 0;
                    input.cursor_pos -= 1;
                    need_redraw = true;
                    input.show_results = false;
                }
            }
            // Delete key - delete character at cursor
            KEY_DELETE => {
                if input.cursor_pos < input.text_len {
                    let mut i = input.cursor_pos;
                    while i < input.text_len - 1 {
                        input.text_buffer[i] = input.text_buffer[i + 1];
                        i += 1;
                    }
                    input.text_len -= 1;
                    input.text_buffer[input.text_len] = 0;
                    need_redraw = true;
                    input.show_results = false;
                }
            }
            // Arrow keys - move cursor
            KEY_ARROW_LEFT => {
                if input.cursor_pos > 0 {
                    input.cursor_pos -= 1;
                    need_redraw = true;
                }
            }
            KEY_ARROW_RIGHT => {
                if input.cursor_pos < input.text_len {
                    input.cursor_pos += 1;
                    need_redraw = true;
                }
            }
            KEY_HOME => {
                if input.cursor_pos != 0 {
                    input.cursor_pos = 0;
                    need_redraw = true;
                }
            }
            KEY_END => {
                if input.cursor_pos != input.text_len {
                    input.cursor_pos = input.text_len;
                    need_redraw = true;
                }
            }
            // Enter - execute command/search
            b'\n' | b'\r' => {
                if input.text_len > 0 {
                    execute_command = true;
                    input.show_results = true;
                    need_redraw = true;
                }
            }
            // Escape - toggle omnibar visibility / clear buffer
            0x1B => {
                if input.show_results {
                    input.show_results = false;
                    need_redraw = true;
                } else if input.text_len > 0 {
                    input.text_len = 0;
                    input.cursor_pos = 0;
                    for i in 0..layout.max_text_len {
                        input.text_buffer[i] = 0;
                    }
                    need_redraw = true;
                } else {
                    input.omnibar_visible = !input.omnibar_visible;
                    need_redraw = true;
                }
            }
            // Printable ASCII - insert at cursor position
            0x20..=0x7E => {
                if input.text_len < layout.max_text_len - 1 {
                    // Shift characters right to make room
                    let mut i = input.text_len;
                    while i > input.cursor_pos {
                        input.text_buffer[i] = input.text_buffer[i - 1];
                        i -= 1;
                    }
                    input.text_buffer[input.cursor_pos] = key;
                    input.text_len += 1;
                    input.cursor_pos += 1;
                    need_redraw = true;
                    input.show_results = false;
                }
            }
            // Ignore other keys (arrow up/down, windows key, etc.)
            _ => {}
        }
    }

    KeyboardResult {
        did_work,
        need_redraw,
        execute_command,
        win_execute_command,
    }
}
