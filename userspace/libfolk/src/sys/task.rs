//! Task management syscalls
//!
//! Functions for controlling the current task's execution.

use crate::syscall::{syscall0, syscall1, syscall2, syscall3, syscall4, syscall6, SYS_EXIT, SYS_YIELD, SYS_GET_PID, SYS_SPAWN, SYS_PARALLEL_GEMM, SYS_ASK_GEMINI, SYS_GPU_FLUSH, SYS_GPU_INFO, SYS_COM3_READ};

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
