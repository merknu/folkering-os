//! Legacy omnibar command dispatch — the giant if-else for builtins.
//!
//! Handles `open`, `run`, `ls`, `ps`, `cat`, `find`, `uptime`, `help`,
//! `term`, `calc`, `save`, `revert`, `dream`, `generate driver`, `drivers`,
//! `lspci`, `https`, `dns`, `poweroff`, `ai-status`, `ask`, `agent`,
//! `gemini`, `load`. Returns the deferred shmem handle (or 0).
//!
//! This file is intentionally large — it's the verbatim command dispatch
//! body extracted from the old monolithic `command_dispatch.rs`. Further
//! per-domain splitting (apps / builtins / ai / drivers / system) is a
//! follow-up refactor (Phase C1.5).

extern crate alloc;

use libfolk::sys::io::{write_char, write_str};
use libfolk::sys::shell::{
    SHELL_OP_CAT_FILE, SHELL_OP_EXEC, SHELL_OP_LIST_FILES, SHELL_OP_OPEN_APP,
    SHELL_OP_PS, SHELL_OP_SEARCH, SHELL_STATUS_NOT_FOUND, SHELL_TASK_ID,
    hash_name as shell_hash_name,
};
use libfolk::sys::{shmem_create, shmem_destroy, shmem_grant, shmem_map, shmem_unmap, uptime};

use crate::util::*;
use crate::ui_dump::*;

use super::deferred::execute_deferred_intent;
use super::preprocess::try_semantic_intent_match;
use super::{DispatchContext, ASK_QUERY_VADDR, COMPOSITOR_SHMEM_VADDR, RING_VADDR, THINK_BUF_SIZE};

/// Run the legacy if-else dispatch chain. Returns `deferred_app_handle` (0 if none).
pub(super) fn dispatch_legacy_command(
    cmd_str: &str,
    ctx: &mut DispatchContext,
    need_redraw: &mut bool,
) -> u32 {
    let mut deferred_app_handle: u32 = 0;

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
                    wasm_fname[nb.len()..nb.len() + ext.len()].copy_from_slice(ext);
                    let wasm_str = unsafe {
                        core::str::from_utf8_unchecked(&wasm_fname[..nb.len() + ext.len()])
                    };
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
                                    screen_width: ctx.fb.width as u32,
                                    screen_height: ctx.fb.height as u32,
                                    uptime_ms: libfolk::sys::uptime() as u32,
                                };
                                match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                    Err(e) => {
                                        write_str("[WM] WASM compile failed: ");
                                        let err_bytes = e.as_bytes();
                                        let show = err_bytes.len().min(512);
                                        for &b in &err_bytes[..show] { write_char(b); }
                                        write_str("\n");
                                    }
                                    Ok(app) => {
                                        ctx.wasm.active_app = Some(app);
                                        ctx.wasm.active_app_key = Some(alloc::string::String::from(app_name));
                                        ctx.wasm.app_open_since_ms = libfolk::sys::uptime();
                                        ctx.wasm.fuel_fail_count = 0;
                                        ctx.wasm.last_bytes = Some(wasm_bytes);
                                        ctx.wasm.last_interactive = true;
                                        opened_wasm = true;
                                        write_str("[WM] Opened WASM fullscreen: ");
                                        write_str(wasm_str);
                                        write_str("\n");
                                        libfolk::sys::com3_write(b"IQE,WIN_OPEN,0\n");
                                        unsafe { libfolk::syscall::syscall3(0x9B, 0, 0, 0); }
                                    }
                                }
                            } else {
                                let _ = shmem_destroy(resp.shmem_handle);
                            }
                        }
                    }
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
                    fname[nb.len()..nb.len() + ext.len()].copy_from_slice(ext);
                    let fname_str = unsafe {
                        core::str::from_utf8_unchecked(&fname[..nb.len() + ext.len()])
                    };
                    if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(fname_str) {
                        deferred_app_handle = resp.shmem_handle;
                        vfs_loaded = true;
                        write_str("[WM] App loaded from VFS: ");
                        write_str(fname_str);
                        write_str("\n");
                    }
                }
                if !vfs_loaded {
                    let name_hash = shell_hash_name(app_name) as u64;
                    let shell_payload = SHELL_OP_OPEN_APP | (name_hash << 8);
                    let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT | (shell_payload << 8);
                    let ipc_result = unsafe {
                        libfolk::syscall::syscall3(
                            libfolk::syscall::SYS_IPC_SEND,
                            libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0,
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
                    f[nb.len()..nb.len() + ext.len()].copy_from_slice(ext);
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
                            screen_width: ctx.fb.width as u32,
                            screen_height: ctx.fb.height as u32,
                            uptime_ms: libfolk::sys::uptime() as u32,
                        };
                        match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                            Ok(app) => {
                                ctx.wasm.active_app = Some(app);
                                ctx.wasm.active_app_key = Some(alloc::string::String::from(app_name));
                                ctx.wasm.app_open_since_ms = libfolk::sys::uptime();
                                ctx.wasm.fuel_fail_count = 0;
                                ctx.wasm.last_bytes = Some(wasm_bytes);
                                ctx.wasm.last_interactive = true;
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
        // M13: semantic intent match BEFORE creating terminal window (skip for gemini)
        let handle = try_semantic_intent_match(cmd_str);
        if handle != 0 {
            deferred_app_handle = handle;
        }
    }

    if deferred_app_handle == 0 && ctx.wasm.active_app.is_none() {
        write_str("[WM] Creating window for: ");
        write_str(cmd_str);
        write_str("\n");

        // Spawn a terminal window at a cascade position
        let win_count = ctx.wm.windows.len() as i32;
        let wx = 80 + win_count * 24;
        let wy = 60 + win_count * 24;
        let win_id = ctx.wm.create_terminal(cmd_str, wx, wy, 480, 200);

        // Pre-compute UI state for gemini command (before win borrow)
        let ui_state_snapshot = {
            let ui_wins: alloc::vec::Vec<compositor::ui_serialize::WindowInfo> =
                ctx.wm.windows.iter().map(|w| {
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
                ctx.fb.width as u32, ctx.fb.height as u32, &ui_wins, "",
            )
        };

        let mut deferred_intent_action: Option<(u32, u32, u32, u32)> = None;
        if let Some(win) = ctx.wm.get_window_mut(win_id) {
            // Execute the command and populate the window
            win.push_line("> ");
            let mut title_line = [0u8; 130];
            title_line[0] = b'>';
            title_line[1] = b' ';
            let tlen = cmd_str.len().min(126);
            title_line[2..2 + tlen].copy_from_slice(&cmd_str.as_bytes()[..tlen]);
            if let Ok(s) = core::str::from_utf8(&title_line[..2 + tlen]) {
                win.push_line(s);
            }

            // ── Built-in commands routed through Intent Service ──
            if cmd_str == "ls" || cmd_str == "files" {
                win.push_line("Files in ramdisk:");
                let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT | (SHELL_OP_LIST_FILES << 8);
                let ipc_result = unsafe {
                    libfolk::syscall::syscall3(
                        libfolk::syscall::SYS_IPC_SEND,
                        libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0,
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
                                let name_end = buf[offset..offset + 24].iter()
                                    .position(|&b| b == 0).unwrap_or(24);
                                let name = unsafe {
                                    core::str::from_utf8_unchecked(&buf[offset..offset + name_end])
                                };
                                let size = u32::from_le_bytes([
                                    buf[offset + 24], buf[offset + 25],
                                    buf[offset + 26], buf[offset + 27],
                                ]) as usize;
                                let kind = u32::from_le_bytes([
                                    buf[offset + 28], buf[offset + 29],
                                    buf[offset + 30], buf[offset + 31],
                                ]);
                                let kind_str = if kind == 0 { "ELF " } else { "DATA" };
                                let mut line = [0u8; 64];
                                line[0] = b' '; line[1] = b' ';
                                line[2..6].copy_from_slice(kind_str.as_bytes());
                                line[6] = b' ';
                                let mut size_buf = [0u8; 16];
                                let size_str = format_usize(size, &mut size_buf);
                                let slen = size_str.len();
                                let pad = 8usize.saturating_sub(slen);
                                for j in 0..pad { line[7 + j] = b' '; }
                                line[7 + pad..7 + pad + slen].copy_from_slice(size_str.as_bytes());
                                line[7 + pad + slen] = b' ';
                                let nlen = name.len().min(40);
                                line[8 + pad + slen..8 + pad + slen + nlen].copy_from_slice(&name.as_bytes()[..nlen]);
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
                    line_buf[clen..clen + slen2].copy_from_slice(suffix);
                    if let Ok(s) = core::str::from_utf8(&line_buf[..clen + slen2]) {
                        win.push_line(s);
                    }
                } else {
                    win.push_line("Shell not responding");
                }
            } else if cmd_str == "ps" || cmd_str == "tasks" {
                win.push_line("Running tasks:");
                let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT | (SHELL_OP_PS << 8);
                let ipc_result = unsafe {
                    libfolk::syscall::syscall3(
                        libfolk::syscall::SYS_IPC_SEND,
                        libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0,
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
                                    buf[offset], buf[offset + 1],
                                    buf[offset + 2], buf[offset + 3],
                                ]);
                                let state = u32::from_le_bytes([
                                    buf[offset + 4], buf[offset + 5],
                                    buf[offset + 6], buf[offset + 7],
                                ]);
                                let name_end = buf[offset + 8..offset + 24].iter()
                                    .position(|&b| b == 0).unwrap_or(16);
                                let name = unsafe {
                                    core::str::from_utf8_unchecked(&buf[offset + 8..offset + 8 + name_end])
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
                                let mut line = [0u8; 64];
                                let mut pos = 0usize;
                                let prefix = b"  Task ";
                                line[..prefix.len()].copy_from_slice(prefix);
                                pos += prefix.len();
                                let mut tid_buf2 = [0u8; 16];
                                let tid_str = format_usize(tid as usize, &mut tid_buf2);
                                let tlen2 = tid_str.len();
                                line[pos..pos + tlen2].copy_from_slice(tid_str.as_bytes());
                                pos += tlen2;
                                line[pos] = b':'; pos += 1;
                                line[pos] = b' '; pos += 1;
                                let nlen = name.len().min(15);
                                if nlen > 0 {
                                    line[pos..pos + nlen].copy_from_slice(&name.as_bytes()[..nlen]);
                                    pos += nlen;
                                } else {
                                    let unk = b"<unnamed>";
                                    line[pos..pos + unk.len()].copy_from_slice(unk);
                                    pos += unk.len();
                                }
                                line[pos] = b' '; pos += 1;
                                line[pos] = b'('; pos += 1;
                                let slen = state_str.len();
                                line[pos..pos + slen].copy_from_slice(state_str.as_bytes());
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
                        let mut count_buf = [0u8; 16];
                        let count_str = format_usize(count, &mut count_buf);
                        win.push_line(count_str);
                        win.push_line("task(s) — no details available");
                    }
                } else {
                    win.push_line("Shell not responding");
                }
            } else if cmd_str.starts_with("cat ") {
                let filename = cmd_str[4..].trim();
                if filename.is_empty() {
                    win.push_line("usage: cat <filename>");
                } else {
                    let name_hash = shell_hash_name(filename) as u64;
                    let shell_payload = SHELL_OP_CAT_FILE | (name_hash << 8);
                    let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT | (shell_payload << 8);
                    let ipc_result = unsafe {
                        libfolk::syscall::syscall3(
                            libfolk::syscall::SYS_IPC_SEND,
                            libfolk::sys::intent::INTENT_TASK_ID as u64, intent_req, 0,
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
            } else if cmd_str == "heap" {
                // Kernel-heap X-ray (PR #74). Syscall 0x85 fills a
                // KernelHeapStats struct with both inner-allocator and
                // requested-bytes views, plus high-water counter.
                // Used to investigate Issue #54 (memory growth under
                // flood). Mirrors `commands::basic::cmd_heap` in shell
                // — duplicated here so COM3 god-mode injection from
                // outside the VM can drive it without going through a
                // shell IPC hop.
                if let Some(stats) = libfolk::sys::heap_walk() {
                    let mut nb = [0u8; 16];
                    let total_kib = stats.total_bytes / 1024;
                    let used_kib = stats.used_bytes / 1024;
                    let free_kib = stats.free_bytes / 1024;
                    let req_kib = stats.requested_bytes / 1024;
                    let hw_kib = stats.high_water_bytes / 1024;
                    let overhead_kib = stats.overhead_bytes() / 1024;
                    let pmille = stats.used_per_mille();
                    win.push_line(&alloc::format!(
                        "Heap v{} | total {}K used {}K ({}.{}%) free {}K",
                        stats.layout_version,
                        total_kib, used_kib, pmille / 10, pmille % 10, free_kib,
                    ));
                    win.push_line(&alloc::format!(
                        "  requested {}K | high-water {}K | overhead {}K",
                        req_kib, hw_kib, overhead_kib,
                    ));
                    win.push_line(&alloc::format!(
                        "  alloc={} dealloc={} live={}",
                        stats.alloc_count, stats.dealloc_count, stats.live_allocs(),
                    ));
                    // Also write to serial so external scrapers can
                    // grep [HEAP] tags from socat captures.
                    libfolk::sys::io::write_str("[HEAP] ");
                    libfolk::sys::io::write_str(crate::util::format_usize(used_kib as usize, &mut nb));
                    libfolk::sys::io::write_str("K used / ");
                    libfolk::sys::io::write_str(crate::util::format_usize(total_kib as usize, &mut nb));
                    libfolk::sys::io::write_str("K total / hw=");
                    libfolk::sys::io::write_str(crate::util::format_usize(hw_kib as usize, &mut nb));
                    libfolk::sys::io::write_str("K req=");
                    libfolk::sys::io::write_str(crate::util::format_usize(req_kib as usize, &mut nb));
                    libfolk::sys::io::write_str("K live=");
                    libfolk::sys::io::write_str(crate::util::format_usize(stats.live_allocs() as usize, &mut nb));
                    libfolk::sys::io::write_str("\n");
                } else {
                    win.push_line("[heap] syscall failed or layout mismatch");
                }
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
                    let query_bytes = query.as_bytes();
                    let query_len = query_bytes.len().min(63);
                    if let Ok(query_handle) = shmem_create(64) {
                        for tid in 2..=8 {
                            let _ = shmem_grant(query_handle, tid);
                        }
                        if shmem_map(query_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                            let buf = unsafe {
                                core::slice::from_raw_parts_mut(COMPOSITOR_SHMEM_VADDR as *mut u8, 64)
                            };
                            buf[..query_len].copy_from_slice(&query_bytes[..query_len]);
                            buf[query_len] = 0;
                            let _ = shmem_unmap(query_handle, COMPOSITOR_SHMEM_VADDR);
                        }
                        let shell_req = SHELL_OP_SEARCH
                            | ((query_handle as u64) << 8)
                            | ((query_len as u64) << 40);
                        let ipc_result = unsafe {
                            libfolk::syscall::syscall3(
                                libfolk::syscall::SYS_IPC_SEND,
                                SHELL_TASK_ID as u64, shell_req, 0,
                            )
                        };
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
                                match_buf[prefix.len()..prefix.len() + nlen]
                                    .copy_from_slice(num_str.as_bytes());
                                if let Ok(s) = core::str::from_utf8(&match_buf[..prefix.len() + nlen]) {
                                    win.push_line(s);
                                }
                                if shmem_map(results_handle, COMPOSITOR_SHMEM_VADDR).is_ok() {
                                    let buf = unsafe {
                                        core::slice::from_raw_parts(
                                            COMPOSITOR_SHMEM_VADDR as *const u8, count * 32,
                                        )
                                    };
                                    for i in 0..count.min(10) {
                                        let offset = i * 32;
                                        let name_end = buf[offset..offset + 24].iter()
                                            .position(|&b| b == 0).unwrap_or(24);
                                        let name = unsafe {
                                            core::str::from_utf8_unchecked(&buf[offset..offset + name_end])
                                        };
                                        let size = u32::from_le_bytes([
                                            buf[offset + 24], buf[offset + 25],
                                            buf[offset + 26], buf[offset + 27],
                                        ]) as usize;
                                        let mut line = [0u8; 64];
                                        line[0] = b' '; line[1] = b' ';
                                        let nlen2 = name.len().min(30);
                                        line[2..2 + nlen2].copy_from_slice(&name.as_bytes()[..nlen2]);
                                        let mut size_buf2 = [0u8; 16];
                                        let size_str = format_usize(size, &mut size_buf2);
                                        let slen = size_str.len();
                                        line[2 + nlen2] = b' ';
                                        line[3 + nlen2] = b'(';
                                        line[4 + nlen2..4 + nlen2 + slen].copy_from_slice(size_str.as_bytes());
                                        let suffix = b" bytes)";
                                        line[4 + nlen2 + slen..4 + nlen2 + slen + suffix.len()]
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
                win.interactive = true;
                win.push_line("Folkering OS Terminal");
                win.push_line("Type commands, Enter to run, Esc for omnibar");
            } else if cmd_str.starts_with("calc ") {
                win.push_line("Calculator: coming soon");
            } else if starts_with_ci(cmd_str, "save app ") {
                let app_name = cmd_str[9..].trim();
                if app_name.is_empty() {
                    win.push_line("Usage: save app <name>");
                } else if let Some(ref bytes) = ctx.wasm.last_bytes {
                    let filename = alloc::format!("{}.wasm", app_name);
                    match libfolk::sys::synapse::write_file(&filename, bytes) {
                        Ok(()) => {
                            win.push_line(&alloc::format!(
                                "[OS] Saved '{}' ({} bytes)", app_name, bytes.len()
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
                let args = &cmd_str[5..];
                let mut parts = args.splitn(2, ' ');
                if let (Some(filename), Some(content)) = (parts.next(), parts.next()) {
                    match libfolk::sys::synapse::write_file(filename, content.as_bytes()) {
                        Ok(()) => {
                            win.push_line("Saved to SQLite!");
                            let mut line = [0u8; 64];
                            let prefix = b"  ";
                            line[0..2].copy_from_slice(prefix);
                            let nlen = filename.len().min(30);
                            line[2..2 + nlen].copy_from_slice(&filename.as_bytes()[..nlen]);
                            let suffix = b" written";
                            let slen = suffix.len();
                            line[2 + nlen..2 + nlen + slen].copy_from_slice(suffix);
                            if let Ok(s) = core::str::from_utf8(&line[..2 + nlen + slen]) {
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
                let parts: alloc::vec::Vec<&str> = cmd_str[7..].trim().split_whitespace().collect();
                if parts.len() >= 2 {
                    let app_name = parts[0];
                    let ver_str = parts[parts.len() - 1].trim_start_matches('v');
                    if let Ok(ver) = ver_str.parse::<u32>() {
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
                ctx.briefing.accept_all();
                let accepted = ctx.briefing.drain_accepted();
                for (name, wasm_bytes) in &accepted {
                    ctx.wasm.cache.insert(name.clone(), wasm_bytes.clone());
                    win.push_line(&alloc::format!("[Dream] Accepted: {}", &name[..name.len().min(30)]));
                }
                if accepted.is_empty() {
                    win.push_line("[Dream] No pending changes");
                }
            } else if cmd_str == "dream reject all" || cmd_str == "dream reject" {
                for i in 0..ctx.briefing.items.len() {
                    ctx.briefing.reject(i);
                }
                ctx.briefing.drain_accepted();
                win.push_line("[Dream] All creative changes rejected");
            } else if cmd_str.starts_with("dream accept ") {
                if let Ok(idx) = cmd_str[13..].trim().parse::<usize>() {
                    if idx > 0 && idx <= ctx.briefing.items.len() {
                        ctx.briefing.accept(idx - 1);
                        let accepted = ctx.briefing.drain_accepted();
                        for (name, wasm_bytes) in &accepted {
                            ctx.wasm.cache.insert(name.clone(), wasm_bytes.clone());
                            win.push_line(&alloc::format!("[Dream] Accepted: {}", name));
                        }
                    } else {
                        win.push_line("[Dream] Invalid index");
                    }
                }
            } else if cmd_str.starts_with("dream reject ") {
                if let Ok(idx) = cmd_str[13..].trim().parse::<usize>() {
                    if idx > 0 && idx <= ctx.briefing.items.len() {
                        ctx.briefing.reject(idx - 1);
                        ctx.briefing.drain_accepted();
                        win.push_line("[Dream] Rejected");
                    }
                }
            } else if starts_with_ci(cmd_str, "generate driver") {
                // Autonomous Driver Generation: WASM driver for a PCI device
                let target = cmd_str.get(15..).unwrap_or("").trim();
                write_str("[DRV] target='");
                write_str(target);
                write_str("'\n");
                let mut pci_buf: [libfolk::sys::pci::PciDeviceInfo; 32] = unsafe { core::mem::zeroed() };
                let count = libfolk::sys::pci::enumerate(&mut pci_buf);
                write_str(&alloc::format!("[DRV] PCI: {} devices\n", count));

                let dev = if target.contains(':') {
                    let parts: alloc::vec::Vec<&str> = target.split(':').collect();
                    if parts.len() == 2 {
                        let vid = u16::from_str_radix(parts[0], 16).unwrap_or(0);
                        let did = u16::from_str_radix(parts[1], 16).unwrap_or(0);
                        write_str(&alloc::format!("[DRV] Looking for {:04x}:{:04x}\n", vid, did));
                        pci_buf[..count].iter().find(|d| d.vendor_id == vid && d.device_id == did)
                    } else { None }
                } else {
                    pci_buf[..count].iter().find(|d|
                        d.class_code != 0x06 && // not a bridge
                        !(d.vendor_id == 0x1AF4 && d.device_id == 0x1050) // not VirtIO GPU
                    )
                };

                if let Some(d) = dev {
                    write_str(&alloc::format!("[DRV] Found {:04x}:{:04x} ({})\n",
                        d.vendor_id, d.device_id, d.class_name()));

                    if !*ctx.drivers_seeded {
                        write_str("[DRV] First driver request — seeding bootstrap drivers\n");
                        compositor::driver_runtime::seed_bootstrap_drivers(&pci_buf, count);
                        *ctx.drivers_seeded = true;
                    }

                    let latest_v = compositor::driver_runtime::find_latest_version(
                        d.vendor_id, d.device_id);

                    if latest_v > 0 {
                        write_str(&alloc::format!("[DRV] Found v{} in Synapse VFS\n", latest_v));
                        win.push_line(&alloc::format!(
                            "[DRV] Loading {:04x}:{:04x} v{} from VFS...",
                            d.vendor_id, d.device_id, latest_v));

                        if let Some(wasm_bytes) = compositor::driver_runtime::load_driver_vfs(
                            d.vendor_id, d.device_id, latest_v
                        ) {
                            write_str(&alloc::format!("[DRV] Loaded {} bytes from VFS\n", wasm_bytes.len()));
                            let mut cap = compositor::driver_runtime::DriverCapability::from_pci(d);
                            let drv_name = alloc::format!("drv_{:04x}_{:04x}", d.vendor_id, d.device_id);
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
                                            ctx.wasm.active_drivers.push(driver);
                                        }
                                        compositor::driver_runtime::DriverResult::Completed => {
                                            write_str("[DRV] Driver completed immediately\n");
                                            win.push_line("[DRV] Driver completed");
                                        }
                                        _ => {
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
                            let desc = alloc::format!("__DRIVER_GEN__{:04x}:{:04x}:{}",
                                d.vendor_id, d.device_id, d.class_name());
                            ctx.mcp.pending_driver_device = Some(d.clone());
                            let _ = libfolk::mcp::client::send_wasm_gen(&desc);
                        }
                    } else if let Some(builtin) = compositor::driver_runtime::get_builtin_driver(
                        d.vendor_id, d.device_id
                    ) {
                        write_str(&alloc::format!("[DRV] Loading built-in driver ({} bytes)\n", builtin.len()));
                        win.push_line("[DRV] Loading built-in driver...");
                        let mut cap = compositor::driver_runtime::DriverCapability::from_pci(d);
                        let drv_name = alloc::format!("drv_{:04x}_{:04x}", d.vendor_id, d.device_id);
                        cap.set_name(&drv_name);
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
                                        ctx.wasm.active_drivers.push(driver);
                                    }
                                    compositor::driver_runtime::DriverResult::Completed => {
                                        write_str("[DRV] Built-in driver completed\n");
                                        win.push_line("[DRV] Driver completed");
                                    }
                                    compositor::driver_runtime::DriverResult::OutOfFuel => {
                                        write_str("[DRV] Built-in driver OUT OF FUEL - scheduling\n");
                                        ctx.wasm.active_drivers.push(driver);
                                    }
                                    compositor::driver_runtime::DriverResult::Trapped(ref msg) => {
                                        write_str("[DRV] Built-in TRAP: ");
                                        write_str(&msg[..msg.len().min(100)]);
                                        write_str("\n");
                                    }
                                    _ => {
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
                        write_str("[DRV] No cached driver, requesting LLM generation\n");
                        win.push_line(&alloc::format!(
                            "[DRV] Generating driver for {:04x}:{:04x} ({})...",
                            d.vendor_id, d.device_id, d.class_name()));
                        ctx.mcp.pending_driver_device = Some(d.clone());
                        let desc = alloc::format!("__DRIVER_GEN__{:04x}:{:04x}:{}",
                            d.vendor_id, d.device_id, d.class_name());
                        if libfolk::mcp::client::send_wasm_gen(&desc) {
                            write_str("[DRV] MCP WasmGenRequest sent\n");
                            win.push_line("[DRV] Request sent to LLM");
                        } else {
                            write_str("[DRV] MCP send FAILED\n");
                            win.push_line("[DRV] MCP send failed");
                            ctx.mcp.pending_driver_device = None;
                        }
                    }
                } else {
                    write_str("[DRV] No matching device found\n");
                    win.push_line("[DRV] No matching PCI device found");
                    win.push_line("[DRV] Usage: generate driver [vendor:device]");
                    win.push_line("[DRV] Example: generate driver 1af4:1042");
                }
            } else if cmd_str == "drivers" {
                win.push_line(&alloc::format!("[DRV] {} active drivers:", ctx.wasm.active_drivers.len()));
                for drv in ctx.wasm.active_drivers.iter() {
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
                                let status = if ctx.wasm.active_drivers.iter().any(|d|
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
                            ctx.wasm.active_drivers.retain(|d|
                                !(d.capability.vendor_id == vid && d.capability.device_id == did));
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
                                                ctx.wasm.active_drivers.push(driver);
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
                let target = if cmd_str.len() > 6 { cmd_str.get(6..).unwrap_or("example.com").trim() }
                             else { "example.com" };
                write_str("[HTTPS] Step 1: DNS lookup for ");
                write_str(target);
                write_str("...\n");
                win.push_line(&alloc::format!("[HTTPS] Looking up {}...", target));
                match libfolk::sys::dns::lookup(target) {
                    Some(ip) => {
                        let msg = alloc::format!("[HTTPS] {} -> {}.{}.{}.{}", target, ip.0, ip.1, ip.2, ip.3);
                        write_str(&msg);
                        write_str("\n");
                        win.push_line(&msg);
                        win.push_line("[HTTPS] Starting TLS 1.3 handshake...");
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
                let hostname = cmd_str.get(4..).unwrap_or("").trim();
                if hostname.is_empty() {
                    win.push_line("[DNS] Usage: dns example.com");
                } else {
                    write_str("[DNS] Looking up: ");
                    write_str(hostname);
                    write_str("\n");
                    win.push_line(&alloc::format!("[DNS] Looking up {}...", hostname));
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
                let name_hash = shell_hash_name("poweroff") as u64;
                let shell_payload = SHELL_OP_EXEC | (name_hash << 8);
                let intent_req = libfolk::sys::intent::INTENT_OP_SUBMIT | (shell_payload << 8);
                let _ = unsafe {
                    libfolk::syscall::syscall3(
                        libfolk::syscall::SYS_IPC_SEND,
                        libfolk::sys::intent::INTENT_TASK_ID as u64,
                        intent_req, 0,
                    )
                };
                win.push_line("Shutting down...");
            } else if cmd_str == "ai-status" {
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
                use libfolk::sys::inference;
                let query = if cmd_str.starts_with("ask ") {
                    &cmd_str[4..]
                } else {
                    &cmd_str[6..]
                };
                let query = query.trim();
                if query.is_empty() {
                    win.push_line("Usage: ask <question>");
                } else if ctx.stream.ring_handle != 0 {
                    win.push_line("[AI is busy]");
                } else {
                    match inference::ping() {
                        Ok(has_model) => {
                            if !has_model {
                                win.push_line("[AI] No model loaded (stub mode)");
                            } else {
                                let _ring_ok = if let Ok(rh) = shmem_create(16384) {
                                    let _ = shmem_grant(rh, inference::inference_task_id());
                                    let query_bytes = query.as_bytes();
                                    if let Ok(qh) = shmem_create(4096) {
                                        let _ = shmem_grant(qh, inference::inference_task_id());
                                        if shmem_map(qh, ASK_QUERY_VADDR).is_ok() {
                                            unsafe {
                                                let ptr = ASK_QUERY_VADDR as *mut u8;
                                                core::ptr::copy_nonoverlapping(
                                                    query_bytes.as_ptr(), ptr, query_bytes.len(),
                                                );
                                            }
                                            let _ = shmem_unmap(qh, ASK_QUERY_VADDR);
                                            match inference::ask_async(qh, query_bytes.len(), rh) {
                                                Ok(()) => {
                                                    if shmem_map(rh, RING_VADDR).is_ok() {
                                                        ctx.stream.ring_handle = rh;
                                                        ctx.stream.ring_read_idx = 0;
                                                        ctx.stream.win_id = win_id;
                                                        ctx.stream.query_handle = qh;
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
                                            ctx.wasm.last_bytes = Some(wasm_bytes.clone());
                                            ctx.wasm.last_interactive = true;
                                            let config = compositor::wasm_runtime::WasmConfig {
                                                screen_width: ctx.fb.width as u32,
                                                screen_height: ctx.fb.height as u32,
                                                uptime_ms: libfolk::sys::uptime() as u32,
                                            };
                                            match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
                                                Ok(app) => {
                                                    win.visible = false;
                                                    ctx.wasm.active_app = Some(app);
                                                    ctx.wasm.active_app_key = Some(alloc::string::String::from(path));
                                                    ctx.wasm.app_open_since_ms = libfolk::sys::uptime();
                                                    ctx.wasm.fuel_fail_count = 0;
                                                }
                                                Err(e) => { win.push_line(&alloc::format!("[OS] Error: {}", &e[..e.len().min(60)])); }
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
            } else if cmd_str.starts_with("agent ") {
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
                    if !flags.force {
                        if let Some(cached_wasm) = ctx.wasm.cache.get(prompt) {
                            win.push_line(&alloc::format!("[Cache] Hit: {} bytes", cached_wasm.len()));
                            ctx.wasm.last_bytes = Some(cached_wasm.clone());
                            let config = compositor::wasm_runtime::WasmConfig {
                                screen_width: ctx.fb.width as u32,
                                screen_height: ctx.fb.height as u32,
                                uptime_ms: libfolk::sys::uptime() as u32,
                            };
                            let (result, output) = compositor::wasm_runtime::execute_wasm(cached_wasm, config);
                            if let compositor::wasm_runtime::WasmResult::Ok = &result {
                                win.push_line("[Cache] Executed from cache (instant)");
                            }
                            if let Some(color) = output.fill_screen {
                                ctx.fb.clear(ctx.fb.color_from_rgb24(color));
                            }
                            for cmd in &output.draw_commands {
                                ctx.fb.fill_rect(cmd.x as usize, cmd.y as usize, cmd.w as usize, cmd.h as usize, ctx.fb.color_from_rgb24(cmd.color));
                            }
                            ctx.damage.damage_full();
                            *need_redraw = true;
                        } else {
                            win.push_line(&alloc::format!("[Agent] Task: {}", &prompt[..prompt.len().min(60)]));
                            let mut session = compositor::agent::AgentSession::new(prompt, win_id);
                            if session.start() {
                                write_str("[AGENT] Session started\n");
                                win.push_line("[Agent] Thinking...");
                                *ctx.active_agent = Some(session);
                            } else {
                                win.push_line("[Agent] Error: failed to start");
                            }
                        }
                    } else {
                        win.push_line(&alloc::format!("[Agent] Task (forced): {}", &prompt[..prompt.len().min(50)]));
                        let mut session = compositor::agent::AgentSession::new(prompt, win_id);
                        if session.start() {
                            write_str("[AGENT] Session started (forced)\n");
                            win.push_line("[Agent] Thinking...");
                            *ctx.active_agent = Some(session);
                        } else {
                            win.push_line("[Agent] Error: failed to start");
                        }
                    }
                }
            } else if cmd_str.starts_with("gemini ") {
                let prompt = cmd_str[7..].trim();
                write_str("[COMPOSITOR] gemini command: ");
                write_str(prompt);
                write_str("\n");
                if prompt.is_empty() {
                    win.push_line("Usage: gemini <prompt>");
                } else if starts_with_ci(prompt, "generate ") {
                    let tool_prompt = prompt[9..].trim();
                    win.push_line(&alloc::format!("[AI] Generating tool: {}...", &tool_prompt[..tool_prompt.len().min(50)]));
                    ctx.mcp.deferred_tool_gen = Some((win_id, alloc::string::String::from(tool_prompt)));
                    ctx.damage.damage_full();
                } else {
                    win.push_line(&alloc::format!("> gemini {}", &prompt[..prompt.len().min(60)]));
                    let full_prompt = alloc::format!(
                        "You are Folkering OS AI assistant. Current screen state:\n{}\nUser command: {}\n\nYou MUST respond with ONLY a JSON object. Choose one:\n{{\"action\": \"move_window\", \"window_id\": N, \"x\": N, \"y\": N}}\n{{\"action\": \"close_window\", \"window_id\": N}}\n{{\"action\": \"generate_tool\", \"prompt\": \"description\"}}\n{{\"action\": \"text\", \"content\": \"your answer\"}}\nNEVER respond with plain text. ALWAYS use JSON.",
                        ui_state_snapshot, prompt
                    );
                    win.push_line("[cloud] Sending with UI context...");
                    const GEMINI_CMD_VADDR: usize = 0x50000000;
                    const GEMINI_CMD_SIZE: usize = 131072;
                    let response_len = if libfolk::sys::mmap_at(GEMINI_CMD_VADDR, GEMINI_CMD_SIZE, 3).is_ok() {
                        let gemini_buf = unsafe {
                            core::slice::from_raw_parts_mut(GEMINI_CMD_VADDR as *mut u8, GEMINI_CMD_SIZE)
                        };
                        libfolk::sys::ask_gemini(&full_prompt, gemini_buf)
                    } else { 0 };
                    let gemini_buf = unsafe {
                        core::slice::from_raw_parts(GEMINI_CMD_VADDR as *const u8, GEMINI_CMD_SIZE)
                    };
                    if response_len > 0 {
                        if let Ok(text) = alloc::str::from_utf8(&gemini_buf[..response_len]) {
                            use compositor::intent::AgentIntent;
                            let intent = compositor::intent::parse_intent(text);
                            write_str("[COMPOSITOR] Intent parsed\n");
                            match intent {
                                AgentIntent::MoveWindow { window_id, x, y } => {
                                    win.push_line(&alloc::format!(
                                        "[AI] Moving window {} to ({},{})", window_id, x, y
                                    ));
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
                                    ctx.mcp.deferred_tool_gen = Some((win_id, tp));
                                    ctx.damage.damage_full();
                                }
                                AgentIntent::TextResponse { text: resp } => {
                                    let mut visible = alloc::string::String::new();
                                    let mut in_think = false;
                                    let mut rest = resp.as_str();
                                    while !rest.is_empty() {
                                        if !in_think {
                                            if let Some(pos) = rest.find("<think>") {
                                                visible.push_str(&rest[..pos]);
                                                rest = &rest[pos + 7..];
                                                in_think = true;
                                                ctx.stream.think_active = true;
                                                ctx.stream.think_display_len = 0;
                                            } else {
                                                visible.push_str(rest);
                                                break;
                                            }
                                        } else {
                                            if let Some(pos) = rest.find("</think>") {
                                                let think_text = &rest[..pos];
                                                let copy_len = think_text.len().min(THINK_BUF_SIZE - ctx.stream.think_display_len);
                                                ctx.stream.think_display[ctx.stream.think_display_len..ctx.stream.think_display_len + copy_len]
                                                    .copy_from_slice(&think_text.as_bytes()[..copy_len]);
                                                ctx.stream.think_display_len += copy_len;
                                                ctx.stream.think_active = false;
                                                ctx.stream.think_fade_timer = 180;
                                                *need_redraw = true;
                                                rest = &rest[pos + 8..];
                                                in_think = false;
                                            } else {
                                                let copy_len = rest.len().min(THINK_BUF_SIZE - ctx.stream.think_display_len);
                                                ctx.stream.think_display[ctx.stream.think_display_len..ctx.stream.think_display_len + copy_len]
                                                    .copy_from_slice(&rest.as_bytes()[..copy_len]);
                                                ctx.stream.think_display_len += copy_len;
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
        if let Some(action) = deferred_intent_action {
            execute_deferred_intent(action, ctx);
        }
    }

    deferred_app_handle
}
