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

/// Write a byte to COM3 (blocking — waits for TX ready).
pub fn com3_write_byte(byte: u8) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut serial = SERIAL3.lock();
        serial.send(byte);
    });
}

/// Write a slice of bytes to COM3.
pub fn com3_write(data: &[u8]) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut serial = SERIAL3.lock();
        for &byte in data {
            serial.send(byte);
        }
    });
}

// ── COM2 (Gemini Proxy Channel) ─────────────────────────────────────────

/// Write bytes to COM2 (Gemini proxy channel)
pub fn com2_write(data: &[u8]) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut serial = SERIAL2.lock();
        for &byte in data {
            serial.send(byte);
        }
    });
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
