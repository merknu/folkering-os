//! Task management syscalls
//!
//! Functions for controlling the current task's execution.

use crate::syscall::{syscall0, syscall1, syscall2, syscall3, syscall4, syscall5, syscall6, SYS_EXIT, SYS_YIELD, SYS_GET_PID, SYS_SPAWN, SYS_PARALLEL_GEMM, SYS_ASK_GEMINI, SYS_GPU_FLUSH, SYS_GPU_INFO, SYS_COM3_READ};

/// Exit the current task with the given exit code
///
/// This function never returns.
pub fn exit(code: u64) -> ! {
    unsafe { syscall1(SYS_EXIT, code) };
    // Should never reach here, but just in case
    loop {
        core::hint::spin_loop();
    }
}

/// Voluntarily yield the CPU to other tasks
///
/// This allows the scheduler to run other tasks. The current task
/// will be resumed later when the scheduler selects it again.
pub fn yield_cpu() {
    unsafe { syscall0(SYS_YIELD) };
}

/// Get the current task's process ID
pub fn get_pid() -> u32 {
    unsafe { syscall0(SYS_GET_PID) as u32 }
}

/// Spawn a new task from an ELF binary
///
/// # Arguments
/// * `binary` - The ELF binary data
///
/// # Returns
/// * `Some(task_id)` - The new task's ID on success
/// * `None` - On failure
pub fn spawn(binary: &[u8]) -> Option<u32> {
    let ptr = binary.as_ptr() as u64;
    let len = binary.len() as u64;
    let ret = unsafe { syscall2(SYS_SPAWN, ptr, len) };
    if ret == u64::MAX {
        None
    } else {
        Some(ret as u32)
    }
}

/// Dispatch parallel GEMM across AP compute workers.
/// Returns true on success (APs available), false on failure (fallback to sequential).
pub fn parallel_gemm(
    input: *const f32,
    weights: *const u8,
    output: *mut f32,
    k: usize,
    n: usize,
    quant_type: u8,
) -> bool {
    let ret = unsafe {
        syscall6(
            SYS_PARALLEL_GEMM,
            input as u64,
            weights as u64,
            output as u64,
            k as u64,
            n as u64,
            quant_type as u64,
        )
    };
    ret == 0
}

/// Ask Gemini cloud API. Returns number of bytes written to response_buf,
/// or 0 on error. The response_buf should be at least 128KB.
pub fn ask_gemini(prompt: &str, response_buf: &mut [u8]) -> usize {
    let ret = unsafe {
        syscall3(
            SYS_ASK_GEMINI,
            prompt.as_ptr() as u64,
            prompt.len() as u64,
            response_buf.as_mut_ptr() as u64,
        )
    };
    if ret == u64::MAX { 0 } else { ret as usize }
}

/// Query an NTP server for current Unix time.
/// server_ip is the resolved IP (use dns_lookup first).
/// Returns Unix timestamp (seconds since 1970-01-01 UTC) or 0 on failure.
pub fn ntp_query(server_ip: [u8; 4]) -> u64 {
    let packed = ((server_ip[0] as u64) << 24)
        | ((server_ip[1] as u64) << 16)
        | ((server_ip[2] as u64) << 8)
        | (server_ip[3] as u64);
    unsafe { syscall1(0x5C, packed) }
}

/// Play raw PCM audio (16-bit signed stereo @ 44100Hz).
/// Returns true on success.
pub fn audio_play(samples: &[i16]) -> bool {
    let ret = unsafe {
        syscall2(
            0x5A, // SYS_AUDIO_PLAY
            samples.as_ptr() as u64,
            samples.len() as u64,
        )
    };
    ret == 0
}

/// Beep — generate a 440Hz tone for the given duration (ms).
pub fn audio_beep(duration_ms: u32) -> bool {
    let ret = unsafe {
        syscall1(0x5B, duration_ms as u64)
    };
    ret == 0
}

/// Send a UDP packet to target IP:port. Returns true on success.
/// Max payload: 1472 bytes (MTU - IP - UDP headers).
pub fn udp_send(target_ip: [u8; 4], target_port: u16, data: &[u8]) -> bool {
    let packed = ((target_ip[0] as u64) << 24)
        | ((target_ip[1] as u64) << 16)
        | ((target_ip[2] as u64) << 8)
        | (target_ip[3] as u64);
    let ret = unsafe {
        syscall4(
            0x58, // SYS_UDP_SEND
            packed,
            target_port as u64,
            data.as_ptr() as u64,
            data.len() as u64,
        )
    };
    ret == 0
}

/// Send a UDP packet and wait for one response. Returns bytes received.
/// Max payload: 1472 bytes. Max response: 4096 bytes.
pub fn udp_send_recv(
    target_ip: [u8; 4],
    target_port: u16,
    data: &[u8],
    response: &mut [u8],
    timeout_ms: u32,
) -> usize {
    let packed = ((target_ip[0] as u64) << 24)
        | ((target_ip[1] as u64) << 16)
        | ((target_ip[2] as u64) << 8)
        | (target_ip[3] as u64);
    // Pack response_len (low 32) and timeout_ms (high 32) into one u64
    let resp_arg = (response.len() as u64) | ((timeout_ms as u64) << 32);
    let ret = unsafe {
        syscall6(
            0x59, // SYS_UDP_SEND_RECV
            packed,
            target_port as u64,
            data.as_ptr() as u64,
            data.len() as u64,
            response.as_mut_ptr() as u64,
            resp_arg,
        )
    };
    if ret == u64::MAX { 0 } else { ret as usize }
}

/// Direct HTTP(S) fetch via kernel TLS stack. No proxy needed.
/// Returns bytes written to response_buf, or 0 on error.
pub fn http_fetch(url: &str, response_buf: &mut [u8]) -> usize {
    let ret = unsafe {
        syscall4(
            0x57, // SYS_HTTP_FETCH
            url.as_ptr() as u64,
            url.len() as u64,
            response_buf.as_mut_ptr() as u64,
            response_buf.len() as u64,
        )
    };
    if ret == u64::MAX { 0 } else { ret as usize }
}

/// Fetch an FBP-encoded DOM snapshot from the host-side folkering-proxy.
///
/// The kernel opens a plain TCP connection to the QEMU SLIRP gateway
/// (10.0.2.2:14711 — which maps to the host's 127.0.0.1:14711), sends
/// `NAVIGATE <url>\n`, reads the `[u32 length][FBP bytes]` reply,
/// strips the length prefix, and writes the raw FBP payload bytes
/// into `response_buf`.
///
/// Returns the number of FBP bytes written, or 0 on any error.
pub fn fbp_request(url: &str, response_buf: &mut [u8]) -> usize {
    let ret = unsafe {
        syscall4(
            0x5E, // SYS_FBP_REQUEST
            url.as_ptr() as u64,
            url.len() as u64,
            response_buf.as_mut_ptr() as u64,
            response_buf.len() as u64,
        )
    };
    if ret == u64::MAX { 0 } else { ret as usize }
}

/// FBP interact: send an INTERACTION_EVENT to the host-side proxy
/// on the same TCP session as a NAVIGATE, then read back the
/// post-click DOM snapshot.
///
/// `action` is an `ACTION_*` constant from fbp_rs (e.g.
/// `ACTION_CLICK = 0x01`). `node_id` is the 1-based element index
/// in the most recent DOM_STATE_UPDATE. Returns the number of FBP
/// bytes written to `response_buf`, or 0 on error.
pub fn fbp_interact(
    url: &str,
    action: u8,
    node_id: u32,
    response_buf: &mut [u8],
) -> usize {
    let action_and_node = (action as u64) | ((node_id as u64) << 8);
    let ret = unsafe {
        syscall5(
            0x5F, // SYS_FBP_INTERACT
            url.as_ptr() as u64,
            url.len() as u64,
            response_buf.as_mut_ptr() as u64,
            response_buf.len() as u64,
            action_and_node,
        )
    };
    if ret == u64::MAX { 0 } else { ret as usize }
}

/// Phase 11 — Draug source-patch channel.
///
/// Ships a Rust source file to the host-side proxy, which writes it
/// into the `draug-sandbox` crate and runs `cargo check` to validate
/// it. Returns `PatchStatus` with the status code from the proxy
/// (0 = OK, 1 = BUILD_FAILED, 2 = BAD_FILENAME, …) and the number of
/// bytes written to `result_buf` (cargo's stderr on failure, compiler
/// summary on success). Returns `None` on IPC/TCP failure.
#[derive(Debug, Clone, Copy)]
pub struct PatchStatus {
    pub status: u32,
    pub output_len: usize,
}

/// Phase 12 — Generative LLM gateway.
///
/// Ships a prompt to the host-side proxy's `LLM <model>` command,
/// which POSTs it to the local Ollama `/api/generate` endpoint and
/// returns the raw response text. Writes up to `result_buf.len()`
/// bytes of the LLM's reply into `result_buf`.
///
/// Returns `PatchStatus` (reused as the generic status/output_len
/// carrier) with the proxy's LLM status code and the number of
/// response bytes actually written. `None` on IPC/TCP failure.
pub fn llm_generate(
    model: &str,
    prompt: &str,
    result_buf: &mut [u8],
) -> Option<PatchStatus> {
    let model_len = model.len() as u64;
    let prompt_len = prompt.len() as u64;
    let result_max = result_buf.len() as u64;
    let mask = 0x1F_FFFFu64; // 21 bits each — matches the kernel unpacker
    let packed = (model_len & mask)
        | ((prompt_len & mask) << 21)
        | ((result_max & mask) << 42);

    let ret = unsafe {
        syscall4(
            0x62, // SYS_LLM_GENERATE
            model.as_ptr() as u64,
            prompt.as_ptr() as u64,
            result_buf.as_mut_ptr() as u64,
            packed,
        )
    };
    if ret == u64::MAX {
        return None;
    }
    let status = ((ret >> 32) & 0xFFFF_FFFF) as u32;
    let output_len = (ret & 0xFFFF_FFFF) as usize;
    Some(PatchStatus { status, output_len })
}

/// Folkering CodeGraph — query callers of a function via the proxy.
///
/// Sends `GRAPH_CALLERS <name>\n` over the kernel's syscall 0x65 to
/// the host-side proxy. The proxy reads from a pre-loaded FCG1 blob
/// (see `tools/folkering-codegraph/dump-graph`) and answers in
/// microseconds — far faster than asking the LLM the same question.
///
/// Reuses [`PatchStatus`] as the generic (status, output_len) carrier:
///   status 0 = OK         — `result_buf[..output_len]` is callers,
///                           one qualified name per line, terminated
///                           with `\n`. Empty output means the fn
///                           exists but has no callers.
///   status 1 = NOT_FOUND  — the name isn't in the call-graph
///   status 2 = NOT_LOADED — the proxy started without --codegraph
///
/// Returns `None` on TCP/syscall failure (see kernel serial log for
/// reason). Caller is responsible for parsing the line-separated body.
pub fn graph_callers(
    fn_name: &str,
    result_buf: &mut [u8],
) -> Option<PatchStatus> {
    let name_len = fn_name.len() as u64;
    let result_max = result_buf.len() as u64;
    let mask = 0x1F_FFFFu64; // 21 bits each — matches the kernel unpacker
    let packed = (name_len & mask) | ((result_max & mask) << 21);

    let ret = unsafe {
        syscall4(
            0x65, // SYS_GRAPH_CALLERS
            fn_name.as_ptr() as u64,
            result_buf.as_mut_ptr() as u64,
            0, // unused, kept 0 for ABI symmetry with llm_generate
            packed,
        )
    };
    if ret == u64::MAX {
        return None;
    }
    let status = ((ret >> 32) & 0xFFFF_FFFF) as u32;
    let output_len = (ret & 0xFFFF_FFFF) as usize;
    Some(PatchStatus { status, output_len })
}

/// Phase 17 — autonomous-refactor cargo check.
///
/// Ship a candidate refactor (`content`) for the file at
/// `target_file` (repo-relative, e.g. `kernel/src/memory/physical.rs`)
/// to the host-side proxy's `CARGO_CHECK` command. The proxy
/// overwrites the file in the live tree, runs `cargo check` in the
/// owning workspace, restores the original, and returns:
///
///   `PatchStatus { status, output_len }`
///
/// where `status` is one of `CC_STATUS_*` from the proxy's
/// `cargo_check.rs` (mirrored here as constants). `result_buf` is
/// filled with up to `output_len` bytes of stderr excerpt.
///
/// Sister of `fbp_patch` — same packed-lengths ABI — but operates
/// on real OS source paths instead of the draug-sandbox crate, so
/// Draug can verify a refactor against actual callers.
///
/// Returns `None` on TCP/syscall failure.
pub fn cargo_check(
    target_file: &str,
    content: &[u8],
    result_buf: &mut [u8],
) -> Option<PatchStatus> {
    let target_len = target_file.len() as u64;
    let content_len = content.len() as u64;
    let result_max = result_buf.len() as u64;
    let mask = 0x1F_FFFFu64; // 21 bits — matches kernel unpacker
    let packed = (target_len & mask)
        | ((content_len & mask) << 21)
        | ((result_max & mask) << 42);

    let ret = unsafe {
        syscall4(
            0x66, // SYS_CARGO_CHECK
            target_file.as_ptr() as u64,
            content.as_ptr() as u64,
            result_buf.as_mut_ptr() as u64,
            packed,
        )
    };
    if ret == u64::MAX {
        return None;
    }
    let status = ((ret >> 32) & 0xFFFF_FFFF) as u32;
    let output_len = (ret & 0xFFFF_FFFF) as usize;
    Some(PatchStatus { status, output_len })
}

/// CARGO_CHECK status codes (mirror `CC_STATUS_*` in
/// `folkering-proxy/src/cargo_check.rs`). 0 = OK, 1 = BUILD_FAILED,
/// 2 = BAD_PATH, 3 = IO_ERROR, 4 = CHECK_TIMEOUT, 5 = TOO_LARGE,
/// 6 = NOT_CONFIGURED.
pub const CC_STATUS_OK: u32             = 0;
pub const CC_STATUS_BUILD_FAILED: u32   = 1;
pub const CC_STATUS_BAD_PATH: u32       = 2;
pub const CC_STATUS_IO_ERROR: u32       = 3;
pub const CC_STATUS_CHECK_TIMEOUT: u32  = 4;
pub const CC_STATUS_TOO_LARGE: u32      = 5;
pub const CC_STATUS_NOT_CONFIGURED: u32 = 6;

/// Phase 17 — fetch the original source of a real OS file.
///
/// Companion to `cargo_check`: Draug needs to read the current text
/// of a target file before she can build a refactor prompt, but the
/// bare-metal OS has no host filesystem. This wrapper ships a
/// `FETCH_SOURCE <path>` request to the proxy and reads back the
/// raw bytes into `result_buf`.
///
/// Returns `Some(PatchStatus { status, output_len })` on success,
/// where `status` is one of `FS_STATUS_*` (0 = OK, 1 = BAD_PATH,
/// 2 = NOT_FOUND, 3 = IO_ERROR, 4 = TOO_LARGE, 5 = NOT_CONFIGURED).
/// Returns `None` on TCP/syscall failure.
pub fn fetch_source(
    target_file: &str,
    result_buf: &mut [u8],
) -> Option<PatchStatus> {
    let target_len = target_file.len() as u64;
    let result_max = result_buf.len() as u64;
    let mask = 0x1F_FFFFu64;
    let packed = (target_len & mask) | ((result_max & mask) << 21);

    let ret = unsafe {
        syscall4(
            0x67, // SYS_FETCH_SOURCE
            target_file.as_ptr() as u64,
            result_buf.as_mut_ptr() as u64,
            0, // unused, kept 0 for ABI symmetry
            packed,
        )
    };
    if ret == u64::MAX {
        return None;
    }
    let status = ((ret >> 32) & 0xFFFF_FFFF) as u32;
    let output_len = (ret & 0xFFFF_FFFF) as usize;
    Some(PatchStatus { status, output_len })
}

/// FETCH_SOURCE status codes (mirror `FS_STATUS_*` in
/// `folkering-proxy/src/fetch_source.rs`).
pub const FS_STATUS_OK: u32             = 0;
pub const FS_STATUS_BAD_PATH: u32       = 1;
pub const FS_STATUS_NOT_FOUND: u32      = 2;
pub const FS_STATUS_IO_ERROR: u32       = 3;
pub const FS_STATUS_TOO_LARGE: u32      = 4;
pub const FS_STATUS_NOT_CONFIGURED: u32 = 5;

pub fn fbp_patch(
    filename: &str,
    content: &[u8],
    result_buf: &mut [u8],
) -> Option<PatchStatus> {
    // The kernel syscall entry asm only reliably passes 5 args
    // (arg0..arg4 via rdi/rsi/rdx/r10/r8); `r9` gets clobbered by
    // the rearrangement pass and arg6 is never loaded from the stack.
    // So we pack all three lengths into one u64 (21 bits each) and
    // use a 4-arg syscall.
    let filename_len = filename.len() as u64;
    let content_len = content.len() as u64;
    let result_max = result_buf.len() as u64;
    let mask = 0x1F_FFFFu64; // 21 bits
    let packed = (filename_len & mask)
        | ((content_len & mask) << 21)
        | ((result_max & mask) << 42);

    let ret = unsafe {
        syscall4(
            0x61, // SYS_FBP_PATCH
            filename.as_ptr() as u64,
            content.as_ptr() as u64,
            result_buf.as_mut_ptr() as u64,
            packed,
        )
    };
    if ret == u64::MAX {
        return None;
    }
    let status = ((ret >> 32) & 0xFFFF_FFFF) as u32;
    let output_len = (ret & 0xFFFF_FFFF) as usize;
    Some(PatchStatus { status, output_len })
}

// ── Async TCP (non-blocking, for Draug state machine) ────────────

/// EAGAIN sentinel — means "not ready, try next frame"
pub const TCP_EAGAIN: u64 = 0xFFFF_FFFE;

/// Start a non-blocking TCP connection. Returns slot_id, EAGAIN, or MAX.
pub fn tcp_connect_async(ip: [u8; 4], port: u16) -> u64 {
    let packed = ((ip[0] as u64) << 24) | ((ip[1] as u64) << 16)
        | ((ip[2] as u64) << 8) | (ip[3] as u64);
    unsafe { crate::syscall::syscall2(0xE0, packed, port as u64) }
}

/// Non-blocking send. Returns bytes_sent, EAGAIN, or MAX.
pub fn tcp_send_async(slot: u64, data: &[u8]) -> u64 {
    unsafe { crate::syscall::syscall3(0xE1, slot, data.as_ptr() as u64, data.len() as u64) }
}

/// Non-blocking receive. Returns bytes_read, EAGAIN, 0 (peer closed), or MAX.
pub fn tcp_poll_recv(slot: u64, buf: &mut [u8]) -> u64 {
    unsafe { crate::syscall::syscall3(0xE2, slot, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

/// Close async TCP connection.
pub fn tcp_close_async(slot: u64) {
    unsafe { crate::syscall::syscall1(0xE3, slot); }
}

/// JIT-compile an MLP to AArch64 and execute on Pi.
/// ip: packed IPv4 (e.g. [192,168,68,50] → 0x3244A8C0)
/// port: daemon port (0 = default 7700)
/// Returns: exit code from Pi, or u64::MAX on error.
pub fn jit_exec_mlp(ip: [u8; 4], port: u16) -> u64 {
    let ip_packed = ip[0] as u64
        | ((ip[1] as u64) << 8)
        | ((ip[2] as u64) << 16)
        | ((ip[3] as u64) << 24);
    unsafe { crate::syscall::syscall2(0xE4, ip_packed, port as u64) }
}

/// Draug Bridge — push status to kernel tcp_shell atomics.
/// Returns true if the remote shell has set the pause flag.
pub fn draug_bridge_update(
    iter: u32, passed: u32, failed: u32, retries: u32,
    l1: u8, l2: u8, l3: u8, plan_mode: u8,
    complex_idx: u8, hibernating: u8, consec_skips: u8,
) -> bool {
    let arg1 = ((iter as u64) << 32) | (passed as u64);
    let arg2 = ((failed as u64) << 32) | (retries as u64);
    let arg3 = ((l1 as u64) << 24) | ((l2 as u64) << 16) | ((l3 as u64) << 8) | (plan_mode as u64);
    let arg4 = ((complex_idx as u64) << 16) | ((hibernating as u64) << 8) | (consec_skips as u64);
    let ret = unsafe { crate::syscall::syscall4(0xD0, arg1, arg2, arg3, arg4) };
    ret != 0 // 1 = paused
}

/// Draug Bridge — set current task name for TCP shell display.
pub fn draug_bridge_set_task(name: &str) {
    let len = name.len().min(31);
    unsafe { crate::syscall::syscall2(0xD1, name.as_ptr() as u64, len as u64); }
}

/// Stability Fix 7 — Proxy health check (TCP).
/// Returns true if the proxy is reachable via TCP (~2s timeout).
/// Note: shares smoltcp TCP state with Phase 17 — under a TCP wedge
/// (Issue #58) this returns false even when the proxy is up. See
/// `proxy_ping_udp()` for an independent UDP-based recovery probe.
pub fn proxy_ping() -> bool {
    unsafe { crate::syscall::syscall0(0x64) == 1 }
}

/// Issue #55 — query the proxy for our most recent cached verdict.
///
/// Returns `Some(PatchStatus { status, output_len })` if the proxy
/// has a cached verdict for our source IP. Returns `None` on
/// transport failure or cache miss. The verdict body is written
/// into `buf` (caller must allocate at least 16 KB).
pub fn proxy_last_verdict(buf: &mut [u8]) -> Option<PatchStatus> {
    let ret = unsafe {
        crate::syscall::syscall2(
            0x6A,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    };
    if ret == u64::MAX { return None; }
    let status = ((ret >> 32) & 0xFFFF_FFFF) as u32;
    let output_len = (ret & 0xFFFF_FFFF) as usize;
    Some(PatchStatus { status, output_len })
}

/// PATCH_DEDUP — query the proxy for a cached verdict by content hash.
///
/// Daemon hashes the source it's about to ship (SHA-256, hex-encoded
/// 64 chars) and asks the proxy "have you compiled this exact code
/// before?". On hit, the cached PATCH-wire bytes land in `buf` and
/// the daemon can dispatch them directly through its existing
/// `process_patch_result` path — saving a full cargo cycle.
///
/// Returns `Some(PatchStatus { status, output_len })` on hit, `None`
/// on miss / transport failure / old proxy that doesn't understand
/// the command. Treat all `None` paths identically: fall back to a
/// regular PATCH request.
///
/// `hash_hex` must be exactly 64 lowercase hex chars; anything else
/// fails fast at the syscall.
pub fn proxy_patch_dedup(hash_hex: &str, buf: &mut [u8]) -> Option<PatchStatus> {
    if hash_hex.len() != 64 { return None; }
    let ret = unsafe {
        crate::syscall::syscall4(
            0x6C,
            hash_hex.as_ptr() as u64,
            hash_hex.len() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    };
    if ret == u64::MAX { return None; }
    let status = ((ret >> 32) & 0xFFFF_FFFF) as u32;
    let output_len = (ret & 0xFFFF_FFFF) as usize;
    Some(PatchStatus { status, output_len })
}

/// Issue #55 — explicit ACK that the daemon has persisted a verdict.
///
/// Tells the proxy to drop its cached per-source-IP verdict. Call
/// this AFTER `save_state` / Synapse persist completes successfully
/// — at that point the daemon no longer needs the proxy's safety
/// net for this task, and the cache slot can be reused.
///
/// Returns `true` if the proxy ACK'd (or had nothing cached). Soft
/// fails to `false` on transport error; the proxy's 30-day TTL
/// backstop garbage-collects unack'd entries either way.
pub fn proxy_ack_verdict() -> bool {
    unsafe { crate::syscall::syscall0(0x6B) == 1 }
}

/// Issue #58 — Proxy health check (UDP).
/// Returns true if the proxy responds to a UDP "PING" with "PONG"
/// before the kernel-side probe expires. The kernel timeout is
/// expressed in `tsc_ms` units (calibrated TSC ticks divided down
/// to milliseconds), which is approximately wall-clock 1s on a
/// well-calibrated TSC; on a host where IQE fell back to the 3 GHz
/// default it can drift proportionally to the real CPU frequency.
/// Uses smoltcp's UDP socket type, a different code path than
/// `proxy_ping()` (TCP), so it can succeed when the TCP-side state
/// is wedged.
pub fn proxy_ping_udp() -> bool {
    unsafe { crate::syscall::syscall0(0x68) == 1 }
}

/// Phase 16 — WASM compilation.
///
/// Sends `WASM_COMPILE` to the proxy which compiles the sandbox to
/// `wasm32-unknown-unknown`. Returns the .wasm binary bytes in
/// `wasm_buf`. Returns `Some(PatchStatus)` where:
///   - status=0, output_len=wasm_bytes_written on success
///   - status>0 on error (output_len = error message length)
///   - None on TCP/IPC failure
pub fn wasm_compile(wasm_buf: &mut [u8]) -> Option<PatchStatus> {
    let ret = unsafe {
        crate::syscall::syscall2(
            0x63, // SYS_WASM_COMPILE
            wasm_buf.as_mut_ptr() as u64,
            wasm_buf.len() as u64,
        )
    };
    if ret == u64::MAX {
        return None;
    }
    let status = ((ret >> 32) & 0xFFFF_FFFF) as u32;
    let output_len = (ret & 0xFFFF_FFFF) as usize;
    Some(PatchStatus { status, output_len })
}

/// HTTP(S) POST via kernel TLS stack. Sends `body` as the request body
/// with `Content-Type: application/x-www-form-urlencoded`. Returns the
/// number of response-body bytes written to `response_buf`, or 0 on error.
pub fn http_post(url: &str, body: &[u8], response_buf: &mut [u8]) -> usize {
    let ret = unsafe {
        crate::syscall::syscall6(
            0x5D, // SYS_HTTP_POST
            url.as_ptr() as u64,
            url.len() as u64,
            body.as_ptr() as u64,
            body.len() as u64,
            response_buf.as_mut_ptr() as u64,
            response_buf.len() as u64,
        )
    };
    if ret == u64::MAX { 0 } else { ret as usize }
}

/// Flush GPU framebuffer dirty rectangle to display (fire-and-forget).
pub fn gpu_flush(x: u32, y: u32, w: u32, h: u32) {
    unsafe { syscall4(SYS_GPU_FLUSH, x as u64, y as u64, w as u64, h as u64); }
}

/// Flush GPU and wait for VSync (fence completion). CPU sleeps via HLT.
/// Blocks until the GPU has finished presenting the frame.
/// Use this instead of gpu_flush() for frame-paced rendering.
pub fn gpu_vsync(x: u32, y: u32, w: u32, h: u32) {
    unsafe { syscall4(0x82, x as u64, y as u64, w as u64, h as u64); }
}

/// Move hardware cursor to (x, y) via VirtIO-GPU VIRTQ 1.
/// This bypasses the controlq entirely — cursor position updates at 1000Hz
/// independently of the 2D render pipeline. No VM-Exit storm.
pub fn gpu_move_cursor(x: u32, y: u32) {
    unsafe { syscall2(0x85, x as u64, y as u64); }
}

/// IQE: Read telemetry events from kernel ring buffer.
/// Returns number of events copied. Each event is 24 bytes.
pub fn iqe_read(buf: &mut [u8], max_events: usize) -> usize {
    let ret = unsafe { syscall2(0x91, buf.as_mut_ptr() as u64, max_events as u64) };
    ret as usize
}

/// IQE: Get TSC ticks per microsecond (calibrated at boot).
pub fn iqe_tsc_freq() -> u64 {
    unsafe { syscall0(0x92) }
}

/// WebSocket: Connect to a WebSocket server.
/// Returns slot_id (0-3) on success, u64::MAX on error.
pub fn ws_connect(ip: [u8; 4], port: u16, host: &str, path: &str) -> Option<u8> {
    // Pack: "host\0path" into a buffer
    let mut buf = [0u8; 256];
    let hb = host.as_bytes();
    let pb = path.as_bytes();
    let total = hb.len() + 1 + pb.len();
    if total > 256 { return None; }
    buf[..hb.len()].copy_from_slice(hb);
    buf[hb.len()] = 0; // null separator
    buf[hb.len()+1..hb.len()+1+pb.len()].copy_from_slice(pb);

    let packed_ip = ip[0] as u64 | ((ip[1] as u64) << 8) | ((ip[2] as u64) << 16) | ((ip[3] as u64) << 24);
    let packed_port = port as u64 | ((total as u64) << 16);

    // Phase B4: moved from 0xA0 (collided with SYS_PCI_ENUMERATE) to 0xC0
    let ret = unsafe { syscall3(0xC0, packed_ip, packed_port, buf.as_ptr() as u64) };
    if ret == u64::MAX { None } else { Some(ret as u8) }
}

/// WebSocket: Send text data on a connection.
pub fn ws_send(slot_id: u8, data: &[u8]) -> bool {
    // Phase B4: moved from 0xA1 (collided with SYS_PORT_INB) to 0xC1
    let ret = unsafe { syscall3(0xC1, slot_id as u64, data.as_ptr() as u64, data.len() as u64) };
    ret == 0
}

/// WebSocket: Non-blocking receive poll. Returns bytes read (0 = nothing yet).
/// Returns None on connection closed/error.
pub fn ws_poll_recv(slot_id: u8, buf: &mut [u8]) -> Option<usize> {
    // Phase B4: moved from 0xA2 (collided with SYS_PORT_INW) to 0xC2
    let ret = unsafe { syscall3(0xC2, slot_id as u64, buf.as_mut_ptr() as u64, buf.len() as u64) };
    if ret == u64::MAX { None } else { Some(ret as usize) }
}

/// WebSocket: Close a connection.
pub fn ws_close(slot_id: u8) {
    // Phase B4: moved from 0xA3 (collided with SYS_PORT_INL) to 0xC3
    unsafe { syscall1(0xC3, slot_id as u64); }
}

/// Telemetry: Record an app-level event for AutoDream pattern mining.
/// action_type: 0=AppOpened, 1=AppClosed, 2=IpcMessageSent, 3=UiInteraction,
///   4=AiInferenceRequested, 5=AiInferenceCompleted, 6=FileAccessed,
///   7=FileWritten, 8=OmnibarCommand, 9=MetricAlert
pub fn telemetry_log(action_type: u8, target_id: u32, duration_ms: u32) {
    unsafe { syscall3(0x9B, action_type as u64, target_id as u64, duration_ms as u64); }
}

/// Telemetry: Drain all pending events to buffer (for AutoDream).
/// Returns number of events drained. Each event is 16 bytes.
pub fn telemetry_drain(buf: &mut [u8], max_events: usize) -> usize {
    let ret = unsafe { syscall2(0x9C, buf.as_mut_ptr() as u64, max_events as u64) };
    ret as usize
}

/// Telemetry: Get ring buffer stats.
/// Returns (pending_count, total_recorded, overflow_count).
pub fn telemetry_stats() -> (u32, u32, u32) {
    let packed = unsafe { syscall0(0x9D) };
    let pending = (packed & 0xFFFF) as u32;
    let total = ((packed >> 16) & 0xFFFF) as u32;
    let overflow = ((packed >> 32) & 0xFFFF) as u32;
    (pending, total, overflow)
}

/// Write bytes to COM3 via syscall 0x94.
pub fn com3_write(data: &[u8]) {
    unsafe { syscall2(0x94, data.as_ptr() as u64, data.len() as u64); }
}

/// Batched GPU flush: transfer N rects with 1 doorbell (1 VM-exit).
/// Each rect is (x, y, w, h) as u32. Max 4 rects.
pub fn gpu_flush_batch(rects: &[[u32; 4]]) {
    if rects.is_empty() { return; }
    unsafe { syscall2(0x95, rects.as_ptr() as u64, rects.len() as u64); }
}

/// Read Real-Time Clock (CMOS RTC). Returns packed DateTime.
/// Unpack: year=2000+(v>>26)&0x3F, month=(v>>22)&0xF, day=(v>>17)&0x1F,
///         hour=(v>>12)&0x1F, minute=(v>>6)&0x3F, second=v&0x3F
pub fn get_rtc_packed() -> u64 {
    unsafe { syscall0(0x83) }
}

/// Parsed date/time from RTC
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Read Real-Time Clock and return parsed DateTime
pub fn get_rtc() -> DateTime {
    let v = get_rtc_packed();
    DateTime {
        year: 2000 + ((v >> 26) & 0x3F) as u16,
        month: ((v >> 22) & 0x0F) as u8,
        day: ((v >> 17) & 0x1F) as u8,
        hour: ((v >> 12) & 0x1F) as u8,
        minute: ((v >> 6) & 0x3F) as u8,
        second: (v & 0x3F) as u8,
    }
}

/// Get system memory statistics: (total_mb, used_mb, usage_percent)
pub fn memory_stats() -> (u32, u32, u32) {
    let raw = unsafe { syscall0(0x84) };
    let total_pages = (raw >> 32) as u32;
    let free_pages = (raw & 0xFFFFFFFF) as u32;
    let total_mb = total_pages * 4 / 1024; // 4KB pages → MB
    let used_pages = total_pages.saturating_sub(free_pages);
    let used_mb = used_pages * 4 / 1024;
    let pct = if total_pages > 0 { (used_pages * 100 / total_pages) as u32 } else { 0 };
    (total_mb, used_mb, pct)
}

/// Get GPU info and map framebuffer at given virtual address.
/// Returns (width, height) on success, None if no GPU.
pub fn gpu_info(virt_addr: usize) -> Option<(u32, u32)> {
    let ret = unsafe { syscall1(SYS_GPU_INFO, virt_addr as u64) };
    if ret == u64::MAX {
        None
    } else {
        let w = (ret >> 32) as u32;
        let h = (ret & 0xFFFFFFFF) as u32;
        Some((w, h))
    }
}

/// Halt CPU until next interrupt (HLT). Wakes instantly on mouse/keyboard/timer IRQ.
/// Under WHPX, this causes a VM-exit so the hypervisor can inject pending interrupts.
/// Much better than spin_loop() which prevents interrupt delivery.
pub fn wait_for_irq() {
    unsafe { syscall0(0x99); }
}

/// Raw COM2 TX write — does NOT reset async RX state.
/// Used for ACK/NACK frames during active async sessions.
pub fn com2_write_raw(data: &[u8]) {
    unsafe { syscall2(0x9A, data.as_ptr() as u64, data.len() as u64); }
}

/// Async COM2: send request bytes (non-blocking). Starts async session.
pub fn com2_async_send(data: &[u8]) {
    unsafe { syscall2(0x96, data.as_ptr() as u64, data.len() as u64); }
}

/// Async COM2: poll for COBS frame (0x00 sentinel). Returns Some(len) if complete, None if waiting.
pub fn com2_async_poll() -> Option<usize> {
    let ret = unsafe { syscall1(0x97, 0) }; // 0 = COBS sentinel mode
    if ret == 0 { None } else { Some(ret as usize) }
}

/// Async COM2: poll for legacy @@END@@ delimiter. Returns Some(len) if complete, None if waiting.
pub fn com2_async_poll_legacy() -> Option<usize> {
    let ret = unsafe { syscall1(0x97, 1) }; // 1 = legacy mode
    if ret == 0 { None } else { Some(ret as usize) }
}

/// Async COM2: read completed response into buffer. Returns bytes copied.
pub fn com2_async_read(buf: &mut [u8]) -> usize {
    let ret = unsafe { syscall2(0x98, buf.as_mut_ptr() as u64, buf.len() as u64) };
    if ret == u64::MAX { 0 } else { ret as usize }
}

/// Read a byte from COM3 God Mode Pipe (non-blocking).
pub fn com3_read() -> Option<u8> {
    let ret = unsafe { syscall0(SYS_COM3_READ) };
    if ret == u64::MAX { None } else { Some(ret as u8) }
}
