//! Interaction Quality Engine (IQE) — Lock-free telemetry ring buffer
//!
//! Records raw TSC timestamps at critical points in the input→render→display
//! pipeline. Kernel records only raw data; userspace computes derived metrics.
//!
//! Architecture: Single-Producer Single-Consumer (SPSC) lock-free ring.
//! Producer: ISR/kernel code (mouse IRQ, GPU flush, fence complete)
//! Consumer: userspace via SYS_IQE_READ syscall (0x91)

use core::sync::atomic::{AtomicUsize, Ordering};

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

            // Validate userspace address (basic bounds check)
            if buf_vaddr < 0x1000 || buf_vaddr > 0x7FFF_FFFF_FFFF {
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
