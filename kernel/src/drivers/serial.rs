//! Serial Console Driver (COM1)
//!
//! Simple serial port driver for early boot logging.
//! All output functions disable interrupts while holding the serial lock
//! to prevent deadlocks with interrupt handlers that also use serial output.

use spin::Mutex;
use uart_16550::SerialPort;

static SERIAL1: Mutex<SerialPort> = Mutex::new(unsafe { SerialPort::new(0x3F8) });
static SERIAL2: Mutex<SerialPort> = Mutex::new(unsafe { SerialPort::new(0x2F8) });
static SERIAL3: Mutex<SerialPort> = Mutex::new(unsafe { SerialPort::new(0x3E8) });

/// Initialize serial consoles (COM1=log, COM2=Gemini proxy, COM3=God Mode Pipe)
pub fn init() {
    SERIAL1.lock().init();
    SERIAL2.lock().init();
    SERIAL3.lock().init();
}

// ── COM3 (God Mode Pipe — direct command injection) ─────────────────────

/// Read a byte from COM3 (non-blocking). Returns None if no data.
pub fn com3_read_byte() -> Option<u8> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _serial = SERIAL3.lock();
        let lsr: u8 = unsafe {
            x86_64::instructions::port::Port::<u8>::new(0x3E8 + 5).read()
        };
        if (lsr & 0x01) != 0 {
            Some(unsafe { x86_64::instructions::port::Port::<u8>::new(0x3E8).read() })
        } else {
            None
        }
    })
}

/// Write a byte to COM3 via raw port I/O (uart_16550 send() is broken for COM3).
///
/// Capped at 1_000_000 iterations of the TX-empty wait — same defense as
/// `com2_write` directly below. On QEMU/KVM/WHPX configurations where the
/// COM3 backend is missing or the emulator never asserts LSR bit 5, an
/// unbounded wait would freeze the kernel during any telemetry/IQE flush.
/// Same bug class as Issue #49 (poll_com3 RX) — the omission was symmetric
/// across read and write paths; this closes the write side.
pub fn com3_write_byte(byte: u8) {
    unsafe {
        // Wait for TX buffer empty (LSR bit 5) with timeout
        let mut wait = 0u32;
        loop {
            let lsr: u8 = x86_64::instructions::port::Port::<u8>::new(0x3E8 + 5).read();
            if lsr & 0x20 != 0 { break; }
            wait += 1;
            if wait > 1_000_000 { break; } // Safety timeout — don't hang forever
            core::hint::spin_loop();
        }
        x86_64::instructions::port::Port::<u8>::new(0x3E8).write(byte);
    }
}

/// Write a slice of bytes to COM3 via raw port I/O.
pub fn com3_write(data: &[u8]) {
    for &byte in data {
        com3_write_byte(byte);
    }
}

// ── COM2 (Gemini Proxy Channel) ─────────────────────────────────────────

/// Write bytes to COM2 (Gemini proxy channel).
/// Uses raw port I/O. Interrupts are NOT disabled — WHPX needs VM-exits
/// (triggered by interrupts) to process UART TX buffer. Disabling interrupts
/// causes an infinite busy-wait deadlock.
pub fn com2_write(data: &[u8]) {
    for &byte in data {
        unsafe {
            // Wait for TX buffer empty (LSR bit 5) with timeout
            let mut wait = 0u32;
            loop {
                let lsr: u8 = x86_64::instructions::port::Port::<u8>::new(0x2F8 + 5).read();
                if lsr & 0x20 != 0 { break; }
                wait += 1;
                if wait > 1_000_000 { break; } // Safety timeout — don't hang forever
                core::hint::spin_loop();
            }
            x86_64::instructions::port::Port::<u8>::new(0x2F8).write(byte);
        }
    }
}

/// Read a byte from COM2 (non-blocking). Returns None if no data.
pub fn com2_read_byte() -> Option<u8> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _serial = SERIAL2.lock();
        let lsr: u8 = unsafe {
            x86_64::instructions::port::Port::<u8>::new(0x2F8 + 5).read()
        };
        if (lsr & 0x01) != 0 {
            Some(unsafe { x86_64::instructions::port::Port::<u8>::new(0x2F8).read() })
        } else {
            None
        }
    })
}

/// Read until delimiter or timeout (blocking with yield).
/// Returns bytes read (excluding delimiter).
pub fn com2_read_until(delimiter: &[u8], buf: &mut [u8], timeout_ms: u64) -> usize {
    // Use TSC for all timing (uptime_ms doesn't advance in syscall context)
    fn tsc_ms() -> u64 {
        let tsc: u64;
        unsafe { core::arch::asm!("rdtsc", "shl rdx, 32", "or rax, rdx", out("rax") tsc, out("rdx") _); }
        tsc / 2_000_000
    }

    let start = tsc_ms();
    let mut pos = 0;
    let mut delim_match = 0;

    loop {
        if let Some(byte) = com2_read_byte() {
            if pos < buf.len() {
                buf[pos] = byte;
                pos += 1;
            }
            // Check for delimiter match
            if byte == delimiter[delim_match] {
                delim_match += 1;
                if delim_match == delimiter.len() {
                    return pos - delimiter.len();
                }
            } else {
                delim_match = if byte == delimiter[0] { 1 } else { 0 };
            }
        }

        let now = tsc_ms();
        if pos == 0 && now - start > timeout_ms {
            break; // No data at all within timeout
        }
        if pos > 0 && now - start > timeout_ms + 30_000 {
            break; // Data started but incomplete after extended timeout
        }

        for _ in 0..100 { core::hint::spin_loop(); }
    }
    pos
}

// ── COM2 Async Ring Buffer ──────────────────────────────────────────────
// Non-blocking COM2 I/O for the compositor: send request, poll for response
// without blocking the main event loop.

use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};

const COM2_RING_SIZE: usize = 131072; // 128KB — enough for WASM base64 payloads
static mut COM2_RX_RING: [u8; COM2_RING_SIZE] = [0; COM2_RING_SIZE];
static COM2_RX_HEAD: AtomicUsize = AtomicUsize::new(0); // write position
static COM2_RX_TAIL: AtomicUsize = AtomicUsize::new(0); // read position
static COM2_ASYNC_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Start async COM2 session: send request bytes, then enable background polling.
/// Call com2_async_poll() each frame to drain COM2 RX into the ring buffer.
pub fn com2_async_send(data: &[u8]) {
    // Reset ring buffer
    COM2_RX_HEAD.store(0, Ordering::Release);
    COM2_RX_TAIL.store(0, Ordering::Release);
    // Write request to COM2
    com2_write(data);
    // Enable async polling
    COM2_ASYNC_ACTIVE.store(true, Ordering::Release);
}

/// Poll COM2 RX (non-blocking). Call this every frame from the compositor.
/// Drains any available COM2 bytes into the ring buffer.
pub fn com2_async_poll() {
    if !COM2_ASYNC_ACTIVE.load(Ordering::Acquire) {
        return;
    }
    // Drain all available COM2 bytes into ring (up to 4096 per poll to avoid starving the main loop)
    for _ in 0..4096 {
        if let Some(byte) = com2_read_byte() {
            let head = COM2_RX_HEAD.load(Ordering::Relaxed);
            if head < COM2_RING_SIZE {
                unsafe { COM2_RX_RING[head] = byte; }
                COM2_RX_HEAD.store(head + 1, Ordering::Release);
            }
            // else: ring full, drop byte (shouldn't happen with 128KB)
        } else {
            break; // No more data available
        }
    }
}

/// Check if async COM2 response contains a 0x00 sentinel (COBS frame delimiter).
/// Returns Some(len) = frame length BEFORE the sentinel, None if still waiting.
/// Also supports legacy @@END@@ delimiter for backward compatibility.
pub fn com2_async_check_sentinel() -> Option<usize> {
    if !COM2_ASYNC_ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    let head = COM2_RX_HEAD.load(Ordering::Acquire);
    if head == 0 {
        return None;
    }
    let ring = unsafe { &COM2_RX_RING[..head] };
    // Search for 0x00 sentinel (COBS frame end)
    for i in 0..head {
        if ring[i] == 0x00 {
            return Some(i); // Length of COBS-encoded data before sentinel
        }
    }
    None
}

/// Legacy: check for @@END@@ delimiter (backward compat with old protocol)
pub fn com2_async_check_legacy() -> Option<usize> {
    if !COM2_ASYNC_ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    let head = COM2_RX_HEAD.load(Ordering::Acquire);
    let delimiter = b"@@END@@";
    if head < delimiter.len() {
        return None;
    }
    let ring = unsafe { &COM2_RX_RING[..head] };
    for i in 0..=(head - delimiter.len()) {
        if &ring[i..i + delimiter.len()] == delimiter {
            return Some(i);
        }
    }
    None
}

/// Read async COM2 response data into userspace buffer.
/// Resets the ring buffer position but keeps async mode ACTIVE for next frame.
/// Returns number of bytes copied.
pub fn com2_async_read(buf: &mut [u8], len: usize) -> usize {
    let head = COM2_RX_HEAD.load(Ordering::Acquire);
    let copy_len = len.min(buf.len()).min(head);
    let ring = unsafe { &COM2_RX_RING[..copy_len] };
    buf[..copy_len].copy_from_slice(ring);
    // Reset ring position for next frame — but keep ACTIVE so polling continues!
    COM2_RX_HEAD.store(0, Ordering::Release);
    COM2_RX_TAIL.store(0, Ordering::Release);
    // DO NOT set ASYNC_ACTIVE to false — we want to keep receiving
    copy_len
}

/// Cancel async COM2 session.
pub fn com2_async_cancel() {
    COM2_ASYNC_ACTIVE.store(false, Ordering::Release);
    COM2_RX_HEAD.store(0, Ordering::Release);
}

/// Check if async COM2 session is active.
pub fn com2_async_is_active() -> bool {
    COM2_ASYNC_ACTIVE.load(Ordering::Acquire)
}

/// Print formatted arguments to serial console
#[doc(hidden)]
pub fn _print(args: core::fmt::Arguments) {
    use core::fmt::Write;
    x86_64::instructions::interrupts::without_interrupts(|| {
        SERIAL1.lock().write_fmt(args).unwrap();
    });
}

/// Write a static string directly (bypasses format!)
pub fn write_str(s: &str) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut serial = SERIAL1.lock();
        for byte in s.bytes() {
            serial.send(byte);
        }
    });
}

/// Write a single byte
pub fn write_byte(b: u8) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        SERIAL1.lock().send(b);
    });
}

/// Write a u64 as hex (bypasses format!)
pub fn write_hex(val: u64) {
    const HEX_CHARS: &[u8] = b"0123456789abcdef";
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut serial = SERIAL1.lock();

        serial.send(b'0');
        serial.send(b'x');

        // Find first non-zero nibble
        let mut started = false;
        for i in (0..16).rev() {
            let nibble = ((val >> (i * 4)) & 0xF) as usize;
            if nibble != 0 || started || i == 0 {
                started = true;
                serial.send(HEX_CHARS[nibble]);
            }
        }
    });
}

/// Write a u32 as decimal (bypasses format!)
pub fn write_dec(val: u32) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut serial = SERIAL1.lock();

        if val == 0 {
            serial.send(b'0');
            return;
        }

        let mut digits = [0u8; 10];
        let mut i = 0;
        let mut n = val;

        while n > 0 {
            digits[i] = b'0' + (n % 10) as u8;
            n /= 10;
            i += 1;
        }

        while i > 0 {
            i -= 1;
            serial.send(digits[i]);
        }
    });
}

/// Write newline
pub fn write_newline() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        SERIAL1.lock().send(b'\n');
    });
}

/// Read a byte from serial port (non-blocking)
/// Returns None if no data available
/// Uses proper locking to avoid race conditions with serial output.
pub fn read_byte() -> Option<u8> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let _serial = SERIAL1.lock();

        // Check Line Status Register (LSR) at base+5
        // Bit 0 (DR - Data Ready) is set when data is available
        let lsr: u8 = unsafe {
            x86_64::instructions::port::Port::<u8>::new(0x3F8 + 5).read()
        };

        if (lsr & 0x01) != 0 {
            // Data available - read from data port
            let byte: u8 = unsafe {
                x86_64::instructions::port::Port::<u8>::new(0x3F8).read()
            };
            Some(byte)
        } else {
            None
        }
    })
}
