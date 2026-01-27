//! Entry point and panic handler for Folkering OS userspace programs
//!
//! Use the `entry!` macro to define your program's entry point:
//!
//! ```no_run
//! #![no_std]
//! #![no_main]
//!
//! use libfolk::entry;
//!
//! entry!(main);
//!
//! fn main() -> ! {
//!     // Your code here
//!     libfolk::sys::exit(0)
//! }
//! ```

use crate::sys::task;

/// Macro to define the program entry point
///
/// The function must have signature `fn() -> !` (never returns).
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn _start() -> ! {
            // Call the user's main function
            let f: fn() -> ! = $main;
            f()
        }
    };
}

/// Panic handler - prints error and exits
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Try to print panic info via serial
    crate::print!("PANIC: ");
    if let Some(location) = info.location() {
        crate::print!("{}:{}: ", location.file(), location.line());
    }
    if let Some(message) = info.message().as_str() {
        crate::println!("{}", message);
    } else {
        crate::println!("(no message)");
    }

    // Exit with error code
    task::exit(1)
}
