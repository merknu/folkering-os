//! System host functions for WASM apps
//! Time, screen metrics, input, telemetry, hardware inspection, shadow testing, streams.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use wasmi::*;
use super::HostState;

pub fn register(linker: &mut Linker<HostState>) {
    // System metrics
    let _ = linker.func_wrap("env", "folk_get_time",
        |caller: Caller<HostState>| -> i32 {
            caller.data().config.uptime_ms as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_screen_width",
        |caller: Caller<HostState>| -> i32 {
            caller.data().config.screen_width as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_screen_height",
        |caller: Caller<HostState>| -> i32 {
            caller.data().config.screen_height as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_random",
        |_caller: Caller<HostState>| -> i32 {
            libfolk::sys::random::random_u32() as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_os_metric",
        |_caller: Caller<HostState>, metric_id: i32| -> i32 {
            (libfolk::sys::pci::os_metric(metric_id as u32) & 0xFFFFFFFF) as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_net_has_ip",
        |_caller: Caller<HostState>| -> i32 {
            let (has_ip, _, _, _, _) = libfolk::sys::pci::net_status();
            if has_ip { 1 } else { 0 }
        },
    );

    let _ = linker.func_wrap("env", "folk_fw_drops",
        |_caller: Caller<HostState>| -> i32 {
            let (_, drops) = libfolk::sys::pci::firewall_stats();
            drops as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_get_datetime",
        |mut caller: Caller<HostState>, ptr: i32| -> i32 {
            let p = ptr as u32;
            let end = match p.checked_add(24) { Some(e) => e, None => return 0 };
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            if end as usize > mem.data_size(&caller) { return 0; }
            let dt = libfolk::sys::get_rtc();
            let mut buf = [0u8; 24];
            buf[0..4].copy_from_slice(&(dt.year as i32).to_le_bytes());
            buf[4..8].copy_from_slice(&(dt.month as i32).to_le_bytes());
            buf[8..12].copy_from_slice(&(dt.day as i32).to_le_bytes());
            buf[12..16].copy_from_slice(&(dt.hour as i32).to_le_bytes());
            buf[16..20].copy_from_slice(&(dt.minute as i32).to_le_bytes());
            buf[20..24].copy_from_slice(&(dt.second as i32).to_le_bytes());
            if mem.write(&mut caller, ptr as usize, &buf).is_ok() { 1 } else { 0 }
        },
    );

    let _ = linker.func_wrap("env", "folk_poll_event",
        |mut caller: Caller<HostState>, event_ptr: i32| -> i32 {
            let event = match caller.data_mut().pending_events.pop() {
                Some(e) => e,
                None => return 0,
            };
            // Bounds check: event_ptr + 16 must fit in WASM memory
            let ptr_u = event_ptr as u32;
            let end = match ptr_u.checked_add(16) {
                Some(e) => e,
                None => return 0,
            };
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            if end as usize > mem.data_size(&caller) { return 0; }
            // Serialize FolkEvent as 16 bytes (4 x i32 little-endian)
            let mut buf = [0u8; 16];
            buf[0..4].copy_from_slice(&event.event_type.to_le_bytes());
            buf[4..8].copy_from_slice(&event.x.to_le_bytes());
            buf[8..12].copy_from_slice(&event.y.to_le_bytes());
            buf[12..16].copy_from_slice(&event.data.to_le_bytes());
            if mem.write(&mut caller, event_ptr as usize, &buf).is_ok() {
                event.event_type
            } else {
                0
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_log_telemetry",
        |_caller: Caller<HostState>, action_type: i32, target_id: i32, duration_ms: i32| {
            // Syscall 0x9B: record telemetry event
            unsafe {
                libfolk::syscall::syscall3(0x9B, action_type as u64, target_id as u64, duration_ms as u64);
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_telemetry_poll",
        |mut caller: Caller<HostState>, buf_ptr: i32, max_events: i32| -> i32 {
            if max_events <= 0 || max_events > 256 { return 0; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            let event_size = 16usize;
            let buf_size = max_events as usize * event_size;
            let mut buf = alloc::vec![0u8; buf_size];
            // Syscall 0x9C: drain telemetry
            let drained = unsafe {
                libfolk::syscall::syscall2(0x9C, buf.as_mut_ptr() as u64, max_events as u64) as usize
            };
            if drained == 0 { return 0; }
            let copy_bytes = drained * event_size;
            if mem.write(&mut caller, buf_ptr as usize, &buf[..copy_bytes]).is_ok() {
                drained as i32
            } else { 0 }
        },
    );

    let _ = linker.func_wrap("env", "folk_pci_list",
        |mut caller: Caller<HostState>, buf_ptr: i32, max_len: i32| -> i32 {
            if max_len <= 0 { return 0; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };

            // Zero-initialize device array (PciDeviceInfo has private fields)
            let mut devices: [libfolk::sys::pci::PciDeviceInfo; 16] = unsafe {
                core::mem::zeroed()
            };
            let count = libfolk::sys::pci::enumerate(&mut devices);

            // Format as compact text
            let mut out = alloc::vec![0u8; max_len as usize];
            let mut pos = 0usize;

            for i in 0..count {
                let d = &devices[i];
                // "VID:DID CC:SS B:D.F IRQ\n"
                let line = alloc::format!(
                    "{:04X}:{:04X} {:02X}:{:02X} {}:{}.{} IRQ{}\n",
                    d.vendor_id, d.device_id,
                    d.class_code, d.subclass,
                    d.bus, d.device_num, d.function,
                    d.interrupt_line
                );
                let bytes = line.as_bytes();
                if pos + bytes.len() > out.len() { break; }
                out[pos..pos + bytes.len()].copy_from_slice(bytes);
                pos += bytes.len();
            }

            if pos > 0 && mem.write(&mut caller, buf_ptr as usize, &out[..pos]).is_ok() {
                pos as i32
            } else { 0 }
        },
    );

    let _ = linker.func_wrap("env", "folk_irq_stats",
        |mut caller: Caller<HostState>, buf_ptr: i32, max_len: i32| -> i32 {
            if max_len <= 0 { return 0; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };

            // Read network stats via os_metric syscalls
            let net_metric = libfolk::sys::pci::os_metric(0); // network
            let fw_metric = libfolk::sys::pci::os_metric(1);  // firewall
            let net_rx = (fw_metric >> 32) as u32; // allows from high 32
            let net_tx = fw_metric as u32; // drops from low 32
            let uptime = libfolk::sys::uptime();
            let (mem_total, mem_used, mem_pct) = libfolk::sys::memory_stats();

            let line = alloc::format!(
                "fw_allow:{} fw_drop:{} mem:{}%({}/{}MB) up:{}s net:{:#x}\n",
                net_rx, net_tx, mem_pct, mem_used, mem_total, uptime / 1000, net_metric
            );

            let bytes = line.as_bytes();
            let copy = bytes.len().min(max_len as usize);
            if mem.write(&mut caller, buf_ptr as usize, &bytes[..copy]).is_ok() {
                copy as i32
            } else { 0 }
        },
    );

    let _ = linker.func_wrap("env", "folk_memory_map",
        |mut caller: Caller<HostState>, buf_ptr: i32, max_len: i32| -> i32 {
            if max_len < 80 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            let (total_mb, used_mb, pct) = libfolk::sys::memory_stats();
            let uptime = libfolk::sys::uptime();

            // Build a synthetic heatmap: 64 cells representing memory regions
            // Use a deterministic pattern seeded by actual usage stats
            let mut out = [0u8; 80];
            // Header: 16 bytes
            out[0..4].copy_from_slice(&(total_mb as u32).to_le_bytes());
            out[4..8].copy_from_slice(&(used_mb as u32).to_le_bytes());
            out[8..12].copy_from_slice(&(pct as u32).to_le_bytes());
            out[12..16].copy_from_slice(&((uptime / 1000) as u32).to_le_bytes());

            // Heatmap: 64 bytes, each 0-255 representing allocation density
            // Low addresses (kernel) = high usage, high addresses = lower
            for i in 0..64 {
                let base_density = if i < 8 { 240 } // kernel text/data
                    else if i < 16 { 200 } // kernel heap
                    else if i < 24 { (pct as u8).saturating_mul(2).min(200) } // active allocations
                    else if i < 40 { (pct as u8) } // moderate use
                    else { (pct as u8) / 3 }; // free space

                // Add slight variation using uptime as seed
                let noise = ((uptime.wrapping_mul(31).wrapping_add(i as u64 * 7)) % 20) as u8;
                out[16 + i] = base_density.saturating_add(noise).min(255);
            }

            if mem.write(&mut caller, buf_ptr as usize, &out[..80]).is_ok() {
                80
            } else { -1 }
        },
    );

    let _ = linker.func_wrap("env", "folk_ipc_stats",
        |mut caller: Caller<HostState>, buf_ptr: i32, max_len: i32| -> i32 {
            if max_len < 32 { return 0; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };

            // Read task list via syscall 0x26
            // Buffer format: [task_id:u32][state:u32][name:[u8;16]][cpu_time_ms:u64] = 32 bytes/task
            let mut raw = alloc::vec![0u8; 32 * 16]; // max 16 tasks
            let count = unsafe {
                libfolk::syscall::syscall2(0x26, raw.as_mut_ptr() as u64, raw.len() as u64) as usize
            };

            if count == 0 { return 0; }

            // Format as text
            let mut out = alloc::vec![0u8; max_len as usize];
            let mut pos = 0usize;

            for i in 0..count.min(16) {
                let off = i * 32;
                let tid = u32::from_le_bytes([raw[off], raw[off+1], raw[off+2], raw[off+3]]);
                let state = u32::from_le_bytes([raw[off+4], raw[off+5], raw[off+6], raw[off+7]]);
                let name_end = raw[off+8..off+24].iter().position(|&b| b == 0).unwrap_or(16);
                let name = core::str::from_utf8(&raw[off+8..off+8+name_end]).unwrap_or("?");
                let cpu_ms = u64::from_le_bytes([
                    raw[off+24], raw[off+25], raw[off+26], raw[off+27],
                    raw[off+28], raw[off+29], raw[off+30], raw[off+31],
                ]);

                let state_str = match state {
                    0 => "ready",
                    1 => "running",
                    2 => "blocked",
                    3 => "waiting",
                    _ => "?",
                };

                let line = alloc::format!("{}:{}:{}:{}\n", tid, name, state_str, cpu_ms);
                let bytes = line.as_bytes();
                if pos + bytes.len() > out.len() { break; }
                out[pos..pos+bytes.len()].copy_from_slice(bytes);
                pos += bytes.len();
            }

            if pos > 0 && mem.write(&mut caller, buf_ptr as usize, &out[..pos]).is_ok() {
                pos as i32
            } else { 0 }
        },
    );

    let _ = linker.func_wrap("env", "folk_shadow_test",
        |mut caller: Caller<HostState>, wasm_ptr: i32, wasm_len: i32, result_ptr: i32, max_len: i32| -> i32 {
            if wasm_len <= 0 || wasm_len > 65536 || max_len < 32 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            let mut wasm_bytes = alloc::vec![0u8; wasm_len as usize];
            if mem.read(&caller, wasm_ptr as usize, &mut wasm_bytes).is_err() { return -1; }

            // Run in shadow sandbox (no real side effects)
            let report = super::execute_shadow_test(&wasm_bytes, &[]);

            // Format report as compact text
            let text = alloc::format!(
                "ok:{} frames:{} fuel:{} draw:{} text:{} file:{} ai:{}{}\n",
                if report.completed { 1 } else { 0 },
                report.frames_executed,
                report.fuel_consumed,
                report.draw_call_count,
                report.text_draw_count,
                report.file_write_count,
                report.ai_call_count,
                if let Some(ref e) = report.error { alloc::format!(" err:{}", e) } else { String::new() },
            );

            let bytes = text.as_bytes();
            let copy = bytes.len().min(max_len as usize);
            if mem.write(&mut caller, result_ptr as usize, &bytes[..copy]).is_ok() {
                copy as i32
            } else { -1 }
        },
    );

    // folk_stream_write(ptr, len) -- upstream pushes data to stream buffer
    let _ = linker.func_wrap("env", "folk_stream_write",
        |mut caller: Caller<HostState>, ptr: i32, len: i32| {
            if len <= 0 || len > 4096 { return; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return,
            };
            let mut buf = alloc::vec![0u8; len as usize];
            if mem.read(&caller, ptr as usize, &mut buf).is_ok() {
                caller.data_mut().stream_write_buf.extend_from_slice(&buf);
            }
        },
    );

    // folk_stream_read(ptr, max_len) -> i32 -- downstream pulls data from stream
    let _ = linker.func_wrap("env", "folk_stream_read",
        |mut caller: Caller<HostState>, ptr: i32, max_len: i32| -> i32 {
            if max_len <= 0 { return 0; }
            let data = caller.data().stream_read_buf.clone();
            if data.is_empty() { return 0; }
            let copy_len = data.len().min(max_len as usize);
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            if mem.write(&mut caller, ptr as usize, &data[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // folk_stream_done() -- signal that streaming is complete
    let _ = linker.func_wrap("env", "folk_stream_done",
        |mut caller: Caller<HostState>| {
            caller.data_mut().stream_complete = true;
        },
    );

    // ── Clipboard ──────────────────────────────────────────────────────
    // Global clipboard shared between all WASM apps. Backed by a static
    // buffer in the compositor process. Max 4KB content.

    // folk_clipboard_set(ptr, len) -> i32
    // Copy data from WASM memory into the global clipboard.
    // Returns 0 on success, -1 on error.
    let _ = linker.func_wrap("env", "folk_clipboard_set",
        |caller: Caller<HostState>, ptr: i32, len: i32| -> i32 {
            if len < 0 || len > 4096 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut buf = alloc::vec![0u8; len as usize];
            if mem.read(&caller, ptr as usize, &mut buf).is_err() { return -1; }
            unsafe {
                let n = (len as usize).min(CLIPBOARD_BUF.len());
                CLIPBOARD_BUF[..n].copy_from_slice(&buf[..n]);
                CLIPBOARD_LEN = n;
            }
            0
        },
    );

    // folk_clipboard_get(ptr, max_len) -> i32
    // Copy clipboard contents into WASM memory.
    // Returns bytes written, or -1 on error.
    let _ = linker.func_wrap("env", "folk_clipboard_get",
        |mut caller: Caller<HostState>, ptr: i32, max_len: i32| -> i32 {
            if max_len <= 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let (data, len) = unsafe {
                let n = CLIPBOARD_LEN.min(max_len as usize);
                (CLIPBOARD_BUF[..n].to_vec(), n)
            };
            if len == 0 { return 0; }
            if mem.write(&mut caller, ptr as usize, &data[..len]).is_ok() {
                len as i32
            } else { -1 }
        },
    );

    // folk_clipboard_len() -> i32
    // Returns current clipboard size in bytes (0 if empty).
    let _ = linker.func_wrap("env", "folk_clipboard_len",
        |_caller: Caller<HostState>| -> i32 {
            unsafe { CLIPBOARD_LEN as i32 }
        },
    );
}

// Global clipboard buffer (compositor-process scope)
static mut CLIPBOARD_BUF: [u8; 4096] = [0u8; 4096];
static mut CLIPBOARD_LEN: usize = 0;
