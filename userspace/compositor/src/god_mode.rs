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
///
/// Cap: at most 4096 bytes are drained per call. Without this cap a
/// QEMU/KVM (or WHPX) backend that reports LSR DR=1 indefinitely on an
/// unconnected COM3 would lock the compositor's main loop on iteration
/// #1 — root cause of Issue #49. Same defensive cap used by
/// `com2_async_poll` in the kernel for the analogous COM2 ring drain.
pub fn poll_com3(buf: &mut [u8; 512], len: &mut usize, queue: &mut Vec<String>) -> bool {
    let mut did_work = false;
    for _ in 0..4096 {
        let Some(byte) = libfolk::sys::com3_read() else { break; };
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
