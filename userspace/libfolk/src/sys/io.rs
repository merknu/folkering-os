//! I/O syscalls
//!
//! Functions for basic input/output operations.

use crate::syscall::{syscall0, syscall1, SYS_READ_KEY, SYS_WRITE_CHAR, SYS_POWEROFF, SYS_CHECK_INTERRUPT, SYS_CLEAR_INTERRUPT, SYS_READ_MOUSE};

/// Mouse event with button state and delta movement
#[derive(Debug, Clone, Copy)]
pub struct MouseEvent {
    /// Button state: bit 0 = left, bit 1 = right, bit 2 = middle
    pub buttons: u8,
    /// X movement (signed)
    pub dx: i16,
    /// Y movement (signed, positive = up in PS/2 coordinates)
    pub dy: i16,
}

impl MouseEvent {
    /// Check if left button is pressed
    pub fn left_button(&self) -> bool {
        self.buttons & 0x01 != 0
    }

    /// Check if right button is pressed
    pub fn right_button(&self) -> bool {
        self.buttons & 0x02 != 0
    }

    /// Check if middle button is pressed
    pub fn middle_button(&self) -> bool {
        self.buttons & 0x04 != 0
    }
}

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

/// Read a mouse event from the input buffer (non-blocking)
///
/// # Returns
/// * `Some(event)` - A mouse event with button state and movement delta
/// * `None` - No event available
pub fn read_mouse() -> Option<MouseEvent> {
    let ret = unsafe { syscall0(SYS_READ_MOUSE) };
    // High bit (63) indicates valid event
    if ret & (1u64 << 63) == 0 {
        None
    } else {
        // Unpack: bits 0-7: buttons, bits 8-23: dx, bits 24-39: dy
        let buttons = (ret & 0xFF) as u8;
        let dx = ((ret >> 8) & 0xFFFF) as u16 as i16;
        let dy = ((ret >> 24) & 0xFFFF) as u16 as i16;
        Some(MouseEvent { buttons, dx, dy })
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
