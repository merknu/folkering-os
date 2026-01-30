//! I/O syscalls
//!
//! Functions for basic input/output operations.

use crate::syscall::{syscall0, syscall1, SYS_READ_KEY, SYS_WRITE_CHAR, SYS_POWEROFF, SYS_CHECK_INTERRUPT, SYS_CLEAR_INTERRUPT};

/// Read a key from the keyboard buffer (non-blocking)
///
/// # Returns
/// * `Some(key)` - A key code if a key was pressed
/// * `None` - No key available
pub fn read_key() -> Option<u8> {
    let ret = unsafe { syscall0(SYS_READ_KEY) };
    if ret == 0 {
        None
    } else {
        Some(ret as u8)
    }
}

/// Write a single character to the console
pub fn write_char(c: u8) {
    unsafe { syscall1(SYS_WRITE_CHAR, c as u64) };
}

/// Write a string to the console
pub fn write_str(s: &str) {
    for byte in s.bytes() {
        write_char(byte);
    }
}

/// Write a string followed by a newline
pub fn write_line(s: &str) {
    write_str(s);
    write_char(b'\n');
}

/// Power off the system (exits QEMU)
pub fn poweroff() -> ! {
    unsafe { syscall0(SYS_POWEROFF) };
    // Should never return, but loop just in case
    loop {}
}

/// Check if Ctrl+C interrupt is pending for this task
pub fn check_interrupt() -> bool {
    unsafe { syscall0(SYS_CHECK_INTERRUPT) != 0 }
}

/// Clear the interrupt flag (call after handling)
pub fn clear_interrupt() {
    unsafe { syscall0(SYS_CLEAR_INTERRUPT) };
}
