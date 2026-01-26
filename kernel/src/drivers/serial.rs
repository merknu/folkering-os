//! Serial Console Driver (COM1)
//!
//! Simple serial port driver for early boot logging.

use spin::Mutex;
use uart_16550::SerialPort;

static SERIAL1: Mutex<SerialPort> = Mutex::new(unsafe { SerialPort::new(0x3F8) });

/// Initialize serial console
pub fn init() {
    SERIAL1.lock().init();
}

/// Print formatted arguments to serial console
#[doc(hidden)]
pub fn _print(args: core::fmt::Arguments) {
    use core::fmt::Write;
    SERIAL1.lock().write_fmt(args).unwrap();
}

/// Write a static string directly (bypasses format!)
pub fn write_str(s: &str) {
    use core::fmt::Write;
    let mut serial = SERIAL1.lock();
    for byte in s.bytes() {
        serial.send(byte);
    }
}

/// Write a single byte
pub fn write_byte(b: u8) {
    SERIAL1.lock().send(b);
}

/// Write a u64 as hex (bypasses format!)
pub fn write_hex(val: u64) {
    const HEX_CHARS: &[u8] = b"0123456789abcdef";
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
}

/// Write a u32 as decimal (bypasses format!)
pub fn write_dec(val: u32) {
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
}

/// Write newline
pub fn write_newline() {
    SERIAL1.lock().send(b'\n');
}
