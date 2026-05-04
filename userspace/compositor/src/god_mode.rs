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
    // Iteration cap dropped from 4096 to 256: same root cause as
    // `com2_async_poll` in PR #136. On unconnected COM3 the LSR returns
    // 0xFF every read, the data-ready bit is set, so the loop *would*
    // spin out the full budget on every frame. Each `com3_read` is a
    // syscall + port-I/O VMEXIT (~13µs under KVM), turning into ~38ms
    // of compositor frame budget per call (Issue #135). 256 is well
    // above any realistic god-mode injection burst (the line-oriented
    // dispatch above reads at most ~80 bytes per command); overflow
    // just rolls into the next frame's poll.
    let mut did_work = false;
    for _ in 0..256 {
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
