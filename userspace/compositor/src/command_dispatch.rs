//! Command Dispatch — Omnibar + Terminal command execution
//!
//! Extracted from main.rs lines ~1906-4045. Contains:
//! 1. COM3 God Mode inject (dequeue command from pipe)
//! 2. FolkShell pre-processor (pipes |> and ~>)
//! 3. Legacy omnibar dispatch (open, run, gemini, agent, etc.)
//! 4. Deferred app creation from omnibar
//! 5. Interactive terminal command execution
//! 6. Deferred UI window creation from Shell IPC

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

use compositor::agent::AgentSession;
use compositor::damage::DamageTracker;
use compositor::draug::DraugDaemon;
use compositor::framebuffer::FramebufferView;
use compositor::state::{InputState, WasmState, McpState, StreamState, CursorState};
use compositor::window_manager::WindowManager;

use crate::util::*;
use crate::ui_dump::*;
use crate::ipc_helpers::*;

use libfolk::sys::ipc::{receive, reply, recv_async, reply_with_token, send, IpcError};
use libfolk::sys::{yield_cpu, read_mouse, read_key, uptime, shmem_create, shmem_map, shmem_unmap, shmem_destroy, shmem_grant};
use libfolk::sys::io::{write_char, write_str};
use libfolk::sys::shell::{
    SHELL_TASK_ID, SHELL_OP_LIST_FILES, SHELL_OP_CAT_FILE, SHELL_OP_SEARCH,
    SHELL_OP_PS, SHELL_OP_UPTIME, SHELL_OP_EXEC, SHELL_OP_OPEN_APP,
    SHELL_OP_INJECT_STATE,
    SHELL_STATUS_NOT_FOUND, hash_name as shell_hash_name,
};

/// Virtual address for mapping shared memory received from shell
const COMPOSITOR_SHMEM_VADDR: usize = 0x30000000;

/// Virtual address for mapping TokenRing shmem
const RING_VADDR: usize = 0x32000000;

/// Virtual address for query shmem to inference
const ASK_QUERY_VADDR: usize = 0x30000000;

const THINK_BUF_SIZE: usize = 1024;

/// Result from omnibar dispatch
pub struct DispatchResult {
    pub need_redraw: bool,
    pub did_work: bool,
    pub deferred_app_handle: u32,
}

/// Dispatch omnibar commands: COM3 inject, FolkShell, legacy commands, deferred app creation.
///
/// This is the main entry point called each frame when a command is ready.
/// Returns a DispatchResult indicating what happened.
pub fn dispatch_omnibar(
    input: &mut InputState,
    wasm: &mut WasmState,
    wm: &mut WindowManager,
    mcp: &mut McpState,
    stream: &mut StreamState,
    draug: &mut DraugDaemon,
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    com3_queue: &mut Vec<String>,
    active_agent: &mut Option<AgentSession>,
    drivers_seeded: &mut bool,
    execute_command: bool,
    cursor: &mut CursorState,
) -> DispatchResult {
    let mut need_redraw = false;
    let mut did_work = false;
    let mut deferred_app_handle: u32 = 0;

    // COM3 God Mode: inject command directly (bypasses keyboard)
    // Dequeue ONE command per frame (prevents batch-drop where only last command survived)
    if let Some(injected) = if !com3_queue.is_empty() { Some(com3_queue.remove(0)) } else { None } {
        let bytes = injected.as_bytes();
        let copy_len = bytes.len().min(input.text_buffer.len());
        input.text_buffer[..copy_len].copy_from_slice(&bytes[..copy_len]);
        input.text_len = copy_len;
        // We set execute_command via a local shadow — caller must also set it
        need_redraw = true;
    }

    let should_execute = execute_command || (!com3_queue.is_empty() && input.text_len > 0);
    // Re-check: if COM3 injected above, execute_command was not set in the caller yet.
    // The caller passes execute_command from keyboard result. COM3 injection sets text_len>0
    // and the original code also sets execute_command = true. We replicate that:
    let execute_command = if !com3_queue.is_empty() || execute_command {
        // COM3 inject already happened above — but we need to handle
        // the case where com3 injected in THIS call
        execute_command || (input.text_len > 0 && need_redraw)
    } else {
        execute_command
    };

    if execute_command && input.text_len > 0 {
        if let Ok(cmd_str) = core::str::from_utf8(&input.text_buffer[..input.text_len]) {

            // ═══════ FolkShell Pre-Processor ═══════
            // Try FolkShell first — handles pipes (|>) and JIT command synthesis.
            // Falls through to legacy dispatch for builtins and unrecognized input.
            let mut folkshell_handled = false;
            if cmd_str.contains("|>") || cmd_str.contains("~>") {
                // Pipe syntax (deterministic |> or fuzzy ~>) → FolkShell handles this
                let result = compositor::folkshell::eval(cmd_str, &wasm.cache);
                match result {
                    compositor::folkshell::ShellState::Done(ref output) => {
                        // Create a window for the output
                        let win_count = wm.windows.len() as i32;
                        let wx = 80 + win_count * 24;
                        let wy = 60 + win_count * 24;
                        let win_id = wm.create_terminal(cmd_str, wx, wy, 480, 200);
                        if let Some(win) = wm.get_window_mut(win_id) {
                            for line in output.lines() {
                                win.push_line(line);
                            }
                        }
                        folkshell_handled = true;
                        need_redraw = true;
                    }
                    compositor::folkshell::ShellState::WaitingForJIT {
                        command_name, pipeline, stage, pipe_input
                    } => {
                        let win_count = wm.windows.len() as i32;
                        let wx = 80 + win_count * 24;
                        let wy = 60 + win_count * 24;
                        let win_id = wm.create_terminal(cmd_str, wx, wy, 480, 200);
                        if let Some(win) = wm.get_window_mut(win_id) {
                            win.push_line(&alloc::format!(
                                "[FolkShell] Synthesizing '{}'...", command_name
                            ));
                        }
                        let prompt = compositor::folkshell::jit_prompt(&command_name, &pipe_input);
                        if libfolk::mcp::client::send_wasm_gen(&prompt) {
                            mcp.pending_shell_jit = Some(command_name);
                            mcp.shell_jit_pipeline = Some((pipeline, stage, pipe_input));
                            write_str("[FolkShell] JIT request sent\n");
                        }
                        folkshell_handled = true;
                        need_redraw = true;
                    }
                    compositor::folkshell::ShellState::Widget { wasm_bytes, title } => {
                        // ═══════ Holographic Output ═══════
                        // Launch the WASM as a live interactive widget in a floating window
                        write_str("[FolkShell] Holographic widget: ");
                        write_str(&title[..title.len().min(30)]);
                        write_str("\n");
                        let config = compositor::wasm_runtime::WasmConfig {
                            screen_width: fb.width as u32,
                            screen_height: fb.height as u32,
                            uptime_ms: libfolk::sys::uptime() as u32,
                        };
                        match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                            Ok(app) => {
                                wasm.active_app = Some(app);
                                wasm.active_app_key = Some(title.clone());
                                wasm.app_open_since_ms = libfolk::sys::uptime();
                                wasm.fuel_fail_count = 0;
                                write_str("[FolkShell] Widget launched fullscreen!\n");
                            }
                            Err(e) => {
                                // Fallback: show as text in terminal window
                                let win_count = wm.windows.len() as i32;
                                let wx = 80 + win_count * 24;
                                let wy = 60 + win_count * 24;
                                let win_id = wm.create_terminal(cmd_str, wx, wy, 480, 200);
                                if let Some(win) = wm.get_window_mut(win_id) {
                                    win.push_line(&alloc::format!("[Widget] Load error: {}", &e[..e.len().min(60)]));
                                }
                            }
                        }
                        folkshell_handled = true;
                        need_redraw = true;
                        damage.damage_full();
                    }
                    compositor::folkshell::ShellState::Streaming(sp) => {
                        // ═══════ Semantic Streams: Tick-Tock ═══════
                        write_str("[FolkShell] Streaming pipeline: ");
                        write_str(&sp.upstream_title[..sp.upstream_title.len().min(20)]);
                        write_str(" → ");
                        write_str(&sp.downstream_title[..sp.downstream_title.len().min(20)]);
                        write_str("\n");
                        let config = compositor::wasm_runtime::WasmConfig {
                            screen_width: fb.width as u32,
                            screen_height: fb.height as u32,
                            uptime_ms: libfolk::sys::uptime() as u32,
                        };
                        match (
                            compositor::wasm_runtime::PersistentWasmApp::new(&sp.upstream_wasm, config.clone()),
                            compositor::wasm_runtime::PersistentWasmApp::new(&sp.downstream_wasm, config),
                        ) {
                            (Ok(up), Ok(down)) => {
                                wasm.streaming_upstream = Some(up);
                                wasm.streaming_downstream = Some(down);
                                // Hide regular WASM app
                                wasm.active_app = None;
                                write_str("[FolkShell] Tick-Tock streaming started!\n");
                            }
                            _ => {
                                write_str("[FolkShell] Failed to instantiate streaming apps\n");
                            }
                        }
                        folkshell_handled = true;
                        need_redraw = true;
                        damage.damage_full();
                    }
                    _ => {} // Passthrough or error → legacy dispatch
                }
            }

            if !folkshell_handled {
            // ═══════ Legacy Omnibar Dispatch ═══════

            // Special case: `open <app>` — try WASM fullscreen first, then FKUI window
            let is_open_cmd = cmd_str.starts_with("open ");
            if is_open_cmd {
                let app_name = cmd_str[5..].trim();
                if !app_name.is_empty() {
                    let mut opened_wasm = false;

                    // Try WASM fullscreen first (preferred — no window overlap)
                    {
                        let mut wasm_fname = [0u8; 64];
                        let nb = app_name.as_bytes();
                        let ext = b".wasm";
                        if nb.len() + ext.len() < 64 {
                            wasm_fname[..nb.len()].copy_from_slice(nb);
                            wasm_fname[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                            let wasm_str = unsafe { core::str::from_utf8_unchecked(&wasm_fname[..nb.len()+ext.len()]) };
                            const VFS_OPEN_VADDR: usize = 0x50040000;
                            write_str("[OPEN] Trying VFS: ");
                            write_str(wasm_str);
                            write_str("\n");
                            match libfolk::sys::synapse::read_file_shmem(wasm_str) {
                            Err(e) => {
                                write_str("[OPEN] VFS read failed: ");
                                match e {
                                    libfolk::sys::synapse::SynapseError::NotFound => write_str("NotFound"),
                                    libfolk::sys::synapse::SynapseError::ServiceUnavailable => write_str("ServiceUnavailable"),
                                    libfolk::sys::synapse::SynapseError::InvalidRequest => write_str("InvalidRequest"),
                                    libfolk::sys::synapse::SynapseError::IpcFailed => write_str("IpcFailed"),
                                    _ => write_str("Unknown"),
                                }
                                write_str("\n");
                            }
                            Ok(resp) => {
                                if shmem_map(resp.shmem_handle, VFS_OPEN_VADDR).is_ok() {
                                    let data = unsafe {
                                        core::slice::from_raw_parts(VFS_OPEN_VADDR as *const u8, resp.size as usize)
                                    };
                                    let wasm_bytes = alloc::vec::Vec::from(data);
                                    let _ = shmem_unmap(resp.shmem_handle, VFS_OPEN_VADDR);
                                    let _ = shmem_destroy(resp.shmem_handle);
                                    let config = compositor::wasm_runtime::WasmConfig {
                                        screen_width: fb.width as u32,
                                        screen_height: fb.height as u32,
                                        uptime_ms: libfolk::sys::uptime() as u32,
                                    };
                                    match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                    Err(e) => {
                                        write_str("[WM] WASM compile failed: ");
                                        // Print first 80 chars of error
                                        let err_bytes = e.as_bytes();
                                        let show = err_bytes.len().min(80);
                                        for &b in &err_bytes[..show] { write_char(b); }
                                        write_str("\n");
                                    }
                                    Ok(app) => {
                                        wasm.active_app = Some(app);
                                        wasm.active_app_key = Some(alloc::string::String::from(app_name));
                                        wasm.app_open_since_ms = libfolk::sys::uptime();
                                        wasm.fuel_fail_count = 0;
                                        wasm.last_bytes = Some(wasm_bytes);
                                        wasm.last_interactive = true;
                                        opened_wasm = true;
                                        write_str("[WM] Opened WASM fullscreen: ");
                                        write_str(wasm_str);
                                        write_str("\n");
                                        // IQE: window open event
                                        libfolk::sys::com3_write(b"IQE,WIN_OPEN,0\n");
                                        // Telemetry: AppOpened (syscall 0x9B)
                                        unsafe { libfolk::syscall::syscall3(0x9B, 0, 0, 0); }
                                    } // Ok(app)
                                    } // match PersistentWasmApp::new
                                } else {
                                    let _ = shmem_destroy(resp.shmem_handle);
                                }
                            } // Ok(resp)
                            } // match read_file_shmem
                        }
                    }

                    // Fallback: FKUI windowed app
                    if !opened_wasm {
                        let mut fname = [0u8; 64];
                        let nb = app_name.as_bytes();
                        let ext = b".fkui";
                        let mut vfs_loaded = false;
                        if nb.len() + ext.len() < 64 {
                            fname[..nb.len()].copy_from_slice(nb);
                            fname[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                            let fname_str = unsafe { core::str::from_utf8_unchecked(&fname[..nb.len()+ext.len()]) };
                            match libfolk::sys::synapse::read_file_shmem(fname_str) {
                                Ok(resp) => {
                                    deferred_app_handle = resp.shmem_handle;
                                    vfs_loaded = true;
                                    write_str("[WM] App loaded from VFS: ");
                                    write_str(fname_str);
                                    write_str("\n");
                                }
                                Err(_) => {}
                            }
                        }
                        if !vfs_loaded {
                            let name_hash = shell_hash_name(app_name) as u64;
                            let shell_payload = SHELL_OP_OPEN_APP | (name_hash << 8);
                            let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                                | (shell_payload << 8);
                            let ipc_result = unsafe {
                                libfolk::syscall::syscall3(
                                    libfolk::syscall::SYS_IPC_SEND,
                                    libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                                )
                            };
                            let magic = (ipc_result >> 48) as u16;
                            if magic == 0x5549 {
                                deferred_app_handle = (ipc_result & 0xFFFFFFFF) as u32;
                                write_str("[WM] App launch via Shell fallback\n");
                            } else {
                                write_str("[WM] Unknown app\n");
                            }
                        }
                    }
                }
            }

            // Omnibar `run <app>` — load WASM app in fullscreen
            let is_run_cmd = cmd_str.starts_with("run ");
            if is_run_cmd {
                let app_name = cmd_str[4..].trim();
                if !app_name.is_empty() {
                    let filename = if app_name.as_bytes().windows(5).any(|w| w == b".wasm") {
                        let mut f = [0u8; 64];
                        let n = app_name.len().min(63);
                        f[..n].copy_from_slice(&app_name.as_bytes()[..n]);
                        (f, n)
                    } else {
                        let mut f = [0u8; 64];
                        let nb = app_name.as_bytes();
                        let ext = b".wasm";
                        if nb.len() + ext.len() < 64 {
                            f[..nb.len()].copy_from_slice(nb);
                            f[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                            (f, nb.len() + ext.len())
                        } else {
                            (f, 0)
                        }
                    };
                    if filename.1 > 0 {
                        let fname_str = unsafe { core::str::from_utf8_unchecked(&filename.0[..filename.1]) };
                        const VFS_RUN_VADDR: usize = 0x50040000;
                        if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(fname_str) {
                            if shmem_map(resp.shmem_handle, VFS_RUN_VADDR).is_ok() {
                                let data = unsafe {
                                    core::slice::from_raw_parts(VFS_RUN_VADDR as *const u8, resp.size as usize)
                                };
                                let wasm_bytes = alloc::vec::Vec::from(data);
                                let _ = shmem_unmap(resp.shmem_handle, VFS_RUN_VADDR);
                                let _ = shmem_destroy(resp.shmem_handle);

                                let config = compositor::wasm_runtime::WasmConfig {
                                    screen_width: fb.width as u32,
                                    screen_height: fb.height as u32,
                                    uptime_ms: libfolk::sys::uptime() as u32,
                                };
                                match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                    Ok(app) => {
                                        wasm.active_app = Some(app);
                                        wasm.active_app_key = Some(alloc::string::String::from(app_name));
                                        wasm.app_open_since_ms = libfolk::sys::uptime();
                                        wasm.fuel_fail_count = 0;
                                        wasm.last_bytes = Some(wasm_bytes);
                                        wasm.last_interactive = true;
                                        write_str("[WASM] Launched fullscreen: ");
                                        write_str(fname_str);
                                        write_str("\n");
                                    }
                                    Err(_) => {
                                        write_str("[WASM] Failed to instantiate: ");
                                        write_str(fname_str);
                                        write_str("\n");
                                    }
                                }
                            } else {
                                let _ = shmem_destroy(resp.shmem_handle);
                            }
                        } else {
                            write_str("[WASM] App not found: ");
                            write_str(fname_str);
                            write_str("\n");
                        }
                    }
                }
            }

            let is_gemini_cmd = starts_with_ci(cmd_str, "gemini ");
            if !is_open_cmd && !is_run_cmd && !is_gemini_cmd {
                // M13: Try semantic intent match BEFORE creating terminal window (skip for gemini commands)
                if let Some(app_name) = try_intent_match(cmd_str) {
                    let mut fname = [0u8; 64];
                    let nb = app_name.as_bytes();
                    let ext = b".fkui";
                    if nb.len() + ext.len() < 64 {
                        fname[..nb.len()].copy_from_slice(nb);
                        fname[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                        let fname_str = unsafe {
                            core::str::from_utf8_unchecked(&fname[..nb.len()+ext.len()])
                        };
                        if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(fname_str) {
                            deferred_app_handle = resp.shmem_handle;
                            write_str("[WM] Intent match: ");
                            write_str(app_name);
                            write_str("\n");
                        }
                    }
                }
            } // end intent match guard

            if deferred_app_handle == 0 && wasm.active_app.is_none() {
            write_str("[WM] Creating window for: ");
            write_str(cmd_str);
            write_str("\n");

            // Spawn a terminal window at a cascade position
            let win_count = wm.windows.len() as i32;
            let wx = 80 + win_count * 24;
            let wy = 60 + win_count * 24;
            let win_id = wm.create_terminal(cmd_str, wx, wy, 480, 200);

            // Pre-compute UI state for gemini command (before win borrow)
            let ui_state_snapshot = {
                let ui_wins: alloc::vec::Vec<compositor::ui_serialize::WindowInfo> =
                    wm.windows.iter().map(|w| {
                        let t = alloc::str::from_utf8(&w.title[..w.title_len]).unwrap_or("?");
                        let ll = w.lines.last().map(|l|
                            alloc::str::from_utf8(&l.buf[..l.len]).unwrap_or("")
                        ).unwrap_or("");
                        compositor::ui_serialize::WindowInfo {
                            id: w.id, z_index: 0,
                            title: alloc::string::String::from(t),
                            x: w.x as u32, y: w.y as u32,
                            w: w.width, h: w.height,
                            visible_text: alloc::string::String::from(ll),
                        }
                    }).collect();
                compositor::ui_serialize::serialize_ui_state(
                    fb.width as u32, fb.height as u32, &ui_wins, "",
                )
            };

            let mut deferred_intent_action: Option<(u32, u32, u32, u32)> = None;
            if let Some(win) = wm.get_window_mut(win_id) {
                // Execute the command and populate the window
                win.push_line("> ");  // we'll append cmd below
                // Title is already the command, show it as first line too
                let mut title_line = [0u8; 130];
                title_line[0] = b'>';
                title_line[1] = b' ';
                let tlen = cmd_str.len().min(126);
                title_line[2..2+tlen].copy_from_slice(&cmd_str.as_bytes()[..tlen]);
                if let Ok(s) = core::str::from_utf8(&title_line[..2+tlen]) {
                    win.push_line(s);
                }

                // Built-in commands — routed through Intent Service (microkernel IPC)
                if cmd_str == "ls" || cmd_str == "files" {
                    win.push_line("Files in ramdisk:");
                    let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                        | (SHELL_OP_LIST_FILES << 8);
                    let ipc_result = unsafe {
                        libfolk::syscall::syscall3(
                            libfolk::syscall::SYS_IPC_SEND,
                            libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                        )
                    };
                    if ipc_result != u64::MAX {
                        let count = (ipc_result >> 32) as usize;
                        let shmem_handle = (ipc_result & 0xFFFFFFFF) as u32;

                        if shmem_handle != 0 && count > 0 {
                            // Map shmem from shell
                            if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                let buf = unsafe {
                                    core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, count * 32)
                                };
                                for i in 0..count {
                                    let offset = i * 32;
                                    // name: [u8; 24]
                                    let name_end = buf[offset..offset+24].iter()
                                        .position(|&b| b == 0).unwrap_or(24);
                                    let name = unsafe {
                                        core::str::from_utf8_unchecked(&buf[offset..offset+name_end])
                                    };
                                    // size: u32 at offset+24
                                    let size = u32::from_le_bytes([
                                        buf[offset+24], buf[offset+25],
                                        buf[offset+26], buf[offset+27]
                                    ]) as usize;
                                    // type: u32 at offset+28 (0=ELF, 1=data)
                                    let kind = u32::from_le_bytes([
                                        buf[offset+28], buf[offset+29],
                                        buf[offset+30], buf[offset+31]
                                    ]);
                                    let kind_str = if kind == 0 { "ELF " } else { "DATA" };

                                    // Format: "  ELF   12345 filename"
                                    let mut line = [0u8; 64];
                                    line[0] = b' '; line[1] = b' ';
                                    line[2..6].copy_from_slice(kind_str.as_bytes());
                                    line[6] = b' ';
                                    let mut size_buf = [0u8; 16];
                                    let size_str = format_usize(size, &mut size_buf);
                                    let slen = size_str.len();
                                    let pad = 8usize.saturating_sub(slen);
                                    for j in 0..pad { line[7 + j] = b' '; }
                                    line[7+pad..7+pad+slen].copy_from_slice(size_str.as_bytes());
                                    line[7+pad+slen] = b' ';
                                    let nlen = name.len().min(40);
                                    line[8+pad+slen..8+pad+slen+nlen].copy_from_slice(&name.as_bytes()[..nlen]);
                                    let total = 8 + pad + slen + nlen;
                                    if let Ok(s) = core::str::from_utf8(&line[..total]) {
                                        win.push_line(s);
                                    }
                                }
                                let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
                                let _ = shmem_destroy(shmem_handle);
                            }
                        }
                        let mut count_buf = [0u8; 16];
                        let count_str = format_usize(count, &mut count_buf);
                        let suffix = b" file(s)";
                        let mut line_buf = [0u8; 32];
                        let clen = count_str.len().min(16);
                        let slen2 = suffix.len();
                        line_buf[..clen].copy_from_slice(&count_str.as_bytes()[..clen]);
                        line_buf[clen..clen+slen2].copy_from_slice(suffix);
                        if let Ok(s) = core::str::from_utf8(&line_buf[..clen+slen2]) {
                            win.push_line(s);
                        }
                    } else {
                        win.push_line("Shell not responding");
                    }
                } else if cmd_str == "ps" || cmd_str == "tasks" {
                    win.push_line("Running tasks:");
                    // Route through Intent Service (4-hop IPC test)
                    let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                        | (SHELL_OP_PS << 8);
                    let ipc_result = unsafe {
                        libfolk::syscall::syscall3(
                            libfolk::syscall::SYS_IPC_SEND,
                            libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                        )
                    };
                    if ipc_result != u64::MAX {
                        let count = (ipc_result >> 32) as usize;
                        let shmem_handle = (ipc_result & 0xFFFFFFFF) as u32;

                        if shmem_handle != 0 && count > 0 {
                            if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                let buf = unsafe {
                                    core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, count * 32)
                                };
                                for i in 0..count {
                                    let offset = i * 32;
                                    let tid = u32::from_le_bytes([
                                        buf[offset], buf[offset+1],
                                        buf[offset+2], buf[offset+3]
                                    ]);
                                    let state = u32::from_le_bytes([
                                        buf[offset+4], buf[offset+5],
                                        buf[offset+6], buf[offset+7]
                                    ]);
                                    let name_end = buf[offset+8..offset+24].iter()
                                        .position(|&b| b == 0).unwrap_or(16);
                                    let name = unsafe {
                                        core::str::from_utf8_unchecked(&buf[offset+8..offset+8+name_end])
                                    };
                                    let state_str = match state {
                                        0 => "Runnable",
                                        1 => "Running",
                                        2 => "Blocked",
                                        3 => "Blocked",
                                        4 => "Waiting",
                                        5 => "Exited",
                                        _ => "Unknown",
                                    };
                                    // Format: "  Task 2: synapse (Blocked)"
                                    let mut line = [0u8; 64];
                                    let mut pos = 0usize;
                                    let prefix = b"  Task ";
                                    line[..prefix.len()].copy_from_slice(prefix);
                                    pos += prefix.len();
                                    let mut tid_buf2 = [0u8; 16];
                                    let tid_str = format_usize(tid as usize, &mut tid_buf2);
                                    let tlen = tid_str.len();
                                    line[pos..pos+tlen].copy_from_slice(tid_str.as_bytes());
                                    pos += tlen;
                                    line[pos] = b':'; pos += 1;
                                    line[pos] = b' '; pos += 1;
                                    let nlen = name.len().min(15);
                                    if nlen > 0 {
                                        line[pos..pos+nlen].copy_from_slice(&name.as_bytes()[..nlen]);
                                        pos += nlen;
                                    } else {
                                        let unk = b"<unnamed>";
                                        line[pos..pos+unk.len()].copy_from_slice(unk);
                                        pos += unk.len();
                                    }
                                    line[pos] = b' '; pos += 1;
                                    line[pos] = b'('; pos += 1;
                                    let slen = state_str.len();
                                    line[pos..pos+slen].copy_from_slice(state_str.as_bytes());
                                    pos += slen;
                                    line[pos] = b')'; pos += 1;
                                    if let Ok(s) = core::str::from_utf8(&line[..pos]) {
                                        win.push_line(s);
                                    }
                                }
                                let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
                                let _ = shmem_destroy(shmem_handle);
                            }
                        } else {
                            // Fallback: count only
                            let mut count_buf = [0u8; 16];
                            let count_str = format_usize(count, &mut count_buf);
                            win.push_line(count_str);
                            win.push_line("task(s) — no details available");
                        }
                    } else {
                        win.push_line("Shell not responding");
                    }
                } else if cmd_str.starts_with("cat ") {
                    // cat <filename> — route through Intent Service → Shell → Synapse
                    let filename = cmd_str[4..].trim();
                    if filename.is_empty() {
                        win.push_line("usage: cat <filename>");
                    } else {
                        // Hash filename and route through Intent Service
                        let name_hash = shell_hash_name(filename) as u64;
                        let shell_payload = SHELL_OP_CAT_FILE | (name_hash << 8);
                        let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                            | (shell_payload << 8);
                        let ipc_result = unsafe {
                            libfolk::syscall::syscall3(
                                libfolk::syscall::SYS_IPC_SEND,
                                libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                            )
                        };
                        if ipc_result != u64::MAX && ipc_result != SHELL_STATUS_NOT_FOUND {
                            let size = (ipc_result >> 32) as usize;
                            let shmem_handle = (ipc_result & 0xFFFFFFFF) as u32;

                            if shmem_handle != 0 && size > 0 {
                                if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                    let buf = unsafe {
                                        core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, size)
                                    };
                                    // Display file contents line by line
                                    let mut line_start = 0;
                                    for pos in 0..size {
                                        if buf[pos] == b'\n' || buf[pos] == 0 {
                                            if pos > line_start {
                                                if let Ok(line) = core::str::from_utf8(&buf[line_start..pos]) {
                                                    win.push_line(line);
                                                }
                                            }
                                            line_start = pos + 1;
                                            if buf[pos] == 0 { break; }
                                        }
                                    }
                                    // Handle last line without newline
                                    if line_start < size {
                                        let end = buf[line_start..size]
                                            .iter().position(|&b| b == 0)
                                            .map(|p| line_start + p)
                                            .unwrap_or(size);
                                        if end > line_start {
                                            if let Ok(line) = core::str::from_utf8(&buf[line_start..end]) {
                                                win.push_line(line);
                                            }
                                        }
                                    }
                                    let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
                                    let _ = shmem_destroy(shmem_handle);
                                }
                            } else {
                                win.push_line("File is empty");
                            }
                        } else {
                            win.push_line("File not found");
                        }
                    }
                } else if cmd_str == "uptime" {
                    let ms = uptime();
                    let mut buf = [0u8; 32];
                    let time_str = format_uptime(ms, &mut buf);
                    win.push_line(time_str);
                } else if cmd_str == "help" {
                    win.push_line("Commands: ls, cat, ps, uptime, mem");
                    win.push_line("lspci, drivers, generate driver [v:d]");
                    win.push_line("drivers versions v:d, drivers rollback v:d vN");
                    win.push_line("find <q>, calc <e>, open <a>");
                    win.push_line("gemini generate <desc>, ls |> cmd, ~>");
                } else if cmd_str.starts_with("find ") || cmd_str.starts_with("search ") {
                    let query = if cmd_str.starts_with("find ") {
                        cmd_str[5..].trim()
                    } else {
                        cmd_str[7..].trim()
                    };
                    if query.is_empty() {
                        win.push_line("usage: find <query>");
                    } else {
                        win.push_line("Searching Synapse...");
                        // Create shmem with query string
                        let query_bytes = query.as_bytes();
                        let query_len = query_bytes.len().min(63);
                        if let Ok(query_handle) = shmem_create(64) {
                            // Grant broadly
                            for tid in 2..=8 {
                                let _ = shmem_grant(query_handle, tid);
                            }
                            if shmem_map(query_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                let buf = unsafe {
                                    core::slice::from_raw_parts_mut(COMPOSITOR_SHMEM_VADDR as *mut u8, 64)
                                };
                                buf[..query_len].copy_from_slice(&query_bytes[..query_len]);
                                buf[query_len] = 0; // null terminate
                                let _ = shmem_unmap(query_handle, COMPOSITOR_SHMEM_VADDR);
                            }

                            // Send to Shell: SHELL_OP_SEARCH | (query_handle << 8) | (query_len << 40)
                            let shell_req = SHELL_OP_SEARCH
                                | ((query_handle as u64) << 8)
                                | ((query_len as u64) << 40);
                            let ipc_result = unsafe {
                                libfolk::syscall::syscall3(
                                    libfolk::syscall::SYS_IPC_SEND,
                                    SHELL_TASK_ID as u64, shell_req, 0
                                )
                            };

                            // Cleanup query shmem
                            let _ = shmem_destroy(query_handle);

                            if ipc_result != u64::MAX && ipc_result != 0 {
                                let count = (ipc_result >> 32) as usize;
                                let results_handle = (ipc_result & 0xFFFFFFFF) as u32;

                                if results_handle != 0 && count > 0 {
                                    win.push_line("");
                                    let mut match_buf = [0u8; 40];
                                    let prefix = b"Matches: ";
                                    match_buf[..prefix.len()].copy_from_slice(prefix);
                                    let mut num_buf = [0u8; 16];
                                    let num_str = format_usize(count, &mut num_buf);
                                    let nlen = num_str.len();
                                    match_buf[prefix.len()..prefix.len()+nlen]
                                        .copy_from_slice(num_str.as_bytes());
                                    if let Ok(s) = core::str::from_utf8(&match_buf[..prefix.len()+nlen]) {
                                        win.push_line(s);
                                    }

                                    // Read results from shmem
                                    if shmem_map(results_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                        let buf = unsafe {
                                            core::slice::from_raw_parts(
                                                COMPOSITOR_SHMEM_VADDR as *const u8, count * 32
                                            )
                                        };
                                        for i in 0..count.min(10) {
                                            let offset = i * 32;
                                            let name_end = buf[offset..offset+24].iter()
                                                .position(|&b| b == 0).unwrap_or(24);
                                            let name = unsafe {
                                                core::str::from_utf8_unchecked(
                                                    &buf[offset..offset+name_end]
                                                )
                                            };
                                            let size = u32::from_le_bytes([
                                                buf[offset+24], buf[offset+25],
                                                buf[offset+26], buf[offset+27]
                                            ]) as usize;
                                            // Format: "  synapse (30774 bytes)"
                                            let mut line = [0u8; 64];
                                            line[0] = b' '; line[1] = b' ';
                                            let nlen2 = name.len().min(30);
                                            line[2..2+nlen2].copy_from_slice(&name.as_bytes()[..nlen2]);
                                            let mut size_buf2 = [0u8; 16];
                                            let size_str = format_usize(size, &mut size_buf2);
                                            let slen = size_str.len();
                                            line[2+nlen2] = b' ';
                                            line[3+nlen2] = b'(';
                                            line[4+nlen2..4+nlen2+slen]
                                                .copy_from_slice(size_str.as_bytes());
                                            let suffix = b" bytes)";
                                            line[4+nlen2+slen..4+nlen2+slen+suffix.len()]
                                                .copy_from_slice(suffix);
                                            let total = 4 + nlen2 + slen + suffix.len();
                                            if let Ok(s) = core::str::from_utf8(&line[..total]) {
                                                win.push_line(s);
                                            }
                                        }
                                        let _ = shmem_unmap(results_handle, COMPOSITOR_SHMEM_VADDR);
                                        let _ = shmem_destroy(results_handle);
                                    }
                                } else {
                                    win.push_line("No matches found");
                                }
                            } else {
                                win.push_line("No matches found");
                            }
                        }
                    }
                } else if cmd_str == "term" || cmd_str == "terminal" {
                    // Open interactive terminal — make this window interactive
                    win.interactive = true;
                    win.push_line("Folkering OS Terminal");
                    win.push_line("Type commands, Enter to run, Esc for omnibar");
                } else if cmd_str.starts_with("calc ") {
                    win.push_line("Calculator: coming soon");
                } else if starts_with_ci(cmd_str, "save app ") {
                    // App Persistence: save last compiled WASM to VFS
                    let app_name = cmd_str[9..].trim();
                    if app_name.is_empty() {
                        win.push_line("Usage: save app <name>");
                    } else if let Some(ref wasm) = wasm.last_bytes {
                        let filename = alloc::format!("{}.wasm", app_name);
                        match libfolk::sys::synapse::write_file(&filename, wasm) {
                            Ok(()) => {
                                win.push_line(&alloc::format!(
                                    "[OS] Saved '{}' ({} bytes)", app_name, wasm.len()
                                ));
                                write_str("[COMPOSITOR] App saved to VFS: ");
                                write_str(&filename);
                                write_str("\n");
                            }
                            Err(_) => {
                                win.push_line("[OS] Save failed — VFS write error");
                            }
                        }
                    } else {
                        win.push_line("[OS] No app to save. Run 'load' or 'gemini generate' first.");
                    }
                } else if cmd_str.starts_with("save ") {
                    // VFS write: save <filename> <content>
                    let args = &cmd_str[5..];
                    let mut parts = args.splitn(2, ' ');
                    if let (Some(filename), Some(content)) = (parts.next(), parts.next()) {
                        match libfolk::sys::synapse::write_file(filename, content.as_bytes()) {
                            Ok(()) => {
                                win.push_line("Saved to SQLite!");
                                // Show filename and size
                                let mut line = [0u8; 64];
                                let prefix = b"  ";
                                line[0..2].copy_from_slice(prefix);
                                let nlen = filename.len().min(30);
                                line[2..2+nlen].copy_from_slice(&filename.as_bytes()[..nlen]);
                                let suffix = b" written";
                                let slen = suffix.len();
                                line[2+nlen..2+nlen+slen].copy_from_slice(suffix);
                                if let Ok(s) = core::str::from_utf8(&line[..2+nlen+slen]) {
                                    win.push_line(s);
                                }
                            }
                            Err(_) => {
                                win.push_line("Save failed!");
                            }
                        }
                    } else {
                        win.push_line("Usage: save <filename> <text>");
                    }
                } else if cmd_str.starts_with("revert ") {
                    // Rollback: "revert ball to v1" or "revert ball 1"
                    let parts: alloc::vec::Vec<&str> = cmd_str[7..].trim().split_whitespace().collect();
                    if parts.len() >= 2 {
                        let app_name = parts[0];
                        let ver_str = parts[parts.len() - 1].trim_start_matches('v');
                        if let Ok(ver) = ver_str.parse::<u32>() {
                            // Send rollback request to proxy via MCP chat
                            let rollback_prompt = alloc::format!("__ROLLBACK__ {} {}", app_name, ver);
                            if libfolk::mcp::client::send_chat(&rollback_prompt).is_some() {
                                win.push_line(&alloc::format!("[Revert] Rolling back '{}' to v{}...", app_name, ver));
                            } else {
                                win.push_line("[Revert] Failed to send rollback request");
                            }
                        } else {
                            win.push_line("Usage: revert <app> <version>");
                        }
                    } else {
                        win.push_line("Usage: revert <app> <version>");
                        win.push_line("Example: revert ball 1");
                    }
                } else if cmd_str == "dream accept all" || cmd_str == "dream accept" {
                    draug.accept_all_creative();
                    let accepted = draug.drain_accepted();
                    for (name, wasm_bytes) in &accepted {
                        wasm.cache.insert(name.clone(), wasm_bytes.clone());
                        win.push_line(&alloc::format!("[Dream] Accepted: {}", &name[..name.len().min(30)]));
                    }
                    if accepted.is_empty() {
                        win.push_line("[Dream] No pending changes");
                    }
                } else if cmd_str == "dream reject all" || cmd_str == "dream reject" {
                    for i in 0..draug.pending_creative.len() {
                        draug.reject_creative(i);
                    }
                    draug.drain_accepted(); // Clear rejected
                    win.push_line("[Dream] All creative changes rejected");
                } else if cmd_str.starts_with("dream accept ") {
                    if let Ok(idx) = cmd_str[13..].trim().parse::<usize>() {
                        if idx > 0 && idx <= draug.pending_creative.len() {
                            draug.accept_creative(idx - 1);
                            let accepted = draug.drain_accepted();
                            for (name, wasm_bytes) in &accepted {
                                wasm.cache.insert(name.clone(), wasm_bytes.clone());
                                win.push_line(&alloc::format!("[Dream] Accepted: {}", name));
                            }
                        } else {
                            win.push_line("[Dream] Invalid index");
                        }
                    }
                } else if cmd_str.starts_with("dream reject ") {
                    if let Ok(idx) = cmd_str[13..].trim().parse::<usize>() {
                        if idx > 0 && idx <= draug.pending_creative.len() {
                            draug.reject_creative(idx - 1);
                            draug.drain_accepted();
                            win.push_line("[Dream] Rejected");
                        }
                    }
                } else if starts_with_ci(cmd_str, "generate driver") {
                    // Autonomous Driver Generation: generate WASM driver for a PCI device
                    let target = cmd_str.get(15..).unwrap_or("").trim();
                    write_str("[DRV] target='");
                    write_str(target);
                    write_str("'\n");
                    let mut pci_buf: [libfolk::sys::pci::PciDeviceInfo; 32] = unsafe { core::mem::zeroed() };
                    let count = libfolk::sys::pci::enumerate(&mut pci_buf);
                    write_str(&alloc::format!("[DRV] PCI: {} devices\n", count));

                    // Find target device (by vendor:device ID or auto-select first non-bridge)
                    let dev = if target.contains(':') {
                        // Parse "1af4:1042" format
                        let parts: alloc::vec::Vec<&str> = target.split(':').collect();
                        if parts.len() == 2 {
                            let vid = u16::from_str_radix(parts[0], 16).unwrap_or(0);
                            let did = u16::from_str_radix(parts[1], 16).unwrap_or(0);
                            write_str(&alloc::format!("[DRV] Looking for {:04x}:{:04x}\n", vid, did));
                            pci_buf[..count].iter().find(|d| d.vendor_id == vid && d.device_id == did)
                        } else { None }
                    } else {
                        // Auto-select: first non-bridge device that isn't VirtIO GPU (already driven)
                        pci_buf[..count].iter().find(|d|
                            d.class_code != 0x06 && // not a bridge
                            !(d.vendor_id == 0x1AF4 && d.device_id == 0x1050) // not VirtIO GPU
                        )
                    };

                    if let Some(d) = dev {
                        write_str(&alloc::format!("[DRV] Found {:04x}:{:04x} ({})\n",
                            d.vendor_id, d.device_id, d.class_name()));

                        // ── Just-in-time bootstrap seeding ──
                        if !*drivers_seeded {
                            write_str("[DRV] First driver request — seeding bootstrap drivers\n");
                            compositor::driver_runtime::seed_bootstrap_drivers(&pci_buf, count);
                            *drivers_seeded = true;
                        }

                        // ── Driver Version Control: check VFS, then built-in, then LLM ──
                        let latest_v = compositor::driver_runtime::find_latest_version(
                            d.vendor_id, d.device_id);

                        if latest_v > 0 {
                            // Cached driver exists — load from Synapse VFS
                            write_str(&alloc::format!("[DRV] Found v{} in Synapse VFS\n", latest_v));
                            win.push_line(&alloc::format!(
                                "[DRV] Loading {:04x}:{:04x} v{} from VFS...",
                                d.vendor_id, d.device_id, latest_v));

                            if let Some(wasm_bytes) = compositor::driver_runtime::load_driver_vfs(
                                d.vendor_id, d.device_id, latest_v
                            ) {
                                write_str(&alloc::format!("[DRV] Loaded {} bytes from VFS\n",
                                    wasm_bytes.len()));
                                // Instantiate driver
                                let mut cap = compositor::driver_runtime::DriverCapability::from_pci(d);
                                let drv_name = alloc::format!("drv_{:04x}_{:04x}",
                                    d.vendor_id, d.device_id);
                                cap.set_name(&drv_name);
                                compositor::driver_runtime::map_device_bars(&mut cap);
                                match compositor::driver_runtime::WasmDriver::new(&wasm_bytes, cap) {
                                    Ok(mut driver) => {
                                        driver.meta.version = latest_v;
                                        driver.meta.source = compositor::driver_runtime::DriverSource::Bootstrap;
                                        let _ = driver.bind_irq();
                                        match driver.start() {
                                            compositor::driver_runtime::DriverResult::WaitingForIrq => {
                                                write_str("[DRV] Driver started (IRQ wait)\n");
                                                win.push_line("[DRV] Driver running (from VFS)");
                                                wasm.active_drivers.push(driver);
                                            }
                                            compositor::driver_runtime::DriverResult::Completed => {
                                                write_str("[DRV] Driver completed immediately\n");
                                                win.push_line("[DRV] Driver completed");
                                            }
                                            other => {
                                                write_str("[DRV] Driver start failed\n");
                                                win.push_line("[DRV] Driver start failed");
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        write_str("[DRV] WASM load error\n");
                                        win.push_line(&alloc::format!("[DRV] Load error: {}", &e[..e.len().min(50)]));
                                    }
                                }
                            } else {
                                write_str("[DRV] VFS read failed, falling back to LLM\n");
                                // Fall through to LLM generation
                                let desc = alloc::format!(
                                    "__DRIVER_GEN__{:04x}:{:04x}:{}",
                                    d.vendor_id, d.device_id, d.class_name());
                                mcp.pending_driver_device = Some(d.clone());
                                let _ = libfolk::mcp::client::send_wasm_gen(&desc);
                            }
                        } else if let Some(builtin) = compositor::driver_runtime::get_builtin_driver(
                            d.vendor_id, d.device_id
                        ) {
                            // Built-in bootstrap driver available
                            write_str(&alloc::format!("[DRV] Loading built-in driver ({} bytes)\n",
                                builtin.len()));
                            win.push_line("[DRV] Loading built-in driver...");
                            let mut cap = compositor::driver_runtime::DriverCapability::from_pci(d);
                            let drv_name = alloc::format!("drv_{:04x}_{:04x}",
                                d.vendor_id, d.device_id);
                            cap.set_name(&drv_name);
                            // MAP MMIO BARs — without this, all MMIO writes go to address 0!
                            let mapped = compositor::driver_runtime::map_device_bars(&mut cap);
                            write_str(&alloc::format!("[DRV] Mapped {} MMIO BARs\n", mapped));
                            match compositor::driver_runtime::WasmDriver::new(builtin, cap) {
                                Ok(mut driver) => {
                                    driver.meta.version = 2;
                                    driver.meta.source = compositor::driver_runtime::DriverSource::Bootstrap;
                                    let _ = driver.bind_irq();
                                    let start_result = driver.start();
                                    write_str("[DRV] start returned\n");
                                    match start_result {
                                        compositor::driver_runtime::DriverResult::WaitingForIrq => {
                                            write_str("[DRV] Built-in driver running (IRQ wait)\n");
                                            win.push_line("[DRV] Driver running (built-in v2)");
                                            wasm.active_drivers.push(driver);
                                        }
                                        compositor::driver_runtime::DriverResult::Completed => {
                                            write_str("[DRV] Built-in driver completed\n");
                                            win.push_line("[DRV] Driver completed");
                                        }
                                        compositor::driver_runtime::DriverResult::OutOfFuel => {
                                            write_str("[DRV] Built-in driver OUT OF FUEL - scheduling\n");
                                            wasm.active_drivers.push(driver);
                                        }
                                        compositor::driver_runtime::DriverResult::Trapped(ref msg) => {
                                            write_str("[DRV] Built-in TRAP: ");
                                            write_str(&msg[..msg.len().min(100)]);
                                            write_str("\n");
                                        }
                                        other => {
                                            write_str("[DRV] Built-in driver start failed\n");
                                            win.push_line("[DRV] Driver start failed");
                                        }
                                    }
                                }
                                Err(e) => {
                                    write_str("[DRV] Built-in load error\n");
                                    win.push_line(&alloc::format!("[DRV] Error: {}", &e[..e.len().min(40)]));
                                }
                            }
                        } else {
                            // No cached driver — generate via LLM
                            write_str("[DRV] No cached driver, requesting LLM generation\n");
                            win.push_line(&alloc::format!(
                                "[DRV] Generating driver for {:04x}:{:04x} ({})...",
                                d.vendor_id, d.device_id, d.class_name()));
                            mcp.pending_driver_device = Some(d.clone());
                            let desc = alloc::format!(
                                "__DRIVER_GEN__{:04x}:{:04x}:{}",
                                d.vendor_id, d.device_id, d.class_name());
                            if libfolk::mcp::client::send_wasm_gen(&desc) {
                                write_str("[DRV] MCP WasmGenRequest sent\n");
                                win.push_line("[DRV] Request sent to LLM");
                            } else {
                                write_str("[DRV] MCP send FAILED\n");
                                win.push_line("[DRV] MCP send failed");
                                mcp.pending_driver_device = None;
                            }
                        }
                    } else {
                        write_str("[DRV] No matching device found\n");
                        win.push_line("[DRV] No matching PCI device found");
                        win.push_line("[DRV] Usage: generate driver [vendor:device]");
                        win.push_line("[DRV] Example: generate driver 1af4:1042");
                    }
                } else if cmd_str == "drivers" {
                    // List active WASM drivers with version/stability info
                    win.push_line(&alloc::format!("[DRV] {} active drivers:", wasm.active_drivers.len()));
                    for drv in wasm.active_drivers.iter() {
                        let src = match drv.meta.source {
                            compositor::driver_runtime::DriverSource::Jit => "jit",
                            compositor::driver_runtime::DriverSource::AutoDream => "dream",
                            compositor::driver_runtime::DriverSource::Bootstrap => "boot",
                        };
                        win.push_line(&alloc::format!(
                            "  {:04x}:{:04x} v{} [{}] irq={}({}) stab={} {}",
                            drv.capability.vendor_id, drv.capability.device_id,
                            drv.meta.version, src,
                            drv.capability.irq_line, drv.meta.irq_count,
                            drv.meta.stability_score,
                            if drv.waiting_for_irq { "waiting" } else { "running" }
                        ));
                    }
                } else if starts_with_ci(cmd_str, "drivers versions") {
                    // List all stored versions for a device
                    let args = cmd_str.get(17..).unwrap_or("").trim();
                    if args.contains(':') {
                        let parts: alloc::vec::Vec<&str> = args.split(':').collect();
                        if parts.len() == 2 {
                            let vid = u16::from_str_radix(parts[0], 16).unwrap_or(0);
                            let did = u16::from_str_radix(parts[1], 16).unwrap_or(0);
                            let latest = compositor::driver_runtime::find_latest_version(vid, did);
                            if latest > 0 {
                                win.push_line(&alloc::format!(
                                    "[DRV] {:04x}:{:04x} — {} versions in VFS:", vid, did, latest));
                                for v in 1..=latest {
                                    let fname = compositor::driver_runtime::driver_vfs_filename(vid, did, v);
                                    let status = if wasm.active_drivers.iter().any(|d|
                                        d.capability.vendor_id == vid &&
                                        d.capability.device_id == did &&
                                        d.meta.version == v
                                    ) { " [ACTIVE]" } else { "" };
                                    win.push_line(&alloc::format!("  v{}: {}{}", v, fname, status));
                                }
                            } else {
                                win.push_line(&alloc::format!("[DRV] No drivers for {:04x}:{:04x}", vid, did));
                            }
                        }
                    } else {
                        win.push_line("[DRV] Usage: drivers versions 8086:100e");
                    }
                } else if starts_with_ci(cmd_str, "drivers rollback") {
                    // Rollback to a specific version: drivers rollback 8086:100e v1
                    let args = cmd_str.get(17..).unwrap_or("").trim();
                    let parts: alloc::vec::Vec<&str> = args.split_whitespace().collect();
                    if parts.len() >= 2 && parts[0].contains(':') {
                        let dev_parts: alloc::vec::Vec<&str> = parts[0].split(':').collect();
                        let ver_str = parts[1].trim_start_matches('v');
                        if dev_parts.len() == 2 {
                            let vid = u16::from_str_radix(dev_parts[0], 16).unwrap_or(0);
                            let did = u16::from_str_radix(dev_parts[1], 16).unwrap_or(0);
                            let target_v = ver_str.parse::<u16>().unwrap_or(0);

                            if target_v == 0 {
                                win.push_line("[DRV] Invalid version");
                            } else if let Some(wasm_bytes) = compositor::driver_runtime::load_driver_vfs(vid, did, target_v) {
                                // Stop current driver for this device
                                wasm.active_drivers.retain(|d|
                                    !(d.capability.vendor_id == vid && d.capability.device_id == did));

                                // Load rolled-back version
                                let mut pci_buf: [libfolk::sys::pci::PciDeviceInfo; 32] = unsafe { core::mem::zeroed() };
                                let count = libfolk::sys::pci::enumerate(&mut pci_buf);
                                if let Some(dev) = pci_buf[..count].iter().find(|d| d.vendor_id == vid && d.device_id == did) {
                                    let mut cap = compositor::driver_runtime::DriverCapability::from_pci(dev);
                                    let drv_name = alloc::format!("drv_{:04x}_{:04x}", vid, did);
                                    cap.set_name(&drv_name);
                                    compositor::driver_runtime::map_device_bars(&mut cap);
                                    match compositor::driver_runtime::WasmDriver::new(&wasm_bytes, cap) {
                                        Ok(mut driver) => {
                                            driver.meta.version = target_v;
                                            let _ = driver.bind_irq();
                                            match driver.start() {
                                                compositor::driver_runtime::DriverResult::WaitingForIrq => {
                                                    write_str(&alloc::format!("[DRV] Rolled back to v{}\n", target_v));
                                                    win.push_line(&alloc::format!("[DRV] Rolled back to v{} — running", target_v));
                                                    wasm.active_drivers.push(driver);
                                                }
                                                _ => { win.push_line("[DRV] Rollback driver failed to start"); }
                                            }
                                        }
                                        Err(e) => { win.push_line(&alloc::format!("[DRV] Load error: {}", &e[..e.len().min(40)])); }
                                    }
                                } else {
                                    win.push_line("[DRV] PCI device not found");
                                }
                            } else {
                                win.push_line(&alloc::format!("[DRV] v{} not found in VFS", target_v));
                            }
                        }
                    } else {
                        win.push_line("[DRV] Usage: drivers rollback 8086:100e v1");
                    }
                } else if cmd_str == "lspci" {
                    // List PCI devices (Autonomous Driver Discovery)
                    let mut pci_buf: [libfolk::sys::pci::PciDeviceInfo; 32] = unsafe { core::mem::zeroed() };
                    let count = libfolk::sys::pci::enumerate(&mut pci_buf);
                    win.push_line(&alloc::format!("[PCI] {} devices:", count));
                    for i in 0..count {
                        let d = &pci_buf[i];
                        win.push_line(&alloc::format!(
                            "  {:02x}:{:02x}.{} {:04x}:{:04x} {} irq={} BAR0={}B",
                            d.bus, d.device_num, d.function,
                            d.vendor_id, d.device_id,
                            d.class_name(),
                            d.interrupt_line,
                            d.bar_sizes[0]
                        ));
                    }
                } else if cmd_str == "https" || starts_with_ci(cmd_str, "https ") {
                    // HTTPS/TLS test — NON-BLOCKING via DNS first, then async TLS
                    let target = if cmd_str.len() > 6 { cmd_str.get(6..).unwrap_or("example.com").trim() }
                                 else { "example.com" };
                    write_str("[HTTPS] Step 1: DNS lookup for ");
                    write_str(target);
                    write_str("...\n");
                    win.push_line(&alloc::format!("[HTTPS] Looking up {}...", target));

                    // DNS is also blocking, but it's much faster (~1-2s typically)
                    // For a true async solution we'd need a state machine, but
                    // dns lookup usually completes within the 10s timeout
                    match libfolk::sys::dns::lookup(target) {
                        Some(ip) => {
                            let msg = alloc::format!("[HTTPS] {} -> {}.{}.{}.{}", target, ip.0, ip.1, ip.2, ip.3);
                            write_str(&msg);
                            write_str("\n");
                            win.push_line(&msg);
                            win.push_line("[HTTPS] Starting TLS 1.3 handshake...");

                            // Now attempt HTTPS — pass DNS-resolved IP to kernel
                            write_str("[HTTPS] TLS connecting...\n");
                            let ip_packed = ((ip.0 as u64) << 24)
                                | ((ip.1 as u64) << 16)
                                | ((ip.2 as u64) << 8)
                                | (ip.3 as u64);
                            let result = unsafe {
                                libfolk::syscall::syscall1(libfolk::syscall::SYS_HTTPS_TEST, ip_packed)
                            };
                            if result == 0 {
                                write_str("[HTTPS] TLS 1.3 SUCCESS!\n");
                                win.push_line("[HTTPS] SUCCESS: TLS 1.3 verified!");
                            } else {
                                write_str("[HTTPS] TLS failed (timeout or connection error)\n");
                                win.push_line("[HTTPS] TLS failed — SLIRP NAT may not support long TCP");
                            }
                        }
                        None => {
                            write_str("[HTTPS] DNS lookup failed\n");
                            win.push_line("[HTTPS] DNS failed — no internet or DNS timeout");
                        }
                    }
                } else if starts_with_ci(cmd_str, "dns ") {
                    // DNS lookup via kernel smoltcp
                    let hostname = cmd_str.get(4..).unwrap_or("").trim();
                    if hostname.is_empty() {
                        win.push_line("[DNS] Usage: dns example.com");
                    } else {
                        write_str("[DNS] Looking up: ");
                        write_str(hostname);
                        write_str("\n");
                        win.push_line(&alloc::format!("[DNS] Looking up {}...", hostname));
                        // SYS_NET_LOOKUP syscall (blocking)
                        match libfolk::sys::dns::lookup(hostname) {
                            Some(ip) => {
                                let msg = alloc::format!("[DNS] {} -> {}.{}.{}.{}",
                                    hostname, ip.0, ip.1, ip.2, ip.3);
                                write_str(&msg);
                                write_str("\n");
                                win.push_line(&msg);
                            }
                            None => {
                                write_str("[DNS] Lookup failed\n");
                                win.push_line("[DNS] Lookup failed");
                            }
                        }
                    }
                } else if cmd_str == "poweroff" || cmd_str == "shutdown" {
                    // M12: Save app states and shut down
                    let name_hash = shell_hash_name("poweroff") as u64;
                    let shell_payload = SHELL_OP_EXEC | (name_hash << 8);
                    let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                        | (shell_payload << 8);
                    let _ = unsafe {
                        libfolk::syscall::syscall3(
                            libfolk::syscall::SYS_IPC_SEND,
                            libfolk::sys::intent::INTENT_TASK_ID as u64,
                            intent_req, 0
                        )
                    };
                    win.push_line("Shutting down...");
                } else if cmd_str == "ai-status" {
                    // Query inference server status
                    use libfolk::sys::inference;
                    match inference::status() {
                        Ok((has_model, arena_size)) => {
                            if has_model {
                                win.push_line("AI: model loaded");
                            } else {
                                win.push_line("AI: stub mode (no model)");
                            }
                            let kb = arena_size / 1024;
                            let mut buf = [0u8; 32];
                            let s = format_arena_line(&mut buf, kb);
                            win.push_line(s);
                        }
                        Err(_) => {
                            win.push_line("AI: server unavailable");
                        }
                    }
                } else if cmd_str.starts_with("ask ") || cmd_str.starts_with("infer ") {
                    // AI inference command — async streaming via TokenRing
                    use libfolk::sys::inference;
                    let query = if cmd_str.starts_with("ask ") {
                        &cmd_str[4..]
                    } else {
                        &cmd_str[6..]
                    };
                    let query = query.trim();
                    if query.is_empty() {
                        win.push_line("Usage: ask <question>");
                    } else if stream.ring_handle != 0 {
                        // ULTRA 42: Already generating
                        win.push_line("[AI is busy]");
                    } else {
                        match inference::ping() {
                            Ok(has_model) => {
                                if !has_model {
                                    win.push_line("[AI] No model loaded (stub mode)");
                                } else {
                                    // Create TokenRing shmem (4 pages = 16KB)
                                    let ring_ok = if let Ok(rh) = shmem_create(16384) {
                                        let _ = shmem_grant(rh, inference::inference_task_id());
                                        // Create query shmem
                                        let query_bytes = query.as_bytes();
                                        if let Ok(qh) = shmem_create(4096) {
                                            let _ = shmem_grant(qh, inference::inference_task_id());
                                            if shmem_map(qh, ASK_QUERY_VADDR).is_ok() {
                                                unsafe {
                                                    let ptr = ASK_QUERY_VADDR as *mut u8;
                                                    core::ptr::copy_nonoverlapping(
                                                        query_bytes.as_ptr(), ptr, query_bytes.len()
                                                    );
                                                }
                                                let _ = shmem_unmap(qh, ASK_QUERY_VADDR);

                                                // Send async request
                                                match inference::ask_async(qh, query_bytes.len(), rh) {
                                                    Ok(()) => {
                                                        // Map ring for polling
                                                        if shmem_map(rh, RING_VADDR).is_ok() {
                                                            stream.ring_handle = rh;
                                                            stream.ring_read_idx = 0;
                                                            stream.win_id = win_id;
                                                            stream.query_handle = qh;
                                                            win.push_line("[AI] Thinking...");
                                                            win.typing = true;
                                                            true
                                                        } else {
                                                            let _ = shmem_destroy(rh);
                                                            let _ = shmem_destroy(qh);
                                                            win.push_line("[AI] Ring map failed");
                                                            false
                                                        }
                                                    }
                                                    Err(_) => {
                                                        let _ = shmem_destroy(rh);
                                                        let _ = shmem_destroy(qh);
                                                        win.push_line("[AI] Server offline — AI Core may need restart");
                                                        false
                                                    }
                                                }
                                            } else {
                                                let _ = shmem_destroy(rh);
                                                let _ = shmem_destroy(qh);
                                                win.push_line("[AI] Query map failed");
                                                false
                                            }
                                        } else {
                                            let _ = shmem_destroy(rh);
                                            win.push_line("[AI] Query alloc failed");
                                            false
                                        }
                                    } else {
                                        win.push_line("[AI] Ring alloc failed");
                                        false
                                    };
                                    let _ = ring_ok; // suppress unused warning
                                }
                            }
                            Err(_) => {
                                win.push_line("[AI] Inference server unavailable (may have crashed)");
                                win.push_line("[AI] Try again — server may auto-recover on next request");
                            }
                        }
                    }
                } else if starts_with_ci(cmd_str, "load ") {
                    // Load precompiled WASM from host filesystem via proxy
                    let path = cmd_str[5..].trim();
                    if path.is_empty() {
                        win.push_line("Usage: load <path.wasm>");
                    } else {
                        win.push_line(&alloc::format!("[OS] Loading {}...", path));
                        // Send [LOAD_WASM:path] to proxy via COM2
                        let load_cmd = alloc::format!("[LOAD_WASM:{}]", path);
                        const LOAD_BUF_VADDR: usize = 0x50080000;
                        const LOAD_BUF_SIZE: usize = 131072;
                        if libfolk::sys::mmap_at(LOAD_BUF_VADDR, LOAD_BUF_SIZE, 3).is_ok() {
                            let load_buf = unsafe {
                                core::slice::from_raw_parts_mut(LOAD_BUF_VADDR as *mut u8, LOAD_BUF_SIZE)
                            };
                            let resp_len = libfolk::sys::ask_gemini(&load_cmd, load_buf);
                            if resp_len > 0 {
                                if let Ok(text) = core::str::from_utf8(&load_buf[..resp_len]) {
                                    use compositor::intent::AgentIntent;
                                    let intent = compositor::intent::parse_intent(text);
                                    match intent {
                                        AgentIntent::ToolReady { binary_base64 } => {
                                            if let Some(wasm_bytes) = compositor::intent::base64_decode(&binary_base64) {
                                                win.push_line(&alloc::format!("[OS] Loaded {} bytes", wasm_bytes.len()));
                                                wasm.last_bytes = Some(wasm_bytes.clone());

                                                // load command ALWAYS launches as interactive app
                                                let interactive = true;
                                                wasm.last_interactive = true;

                                                let config = compositor::wasm_runtime::WasmConfig {
                                                    screen_width: fb.width as u32,
                                                    screen_height: fb.height as u32,
                                                    uptime_ms: libfolk::sys::uptime() as u32,
                                                };

                                                if interactive {
                                                    match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                                        Ok(app) => {
                                                            // Hide the load window — WASM app takes over screen
                                                            win.visible = false;
                                                            wasm.active_app = Some(app);
                                                            wasm.active_app_key = Some(alloc::string::String::from(path));
                                                            wasm.app_open_since_ms = libfolk::sys::uptime();
                                                            wasm.fuel_fail_count = 0;
                                                        }
                                                        Err(e) => { win.push_line(&alloc::format!("[OS] Error: {}", &e[..e.len().min(60)])); }
                                                    }
                                                } else {
                                                    let (result, output) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, config);
                                                    win.push_line(&alloc::format!("[OS] One-shot: {} commands", output.draw_commands.len()));
                                                    if let Some(color) = output.fill_screen { fb.clear(fb.color_from_rgb24(color)); }
                                                    for cmd in &output.draw_commands { fb.fill_rect(cmd.x as usize, cmd.y as usize, cmd.w as usize, cmd.h as usize, fb.color_from_rgb24(cmd.color)); }
                                                    for cmd in &output.line_commands { let c = fb.color_from_rgb24(cmd.color); compositor::graphics::draw_line(&mut *fb, cmd.x1, cmd.y1, cmd.x2, cmd.y2, c); }
                                                    for cmd in &output.circle_commands { let c = fb.color_from_rgb24(cmd.color); compositor::graphics::draw_circle(&mut *fb, cmd.cx, cmd.cy, cmd.r, c); }
                                                    for cmd in &output.text_commands { fb.draw_string(cmd.x as usize, cmd.y as usize, &cmd.text, fb.color_from_rgb24(cmd.color), fb.color_from_rgb24(0)); }
                                                    damage.damage_full();
                                                }
                                            } else {
                                                write_str("[LOAD] base64 decode FAILED\n");
                                                win.push_line("[OS] base64 decode failed");
                                            }
                                        }
                                        _ => {
                                            win.push_line(&alloc::format!("[OS] Error: {}", &text[..text.len().min(80)]));
                                        }
                                    }
                                }
                            } else {
                                win.push_line("[OS] No response from proxy");
                            }
                            let _ = libfolk::sys::munmap(LOAD_BUF_VADDR as *mut u8, LOAD_BUF_SIZE);
                        }
                    }
                } else if starts_with_ci(cmd_str, "run ") {
                    // App Persistence: load and execute saved WASM from VFS
                    let app_name = cmd_str[4..].trim();
                    if app_name.is_empty() {
                        win.push_line("Usage: run <name>");
                    } else {
                        let filename = if app_name.ends_with(".wasm") {
                            alloc::string::String::from(app_name)
                        } else {
                            alloc::format!("{}.wasm", app_name)
                        };
                        win.push_line(&alloc::format!("[OS] Loading {}...", filename));

                        // Read WASM from Synapse VFS via shmem
                        const VFS_READ_VADDR: usize = 0x50040000;
                        match libfolk::sys::synapse::read_file_shmem(&filename) {
                            Ok(resp) => {
                                if shmem_map(resp.shmem_handle, VFS_READ_VADDR).is_ok() {
                                    let data = unsafe {
                                        core::slice::from_raw_parts(VFS_READ_VADDR as *const u8, resp.size as usize)
                                    };
                                    let wasm_bytes = alloc::vec::Vec::from(data);
                                    let _ = shmem_unmap(resp.shmem_handle, VFS_READ_VADDR);
                                    let _ = shmem_destroy(resp.shmem_handle);

                                    win.push_line(&alloc::format!("[OS] Loaded {} bytes", wasm_bytes.len()));

                                    let config = compositor::wasm_runtime::WasmConfig {
                                        screen_width: fb.width as u32,
                                        screen_height: fb.height as u32,
                                        uptime_ms: libfolk::sys::uptime() as u32,
                                    };

                                    match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                        Ok(app) => {
                                            win.push_line("[OS] App launched! Press ESC to exit.");
                                            wasm.active_app = Some(app);
                                            wasm.active_app_key = Some(alloc::string::String::from(app_name));
                                            wasm.app_open_since_ms = libfolk::sys::uptime();
                                            wasm.fuel_fail_count = 0;
                                            wasm.last_bytes = Some(wasm_bytes);
                                            wasm.last_interactive = true;
                                        }
                                        Err(e) => {
                                            win.push_line(&alloc::format!("[OS] Load error: {}", &e[..e.len().min(60)]));
                                        }
                                    }
                                } else {
                                    let _ = shmem_destroy(resp.shmem_handle);
                                    win.push_line("[OS] Failed to map file data");
                                }
                            }
                            Err(_) => {
                                win.push_line(&alloc::format!("[OS] App '{}' not found", app_name));
                            }
                        }
                    }
                } else if cmd_str.starts_with("agent ") {
                    // Agentic ReAct loop via MCP
                    // Flags: --force (skip cache), --tweak "mod" (modify existing)
                    let raw = cmd_str[6..].trim();
                    let (flags, prompt) = parse_agent_flags(raw);
                    write_str("[AGENT] Command: ");
                    write_str(prompt);
                    if flags.force { write_str(" [--force]"); }
                    if flags.tweak_msg.is_some() { write_str(" [--tweak]"); }
                    write_str("\n");
                    if prompt.is_empty() {
                        win.push_line("Usage: agent <task>");
                        win.push_line("  --force: skip WASM cache");
                        win.push_line("  --tweak \"change\": modify cached version");
                    } else {
                        // Record command for Draug prediction
                        draug.record_command(prompt);

                        // Check WASM cache (Pillar 4)
                        if !flags.force {
                            if let Some(cached_wasm) = wasm.cache.get(prompt) {
                                win.push_line(&alloc::format!("[Cache] Hit: {} bytes", cached_wasm.len()));
                                // Use cached WASM directly
                                wasm.last_bytes = Some(cached_wasm.clone());
                                let config = compositor::wasm_runtime::WasmConfig {
                                    screen_width: fb.width as u32,
                                    screen_height: fb.height as u32,
                                    uptime_ms: libfolk::sys::uptime() as u32,
                                };
                                let (result, output) = compositor::wasm_runtime::execute_wasm(cached_wasm, config);
                                if let compositor::wasm_runtime::WasmResult::Ok = &result {
                                    win.push_line("[Cache] Executed from cache (instant)");
                                }
                                if let Some(color) = output.fill_screen { fb.clear(fb.color_from_rgb24(color)); }
                                for cmd in &output.draw_commands {
                                    fb.fill_rect(cmd.x as usize, cmd.y as usize, cmd.w as usize, cmd.h as usize, fb.color_from_rgb24(cmd.color));
                                }
                                damage.damage_full();
                                need_redraw = true;
                                // Skip agent — served from cache
                            } else {
                                // No cache hit — run agent
                                win.push_line(&alloc::format!("[Agent] Task: {}", &prompt[..prompt.len().min(60)]));
                                let mut session = compositor::agent::AgentSession::new(prompt, win_id);
                                if session.start() {
                                    write_str("[AGENT] Session started\n");
                                    win.push_line("[Agent] Thinking...");
                                    *active_agent = Some(session);
                                } else {
                                    win.push_line("[Agent] Error: failed to start");
                                }
                            }
                        } else {
                            // --force: skip cache, always ask LLM
                            win.push_line(&alloc::format!("[Agent] Task (forced): {}", &prompt[..prompt.len().min(50)]));
                            let mut session = compositor::agent::AgentSession::new(prompt, win_id);
                            if session.start() {
                                write_str("[AGENT] Session started (forced)\n");
                                win.push_line("[Agent] Thinking...");
                                *active_agent = Some(session);
                            } else {
                                win.push_line("[Agent] Error: failed to start");
                            }
                        }
                    }
                } else if cmd_str.starts_with("gemini ") {
                    // Legacy: direct LLM query (blocking, no tool chaining)
                    let prompt = cmd_str[7..].trim();
                    write_str("[COMPOSITOR] gemini command: ");
                    write_str(prompt);
                    write_str("\n");
                    if prompt.is_empty() {
                        win.push_line("Usage: gemini <prompt>");
                    } else if starts_with_ci(prompt, "generate ") {
                        // Direct WASM tool generation — skip AI agent, go straight to compiler
                        let tool_prompt = prompt[9..].trim();
                        win.push_line(&alloc::format!("[AI] Generating tool: {}...", &tool_prompt[..tool_prompt.len().min(50)]));
                        mcp.deferred_tool_gen = Some((win_id, alloc::string::String::from(tool_prompt)));
                        damage.damage_full();
                    } else {
                        win.push_line(&alloc::format!("> gemini {}", &prompt[..prompt.len().min(60)]));

                        // Agentic AI: serialize UI state → send to proxy → parse intent
                        let full_prompt = alloc::format!(
                            "You are Folkering OS AI assistant. Current screen state:\n{}\nUser command: {}\n\nYou MUST respond with ONLY a JSON object. Choose one:\n{{\"action\": \"move_window\", \"window_id\": N, \"x\": N, \"y\": N}}\n{{\"action\": \"close_window\", \"window_id\": N}}\n{{\"action\": \"generate_tool\", \"prompt\": \"description\"}}\n{{\"action\": \"text\", \"content\": \"your answer\"}}\nNEVER respond with plain text. ALWAYS use JSON.",
                            ui_state_snapshot, prompt
                        );

                        win.push_line("[cloud] Sending with UI context...");

                        // Step 3: Call Gemini proxy (128KB via mmap — bump allocator is only 64KB)
                        const GEMINI_CMD_VADDR: usize = 0x50000000; // Must be >= 0x40000000 (MMAP_BASE)
                        const GEMINI_CMD_SIZE: usize = 131072;
                        let response_len = if libfolk::sys::mmap_at(GEMINI_CMD_VADDR, GEMINI_CMD_SIZE, 3).is_ok() {
                            let gemini_buf = unsafe {
                                core::slice::from_raw_parts_mut(GEMINI_CMD_VADDR as *mut u8, GEMINI_CMD_SIZE)
                            };
                            let rlen = libfolk::sys::ask_gemini(&full_prompt, gemini_buf);
                            rlen
                        } else { 0 };
                        let gemini_buf = unsafe {
                            core::slice::from_raw_parts(GEMINI_CMD_VADDR as *const u8, GEMINI_CMD_SIZE)
                        };

                        if response_len > 0 {
                            if let Ok(text) = alloc::str::from_utf8(&gemini_buf[..response_len]) {
                                // Step 4: Parse intent and display result
                                use compositor::intent::AgentIntent;
                                let intent = compositor::intent::parse_intent(text);
                                write_str("[COMPOSITOR] Intent parsed\n");

                                match intent {
                                    AgentIntent::MoveWindow { window_id, x, y } => {
                                        win.push_line(&alloc::format!(
                                            "[AI] Moving window {} to ({},{})", window_id, x, y
                                        ));
                                        // Deferred: execute after dropping win
                                        deferred_intent_action = Some((1, window_id, x, y));
                                    }
                                    AgentIntent::CloseWindow { window_id } => {
                                        win.push_line(&alloc::format!("[AI] Closing window {}", window_id));
                                        deferred_intent_action = Some((2, window_id, 0, 0));
                                    }
                                    AgentIntent::ResizeWindow { window_id, w, h } => {
                                        win.push_line(&alloc::format!(
                                            "[AI] Resizing window {} to {}x{}", window_id, w, h
                                        ));
                                        deferred_intent_action = Some((3, window_id, w, h));
                                    }
                                    AgentIntent::GenerateTool { prompt: tp } => {
                                        win.push_line(&alloc::format!(
                                            "[AI] Generating tool: {}...", &tp[..tp.len().min(50)]
                                        ));
                                        // Deferred 2-frame: this frame renders the message,
                                        // next frame executes the WASM pipeline
                                        mcp.deferred_tool_gen = Some((win_id, tp));
                                        damage.damage_full();
                                    }
                                    AgentIntent::TextResponse { text: resp } => {
                                        // Filter <think>...</think> from response → overlay
                                        let mut visible = alloc::string::String::new();
                                        let mut in_think = false;
                                        let mut rest = resp.as_str();
                                        while !rest.is_empty() {
                                            if !in_think {
                                                if let Some(pos) = rest.find("<think>") {
                                                    visible.push_str(&rest[..pos]);
                                                    rest = &rest[pos + 7..];
                                                    in_think = true;
                                                    stream.think_active = true;
                                                    stream.think_display_len = 0;
                                                } else {
                                                    visible.push_str(rest);
                                                    break;
                                                }
                                            } else {
                                                if let Some(pos) = rest.find("</think>") {
                                                    // Store think content in overlay buffer
                                                    let think_text = &rest[..pos];
                                                    let copy_len = think_text.len().min(THINK_BUF_SIZE - stream.think_display_len);
                                                    stream.think_display[stream.think_display_len..stream.think_display_len + copy_len]
                                                        .copy_from_slice(&think_text.as_bytes()[..copy_len]);
                                                    stream.think_display_len += copy_len;
                                                    stream.think_active = false;
                                                    stream.think_fade_timer = 180; // 3 seconds visible
                                                    need_redraw = true;
                                                    rest = &rest[pos + 8..];
                                                    in_think = false;
                                                } else {
                                                    // Unclosed think — store all, show nothing
                                                    let copy_len = rest.len().min(THINK_BUF_SIZE - stream.think_display_len);
                                                    stream.think_display[stream.think_display_len..stream.think_display_len + copy_len]
                                                        .copy_from_slice(&rest.as_bytes()[..copy_len]);
                                                    stream.think_display_len += copy_len;
                                                    break;
                                                }
                                            }
                                        }
                                        win.push_line("[Gemini]:");
                                        for line in visible.split('\n') {
                                            let trimmed = line.trim();
                                            if !trimmed.is_empty() {
                                                win.push_line(trimmed);
                                            }
                                        }
                                    }
                                    AgentIntent::Error { message } => {
                                        win.push_line(&alloc::format!("[AI Error] {}", message));
                                    }
                                    _ => {
                                        win.push_line("[AI] Unhandled intent");
                                    }
                                }
                            } else {
                                win.push_line("[cloud] Response not valid UTF-8");
                            }
                        } else {
                            win.push_line("[cloud] Error: no response from Gemini API");
                        }
                        // Free mmap'd buffer
                        let _ = libfolk::sys::munmap(GEMINI_CMD_VADDR as *mut u8, GEMINI_CMD_SIZE);
                    }
                } else {
                    win.push_line("Sent to shell...");
                }
                if !win.interactive {
                    win.push_line("---");
                }
            }
            // Execute deferred AI intent actions (after win borrow is dropped)
            if let Some((action, wid, a1, a2)) = deferred_intent_action {
                match action {
                    1 => { // MoveWindow
                        if let Some(w) = wm.get_window_mut(wid) {
                            w.x = a1 as i32;
                            w.y = a2 as i32;
                        }
                        damage.damage_full();
                    }
                    2 => { // CloseWindow
                        wm.close_window(wid);
                        damage.damage_full();
                    }
                    3 => { // ResizeWindow
                        if let Some(w) = wm.get_window_mut(wid) {
                            w.width = a1;
                            w.height = a2;
                        }
                        damage.damage_full();
                    }
                    _ => {}
                }
            }
            } // end if deferred_app_handle == 0
            } // end if !folkshell_handled

            // Clear the omnibar input after executing
            input.text_len = 0;
            input.cursor_pos = 0;
            for i in 0..256 { input.text_buffer[i] = 0; }
            input.show_results = false;
            cursor.bg_dirty = true;
        }
    }

    // ===== Deferred app creation from omnibar `open` command =====
    if deferred_app_handle != 0 {
        if shmem_map(deferred_app_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
            let buf = unsafe {
                core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, 4096)
            };
            if let Some(header) = libfolk::ui::parse_header(buf) {
                let wc = wm.windows.len() as i32;
                let app_id = wm.create_terminal(
                    header.title,
                    120 + wc * 30, 100 + wc * 30,
                    header.width as u32, header.height as u32,
                );
                if let Some(app_win) = wm.get_window_mut(app_id) {
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
                need_redraw = true;
            }
            let _ = shmem_unmap(deferred_app_handle, COMPOSITOR_SHMEM_VADDR);
        }
        let _ = shmem_destroy(deferred_app_handle);
    }

    DispatchResult {
        need_redraw,
        did_work,
        deferred_app_handle,
    }
}

/// Execute a command typed into an interactive terminal window.
///
/// This handles the same built-in commands as the omnibar but outputs
/// to the terminal window instead of creating a new one.
pub fn execute_terminal_command(
    win_id: u32,
    wm: &mut WindowManager,
    wasm: &mut WasmState,
    mcp: &mut McpState,
) -> bool {
    let mut need_redraw = false;

    if let Some(win) = wm.get_window_mut(win_id) {
        let cmd_len = win.input_len;
        let mut cmd_buf = [0u8; 128];
        cmd_buf[..cmd_len].copy_from_slice(&win.input_buf[..cmd_len]);
        win.clear_input();

        if let Ok(cmd_str) = core::str::from_utf8(&cmd_buf[..cmd_len]) {
            // Echo the command
            let mut echo = [0u8; 132];
            echo[0] = b'f'; echo[1] = b'o'; echo[2] = b'l';
            echo[3] = b'k'; echo[4] = b'>'; echo[5] = b' ';
            let elen = cmd_len.min(125);
            echo[6..6+elen].copy_from_slice(&cmd_buf[..elen]);
            if let Ok(s) = core::str::from_utf8(&echo[..6+elen]) {
                win.push_line(s);
            }

            // Execute built-in commands (same as omnibar but output to THIS window)
            if cmd_str == "ls" || cmd_str == "files" {
                let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                    | (SHELL_OP_LIST_FILES << 8);
                let ipc_result = unsafe {
                    libfolk::syscall::syscall3(
                        libfolk::syscall::SYS_IPC_SEND,
                        libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                    )
                };
                if ipc_result != u64::MAX {
                    let count = (ipc_result >> 32) as usize;
                    let shmem_handle = (ipc_result & 0xFFFFFFFF) as u32;
                    if shmem_handle != 0 && count > 0 {
                        if shmem_map(shmem_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                            let buf = unsafe {
                                core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, count * 32)
                            };
                            for i in 0..count {
                                let offset = i * 32;
                                let name_end = buf[offset..offset+24].iter()
                                    .position(|&b| b == 0).unwrap_or(24);
                                let name = unsafe { core::str::from_utf8_unchecked(&buf[offset..offset+name_end]) };
                                let size = u32::from_le_bytes([buf[offset+24], buf[offset+25], buf[offset+26], buf[offset+27]]);
                                let mut line = [0u8; 48];
                                line[0] = b' '; line[1] = b' ';
                                let nlen = name.len().min(30);
                                line[2..2+nlen].copy_from_slice(&name.as_bytes()[..nlen]);
                                let mut sb = [0u8; 16];
                                let ss = format_usize(size as usize, &mut sb);
                                let sl = ss.len();
                                line[3+nlen..3+nlen+sl].copy_from_slice(ss.as_bytes());
                                if let Ok(s) = core::str::from_utf8(&line[..3+nlen+sl]) {
                                    win.push_line(s);
                                }
                            }
                            let _ = shmem_unmap(shmem_handle, COMPOSITOR_SHMEM_VADDR);
                            let _ = shmem_destroy(shmem_handle);
                        }
                    }
                }
            } else if cmd_str == "ps" || cmd_str == "tasks" {
                let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT | (SHELL_OP_PS << 8);
                let ipc_result = unsafe {
                    libfolk::syscall::syscall3(
                        libfolk::syscall::SYS_IPC_SEND,
                        libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                    )
                };
                if ipc_result != u64::MAX {
                    let count = (ipc_result >> 32) as usize;
                    let handle = (ipc_result & 0xFFFFFFFF) as u32;
                    if handle != 0 && count > 0 {
                        if shmem_map(handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                            let buf = unsafe {
                                core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, count * 32)
                            };
                            for i in 0..count {
                                let off = i * 32;
                                let tid = u32::from_le_bytes([buf[off], buf[off+1], buf[off+2], buf[off+3]]);
                                let state = u32::from_le_bytes([buf[off+4], buf[off+5], buf[off+6], buf[off+7]]);
                                let ne = buf[off+8..off+24].iter().position(|&b| b == 0).unwrap_or(16);
                                let name = unsafe { core::str::from_utf8_unchecked(&buf[off+8..off+8+ne]) };
                                let ss = match state { 0=>"Run", 1=>"Run", 2=>"Blk", 3=>"Blk", 4=>"Wait", 5=>"Exit", _=>"?" };
                                let mut line = [0u8; 48];
                                let mut p = 0;
                                let mut tb = [0u8; 16];
                                let ts = format_usize(tid as usize, &mut tb);
                                line[p..p+ts.len()].copy_from_slice(ts.as_bytes()); p += ts.len();
                                line[p] = b' '; p += 1;
                                let nl = name.len().min(15);
                                line[p..p+nl].copy_from_slice(&name.as_bytes()[..nl]); p += nl;
                                line[p] = b' '; p += 1;
                                line[p..p+ss.len()].copy_from_slice(ss.as_bytes()); p += ss.len();
                                if let Ok(s) = core::str::from_utf8(&line[..p]) { win.push_line(s); }
                            }
                            let _ = shmem_unmap(handle, COMPOSITOR_SHMEM_VADDR);
                            let _ = shmem_destroy(handle);
                        }
                    }
                }
            } else if cmd_str == "uptime" {
                let ms = uptime();
                let mut buf = [0u8; 32];
                let time_str = format_uptime(ms, &mut buf);
                win.push_line(time_str);
            } else if cmd_str.starts_with("cat ") {
                let filename = cmd_str[4..].trim();
                if filename.is_empty() {
                    win.push_line("usage: cat <filename>");
                } else {
                    let name_hash = shell_hash_name(filename) as u64;
                    let shell_payload = SHELL_OP_CAT_FILE | (name_hash << 8);
                    let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                        | (shell_payload << 8);
                    let ipc_result = unsafe {
                        libfolk::syscall::syscall3(
                            libfolk::syscall::SYS_IPC_SEND,
                            libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                        )
                    };
                    if ipc_result != u64::MAX && ipc_result != SHELL_STATUS_NOT_FOUND {
                        let size = (ipc_result >> 32) as usize;
                        let handle = (ipc_result & 0xFFFFFFFF) as u32;
                        if handle != 0 && size > 0 {
                            if shmem_map(handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                let buf = unsafe {
                                    core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, size)
                                };
                                let mut start = 0;
                                for pos in 0..size {
                                    if buf[pos] == b'\n' || buf[pos] == 0 {
                                        if pos > start {
                                            if let Ok(line) = core::str::from_utf8(&buf[start..pos]) {
                                                win.push_line(line);
                                            }
                                        }
                                        start = pos + 1;
                                        if buf[pos] == 0 { break; }
                                    }
                                }
                                if start < size {
                                    let end = buf[start..size].iter().position(|&b| b == 0)
                                        .map(|p| start + p).unwrap_or(size);
                                    if end > start {
                                        if let Ok(line) = core::str::from_utf8(&buf[start..end]) {
                                            win.push_line(line);
                                        }
                                    }
                                }
                                let _ = shmem_unmap(handle, COMPOSITOR_SHMEM_VADDR);
                                let _ = shmem_destroy(handle);
                            }
                        } else {
                            win.push_line("File is empty");
                        }
                    } else {
                        win.push_line("File not found");
                    }
                }
            } else if cmd_str.starts_with("find ") || cmd_str.starts_with("search ") {
                let query = if cmd_str.starts_with("find ") {
                    cmd_str[5..].trim()
                } else {
                    cmd_str[7..].trim()
                };
                if query.is_empty() {
                    win.push_line("usage: find <query>");
                } else {
                    // Create shmem with query string
                    let qb = query.as_bytes();
                    let qlen = qb.len().min(63);
                    if let Ok(qh) = shmem_create(64) {
                        for tid in 2..=8 { let _ = shmem_grant(qh, tid); }
                        if shmem_map(qh, COMPOSITOR_SHMEM_VADDR).is_ok() {
                            let buf = unsafe {
                                core::slice::from_raw_parts_mut(COMPOSITOR_SHMEM_VADDR as *mut u8, 64)
                            };
                            buf[..qlen].copy_from_slice(&qb[..qlen]);
                            buf[qlen] = 0;
                            let _ = shmem_unmap(qh, COMPOSITOR_SHMEM_VADDR);
                        }
                        let shell_req = SHELL_OP_SEARCH
                            | ((qh as u64) << 8)
                            | ((qlen as u64) << 40);
                        let ipc_result = unsafe {
                            libfolk::syscall::syscall3(
                                libfolk::syscall::SYS_IPC_SEND,
                                SHELL_TASK_ID as u64, shell_req, 0
                            )
                        };
                        let _ = shmem_destroy(qh);

                        if ipc_result != u64::MAX && ipc_result != 0 {
                            let count = (ipc_result >> 32) as usize;
                            let rh = (ipc_result & 0xFFFFFFFF) as u32;
                            if rh != 0 && count > 0 {
                                let mut mb = [0u8; 24];
                                let prefix = b"Matches: ";
                                mb[..prefix.len()].copy_from_slice(prefix);
                                let mut nb = [0u8; 16];
                                let ns = format_usize(count, &mut nb);
                                let nl = ns.len();
                                mb[prefix.len()..prefix.len()+nl].copy_from_slice(ns.as_bytes());
                                if let Ok(s) = core::str::from_utf8(&mb[..prefix.len()+nl]) {
                                    win.push_line(s);
                                }
                                if shmem_map(rh, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                    let buf = unsafe {
                                        core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, count * 32)
                                    };
                                    for i in 0..count.min(10) {
                                        let off = i * 32;
                                        let ne = buf[off..off+24].iter()
                                            .position(|&b| b == 0).unwrap_or(24);
                                        let name = unsafe {
                                            core::str::from_utf8_unchecked(&buf[off..off+ne])
                                        };
                                        let size = u32::from_le_bytes([
                                            buf[off+24], buf[off+25], buf[off+26], buf[off+27]
                                        ]);
                                        let mut line = [0u8; 48];
                                        line[0] = b' '; line[1] = b' ';
                                        let nnl = name.len().min(24);
                                        line[2..2+nnl].copy_from_slice(&name.as_bytes()[..nnl]);
                                        let mut sb = [0u8; 16];
                                        let ss = format_usize(size as usize, &mut sb);
                                        let sl = ss.len();
                                        line[3+nnl..3+nnl+sl].copy_from_slice(ss.as_bytes());
                                        if let Ok(s) = core::str::from_utf8(&line[..3+nnl+sl]) {
                                            win.push_line(s);
                                        }
                                    }
                                    let _ = shmem_unmap(rh, COMPOSITOR_SHMEM_VADDR);
                                    let _ = shmem_destroy(rh);
                                }
                            } else {
                                win.push_line("No matches");
                            }
                        } else {
                            win.push_line("No matches");
                        }
                    }
                }
            } else if cmd_str.starts_with("open ") {
                // Open app — try VFS first, fallback to Shell
                let app_name = cmd_str[5..].trim();
                if app_name.is_empty() {
                    win.push_line("usage: open <app>");
                } else {
                    let mut vfs_ok = false;
                    // Build filename: "calc" → "calc.fkui"
                    let mut fname = [0u8; 64];
                    let nb = app_name.as_bytes();
                    let ext = b".fkui";
                    if nb.len() + ext.len() < 64 {
                        fname[..nb.len()].copy_from_slice(nb);
                        fname[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                        let fname_str = unsafe { core::str::from_utf8_unchecked(&fname[..nb.len()+ext.len()]) };

                        if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(fname_str) {
                            win.push_line("App loaded from VFS!");
                            win.input_buf[0..4].copy_from_slice(&resp.shmem_handle.to_le_bytes());
                            win.input_buf[4] = 0xAA; // marker
                            vfs_ok = true;
                        }
                    }

                    if !vfs_ok {
                        // Fallback: Shell IPC
                        let name_hash = shell_hash_name(app_name) as u64;
                        let shell_payload = SHELL_OP_OPEN_APP | (name_hash << 8);
                        let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                            | (shell_payload << 8);
                        let ipc_result = unsafe {
                            libfolk::syscall::syscall3(
                                libfolk::syscall::SYS_IPC_SEND,
                                libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                            )
                        };
                        let magic = (ipc_result >> 48) as u16;
                        if magic == 0x5549 {
                            let ui_handle = (ipc_result & 0xFFFFFFFF) as u32;
                            win.push_line("App opened via Shell!");
                            win.input_buf[0..4].copy_from_slice(&ui_handle.to_le_bytes());
                            win.input_buf[4] = 0xAA; // marker
                        } else {
                            win.push_line("Unknown app. Try: open calc");
                        }
                    }
                }
            } else if cmd_str == "ui_test" || cmd_str == "app" {
                // Route to Shell via Intent Service — Shell builds UI shmem
                let name_hash = shell_hash_name("ui_test") as u64;
                let shell_payload = SHELL_OP_EXEC | (name_hash << 8);
                let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                    | (shell_payload << 8);
                let ipc_result = unsafe {
                    libfolk::syscall::syscall3(
                        libfolk::syscall::SYS_IPC_SEND,
                        libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0
                    )
                };
                // Check for UI shmem response: magic 0x5549 in upper 16 bits
                let magic = (ipc_result >> 48) as u16;
                if magic == 0x5549 {
                    let ui_handle = (ipc_result & 0xFFFFFFFF) as u32;
                    win.push_line("App received from Shell!");
                    // Signal: create UI window after win borrow released
                    // Store handle in input_buf temporarily
                    win.input_buf[0..4].copy_from_slice(&ui_handle.to_le_bytes());
                    win.input_buf[4] = 0xAA; // marker
                } else {
                    win.push_line("App launch failed");
                }
            } else if cmd_str == "poweroff" || cmd_str == "shutdown" {
                // M12: Route poweroff to Shell via Intent Service
                // Shell will save app states before shutting down
                let name_hash = shell_hash_name("poweroff") as u64;
                let shell_payload = SHELL_OP_EXEC | (name_hash << 8);
                let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT
                    | (shell_payload << 8);
                let _ = unsafe {
                    libfolk::syscall::syscall3(
                        libfolk::syscall::SYS_IPC_SEND,
                        libfolk::sys::intent::INTENT_TASK_ID as u64,
                        intent_req, 0
                    )
                };
                win.push_line("Shutting down...");
            } else if cmd_str == "help" {
                win.push_line("ls ps cat find uptime open app poweroff help");
            } else if let Some(app_name) = try_intent_match(cmd_str) {
                // M13: Semantic intent match — open app from terminal
                let mut fname = [0u8; 64];
                let nb = app_name.as_bytes();
                let ext = b".fkui";
                if nb.len() + ext.len() < 64 {
                    fname[..nb.len()].copy_from_slice(nb);
                    fname[nb.len()..nb.len()+ext.len()].copy_from_slice(ext);
                    let fname_str = unsafe {
                        core::str::from_utf8_unchecked(&fname[..nb.len()+ext.len()])
                    };
                    if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(fname_str) {
                        win.input_buf[0..4].copy_from_slice(&resp.shmem_handle.to_le_bytes());
                        win.input_buf[4] = 0xAA; // marker
                        win.push_line("Intent match: opening ");
                        win.push_line(app_name);
                    } else {
                        win.push_line("Intent matched but app not found");
                    }
                }
            } else {
                win.push_line("Unknown command. Try: help");
            }
        }
    }

    // ===== Deferred UI window creation from Shell IPC =====
    let should_create = if let Some(w) = wm.get_window_mut(win_id) {
        w.input_buf[4] == 0xAA // marker from app command
    } else {
        false
    };
    if should_create {
        let ui_handle = if let Some(w) = wm.get_window_mut(win_id) {
            w.input_buf[4] = 0; // clear marker
            u32::from_le_bytes([w.input_buf[0], w.input_buf[1], w.input_buf[2], w.input_buf[3]])
        } else { 0 };

        if ui_handle != 0 {
            if shmem_map(ui_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                let buf = unsafe {
                    core::slice::from_raw_parts(COMPOSITOR_SHMEM_VADDR as *const u8, 4096)
                };
                if let Some(header) = libfolk::ui::parse_header(buf) {
                    let wc = wm.windows.len() as i32;
                    let app_id = wm.create_terminal(
                        header.title,
                        120 + wc * 30, 100 + wc * 30,
                        header.width as u32, header.height as u32,
                    );
                    if let Some(app_win) = wm.get_window_mut(app_id) {
                        app_win.kind = compositor::window_manager::WindowKind::App;
                        app_win.owner_task = libfolk::sys::shell::SHELL_TASK_ID;
                        let (root, _) = parse_widget_tree(header.widget_data);
                        if let Some(widget) = root {
                            app_win.widgets.push(widget);
                        }
                    }
                    write_str("[WM] Created UI window: ");
                    write_str(header.title);
                    write_str("\n");
                    need_redraw = true;
                }
                let _ = shmem_unmap(ui_handle, COMPOSITOR_SHMEM_VADDR);
            }
            let _ = shmem_destroy(ui_handle);
        }
    }

    need_redraw
}
