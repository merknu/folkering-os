//! Post-dispatch stages: deferred AI intent execution + deferred app
//! window creation from a shmem handle.

extern crate alloc;

use libfolk::sys::io::write_str;
use libfolk::sys::shell::SHELL_TASK_ID;
use libfolk::sys::{shmem_destroy, shmem_map, shmem_unmap};

use crate::ipc_helpers::*;

use super::{DispatchContext, COMPOSITOR_SHMEM_VADDR};

/// AI-generated window action that must run AFTER the per-window borrow drops.
/// Encoded as `(action_code, window_id, arg1, arg2)`:
///   1 = MoveWindow{x, y}, 2 = CloseWindow, 3 = ResizeWindow{w, h}
pub(super) type DeferredIntentAction = (u32, u32, u32, u32);

/// Execute a deferred intent action (MoveWindow / CloseWindow / ResizeWindow)
/// emitted by the gemini handler. Called by `legacy_dispatch` after the
/// window borrow has been dropped.
pub(super) fn execute_deferred_intent(action: DeferredIntentAction, ctx: &mut DispatchContext) {
    let (kind, wid, a1, a2) = action;
    match kind {
        1 => {
            // MoveWindow
            if let Some(w) = ctx.wm.get_window_mut(wid) {
                w.x = a1 as i32;
                w.y = a2 as i32;
            }
            ctx.damage.damage_full();
        }
        2 => {
            // CloseWindow
            ctx.wm.close_window(wid);
            ctx.damage.damage_full();
        }
        3 => {
            // ResizeWindow
            if let Some(w) = ctx.wm.get_window_mut(wid) {
                w.width = a1;
                w.height = a2;
            }
            ctx.damage.damage_full();
        }
        _ => {}
    }
}

/// Stage 5 — Create a deferred app window from a shmem handle.
///
/// Called by `dispatch_omnibar` after legacy_dispatch returns. Reads the
/// `.fkui` header from `handle`, parses widget tree, creates the app window.
/// Returns `true` if a window was created (caller should set `need_redraw`).
pub(super) fn create_deferred_app_window(handle: u32, ctx: &mut DispatchContext) -> bool {
    let mut redrew = false;

    if shmem_map(handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
        let buf = unsafe {
            core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, 4096)
        };
        if let Some(header) = libfolk::ui::parse_header(buf) {
            let wc = ctx.wm.windows.len() as i32;
            let app_id = ctx.wm.create_terminal(
                header.title,
                120 + wc * 30, 100 + wc * 30,
                header.width as u32, header.height as u32,
            );
            if let Some(app_win) = ctx.wm.get_window_mut(app_id) {
                app_win.kind = compositor::window_manager::WindowKind::App;
                app_win.owner_task = SHELL_TASK_ID;
                let (root, _) = parse_widget_tree(header.widget_data);
                if let Some(widget) = root {
                    app_win.widgets.push(widget);
                }
            }
            write_str("[WM] Created app: ");
            write_str(header.title);
            write_str("\n");
            redrew = true;
        }
        let _ = shmem_unmap(handle, COMPOSITOR_SHMEM_VADDR);
    }
    let _ = shmem_destroy(handle);

    redrew
}
