//! God Mode Pipe (COM3) — direct command injection.
//!
//! Reads commands from COM3 serial port and queues them for
//! the omnibar command dispatcher.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use libfolk::sys::io::write_str;

/// Poll COM3 serial port for injected commands.
/// Returns true if any work was done.
pub fn poll_com3(buf: &mut [u8; 512], len: &mut usize, queue: &mut Vec<String>) -> bool {
    let mut did_work = false;
    while let Some(byte) = libfolk::sys::com3_read() {
        if byte == b'\n' && *len > 0 {
            if let Ok(cmd) = alloc::str::from_utf8(&buf[..*len]) {
                write_str("[COM3] Inject: ");
                write_str(cmd);
                write_str("\n");
                queue.push(String::from(cmd));
            }
            *len = 0;
            did_work = true;
        } else if byte != b'\n' && byte != b'\r' && *len < buf.len() {
            buf[*len] = byte;
            *len += 1;
        }
    }
    did_work
}
