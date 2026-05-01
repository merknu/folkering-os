//! WASM rendering layer: streaming tick-tock + fullscreen app render path.

extern crate alloc;

use libfolk::sys::io::write_str;
use libfolk::sys::{shmem_destroy, shmem_map, shmem_unmap};

use crate::util::format_usize;

use super::RenderContext;

/// Stage 1 — Streaming tick-tock pipeline.
///
/// When two WASM apps are connected via `streaming_upstream` →
/// `streaming_downstream`, run them in lockstep: upstream produces stream
/// data, downstream injects it and renders to the framebuffer.
pub(super) fn render_streaming(ctx: &mut RenderContext) {
    let config = compositor::wasm_runtime::WasmConfig {
        screen_width: ctx.fb.width as u32,
        screen_height: ctx.fb.height as u32,
        uptime_ms: libfolk::sys::uptime() as u32,
    };

    // TICK: Run upstream → produces stream data
    let stream_data = if let Some(up) = &mut ctx.wasm.streaming_upstream {
        let (_, up_output) = up.run_frame(config.clone());
        up_output.stream_data
    } else {
        alloc::vec::Vec::new()
    };

    // Inject stream data into downstream's read buffer
    if let Some(down) = &mut ctx.wasm.streaming_downstream {
        down.inject_stream_data(&stream_data);

        // TOCK: Run downstream → reads data and draws
        let (_result, output) = down.run_frame(config);

        if let Some(color) = output.fill_screen {
            ctx.fb.clear(ctx.fb.color_from_rgb24(color));
        }
        for cmd in &output.draw_commands {
            ctx.fb.fill_rect(
                cmd.x as usize, cmd.y as usize,
                cmd.w as usize, cmd.h as usize,
                ctx.fb.color_from_rgb24(cmd.color),
            );
        }
        for cmd in &output.text_commands {
            ctx.fb.draw_string(
                cmd.x as usize, cmd.y as usize, &cmd.text,
                ctx.fb.color_from_rgb24(cmd.color),
                ctx.fb.color_from_rgb24(0),
            );
        }
        for cmd in &output.circle_commands {
            let c = ctx.fb.color_from_rgb24(cmd.color);
            compositor::graphics::draw_circle(&mut *ctx.fb, cmd.cx, cmd.cy, cmd.r, c);
        }
        for cmd in &output.line_commands {
            let c = ctx.fb.color_from_rgb24(cmd.color);
            compositor::graphics::draw_line(&mut *ctx.fb, cmd.x1, cmd.y1, cmd.x2, cmd.y2, c);
        }
    }

    ctx.damage.damage_full();
}

/// Stage 2 — WASM fullscreen render path.
///
/// When a WASM app is active and `app.active == true`, it owns the
/// entire framebuffer. This handles run_frame, all draw commands,
/// surface blits, asset loading (with semantic VFS query/mime/adapt),
/// and crash recovery via Live Patching.
///
/// Returns true if any work was done.
pub(super) fn render_fullscreen_app(ctx: &mut RenderContext) -> bool {
    let mut did_work = false;

    // We need a separate scope so we can take a mut borrow of wasm.active_app
    // and still touch other ctx fields after.
    let app_active_key = ctx.wasm.active_app_key.clone();

    if let Some(app) = &mut ctx.wasm.active_app {
        if app.active {
            app.fuel_budget = compositor::wasm_runtime::FUEL_FOREGROUND;
            let config = compositor::wasm_runtime::WasmConfig {
                screen_width: ctx.fb.width as u32,
                screen_height: ctx.fb.height as u32,
                uptime_ms: libfolk::sys::uptime() as u32,
            };
            let (result, output) = app.run_frame(config);

            // Handle WASM result (fuel exhaustion, traps, success)
            match &result {
                compositor::wasm_runtime::WasmResult::OutOfFuel => {
                    ctx.wasm.fuel_fail_count = ctx.wasm.fuel_fail_count.saturating_add(1);
                    if ctx.wasm.fuel_fail_count >= 3 && ctx.mcp.immune_patching.is_none() {
                        // Live Patching: 3 consecutive fuel failures → request fix
                        app.active = false;
                        write_str("[IMMUNE] App fuel-limited 3x — requesting live patch\n");
                        if let Some(ref k) = app_active_key {
                            let desc = alloc::format!(
                                "This WASM app '{}' hits fuel limit every frame. \
                                 It has run() called per frame with 1M instruction budget. \
                                 Find the infinite loop or expensive computation and fix it. \
                                 Return ONLY the fixed Rust source code.",
                                k
                            );
                            if libfolk::mcp::client::send_wasm_gen(&desc) {
                                ctx.mcp.immune_patching = Some(k.clone());
                                write_str("[IMMUNE] Patch request sent via MCP\n");
                            } else {
                                write_str("[IMMUNE] Failed to send patch request\n");
                            }
                            let h = compositor::draug::DraugDaemon::key_hash_pub(k);
                            libfolk::sys::draug::record_crash(h as u64);
                        }
                    } else if ctx.wasm.fuel_fail_count < 3 {
                        write_str("[WASM APP] Fuel exhausted (");
                        write_str(match ctx.wasm.fuel_fail_count {
                            1 => "1/3",
                            2 => "2/3",
                            _ => "?",
                        });
                        write_str(")\n");
                    }
                }
                compositor::wasm_runtime::WasmResult::Trap(msg) => {
                    app.active = false;
                    write_str("[WASM APP] Trap: ");
                    write_str(&msg[..msg.len().min(80)]);
                    write_str("\n");
                    if let Some(ref k) = app_active_key {
                        let h = compositor::draug::DraugDaemon::key_hash_pub(k);
                        libfolk::sys::draug::record_crash(h as u64);
                    }
                }
                _ => {
                    ctx.wasm.fuel_fail_count = 0;
                }
            }

            // Render WASM output to framebuffer
            if let Some(color) = output.fill_screen {
                ctx.fb.clear(ctx.fb.color_from_rgb24(color));
            }
            for cmd in &output.draw_commands {
                ctx.fb.fill_rect(
                    cmd.x as usize, cmd.y as usize,
                    cmd.w as usize, cmd.h as usize,
                    ctx.fb.color_from_rgb24(cmd.color),
                );
            }
            for cmd in &output.line_commands {
                let c = ctx.fb.color_from_rgb24(cmd.color);
                compositor::graphics::draw_line(&mut *ctx.fb, cmd.x1, cmd.y1, cmd.x2, cmd.y2, c);
            }
            for cmd in &output.circle_commands {
                let c = ctx.fb.color_from_rgb24(cmd.color);
                compositor::graphics::draw_circle(&mut *ctx.fb, cmd.cx, cmd.cy, cmd.r, c);
            }
            for cmd in &output.text_commands {
                ctx.fb.draw_string(
                    cmd.x as usize, cmd.y as usize, &cmd.text,
                    ctx.fb.color_from_rgb24(cmd.color),
                    ctx.fb.color_from_rgb24(0),
                );
            }

            // Phase 24: Pixel blits (folk_draw_pixels)
            for blit in &output.pixel_blits {
                let bw = blit.w as usize;
                let bh = blit.h as usize;
                let bx = blit.x as usize;
                let by = blit.y as usize;
                if blit.data.len() >= bw * bh * 4 {
                    for row in 0..bh {
                        let py = by + row;
                        if py >= ctx.fb.height { break; }
                        for col in 0..bw {
                            let px = bx + col;
                            if px >= ctx.fb.width { break; }
                            let off = (row * bw + col) * 4;
                            let r = blit.data[off] as u32;
                            let g = blit.data[off + 1] as u32;
                            let b = blit.data[off + 2] as u32;
                            let color = (r << 16) | (g << 8) | b;
                            ctx.fb.set_pixel(px, py, color);
                        }
                    }
                }
            }

            // Phase 3: Surface blit
            if output.surface_dirty {
                if let Some(mem_data) = app.get_memory_slice() {
                    let surface_offset = app.surface_offset();
                    let fb_size = ctx.fb.width * ctx.fb.height * 4;
                    if surface_offset + fb_size <= mem_data.len() {
                        let surface = &mem_data[surface_offset..surface_offset + fb_size];
                        if ctx.fb.pitch == ctx.fb.width * 4 {
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    surface.as_ptr(),
                                    ctx.fb.pixel_ptr(0, 0) as *mut u8,
                                    fb_size,
                                );
                            }
                        } else {
                            for y in 0..ctx.fb.height {
                                let src_off = y * ctx.fb.width * 4;
                                unsafe {
                                    core::ptr::copy_nonoverlapping(
                                        surface[src_off..].as_ptr(),
                                        ctx.fb.pixel_ptr(0, y) as *mut u8,
                                        ctx.fb.width * 4,
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // Phase 4: Async asset loading + View Adapter pipeline
            if !output.asset_requests.is_empty() {
                handle_asset_requests(app, &output.asset_requests, ctx.mcp);
            }

            did_work = true;
        }
    }

    if did_work {
        ctx.damage.damage_full();
    }

    did_work
}

/// Process WASM asset_requests with semantic VFS resolution (query://,
/// mime://, adapt:// prefixes) and shmem-based loading.
fn handle_asset_requests(
    app: &mut compositor::wasm_runtime::PersistentWasmApp,
    requests: &[compositor::wasm_runtime::PendingAssetRequest],
    mcp: &mut compositor::state::McpState,
) {
    for req in requests {
        const VFS_ASSET_VADDR: usize = 0x50060000;

        // Semantic VFS prefix resolution
        let actual_filename = if req.filename.starts_with("query://") {
            let query = &req.filename[8..];
            match libfolk::sys::synapse::query_intent(query) {
                Ok(info) => {
                    write_str("[Synapse] query:// '");
                    write_str(&query[..query.len().min(30)]);
                    write_str("' → file_id=");
                    let mut nb3 = [0u8; 16];
                    write_str(format_usize(info.file_id as usize, &mut nb3));
                    write_str("\n");
                    alloc::format!("{}.wasm", query)
                }
                Err(_) => {
                    write_str("[Synapse] query:// '");
                    write_str(&query[..query.len().min(30)]);
                    write_str("' → not found\n");
                    req.filename.clone()
                }
            }
        } else if req.filename.starts_with("mime://") {
            let mime = &req.filename[7..];
            let mime_hash = libfolk::sys::synapse::hash_name(mime);
            let request = libfolk::sys::synapse::SYN_OP_QUERY_MIME | ((mime_hash as u64) << 32);
            let ret = unsafe {
                libfolk::syscall::syscall3(
                    libfolk::syscall::SYS_IPC_SEND,
                    libfolk::sys::synapse::SYNAPSE_TASK_ID as u64,
                    request, 0,
                )
            };
            if ret != libfolk::sys::synapse::SYN_STATUS_NOT_FOUND && ret != u64::MAX {
                let file_id = (ret & 0xFFFF) as u16;
                write_str("[Synapse] mime:// → file_id=");
                let mut nb3 = [0u8; 16];
                write_str(format_usize(file_id as usize, &mut nb3));
                write_str("\n");
            }
            req.filename.clone()
        } else if req.filename.starts_with("adapt://") {
            let parts: alloc::vec::Vec<&str> = req.filename[8..].splitn(3, '/').collect();
            if parts.len() == 3 {
                let adapter_key = alloc::format!("{}|{}", parts[0], parts[1]);
                if !mcp.adapter_cache.contains_key(&adapter_key) && mcp.pending_adapter.is_none() {
                    let prompt = compositor::wasm_runtime::adapter_generation_prompt(
                        parts[0], parts[1], "",
                    );
                    if libfolk::mcp::client::send_wasm_gen(&prompt) {
                        mcp.pending_adapter = Some(adapter_key);
                        write_str("[ViewAdapter] Generating adapter: ");
                        write_str(parts[0]);
                        write_str(" → ");
                        write_str(parts[1]);
                        write_str("\n");
                    }
                }
                alloc::string::String::from(parts[2])
            } else {
                req.filename.clone()
            }
        } else {
            req.filename.clone()
        };

        // Load file via Synapse shmem
        match libfolk::sys::synapse::read_file_shmem(&actual_filename) {
            Ok(resp) => {
                if shmem_map(resp.shmem_handle, VFS_ASSET_VADDR).is_ok() {
                    let file_data = unsafe {
                        core::slice::from_raw_parts(VFS_ASSET_VADDR as *const u8, resp.size as usize)
                    };

                    // View Adapter: try transform if adapt:// was used
                    let transformed = if req.filename.starts_with("adapt://") {
                        let parts: alloc::vec::Vec<&str> = req.filename[8..].splitn(3, '/').collect();
                        if parts.len() == 3 {
                            let adapter_key = alloc::format!("{}|{}", parts[0], parts[1]);
                            if let Some(adapter_wasm) = mcp.adapter_cache.get(&adapter_key) {
                                compositor::wasm_runtime::execute_adapter(
                                    adapter_wasm,
                                    &file_data[..resp.size as usize],
                                )
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let final_data = transformed.as_deref().unwrap_or(&file_data[..resp.size as usize]);
                    let copy_len = final_data.len().min(req.dest_len as usize);
                    app.write_memory(req.dest_ptr as usize, &final_data[..copy_len]);
                    let _ = shmem_unmap(resp.shmem_handle, VFS_ASSET_VADDR);
                    let _ = shmem_destroy(resp.shmem_handle);
                    app.push_event(compositor::wasm_runtime::FolkEvent {
                        event_type: 4,
                        x: req.handle as i32,
                        y: 0,
                        data: copy_len as i32,
                    });
                } else {
                    let _ = shmem_destroy(resp.shmem_handle);
                    app.push_event(compositor::wasm_runtime::FolkEvent {
                        event_type: 4,
                        x: req.handle as i32,
                        y: 2,
                        data: 0,
                    });
                }
            }
            Err(_) => {
                app.push_event(compositor::wasm_runtime::FolkEvent {
                    event_type: 4,
                    x: req.handle as i32,
                    y: 1,
                    data: 0,
                });
            }
        }
    }
}
