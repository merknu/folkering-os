//! Pre-dispatch stages: COM3 god-mode inject, FolkShell pipe pre-processor,
//! and semantic intent matching. These all run BEFORE the legacy command
//! dispatch and may early-out it via `folkshell_handled` / a deferred handle.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use compositor::state::InputState;
use libfolk::sys::io::write_str;
use libfolk::sys::{shmem_destroy, shmem_map, shmem_unmap};

use super::DispatchContext;

/// Stage 1 — COM3 god-mode inject.
///
/// Dequeues ONE command per frame from `com3_queue` (prevents batch-drop
/// where only the last command survived) and copies it into the omnibar
/// text buffer. Returns `true` if something was injected (= caller should
/// trigger a redraw and treat this as `execute_command`).
pub(super) fn inject_god_mode_command(
    input: &mut InputState,
    com3_queue: &mut Vec<String>,
) -> bool {
    if com3_queue.is_empty() {
        return false;
    }
    let injected = com3_queue.remove(0);
    let bytes = injected.as_bytes();
    let copy_len = bytes.len().min(input.text_buffer.len());
    input.text_buffer[..copy_len].copy_from_slice(&bytes[..copy_len]);
    input.text_len = copy_len;
    true
}

/// Stage 2 — FolkShell pre-processor.
///
/// Handles pipe syntax (deterministic `|>` or fuzzy `~>`). Falls through to
/// the legacy dispatcher for builtins and unrecognized input. Returns
/// `true` if FolkShell handled the command (caller should skip legacy).
pub(super) fn handle_folkshell(
    cmd_str: &str,
    ctx: &mut DispatchContext,
    need_redraw: &mut bool,
) -> bool {
    if !cmd_str.contains("|>") && !cmd_str.contains("~>") {
        return false;
    }

    let result = compositor::folkshell::eval(cmd_str, &ctx.wasm.cache);
    let mut handled = false;

    match result {
        compositor::folkshell::ShellState::Done(ref output) => {
            // Create a window for the output
            let win_count = ctx.wm.windows.len() as i32;
            let wx = 80 + win_count * 24;
            let wy = 60 + win_count * 24;
            let win_id = ctx.wm.create_terminal(cmd_str, wx, wy, 480, 200);
            if let Some(win) = ctx.wm.get_window_mut(win_id) {
                for line in output.lines() {
                    win.push_line(line);
                }
            }
            handled = true;
            *need_redraw = true;
        }
        compositor::folkshell::ShellState::WaitingForJIT {
            command_name, pipeline, stage, pipe_input,
        } => {
            let win_count = ctx.wm.windows.len() as i32;
            let wx = 80 + win_count * 24;
            let wy = 60 + win_count * 24;
            let win_id = ctx.wm.create_terminal(cmd_str, wx, wy, 480, 200);
            if let Some(win) = ctx.wm.get_window_mut(win_id) {
                win.push_line(&alloc::format!(
                    "[FolkShell] Synthesizing '{}'...", command_name
                ));
            }
            let prompt = compositor::folkshell::jit_prompt(&command_name, &pipe_input);
            if libfolk::mcp::client::send_wasm_gen(&prompt) {
                ctx.mcp.pending_shell_jit = Some(command_name);
                ctx.mcp.shell_jit_pipeline = Some((pipeline, stage, pipe_input));
                write_str("[FolkShell] JIT request sent\n");
            }
            handled = true;
            *need_redraw = true;
        }
        compositor::folkshell::ShellState::Widget { wasm_bytes, title } => {
            // Holographic widget — launch as fullscreen WASM
            write_str("[FolkShell] Holographic widget: ");
            write_str(&title[..title.len().min(30)]);
            write_str("\n");
            let config = compositor::wasm_runtime::WasmConfig {
                screen_width: ctx.fb.width as u32,
                screen_height: ctx.fb.height as u32,
                uptime_ms: libfolk::sys::uptime() as u32,
            };
            match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                Ok(app) => {
                    ctx.wasm.active_app = Some(app);
                    ctx.wasm.active_app_key = Some(title.clone());
                    ctx.wasm.app_open_since_ms = libfolk::sys::uptime();
                    ctx.wasm.fuel_fail_count = 0;
                    write_str("[FolkShell] Widget launched fullscreen!\n");
                }
                Err(e) => {
                    let win_count = ctx.wm.windows.len() as i32;
                    let wx = 80 + win_count * 24;
                    let wy = 60 + win_count * 24;
                    let win_id = ctx.wm.create_terminal(cmd_str, wx, wy, 480, 200);
                    if let Some(win) = ctx.wm.get_window_mut(win_id) {
                        win.push_line(&alloc::format!("[Widget] Load error: {}", &e[..e.len().min(60)]));
                    }
                }
            }
            handled = true;
            *need_redraw = true;
            ctx.damage.damage_full();
        }
        compositor::folkshell::ShellState::Streaming(sp) => {
            // Tick-Tock semantic streams
            write_str("[FolkShell] Streaming pipeline: ");
            write_str(&sp.upstream_title[..sp.upstream_title.len().min(20)]);
            write_str(" → ");
            write_str(&sp.downstream_title[..sp.downstream_title.len().min(20)]);
            write_str("\n");
            let config = compositor::wasm_runtime::WasmConfig {
                screen_width: ctx.fb.width as u32,
                screen_height: ctx.fb.height as u32,
                uptime_ms: libfolk::sys::uptime() as u32,
            };
            match (
                compositor::wasm_runtime::PersistentWasmApp::new(&sp.upstream_wasm, config.clone()),
                compositor::wasm_runtime::PersistentWasmApp::new(&sp.downstream_wasm, config),
            ) {
                (Ok(up), Ok(down)) => {
                    ctx.wasm.streaming_upstream = Some(up);
                    ctx.wasm.streaming_downstream = Some(down);
                    ctx.wasm.active_app = None;
                    write_str("[FolkShell] Tick-Tock streaming started!\n");
                }
                _ => {
                    write_str("[FolkShell] Failed to instantiate streaming apps\n");
                }
            }
            handled = true;
            *need_redraw = true;
            ctx.damage.damage_full();
        }
        _ => {} // Passthrough or error → legacy dispatch
    }

    handled
}

/// Stage 3 — Semantic intent match.
///
/// Tries to map the command string to a known app via semantic matching.
/// On match, looks up `<app_name>.fkui` in Synapse VFS and returns the
/// shmem handle for deferred window creation.
///
/// Called from `legacy_dispatch` (gated by `is_open/run/gemini` checks).
pub(super) fn try_semantic_intent_match(cmd_str: &str) -> u32 {
    if let Some(app_name) = crate::util::try_intent_match(cmd_str) {
        let mut fname = [0u8; 64];
        let nb = app_name.as_bytes();
        let ext = b".fkui";
        if nb.len() + ext.len() < 64 {
            fname[..nb.len()].copy_from_slice(nb);
            fname[nb.len()..nb.len() + ext.len()].copy_from_slice(ext);
            let fname_str = unsafe {
                core::str::from_utf8_unchecked(&fname[..nb.len() + ext.len()])
            };
            if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(fname_str) {
                write_str("[WM] Intent match: ");
                write_str(app_name);
                write_str("\n");
                return resp.shmem_handle;
            }
        }
    }
    0
}
