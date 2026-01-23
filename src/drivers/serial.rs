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
