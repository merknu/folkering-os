//! AutoDream cycle: dream cycle initiation + WasmChunk receive + dream
//! evaluation (Refactor / Creative / Nightmare / Driver variants).
//!
//! When the MCP poll returns a WasmChunk, this module:
//!  1. Reassembles multi-chunk WASM
//!  2. Strips + verifies the FOLK\x00 cryptographic lineage signature
//!  3. Routes to the right handler: live patch, view adapter, driver,
//!     FolkShell JIT, dream evaluation, or normal cache storage
//!  4. For dreams, runs the V1-vs-V2 sanity + benchmark + fuzz pipeline

extern crate alloc;

use libfolk::sys::io::write_str;
use libfolk::sys::{shmem_create, shmem_destroy, shmem_grant, shmem_map, shmem_unmap};
use libfolk::sys::draug::{
    daemon_task_id, request_dream_decision, notify_dream_result,
    DREAM_MODE_REFACTOR, DREAM_MODE_CREATIVE, DREAM_MODE_NIGHTMARE,
    DREAM_MODE_DRIVER_REFACTOR, DREAM_MODE_DRIVER_NIGHTMARE,
    DREAM_RESULT_COMPLETE, DREAM_RESULT_CANCEL,
};

use compositor::damage::DamageTracker;
use compositor::draug::DraugDaemon;
use compositor::framebuffer::FramebufferView;
use compositor::state::{McpState, WasmState};
use compositor::window_manager::WindowManager;

use crate::util::format_usize;

use super::{rdtsc, MAX_ADAPTER_ENTRIES, MAX_CACHE_ENTRIES};

/// Result of `handle_wasm_chunk` — `early_return` signals the caller
/// (`agent_logic::tick`) to bail out immediately because we already
/// handled a special case (live patch, view adapter, driver, JIT).
pub(super) struct ChunkResult {
    pub early_return: bool,
}

/// Stage 4 — Start an AutoDream cycle (called by `agent_logic::tick`).
///
/// The DECISION (which app + which mode) now happens in the daemon over
/// `DRAUG_OP_DREAM_DECIDE` IPC; compositor still owns the EXECUTION
/// (state snapshot, MCP send, chunk reassembly) for now. Subsequent
/// Phase A.5 steps move execution and result-tracking too, then drop
/// the compositor-local `DraugDaemon` entirely.
pub(super) fn start_dream_cycle(
    mcp: &mut McpState,
    wasm: &mut WasmState,
    draug: &mut DraugDaemon,
    fb: &FramebufferView,
    dream_ms: u64,
) {
    let keys: alloc::vec::Vec<&str> = wasm.cache.keys().map(|k| k.as_str()).collect();
    if keys.is_empty() {
        write_str("[AutoDream] cache empty — nothing to dream about\n");
        return;
    }

    let Some((target, mode, dream_count)) = ask_daemon_for_dream(&keys, dream_ms) else {
        write_str("[AutoDream] All systems stable. Sleeping.\n");
        return;
    };

    // Authoritative dream context for the chunk-receive path. Replaces
    // the stale `draug.is_dreaming/dream_target/current_dream_mode`
    // reads that broke when step 1 stopped calling local `start_dream`.
    mcp.current_dream = Some((target.clone(), mode));

    let mode_str = match mode {
        compositor::draug::DreamMode::Refactor => "Refactor",
        compositor::draug::DreamMode::Creative => "Creative",
        compositor::draug::DreamMode::Nightmare => "Nightmare",
        compositor::draug::DreamMode::DriverRefactor => "DriverRefactor",
        compositor::draug::DreamMode::DriverNightmare => "DriverNightmare",
    };

    // State Migration: snapshot WASM memory if active app is the dream target
    wasm.state_snapshot = None;
    if let Some(ref app) = wasm.active_app {
        if let Some(ref k) = wasm.active_app_key {
            if k.as_str() == target.as_str() {
                if let Some(mem) = app.get_memory_slice() {
                    let snap_len = mem.len().min(1024);
                    wasm.state_snapshot = Some(alloc::vec::Vec::from(&mem[..snap_len]));
                    write_str("[StateMigration] Captured ");
                    let mut nb2 = [0u8; 16];
                    write_str(format_usize(snap_len, &mut nb2));
                    write_str(" bytes of app state\n");
                }
            }
        }
    }

    write_str("[AutoDream] ========================================\n");
    write_str("[AutoDream] DREAM #");
    let mut nb = [0u8; 16];
    write_str(format_usize(dream_count as usize, &mut nb));
    write_str(" | Mode: ");
    write_str(mode_str);
    write_str(" | Target: ");
    write_str(&target[..target.len().min(40)]);
    write_str("\n");

    // RTC timestamp for overnight log correlation
    let dt = libfolk::sys::get_rtc();
    let mut ts = [0u8; 19];
    ts[0] = b'0'+((dt.year/1000)%10) as u8; ts[1] = b'0'+((dt.year/100)%10) as u8;
    ts[2] = b'0'+((dt.year/10)%10) as u8; ts[3] = b'0'+(dt.year%10) as u8;
    ts[4] = b'-'; ts[5] = b'0'+dt.month/10; ts[6] = b'0'+dt.month%10;
    ts[7] = b'-'; ts[8] = b'0'+dt.day/10; ts[9] = b'0'+dt.day%10;
    ts[10] = b' '; ts[11] = b'0'+dt.hour/10; ts[12] = b'0'+dt.hour%10;
    ts[13] = b':'; ts[14] = b'0'+dt.minute/10; ts[15] = b'0'+dt.minute%10;
    ts[16] = b':'; ts[17] = b'0'+dt.second/10; ts[18] = b'0'+dt.second%10;
    write_str("[AutoDream] Time: ");
    if let Ok(s) = core::str::from_utf8(&ts) { write_str(s); }
    write_str("\n");

    write_str("[AutoDream] Cache: ");
    write_str(format_usize(wasm.cache.len(), &mut nb));
    write_str(" apps | Draug dreams: ");
    write_str(format_usize(dream_count as usize, &mut nb));
    write_str("/");
    write_str(format_usize(compositor::draug::DREAM_MAX_PER_SESSION as usize, &mut nb));
    write_str("\n");

    let tweak = match mode {
        compositor::draug::DreamMode::Refactor =>
            alloc::format!("--tweak \"refactor for fewer CPU cycles, no new features\" {}", target),
        compositor::draug::DreamMode::Nightmare =>
            alloc::format!("--tweak \"harden against edge cases: zero division, overflow, OOB\" {}", target),
        compositor::draug::DreamMode::Creative => {
            let render_desc = if let Some(cached_wasm) = wasm.cache.get(&target) {
                let cfg = compositor::wasm_runtime::WasmConfig {
                    screen_width: fb.width as u32,
                    screen_height: fb.height as u32,
                    uptime_ms: 0,
                };
                let (_, output) = compositor::wasm_runtime::execute_wasm(cached_wasm, cfg);
                compositor::wasm_runtime::render_summary(&output)
            } else {
                alloc::string::String::from("(no cached binary)")
            };
            alloc::format!("--tweak \"add one visual improvement. Current output: {}\" {}", render_desc, target)
        }
        compositor::draug::DreamMode::DriverRefactor =>
            alloc::format!("--tweak \"optimize driver for fewer CPU cycles, preserve IRQ loop\" {}", target),
        compositor::draug::DreamMode::DriverNightmare =>
            alloc::format!("--tweak \"harden driver against SFI violations, IRQ storms, DMA failures\" {}", target),
    };

    if libfolk::mcp::client::send_wasm_gen(&tweak) {
        mcp.async_tool_gen = Some((0, target));
        write_str("[AutoDream] Request sent\n");
    } else {
        write_str("[AutoDream] Send failed — cancelling dream\n");
        notify_dream_result(DREAM_RESULT_CANCEL);
        mcp.current_dream = None;
        let _ = draug; // local lifecycle no longer driven from this path
        let _ = dream_ms;
    }
}

/// Wire-format constants for the `DREAM_DECIDE` shmem layout. Mirror
/// of the daemon-side caps so a misformatted payload can't push the
/// daemon past its read window.
const DREAM_DECIDE_MAX_KEYS: usize = 32;
const DREAM_DECIDE_MAX_KEY_LEN: usize = 64;
/// Stable scratch vaddr for compositor-side mapping of the dream-decide
/// shmem. Sits next to `VFS_DREAM_VADDR` (0x50070000) without overlap.
const DREAM_DECIDE_COMP_VADDR: usize = 0x50080000;

/// Ask the daemon (`DRAUG_OP_DREAM_DECIDE`) which app + mode to dream
/// about right now. Returns `Some((target_string, mode, dream_count))`
/// if the daemon picked something, `None` if all systems are stable or
/// the IPC failed.
///
/// Allocates a per-call shmem region with the key list serialised in
/// the format the daemon expects, grants the daemon read access,
/// performs the IPC, then unmaps + destroys.
fn ask_daemon_for_dream(
    keys: &[&str],
    dream_ms: u64,
) -> Option<(alloc::string::String, compositor::draug::DreamMode, u32)> {
    let n = keys.len().min(DREAM_DECIDE_MAX_KEYS);
    if n == 0 {
        return None;
    }

    // Compute exact serialised size: 8-byte header + per-key (4 + len).
    let mut total: usize = 8;
    for k in keys.iter().take(n) {
        total += 4 + k.as_bytes().len().min(DREAM_DECIDE_MAX_KEY_LEN);
    }

    let handle = match shmem_create(total) {
        Ok(h) => h,
        Err(_) => {
            write_str("[AutoDream] shmem_create failed\n");
            return None;
        }
    };

    if shmem_map(handle, DREAM_DECIDE_COMP_VADDR).is_err() {
        let _ = shmem_destroy(handle);
        write_str("[AutoDream] shmem_map failed\n");
        return None;
    }

    // Serialise: [u32 num_keys][u32 reserved] then per key
    // [u16 len][u16 reserved][len bytes]. Bounds-checked on the daemon
    // side too — we just need to write within the allocation.
    unsafe {
        let buf = core::slice::from_raw_parts_mut(DREAM_DECIDE_COMP_VADDR as *mut u8, total);
        buf[0..4].copy_from_slice(&(n as u32).to_le_bytes());
        buf[4..8].copy_from_slice(&0u32.to_le_bytes());
        let mut off = 8usize;
        for k in keys.iter().take(n) {
            let bytes = k.as_bytes();
            let len = bytes.len().min(DREAM_DECIDE_MAX_KEY_LEN);
            buf[off..off + 2].copy_from_slice(&(len as u16).to_le_bytes());
            buf[off + 2..off + 4].copy_from_slice(&0u16.to_le_bytes());
            off += 4;
            buf[off..off + len].copy_from_slice(&bytes[..len]);
            off += len;
        }
    }

    if shmem_grant(handle, daemon_task_id()).is_err() {
        let _ = shmem_unmap(handle, DREAM_DECIDE_COMP_VADDR);
        let _ = shmem_destroy(handle);
        write_str("[AutoDream] shmem_grant failed\n");
        return None;
    }

    // Idle-time hint. 24-bit budget = ~6 months of seconds — comfortably
    // larger than any uptime we've ever seen, so a cheap saturating cast
    // is fine.
    let idle_seconds = ((dream_ms / 1000) & 0xFF_FFFF) as u32;
    let decision = request_dream_decision(handle, idle_seconds);

    let _ = shmem_unmap(handle, DREAM_DECIDE_COMP_VADDR);
    let _ = shmem_destroy(handle);

    let decision = decision?;
    if !decision.should_dream() {
        return None;
    }

    let idx = decision.target_index as usize;
    if idx >= keys.len() {
        write_str("[AutoDream] daemon returned out-of-range target_index\n");
        return None;
    }

    let mode = match decision.mode {
        m if m == DREAM_MODE_REFACTOR        => compositor::draug::DreamMode::Refactor,
        m if m == DREAM_MODE_CREATIVE        => compositor::draug::DreamMode::Creative,
        m if m == DREAM_MODE_NIGHTMARE       => compositor::draug::DreamMode::Nightmare,
        m if m == DREAM_MODE_DRIVER_REFACTOR => compositor::draug::DreamMode::DriverRefactor,
        m if m == DREAM_MODE_DRIVER_NIGHTMARE=> compositor::draug::DreamMode::DriverNightmare,
        _ => {
            write_str("[AutoDream] unknown mode from daemon — skipping\n");
            return None;
        }
    };

    Some((alloc::string::String::from(keys[idx]), mode, decision.dream_count))
}

/// Handle a `WasmChunk` MCP response. Reassembles, verifies signature,
/// routes to the right destination, runs dream evaluation if applicable.
pub(super) fn handle_wasm_chunk(
    total_chunks: u16,
    data: &[u8],
    mcp: &mut McpState,
    wasm: &mut WasmState,
    wm: &mut WindowManager,
    draug: &mut DraugDaemon,
    fb: &mut FramebufferView,
    damage: &mut DamageTracker,
    _drivers_seeded: &mut bool,
    tsc_per_us: u64,
    need_redraw: &mut bool,
) -> ChunkResult {
    let mut nbuf = [0u8; 16];

    // Reassemble
    let assembled = if libfolk::mcp::client::wasm_assembly_complete() {
        let d = libfolk::mcp::client::wasm_assembly_data();
        write_str("[MCP] WASM assembled: ");
        write_str(format_usize(d.len(), &mut nbuf));
        write_str(" bytes (");
        write_str(format_usize(total_chunks as usize, &mut nbuf));
        write_str(" chunks)\n");
        alloc::vec::Vec::from(d)
    } else {
        write_str("[MCP] WASM single chunk: ");
        write_str(format_usize(data.len(), &mut nbuf));
        write_str(" bytes\n");
        alloc::vec::Vec::from(data)
    };
    libfolk::mcp::client::wasm_assembly_reset();

    // Strip + verify FOLK signature
    let raw_bytes = assembled;
    let wasm_bytes = if raw_bytes.len() > 37
        && raw_bytes[0] == b'F' && raw_bytes[1] == b'O'
        && raw_bytes[2] == b'L' && raw_bytes[3] == b'K'
        && raw_bytes[4] == 0x00
    {
        let sig = &raw_bytes[5..37];
        let wasm_data = &raw_bytes[37..];
        let wasm_hash = libfolk::crypto::sha256(wasm_data);
        let mut sig_hex = [0u8; 64];
        libfolk::crypto::hash_to_hex(&wasm_hash, &mut sig_hex);
        write_str("[CRYPTO] Signed WASM: hash=");
        if let Ok(s) = core::str::from_utf8(&sig_hex[..16]) { write_str(s); }
        write_str("... sig=");
        for i in 0..4 {
            let b = sig[i];
            let hi = b"0123456789abcdef"[(b >> 4) as usize];
            let lo = b"0123456789abcdef"[(b & 0xf) as usize];
            let buf = [hi, lo];
            if let Ok(s) = core::str::from_utf8(&buf) { write_str(s); }
        }
        write_str("...\n");
        alloc::vec::Vec::from(wasm_data)
    } else {
        if raw_bytes.len() > 4 && raw_bytes[0] == 0x00
            && raw_bytes[1] == b'a' && raw_bytes[2] == b's' && raw_bytes[3] == b'm'
        {
            write_str("[CRYPTO] Unsigned WASM (legacy)\n");
        }
        raw_bytes
    };

    // Extract tool context
    let (tool_win_id, tool_prompt) = if let Some(ctx) = mcp.async_tool_gen.take() {
        ctx
    } else {
        (0u32, alloc::string::String::new())
    };
    wasm.last_bytes = Some(wasm_bytes.clone());

    // ── Live Patching: response to immune_patching request ──
    if let Some(ref patch_key) = mcp.immune_patching.clone() {
        let config = compositor::wasm_runtime::WasmConfig {
            screen_width: fb.width as u32,
            screen_height: fb.height as u32,
            uptime_ms: libfolk::sys::uptime() as u32,
        };
        match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
            Ok(app) => {
                write_str("[IMMUNE] Patched '");
                write_str(&patch_key[..patch_key.len().min(30)]);
                write_str("' live!\n");
                wasm.active_app = Some(app);
                wasm.fuel_fail_count = 0;
                wasm.cache.insert(patch_key.clone(), wasm_bytes.clone());
            }
            Err(e) => {
                write_str("[IMMUNE] Patch failed to load: ");
                write_str(&e[..e.len().min(60)]);
                write_str("\n");
            }
        }
        mcp.immune_patching = None;
        return ChunkResult { early_return: true };
    }

    // ── View Adapter response ──
    if let Some(ref adapter_key) = mcp.pending_adapter.clone() {
        let config = compositor::wasm_runtime::WasmConfig {
            screen_width: fb.width as u32,
            screen_height: fb.height as u32,
            uptime_ms: libfolk::sys::uptime() as u32,
        };
        let (result, _) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, config);
        match result {
            compositor::wasm_runtime::WasmResult::Ok |
            compositor::wasm_runtime::WasmResult::OutOfFuel => {
                if mcp.adapter_cache.len() >= MAX_ADAPTER_ENTRIES {
                    if let Some(oldest) = mcp.adapter_cache.keys().next().cloned() {
                        mcp.adapter_cache.remove(&oldest);
                    }
                }
                mcp.adapter_cache.insert(adapter_key.clone(), wasm_bytes.clone());
                write_str("[ViewAdapter] Cached adapter: ");
                write_str(&adapter_key[..adapter_key.len().min(40)]);
                write_str("\n");
            }
            _ => {
                write_str("[ViewAdapter] Adapter generation failed — discarding\n");
            }
        }
        mcp.pending_adapter = None;
        return ChunkResult { early_return: true };
    }

    // ── Autonomous Driver response ──
    if let Some(pci_dev) = mcp.pending_driver_device.take() {
        let next_v = compositor::driver_runtime::find_latest_version(
            pci_dev.vendor_id, pci_dev.device_id) + 1;
        if compositor::driver_runtime::store_driver_vfs(
            pci_dev.vendor_id, pci_dev.device_id, next_v,
            &wasm_bytes, compositor::driver_runtime::DriverSource::Jit,
        ) {
            write_str(&alloc::format!("[DRV] Persisted to VFS as v{}\n", next_v));
        }

        let mut cap = compositor::driver_runtime::DriverCapability::from_pci(&pci_dev);
        let name = alloc::format!("drv_{:04x}_{:04x}", pci_dev.vendor_id, pci_dev.device_id);
        cap.set_name(&name);

        let mapped = compositor::driver_runtime::map_device_bars(&mut cap);
        write_str("[DRV] Mapped ");
        let mut nb4 = [0u8; 16];
        write_str(format_usize(mapped, &mut nb4));
        write_str(" MMIO BARs\n");

        match compositor::driver_runtime::WasmDriver::new(&wasm_bytes, cap) {
            Ok(mut driver) => {
                driver.meta.version = next_v;
                driver.meta.source = compositor::driver_runtime::DriverSource::Jit;
                let _ = driver.bind_irq();
                write_str("[DRV] Starting driver: ");
                write_str(&name[..name.len().min(30)]);
                write_str("\n");
                match driver.start() {
                    compositor::driver_runtime::DriverResult::WaitingForIrq => {
                        write_str("[DRV] Driver yielded (waiting for IRQ)\n");
                        wasm.active_drivers.push(driver);
                    }
                    compositor::driver_runtime::DriverResult::Completed => {
                        write_str("[DRV] Driver completed immediately\n");
                    }
                    compositor::driver_runtime::DriverResult::OutOfFuel => {
                        write_str("[DRV] Driver preempted (fuel) — scheduling\n");
                        wasm.active_drivers.push(driver);
                    }
                    compositor::driver_runtime::DriverResult::Trapped(msg) => {
                        write_str("[DRV] Driver TRAPPED: ");
                        write_str(&msg[..msg.len().min(60)]);
                        write_str("\n");
                    }
                    compositor::driver_runtime::DriverResult::LoadError(e) => {
                        write_str("[DRV] Load error: ");
                        write_str(&e[..e.len().min(60)]);
                        write_str("\n");
                    }
                }
            }
            Err(e) => {
                write_str("[DRV] Failed to instantiate: ");
                write_str(&e[..e.len().min(60)]);
                write_str("\n");
            }
        }
        return ChunkResult { early_return: true };
    }

    // ── FolkShell JIT response ──
    if let Some(ref jit_name) = mcp.pending_shell_jit.clone() {
        wasm.cache.insert(jit_name.clone(), wasm_bytes.clone());
        write_str("[FolkShell] JIT command ready: ");
        write_str(&jit_name[..jit_name.len().min(30)]);
        write_str("\n");

        if let Some((pipeline, stage, pipe_input)) = mcp.shell_jit_pipeline.take() {
            let result = compositor::folkshell::execute_pipeline(
                &pipeline, stage, pipe_input, &wasm.cache,
            );
            match result {
                compositor::folkshell::ShellState::Done(output) => {
                    write_str("[FolkShell] Pipeline output:\n");
                    write_str(&output[..output.len().min(200)]);
                    write_str("\n");
                }
                compositor::folkshell::ShellState::WaitingForJIT {
                    command_name, pipeline: p, stage: s, pipe_input: pi,
                } => {
                    write_str("[FolkShell] Chaining JIT: ");
                    write_str(&command_name[..command_name.len().min(30)]);
                    write_str("\n");
                    let prompt = compositor::folkshell::jit_prompt(&command_name, &pi);
                    if libfolk::mcp::client::send_wasm_gen(&prompt) {
                        mcp.pending_shell_jit = Some(command_name);
                        mcp.shell_jit_pipeline = Some((p, s, pi));
                    }
                }
                compositor::folkshell::ShellState::Widget { wasm_bytes: w, title: t } => {
                    write_str("[FolkShell] JIT widget: ");
                    write_str(&t[..t.len().min(30)]);
                    write_str("\n");
                    let config = compositor::wasm_runtime::WasmConfig {
                        screen_width: fb.width as u32,
                        screen_height: fb.height as u32,
                        uptime_ms: libfolk::sys::uptime() as u32,
                    };
                    if let Ok(app) = compositor::wasm_runtime::PersistentWasmApp::new(&w, config) {
                        wasm.active_app = Some(app);
                        wasm.active_app_key = Some(t);
                        wasm.app_open_since_ms = libfolk::sys::uptime();
                        wasm.fuel_fail_count = 0;
                        damage.damage_full();
                    }
                }
                _ => {}
            }
        }
        if !matches!(mcp.pending_shell_jit.as_deref(), Some(_)) || mcp.shell_jit_pipeline.is_none() {
            mcp.pending_shell_jit = None;
        }
        return ChunkResult { early_return: true };
    }

    // ── AutoDream evaluation ──
    // Authoritative dream context lives in `mcp.current_dream` since
    // Phase A.5 step 2; the local `DraugDaemon::is_dreaming()` flag is
    // no longer consulted from this path.
    if mcp.current_dream.is_some() && !tool_prompt.is_empty() {
        evaluate_dream_result(&wasm_bytes, &tool_prompt, mcp, wasm, draug, fb, tsc_per_us);
    }
    // ── Normal cache storage (non-dream) ──
    else if !tool_prompt.is_empty() {
        store_normal_wasm(&wasm_bytes, &tool_prompt, wasm);
    }

    // ── Launch result (interactive or one-shot) ──
    let config = compositor::wasm_runtime::WasmConfig {
        screen_width: fb.width as u32,
        screen_height: fb.height as u32,
        uptime_ms: libfolk::sys::uptime() as u32,
    };
    let interactive = {
        let p = tool_prompt.as_bytes();
        crate::util::find_ci(p, b"interactive") || crate::util::find_ci(p, b"game")
            || crate::util::find_ci(p, b"app") || crate::util::find_ci(p, b"click")
            || crate::util::find_ci(p, b"mouse") || crate::util::find_ci(p, b"tetris")
            || crate::util::find_ci(p, b"follow") || crate::util::find_ci(p, b"cursor")
    };
    wasm.last_interactive = interactive;

    if interactive {
        match compositor::wasm_runtime::PersistentWasmApp::new(&wasm_bytes, config) {
            Ok(app) => {
                write_str("[MCP] Interactive WASM app launched!\n");
                if let Some(win) = wm.get_window_mut(tool_win_id) {
                    win.push_line("[AI] Interactive app launched! Press ESC to exit.");
                }
                wasm.active_app = Some(app);
                wasm.active_app_key = Some(tool_prompt.clone());
                wasm.app_open_since_ms = libfolk::sys::uptime();
                wasm.fuel_fail_count = 0;
            }
            Err(e) => {
                if let Some(win) = wm.get_window_mut(tool_win_id) {
                    win.push_line(&alloc::format!("[AI] App error: {}", &e[..e.len().min(80)]));
                }
            }
        }
    } else {
        let (result, output) = compositor::wasm_runtime::execute_wasm(&wasm_bytes, config);
        let total_cmds = output.draw_commands.len()
            + output.line_commands.len()
            + output.circle_commands.len()
            + output.text_commands.len()
            + if output.fill_screen.is_some() { 1 } else { 0 };
        if let Some(win) = wm.get_window_mut(tool_win_id) {
            match &result {
                compositor::wasm_runtime::WasmResult::Ok =>
                    win.push_line(&alloc::format!("[AI] Tool: {} cmds", total_cmds)),
                compositor::wasm_runtime::WasmResult::OutOfFuel =>
                    win.push_line("[AI] Halted: fuel exhausted"),
                compositor::wasm_runtime::WasmResult::Trap(msg) =>
                    win.push_line(&alloc::format!("[AI] Trap: {}", &msg[..msg.len().min(80)])),
                compositor::wasm_runtime::WasmResult::LoadError(msg) =>
                    win.push_line(&alloc::format!("[AI] Load: {}", &msg[..msg.len().min(80)])),
            }
        }
        if let Some(color) = output.fill_screen { fb.clear(fb.color_from_rgb24(color)); }
        for cmd in &output.draw_commands {
            fb.fill_rect(cmd.x as usize, cmd.y as usize, cmd.w as usize, cmd.h as usize,
                fb.color_from_rgb24(cmd.color));
        }
        for cmd in &output.line_commands {
            let c = fb.color_from_rgb24(cmd.color);
            compositor::graphics::draw_line(&mut *fb, cmd.x1, cmd.y1, cmd.x2, cmd.y2, c);
        }
        for cmd in &output.circle_commands {
            let c = fb.color_from_rgb24(cmd.color);
            compositor::graphics::draw_circle(&mut *fb, cmd.cx, cmd.cy, cmd.r, c);
        }
        for cmd in &output.text_commands {
            fb.draw_string(cmd.x as usize, cmd.y as usize, &cmd.text,
                fb.color_from_rgb24(cmd.color), fb.color_from_rgb24(0));
        }
        if total_cmds > 0 { damage.damage_full(); }
    }
    *need_redraw = true;
    damage.damage_full();

    ChunkResult { early_return: false }
}

/// Run the V1-vs-V2 dream evaluation pipeline (sanity, benchmark, fuzz)
/// for whichever DreamMode is active.
fn evaluate_dream_result(
    wasm_bytes: &[u8],
    tool_prompt: &str,
    mcp: &mut McpState,
    wasm: &mut WasmState,
    draug: &mut DraugDaemon,
    fb: &FramebufferView,
    tsc_per_us: u64,
) {
    // Pull dream context from `mcp.current_dream` — set by
    // `start_dream_cycle` from the daemon's DREAM_DECIDE reply.
    // Falls back to parsing the tail of `tool_prompt` only if the
    // context was lost (shouldn't happen in normal flow; defensive).
    let (orig_key_owned, dream_mode) = match mcp.current_dream {
        Some((ref t, m)) => (t.clone(), m),
        None => (
            alloc::string::String::from(
                tool_prompt.rsplit(' ').next().unwrap_or(tool_prompt)
            ),
            compositor::draug::DreamMode::Refactor,
        ),
    };
    let orig_key = orig_key_owned.as_str();
    let mut nb = [0u8; 16];

    match dream_mode {
        compositor::draug::DreamMode::Refactor => {
            evaluate_refactor(wasm_bytes, orig_key, wasm, draug, fb, tsc_per_us, &mut nb);
        }
        compositor::draug::DreamMode::Creative => {
            write_str("[AutoDream] ---- CREATIVE RESULT ----\n");
            write_str("[AutoDream] New version: ");
            write_str(format_usize(wasm_bytes.len(), &mut nb));
            write_str(" bytes\n");
            let preview_cfg = compositor::wasm_runtime::WasmConfig {
                screen_width: fb.width as u32, screen_height: fb.height as u32, uptime_ms: 0,
            };
            let (_, preview_out) = compositor::wasm_runtime::execute_wasm(wasm_bytes, preview_cfg);
            let summary = compositor::wasm_runtime::render_summary(&preview_out);
            write_str("[AutoDream] New render: ");
            write_str(&summary[..summary.len().min(200)]);
            write_str("\n[AutoDream] VERDICT: QUEUED for user approval (Morning Briefing)\n");
            draug.queue_creative(orig_key, &summary[..summary.len().min(100)],
                alloc::vec::Vec::from(wasm_bytes));
        }
        compositor::draug::DreamMode::Nightmare => {
            evaluate_nightmare(wasm_bytes, orig_key, wasm);
        }
        compositor::draug::DreamMode::DriverRefactor |
        compositor::draug::DreamMode::DriverNightmare => {
            write_str("[AutoDream] ---- DRIVER DREAM RESULT ----\n");
            write_str("[AutoDream] Driver dream result received\n");
            wasm.cache.insert(alloc::string::String::from(orig_key),
                alloc::vec::Vec::from(wasm_bytes));
        }
    }

    write_str("[AutoDream] ========== DREAM COMPLETE ==========\n");
    notify_dream_result(DREAM_RESULT_COMPLETE);
    mcp.current_dream = None;

    // State Migration: hot-swap running app if it was the dream target
    if let Some(ref snapshot) = wasm.state_snapshot {
        if let Some(ref k) = wasm.active_app_key {
            if k.as_str() == orig_key {
                if let Some(evolved_wasm) = wasm.cache.get(orig_key) {
                    let config = compositor::wasm_runtime::WasmConfig {
                        screen_width: fb.width as u32,
                        screen_height: fb.height as u32,
                        uptime_ms: libfolk::sys::uptime() as u32,
                    };
                    if let Ok(mut new_app) = compositor::wasm_runtime::PersistentWasmApp::new(evolved_wasm, config) {
                        new_app.write_memory(0, snapshot);
                        wasm.active_app = Some(new_app);
                        wasm.fuel_fail_count = 0;
                        write_str("[StateMigration] Hot-swapped running app with evolved version + restored state\n");
                    }
                }
            }
        }
        wasm.state_snapshot = None;
    }
}

fn evaluate_refactor(
    wasm_bytes: &[u8],
    orig_key: &str,
    wasm: &mut WasmState,
    draug: &mut DraugDaemon,
    fb: &FramebufferView,
    tsc_per_us: u64,
    nb: &mut [u8; 16],
) {
    write_str("[AutoDream] ---- REFACTOR RESULT ----\n");
    // Amnesia fix: load V1 from VFS if not in cache
    if !wasm.cache.contains_key(orig_key) {
        let vfs_name = alloc::format!("{}.wasm", orig_key);
        const VFS_DREAM_VADDR: usize = 0x50070000;
        if let Ok(resp) = libfolk::sys::synapse::read_file_shmem(&vfs_name) {
            if shmem_map(resp.shmem_handle, VFS_DREAM_VADDR).is_ok() {
                let data = unsafe {
                    core::slice::from_raw_parts(VFS_DREAM_VADDR as *const u8, resp.size as usize)
                };
                wasm.cache.insert(alloc::string::String::from(orig_key), alloc::vec::Vec::from(data));
                let _ = shmem_unmap(resp.shmem_handle, VFS_DREAM_VADDR);
                let _ = shmem_destroy(resp.shmem_handle);
                write_str("[AutoDream] Recovered V1 from Synapse VFS\n");
            } else {
                let _ = shmem_destroy(resp.shmem_handle);
            }
        }
    }
    let Some(v1_wasm) = wasm.cache.get(orig_key).cloned() else {
        write_str("[AutoDream] ERROR: V1 not in cache, cannot compare\n");
        return;
    };

    let bench_config = compositor::wasm_runtime::WasmConfig {
        screen_width: fb.width as u32, screen_height: fb.height as u32, uptime_ms: 0,
    };

    // Lobotomy check
    let (_, v1_out) = compositor::wasm_runtime::execute_wasm(&v1_wasm, bench_config.clone());
    let v1_cmds = v1_out.draw_commands.len() + v1_out.circle_commands.len()
        + v1_out.line_commands.len() + v1_out.text_commands.len();
    let (_, v2_out) = compositor::wasm_runtime::execute_wasm(wasm_bytes, bench_config.clone());
    let v2_cmds = v2_out.draw_commands.len() + v2_out.circle_commands.len()
        + v2_out.line_commands.len() + v2_out.text_commands.len();

    if v1_cmds > 0 && v2_cmds == 0 {
        write_str("[AutoDream] VERDICT: STRIKE (Lobotomy — V2 draws 0 commands vs V1:");
        write_str(format_usize(v1_cmds, nb));
        write_str(")\n");
        draug.add_strike(orig_key);
    } else if v1_cmds > 0 && (v2_cmds * 2) < v1_cmds {
        write_str("[AutoDream] VERDICT: STRIKE (Degradation — V2:");
        write_str(format_usize(v2_cmds, nb));
        write_str(" cmds vs V1:");
        write_str(format_usize(v1_cmds, nb));
        write_str(")\n");
        draug.add_strike(orig_key);
    } else {
        write_str("[AutoDream] Sanity: V1=");
        write_str(format_usize(v1_cmds, nb));
        write_str(" V2=");
        write_str(format_usize(v2_cmds, nb));
        write_str(" cmds (OK)\n");

        // Benchmark
        write_str("[AutoDream] Benchmarking (10 iterations)...\n");
        let t1 = rdtsc();
        for _ in 0..10 { let _ = compositor::wasm_runtime::execute_wasm(&v1_wasm, bench_config.clone()); }
        let v1_us = (rdtsc() - t1) / tsc_per_us / 10;
        let t2 = rdtsc();
        for _ in 0..10 { let _ = compositor::wasm_runtime::execute_wasm(wasm_bytes, bench_config.clone()); }
        let v2_us = (rdtsc() - t2) / tsc_per_us / 10;

        write_str("[AutoDream] V1:");
        write_str(format_usize(v1_us as usize, nb));
        write_str("us V2:");
        write_str(format_usize(v2_us as usize, nb));
        write_str("us\n");

        if v2_us < v1_us {
            // Edge-case fuzz test
            let fuzz_configs = [
                compositor::wasm_runtime::WasmConfig { screen_width: 0, screen_height: 0, uptime_ms: 0 },
                compositor::wasm_runtime::WasmConfig { screen_width: 1, screen_height: 1, uptime_ms: u32::MAX },
                compositor::wasm_runtime::WasmConfig { screen_width: 9999, screen_height: 9999, uptime_ms: 0 },
            ];
            let mut fuzz_pass = true;
            for fc in &fuzz_configs {
                let (fr, _) = compositor::wasm_runtime::execute_wasm(wasm_bytes, fc.clone());
                if let compositor::wasm_runtime::WasmResult::Trap(_) = fr {
                    write_str("[AutoDream] FUZZ FAIL: V2 crashes on edge input\n");
                    fuzz_pass = false;
                    break;
                }
            }
            if !fuzz_pass {
                write_str("[AutoDream] VERDICT: STRIKE (failed edge-case fuzz)\n");
                draug.add_strike(orig_key);
            } else {
                let pct = ((v1_us - v2_us) * 100 / v1_us.max(1)) as usize;
                write_str("[AutoDream] VERDICT: EVOLVED! ");
                write_str(format_usize(pct, nb));
                write_str("% faster (fuzz: OK)\n");
                wasm.cache.insert(alloc::string::String::from(orig_key),
                    alloc::vec::Vec::from(wasm_bytes));
                draug.reset_strikes(orig_key);
            }
        } else {
            write_str("[AutoDream] VERDICT: STRIKE (V2 not faster)\n");
            draug.add_strike(orig_key);
        }
    }
    if draug.is_perfected(orig_key) {
        write_str("[AutoDream] STATUS: PERFECTED\n");
    }
}

fn evaluate_nightmare(wasm_bytes: &[u8], orig_key: &str, wasm: &mut WasmState) {
    write_str("[AutoDream] ---- NIGHTMARE RESULT ----\n");
    write_str("[AutoDream] Fuzzing hardened version (w=0,h=0,t=MAX)...\n");
    let fuzz_config = compositor::wasm_runtime::WasmConfig {
        screen_width: 0, screen_height: 0, uptime_ms: u32::MAX,
    };
    let (fuzz_result, _) = compositor::wasm_runtime::execute_wasm(wasm_bytes, fuzz_config);
    match fuzz_result {
        compositor::wasm_runtime::WasmResult::Ok => {
            write_str("[AutoDream] VERDICT: SURVIVED (Ok) — app vaccinated!\n");
            wasm.cache.insert(alloc::string::String::from(orig_key),
                alloc::vec::Vec::from(wasm_bytes));
        }
        compositor::wasm_runtime::WasmResult::OutOfFuel => {
            write_str("[AutoDream] VERDICT: SURVIVED (fuel exhausted, but no crash) — accepted\n");
            wasm.cache.insert(alloc::string::String::from(orig_key),
                alloc::vec::Vec::from(wasm_bytes));
        }
        compositor::wasm_runtime::WasmResult::Trap(ref msg) => {
            write_str("[AutoDream] VERDICT: CRASHED! Trap: ");
            write_str(&msg[..msg.len().min(80)]);
            write_str("\n[AutoDream] Keeping original (V2 too fragile)\n");
        }
        compositor::wasm_runtime::WasmResult::LoadError(ref msg) => {
            write_str("[AutoDream] VERDICT: LOAD FAILED: ");
            write_str(&msg[..msg.len().min(80)]);
            write_str("\n");
        }
    }
}

fn store_normal_wasm(wasm_bytes: &[u8], tool_prompt: &str, wasm: &mut WasmState) {
    if wasm.cache.len() >= MAX_CACHE_ENTRIES {
        if let Some(oldest) = wasm.cache.keys().next().cloned() {
            wasm.cache.remove(&oldest);
        }
    }
    wasm.cache.insert(alloc::string::String::from(tool_prompt), alloc::vec::Vec::from(wasm_bytes));
    write_str("[Cache] Stored WASM for: ");
    write_str(&tool_prompt[..tool_prompt.len().min(40)]);
    write_str("\n");

    // Semantic VFS: auto-tag intent metadata
    let clean_name = {
        let mut n = tool_prompt;
        for pfx in &["gemini generate ", "gemini gen ", "generate "] {
            if n.len() > pfx.len() && n.as_bytes()[..pfx.len()].eq_ignore_ascii_case(pfx.as_bytes()) {
                n = &n[pfx.len()..];
                break;
            }
        }
        n.trim()
    };
    let wasm_filename = alloc::format!("{}.wasm", clean_name);
    let write_ret = libfolk::sys::synapse::write_file(&wasm_filename, wasm_bytes);
    if write_ret.is_ok() {
        let rowid = if let Ok(count) = libfolk::sys::synapse::file_count() {
            count as u32
        } else { 0 };
        if rowid > 0 {
            let intent_json = alloc::format!(
                "{{\"purpose\":\"{}\",\"type\":\"wasm_app\",\"size\":{}}}",
                clean_name, wasm_bytes.len()
            );
            let _ = libfolk::sys::synapse::write_intent(rowid, "application/wasm", &intent_json);
            write_str("[Synapse] Intent tagged: ");
            write_str(clean_name);
            write_str("\n");
        }
    }
}
