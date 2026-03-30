//! Timer Subsystem — uptime tracking + TSC (Time Stamp Counter) utilities

use core::sync::atomic::{AtomicU64, Ordering};

static UPTIME_MS: AtomicU64 = AtomicU64::new(0);

/// TSC ticks per microsecond — calibrated at boot via PIT
static TSC_TICKS_PER_US: AtomicU64 = AtomicU64::new(0);

/// Get system uptime in milliseconds (10ms resolution)
pub fn uptime_ms() -> u64 {
    UPTIME_MS.load(Ordering::Relaxed)
}

/// Increment uptime (called by timer interrupt)
pub fn tick() {
    let ms = UPTIME_MS.fetch_add(10, Ordering::Relaxed);

    // Poll network stack every ~50ms (every 5th tick)
    if ms % 50 == 0 {
        crate::net::poll();
    }
}

// ── TSC (Time Stamp Counter) ────────────────────────────────────────────────

/// Read TSC — CPU-cycle precision. Returns raw cycle count.
#[inline(always)]
pub fn tsc_now() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Convert TSC delta to microseconds using calibrated frequency.
/// Returns 0 if TSC not yet calibrated.
#[inline(always)]
pub fn tsc_to_us(tsc_delta: u64) -> u64 {
    let tpu = TSC_TICKS_PER_US.load(Ordering::Relaxed);
    if tpu == 0 { return 0; }
    tsc_delta / tpu
}

/// Get calibrated TSC ticks per microsecond
pub fn tsc_frequency_mhz() -> u64 {
    TSC_TICKS_PER_US.load(Ordering::Relaxed)
}

/// Calibrate TSC frequency against the PIT (Programmable Interval Timer).
///
/// Uses PIT Channel 2 to measure a precise 10ms delay, then calculates
/// TSC ticks per microsecond. Must be called early in kernel boot with
/// interrupts disabled.
pub fn calibrate_tsc() {
    // PIT runs at 1.193182 MHz. For a 10ms delay:
    // count = 1_193_182 * 0.010 = 11932 ticks
    const PIT_FREQ: u64 = 1_193_182;
    const DELAY_MS: u64 = 10;
    const PIT_COUNT: u16 = ((PIT_FREQ * DELAY_MS) / 1000) as u16;

    unsafe {
        // Configure PIT Channel 2 for one-shot mode
        // Control word: channel 2, lobyte/hibyte, mode 0 (one-shot)
        x86_64::instructions::port::Port::<u8>::new(0x43).write(0b10110000);

        // Write count (low byte first, then high byte)
        x86_64::instructions::port::Port::<u8>::new(0x42).write((PIT_COUNT & 0xFF) as u8);
        x86_64::instructions::port::Port::<u8>::new(0x42).write((PIT_COUNT >> 8) as u8);

        // Start PIT Channel 2: set GATE high (bit 0 of port 0x61)
        let port61 = x86_64::instructions::port::Port::<u8>::new(0x61).read();
        x86_64::instructions::port::Port::<u8>::new(0x61).write((port61 & 0xFC) | 0x01);

        // Read TSC before
        let tsc_start = tsc_now();

        // Wait for PIT Channel 2 output (bit 5 of port 0x61 goes high)
        loop {
            let status = x86_64::instructions::port::Port::<u8>::new(0x61).read();
            if status & 0x20 != 0 {
                break;
            }
        }

        // Read TSC after
        let tsc_end = tsc_now();
        let tsc_delta = tsc_end - tsc_start;

        // Calculate: ticks_per_us = tsc_delta / (DELAY_MS * 1000)
        let ticks_per_us = tsc_delta / (DELAY_MS * 1000);
        TSC_TICKS_PER_US.store(ticks_per_us, Ordering::Relaxed);

        // Restore port 0x61
        x86_64::instructions::port::Port::<u8>::new(0x61).write(port61);
    }

    let freq = TSC_TICKS_PER_US.load(Ordering::Relaxed);
    crate::serial_str!("[TSC] Calibrated: ");
    crate::drivers::serial::write_dec(freq as u32);
    crate::serial_strln!(" ticks/us (approx MHz)");
}
