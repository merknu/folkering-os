//! Interaction Quality Engine (IQE) — Lock-free telemetry ring buffer
//!
//! Records raw TSC timestamps at critical points in the input→render→display
//! pipeline. Kernel records only raw data; userspace computes derived metrics.
//!
//! Architecture: Single-Producer Single-Consumer (SPSC) lock-free ring.
//! Producer: ISR/kernel code (mouse IRQ, GPU flush, fence complete)
//! Consumer: userspace via SYS_IQE_READ syscall (0x91)

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

// ── TSC (Time Stamp Counter) ────────────────────────────────────────────────

/// TSC ticks per microsecond, calibrated at boot via PIT Channel 2.
static TSC_TICKS_PER_US: AtomicU64 = AtomicU64::new(0);

/// Read TSC — CPU-cycle precision, ~0.5ns per tick.
#[inline(always)]
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Get calibrated TSC ticks per microsecond (0 if not yet calibrated).
pub fn tsc_ticks_per_us() -> u64 {
    TSC_TICKS_PER_US.load(Ordering::Relaxed)
}

/// Calibrate TSC frequency using PIT Channel 2 (hardware polling, no interrupts).
///
/// PIT Channel 2 is used because it can be polled via port 0x61 bit 5,
/// requiring NO interrupt delivery — bypassing the WHPX "Timer Death Bug"
/// where APIC timer stops after one tick.
pub fn calibrate_tsc() {
    const PIT_FREQ: u64 = 1_193_182;
    const DELAY_MS: u64 = 10;
    const PIT_COUNT: u16 = ((PIT_FREQ * DELAY_MS) / 1000) as u16;

    // Interrupts should already be disabled during early boot.
    // Do NOT call cli/sti here — caller manages interrupt state.
    unsafe {
        // Configure PIT Channel 2: mode 0 (one-shot), lobyte/hibyte
        x86_64::instructions::port::Port::<u8>::new(0x43).write(0b10110000);
        x86_64::instructions::port::Port::<u8>::new(0x42).write((PIT_COUNT & 0xFF) as u8);
        x86_64::instructions::port::Port::<u8>::new(0x42).write((PIT_COUNT >> 8) as u8);

        // Start PIT Channel 2: set GATE high (bit 0 of port 0x61)
        let port61_val = x86_64::instructions::port::Port::<u8>::new(0x61).read();
        x86_64::instructions::port::Port::<u8>::new(0x61).write((port61_val & 0xFC) | 0x01);

        let tsc_start = rdtsc();

        // Poll PIT Channel 2 output (bit 5 of port 0x61 goes high when
        // done). Capped at 10M iterations. Each iteration is one x86
        // `IN` from port 0x61, which is bus-serialised and costs on
        // the order of ~1 µs on real hardware (and several hundred ns
        // even under fast hypervisors), so the cap covers comfortably
        // more than the `DELAY_MS` (= 10) PIT window above. If we
        // exit without seeing the done bit, calibration is unreliable
        // and we hard-code a default (Issue #56 follow-up).
        let mut calibrated = false;
        for _ in 0..10_000_000u64 {
            let status = x86_64::instructions::port::Port::<u8>::new(0x61).read();
            if status & 0x20 != 0 { calibrated = true; break; }
        }

        let tsc_end = rdtsc();

        // Restore port 0x61
        x86_64::instructions::port::Port::<u8>::new(0x61).write(port61_val);

        let ticks_per_us = if calibrated {
            (tsc_end - tsc_start) / (DELAY_MS * 1000)
        } else {
            // PIT didn't report done — fall back to 3 GHz default.
            // Better a wrong-but-bounded value than a dead kernel.
            crate::serial_strln!("[IQE] PIT calibration timeout — defaulting to 3 GHz");
            3000
        };
        TSC_TICKS_PER_US.store(ticks_per_us, Ordering::Relaxed);

        // Be honest in the boot log about whether we actually
        // calibrated or fell back to the default — otherwise the
        // "TSC calibrated" message is misleading on hosts where the
        // PIT poll timed out.
        if calibrated {
            crate::serial_str!("[IQE] TSC calibrated: ");
        } else {
            crate::serial_str!("[IQE] TSC defaulted: ");
        }
        crate::drivers::serial::write_dec(TSC_TICKS_PER_US.load(Ordering::Relaxed) as u32);
        crate::serial_strln!(" ticks/us");
    }
}

/// IQE event types — what happened
#[repr(u8)]
#[derive(Clone, Copy)]
pub enum IqeEventType {
    /// PS/2 mouse IRQ12 fired. data = mouse buffer depth after push.
    MouseIrq = 0,
    /// VirtIO-GPU flush submitted to controlq. data = fence_id.
    GpuFlushSubmit = 1,
    /// VirtIO-GPU fence completed (host rendered). data = fence_id.
    FenceComplete = 2,
    /// Compositor frame started. data = frame number.
    FrameStart = 3,
    /// Compositor frame ended (after gpu_flush). data = frame number.
    FrameEnd = 4,
    /// Keyboard scancode received. data = scancode.
    KeyboardIrq = 5,
    /// Userspace read a key from kernel buffer (syscall_read_key). data = key.
    KeyboardRead = 6,
    /// Userspace read a mouse event from kernel buffer (syscall_read_mouse). data = 0.
    MouseRead = 7,
    /// Window operation started (open/close/drag). data = window_id.
    WindowOp = 8,
}

/// Raw telemetry event — 24 bytes, no derived metrics.
/// Kernel stores only: what happened, when (TSC), and context (data).
/// Userspace computes latencies, EWMA, scores.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IqeEvent {
    pub event_type: u8,
    pub _pad: [u8; 7],
    pub tsc: u64,
    pub data: u64,
}

const RING_SIZE: usize = 256; // Power of 2 for fast modulo
const RING_MASK: usize = RING_SIZE - 1;

/// Lock-free SPSC ring buffer.
/// Head: written by producer (ISR), read by consumer.
/// Tail: written by consumer (syscall), read by producer.
struct IqeRing {
    events: [IqeEvent; RING_SIZE],
    head: AtomicUsize, // Next write position (producer advances)
    tail: AtomicUsize, // Next read position (consumer advances)
}

static mut IQE_RING: IqeRing = IqeRing {
    events: [IqeEvent { event_type: 0, _pad: [0; 7], tsc: 0, data: 0 }; RING_SIZE],
    head: AtomicUsize::new(0),
    tail: AtomicUsize::new(0),
};

/// Record a telemetry event. Called from ISR or kernel code.
/// Lock-free: uses atomic head pointer. Silently drops if ring is full.
#[inline]
pub fn record(event_type: IqeEventType, tsc: u64, data: u64) {
    unsafe {
        let head = IQE_RING.head.load(Ordering::Relaxed);
        let tail = IQE_RING.tail.load(Ordering::Acquire);

        // Check if full: head is one behind tail (wrapped)
        let next_head = (head + 1) & RING_MASK;
        if next_head == tail {
            return; // Ring full — silently drop
        }

        // Write event at head position
        let slot = &mut IQE_RING.events[head];
        slot.event_type = event_type as u8;
        slot._pad = [0; 7];
        slot.tsc = tsc;
        slot.data = data;

        // Advance head (Release ensures event data is visible before head moves)
        IQE_RING.head.store(next_head, Ordering::Release);
    }
}

/// Read up to `max_count` events into a userspace buffer.
/// Returns number of events copied. Called from syscall handler.
pub fn read_to_user(buf_vaddr: usize, max_count: usize) -> usize {
    let event_size = core::mem::size_of::<IqeEvent>();
    let mut copied = 0usize;

    unsafe {
        let tail = IQE_RING.tail.load(Ordering::Relaxed);
        let head = IQE_RING.head.load(Ordering::Acquire);

        let mut pos = tail;
        while pos != head && copied < max_count {
            let event = &IQE_RING.events[pos];
            let dest = (buf_vaddr + copied * event_size) as *mut IqeEvent;

            // Validate userspace address (must be in user half, not kernel)
            if buf_vaddr < 0x1000 || buf_vaddr >= 0xFFFF_8000_0000_0000 {
                break;
            }

            core::ptr::write(dest, *event);
            pos = (pos + 1) & RING_MASK;
            copied += 1;
        }

        // Advance tail (Release ensures we've finished reading before advancing)
        IQE_RING.tail.store(pos, Ordering::Release);
    }

    copied
}

/// Get number of events available to read (non-consuming peek).
pub fn available() -> usize {
    unsafe {
        let head = IQE_RING.head.load(Ordering::Acquire);
        let tail = IQE_RING.tail.load(Ordering::Relaxed);
        (head.wrapping_sub(tail)) & RING_MASK
    }
}
