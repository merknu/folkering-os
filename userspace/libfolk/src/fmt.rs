//! Formatting and printing for Folkering OS userspace
//!
//! Provides `print!` and `println!` macros that output to the kernel's
//! serial console via the `write_char` syscall.

use core::fmt::{self, Write};

/// Console writer that uses the write_char syscall
struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            crate::sys::io::write_char(byte);
        }
        Ok(())
    }
}

/// Internal print function - writes formatted output to console
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _ = Console.write_fmt(args);
}

/// Print formatted text to the console (no newline)
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::fmt::_print(format_args!($($arg)*))
    };
}

/// Print formatted text to the console (with newline)
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {
        $crate::print!("{}\n", format_args!($($arg)*))
    };
}
