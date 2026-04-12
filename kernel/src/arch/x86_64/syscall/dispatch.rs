//! Syscall number → handler dispatch table.

use super::debug::SYSCALL_RESULT;
use super::handlers::*;

/// Syscall handler (called from assembly)
#[no_mangle]
#[inline(never)]
pub(super) extern "C" fn syscall_handler(
    syscall_num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    arg6: u64,
) -> u64 {
    let current_task = crate::task::task::get_current_task();
    crate::task::statistics::record_syscall(current_task);

    let result = match syscall_num {
        0 => syscall_ipc_send(arg1, arg2, arg3),
        1 => syscall_ipc_receive(arg1),
        2 => syscall_ipc_reply(arg1, arg2),
        3 => syscall_shmem_create(arg1),
        4 => syscall_shmem_map(arg1, arg2),
        5 => syscall_spawn(arg1, arg2),
        6 => syscall_exit(arg1),
        7 => syscall_yield(),
        8 => syscall_read_key(),
        9 => syscall_write_char(arg1),
        10 => syscall_get_pid(),
        11 => syscall_task_list(),
        12 => syscall_uptime(),
        13 => syscall_fs_read_dir(arg1, arg2),
        14 => syscall_fs_read_file(arg1, arg2, arg3),
        15 => syscall_shmem_grant(arg1, arg2),
        16 => syscall_poweroff(),
        17 => syscall_check_interrupt(),
        18 => syscall_clear_interrupt(),
        19 => syscall_shmem_unmap(arg1, arg2),
        20 => syscall_shmem_destroy(arg1),
        // Phase 6: Reply-Later IPC
        0x20 => syscall_ipc_recv_async(),
        0x21 => syscall_ipc_reply_token(arg1, arg2, arg3),
        0x22 => syscall_ipc_get_recv_payload(),
        0x23 => syscall_ipc_get_recv_sender(),
        // Phase 6.2: Physical memory mapping
        0x24 => syscall_map_physical(arg1, arg2, arg3, arg4, arg5),
        // Phase 7: Input
        0x25 => syscall_read_mouse(),
        // Phase 8: Detailed task list via userspace buffer
        0x26 => syscall_task_list_detailed(arg1, arg2),
        // Phase 9: Anonymous memory mapping
        0x30 => syscall_mmap(arg1, arg2, arg3),
        0x31 => syscall_munmap(arg1, arg2),
        // Milestone 5: Block device I/O
        0x40 => syscall_block_read(arg1, arg2, arg3),
        0x41 => syscall_block_write(arg1, arg2, arg3),
        // Milestone 26-27: Network
        0x50 => syscall_ping(arg1),
        0x51 => syscall_dns_lookup(arg1, arg2),
        // Milestone 28: Entropy & RTC
        0x52 => syscall_get_time(),
        0x53 => syscall_get_random(arg1, arg2),
        // Milestone 30-32: HTTPS, GitHub & Clone
        0x54 => syscall_https_test(arg1),
        0x55 => syscall_github_fetch(arg1, arg2, arg3, arg4),
        0x56 => syscall_github_clone(arg1, arg2, arg3, arg4),
        // Direct HTTP fetch (URL → DNS → TLS → body)
        0x57 => syscall_http_fetch(arg1, arg2, arg3, arg4),
        // UDP send (target_ip:port, data)
        0x58 => syscall_udp_send(arg1, arg2, arg3, arg4),
        // UDP send + recv (target_ip:port, data, response_buf, timeout_ms)
        0x59 => syscall_udp_send_recv(arg1, arg2, arg3, arg4, arg5, arg6),
        // Audio: play raw PCM samples (16-bit signed stereo @ 44100Hz)
        0x5A => syscall_audio_play(arg1, arg2),
        // Audio: beep (440Hz sine wave for duration_ms)
        0x5B => syscall_audio_beep(arg1),
        // NTP query: returns Unix timestamp from NTP server
        0x5C => syscall_ntp_query(arg1),
        // HTTP POST: url, body, response buffer (form submission)
        0x5D => syscall_http_post(arg1, arg2, arg3, arg4, arg5, arg6),
        // FBP request: fetch DOM snapshot from host-side proxy
        0x5E => syscall_fbp_request(arg1, arg2, arg3, arg4),
        // FBP interact: send an INTERACTION_EVENT and read the post-
        // click DOM snapshot on a single persistent TCP session.
        0x5F => syscall_fbp_interact(arg1, arg2, arg3, arg4, arg5),
        // FBP patch (Phase 11): ship a .rs file to the proxy's
        // draug-sandbox crate and run cargo check on it.
        // 4 args — lengths packed into arg4 to dodge the pre-existing
        // 6-arg ABI gap in the syscall entry asm.
        0x61 => syscall_fbp_patch(arg1, arg2, arg3, arg4),
        // LLM gateway (Phase 12): Ollama bridge via proxy.
        // Same 4-arg packed-lengths pattern as fbp_patch.
        0x62 => syscall_llm_generate(arg1, arg2, arg3, arg4),
        // WASM compilation (Phase 16): compile sandbox to wasm32
        0x63 => syscall_wasm_compile(arg1, arg2),
        // Proxy health check (Stability Fix 7)
        0x64 => syscall_proxy_ping(),
        // Draug bridge: push status to tcp_shell atomics, return pause flag
        0xD0 => {
            use crate::net::tcp_shell::*;
            use core::sync::atomic::Ordering::Relaxed;
            DRAUG_ITER.store((arg1 >> 32) as u32, Relaxed);
            DRAUG_PASSED.store(arg1 as u32, Relaxed);
            DRAUG_FAILED.store((arg2 >> 32) as u32, Relaxed);
            DRAUG_RETRIES.store(arg2 as u32, Relaxed);
            DRAUG_STATE[0].store((arg3 >> 24) as u8, Relaxed); // L1
            DRAUG_STATE[1].store((arg3 >> 16) as u8, Relaxed); // L2
            DRAUG_STATE[2].store((arg3 >> 8) as u8, Relaxed);  // L3
            DRAUG_STATE[3].store(arg3 as u8, Relaxed);          // plan_mode
            DRAUG_STATE[4].store((arg4 >> 16) as u8, Relaxed); // complex_idx
            DRAUG_STATE[5].store((arg4 >> 8) as u8, Relaxed);  // hibernating
            DRAUG_STATE[6].store(arg4 as u8, Relaxed);          // consec_skips
            DRAUG_PAUSE_FLAG.load(Relaxed) as u64
        },
        // SMP: Parallel GEMM
        0x60 => syscall_parallel_gemm(arg1, arg2, arg3, arg4, arg5, arg6),
        // Hybrid AI: Ask Gemini cloud API
        0x70 => syscall_ask_gemini(arg1, arg2, arg3),
        // VirtIO GPU
        0x80 => syscall_gpu_flush(arg1, arg2, arg3, arg4),
        0x81 => syscall_gpu_info(arg1),
        // VSync: flush + wait for GPU fence completion (CPU sleeps via HLT)
        0x82 => {
            crate::drivers::virtio_gpu::flush_and_vsync(
                arg1 as u32, arg2 as u32, arg3 as u32, arg4 as u32
            );
            0
        },
        // Real-Time Clock (CMOS RTC)
        0x83 => super::super::rtc::read_rtc_packed(),
        // System stats: (total_pages << 32 | free_pages)
        0x84 => {
            let (total, free) = crate::memory::physical::memory_stats();
            ((total as u64) << 32) | (free as u64 & 0xFFFFFFFF)
        },
        // God Mode Pipe: read byte from COM3
        0x90 => {
            match crate::drivers::serial::com3_read_byte() {
                Some(b) => b as u64,
                None => u64::MAX,
            }
        },
        // IQE: Interaction Quality Engine telemetry
        0x91 => crate::drivers::iqe::read_to_user(arg1 as usize, arg2 as usize) as u64,
        0x92 => crate::drivers::iqe::tsc_ticks_per_us(),
        // Batched GPU flush: transfer N rects with 1 doorbell (1 VM-exit)
        // arg1 = ptr to [(x,y,w,h); N] as [u32; N*4], arg2 = N (max 4)
        0x95 => {
            let n = (arg2 as usize).min(4);
            let ptr = arg1 as *const u32;
            if !ptr.is_null() && arg1 >= 0x200000 && arg1 < 0xFFFF_8000_0000_0000 && n > 0 {
                let mut rects = [(0u32, 0u32, 0u32, 0u32); 4];
                for i in 0..n {
                    unsafe {
                        rects[i] = (
                            core::ptr::read_volatile(ptr.add(i * 4)),
                            core::ptr::read_volatile(ptr.add(i * 4 + 1)),
                            core::ptr::read_volatile(ptr.add(i * 4 + 2)),
                            core::ptr::read_volatile(ptr.add(i * 4 + 3)),
                        );
                    }
                }
                crate::drivers::virtio_gpu::flush_rects_batched(&rects[..n]);
            }
            0
        },
        // Async COM2: send + activate RX polling. len=0 activates polling without sending.
        0x96 => {
            let len = (arg2 as usize).min(8192);
            if len == 0 {
                // Activate RX polling only (no TX)
                crate::drivers::serial::com2_async_send(&[]);
                0
            } else {
                let ptr = arg1 as *const u8;
                if !ptr.is_null() && arg1 >= 0x200000 && arg1 < 0xFFFF_8000_0000_0000 {
                    let data = unsafe { core::slice::from_raw_parts(ptr, len) };
                    crate::drivers::serial::com2_async_send(data);
                    0
                } else {
                    u64::MAX
                }
            }
        },
        // Async COM2: poll for RX bytes + check for 0x00 COBS sentinel
        // arg1: 0 = COBS sentinel (0x00), 1 = legacy @@END@@ delimiter
        // Returns: 0 = still waiting, >0 = frame length before delimiter
        0x97 => {
            crate::drivers::serial::com2_async_poll();
            let use_legacy = arg1 == 1;
            let result = if use_legacy {
                crate::drivers::serial::com2_async_check_legacy()
            } else {
                crate::drivers::serial::com2_async_check_sentinel()
            };
            match result {
                Some(len) => len as u64,
                None => 0,
            }
        },
        // Async COM2: read response into userspace buffer, arg1=buf_ptr, arg2=max_len
        // Returns bytes copied
        0x98 => {
            let max_len = arg2 as usize;
            let ptr = arg1 as *mut u8;
            if !ptr.is_null() && arg1 >= 0x200000 && arg1 < 0xFFFF_8000_0000_0000 && max_len > 0 {
                let buf = unsafe { core::slice::from_raw_parts_mut(ptr, max_len.min(131072)) };
                crate::drivers::serial::com2_async_read(buf, max_len) as u64
            } else {
                u64::MAX
            }
        },
        // Wait for interrupt (HLT). Enables interrupts, halts CPU, wakes on ANY IRQ.
        // This is the correct idle primitive under WHPX: causes VM-exit so hypervisor
        // can inject pending interrupts (mouse, keyboard, timer).
        0x99 => {
            // Poll network stack before halting (replaces timer-ISR polling
            // which caused #GP from misaligned SSE in smoltcp)
            crate::net::poll();
            unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)); }
            0
        },
        // COM2 raw TX write (does NOT reset async RX state).
        // Used for MCP frames (send without disrupting RX polling).
        // arg1=buf_ptr, arg2=len (max 8KB)
        0x9A => {
            let len = (arg2 as usize).min(8192);
            let ptr = arg1 as *const u8;
            if !ptr.is_null() && arg1 >= 0x200000 && arg1 < 0xFFFF_8000_0000_0000 && len > 0 {
                let data = unsafe { core::slice::from_raw_parts(ptr, len) };
                crate::drivers::serial::com2_write(data);
                len as u64
            } else {
                u64::MAX
            }
        },
        // COM3 write: export telemetry to host (arg1=buf_ptr, arg2=len)
        0x94 => {
            let len = (arg2 as usize).min(64); // cap at 64 bytes for safety
            let ptr = arg1 as *const u8;
            if !ptr.is_null() && arg1 >= 0x200000 && arg1 < 0xFFFF_8000_0000_0000 {
                // Read bytes one at a time from userspace (safe, no bulk copy)
                for i in 0..len {
                    let byte = unsafe { core::ptr::read_volatile(ptr.add(i)) };
                    crate::drivers::serial::com3_write_byte(byte);
                }
            }
            len as u64
        },
        // Phase 10: Hardware Discovery — PCI device enumeration for WASM drivers
        // arg1 = userspace buffer ptr, arg2 = buffer size in bytes
        // Returns: number of devices written, or u64::MAX on error
        // Each device is 64 bytes (PciDeviceUserInfo struct)
        0xA0 => syscall_pci_enumerate(arg1, arg2),
        // Phase 10: Capability-gated Port I/O for WASM drivers
        // arg1 = port number, arg2 = value (for OUT), returns value (for IN)
        0xA1 => syscall_port_inb(arg1),     // IN byte
        0xA2 => syscall_port_inw(arg1),     // IN word
        0xA3 => syscall_port_inl(arg1),     // IN dword
        0xA4 => syscall_port_outb(arg1, arg2), // OUT byte
        0xA5 => syscall_port_outw(arg1, arg2), // OUT word
        0xA6 => syscall_port_outl(arg1, arg2), // OUT dword
        // Phase 10: IRQ routing for WASM drivers
        0xA7 => syscall_bind_irq(arg1, arg2),  // Bind IRQ vector to task
        0xA8 => syscall_ack_irq(arg1),          // Acknowledge IRQ (unmask)
        0xA9 => syscall_check_irq(arg1),         // Check if IRQ fired (non-blocking)
        // Phase 10: DMA + IOMMU
        0xAA => syscall_dma_alloc(arg1, arg2),   // Allocate DMA buffer (size, vaddr)
        0xAB => syscall_iommu_status(),            // Query IOMMU availability
        // Phase 11: WASM Network Driver Bridge
        0xAC => syscall_net_register(arg1, arg2),    // Register WASM net driver (mac_hi, mac_lo)
        0xAD => syscall_net_submit_rx(arg1, arg2),   // Submit received packet (vaddr, len)
        0xAE => syscall_net_poll_tx(arg1, arg2),     // Poll for TX packet (vaddr, max_len)
        0xAF => syscall_dma_sync_read(arg1, arg2),  // Read physical memory via HHDM
        0xB0 => syscall_net_dma_rx(arg1, arg2),     // Kernel-assisted RX: read DMA + deliver to smoltcp
        0xB1 => syscall_dma_sync_write(arg1, arg2), // Write to physical memory via HHDM
        0xB2 => syscall_net_metrics(arg1, arg2),    // OS metrics for AI introspection
        // ── WebSocket (0xC0-0xC3) ───────────────────────────────────────
        // NOTE: These were originally at 0xA0-0xA3 but collided with PCI
        // enumerate / port I/O. Moved to 0xC0-0xC3 in Phase B4. libfolk's
        // ws_connect/send/poll_recv/close were updated to match.
        //
        // WebSocket: connect to server
        // arg1 = packed IP (a | b<<8 | c<<16 | d<<24), arg2 = port | (path_len << 16)
        // arg3 = ptr to "host\0path" string
        // Returns: slot_id (0-3) or u64::MAX on error
        0xC0 => {
            let ip = [arg1 as u8, (arg1 >> 8) as u8, (arg1 >> 16) as u8, (arg1 >> 24) as u8];
            let port = (arg2 & 0xFFFF) as u16;
            let path_len = ((arg2 >> 16) & 0xFFFF) as usize;
            let ptr = arg3 as *const u8;
            if ptr.is_null() || arg3 < 0x200000 { u64::MAX } else {
                let data = unsafe { core::slice::from_raw_parts(ptr, path_len.min(256)) };
                // Split at first null byte: host\0path
                let split = data.iter().position(|&b| b == 0).unwrap_or(data.len());
                let host = core::str::from_utf8(&data[..split]).unwrap_or("localhost");
                let path = if split + 1 < data.len() {
                    core::str::from_utf8(&data[split+1..]).unwrap_or("/")
                } else { "/" };
                match crate::net::websocket::ws_connect(ip, port, host, path) {
                    Ok(id) => id as u64,
                    Err(_) => u64::MAX,
                }
            }
        },
        // WebSocket: send text data
        // arg1 = slot_id, arg2 = data_ptr, arg3 = data_len
        // Returns: 0 on success, u64::MAX on error
        0xC1 => {
            let ptr = arg2 as *const u8;
            let len = (arg3 as usize).min(8192);
            if ptr.is_null() || arg2 < 0x200000 || len == 0 { u64::MAX } else {
                let data = unsafe { core::slice::from_raw_parts(ptr, len) };
                match crate::net::websocket::ws_send(arg1 as u8, data) {
                    Ok(()) => 0,
                    Err(_) => u64::MAX,
                }
            }
        },
        // WebSocket: non-blocking receive poll
        // arg1 = slot_id, arg2 = buf_ptr, arg3 = max_len
        // Returns: bytes read (0 = nothing yet, u64::MAX-1 = closed/error)
        0xC2 => {
            let ptr = arg2 as *mut u8;
            let max = (arg3 as usize).min(8192);
            if ptr.is_null() || arg2 < 0x200000 { u64::MAX } else {
                let buf = unsafe { core::slice::from_raw_parts_mut(ptr, max) };
                let result = crate::net::websocket::ws_poll_recv(arg1 as u8, buf);
                if result < 0 { u64::MAX } else { result as u64 }
            }
        },
        // WebSocket: close connection
        // arg1 = slot_id
        0xC3 => {
            crate::net::websocket::ws_close(arg1 as u8);
            0
        },

        // Telemetry Ring: record app-level event for AutoDream pattern mining
        // arg1 = action_type (u8), arg2 = target_id (u32), arg3 = duration_ms (u32)
        0x9B => {
            crate::drivers::telemetry::record(
                crate::drivers::telemetry::ActionType::from_u8(arg1 as u8),
                arg2 as u32,
                arg3 as u32,
            );
            0
        },
        // Telemetry Ring: drain all events to userspace buffer (AutoDream)
        // arg1 = buf_ptr, arg2 = max_events
        // Returns: number of events drained
        0x9C => {
            crate::drivers::telemetry::drain_to_user(arg1 as usize, arg2 as usize) as u64
        },
        // Telemetry Ring: get stats (pending, total, overflow)
        // Returns: pending in bits 0-15, total in bits 16-31, overflow in bits 32-47
        0x9D => {
            let (pending, total, overflow) = crate::drivers::telemetry::stats();
            (pending as u64) | ((total as u64) << 16) | ((overflow as u64) << 32)
        },

        _ => {
            crate::drivers::serial::write_str("[HANDLER] Invalid syscall!\n");
            u64::MAX // Return error
        }
    };

    // WORKAROUND: Save result to static because RAX is being clobbered
    // somewhere between function return and assembly reading it
    // Store for ALL syscalls so get_result_fn always returns the right value
    unsafe { SYSCALL_RESULT = result; }

    result
}
