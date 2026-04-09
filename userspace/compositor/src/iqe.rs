//! IQE (Interaction Quality Engine) — input latency telemetry.
//!
//! Polls kernel IQE events and computes EWMA latency metrics for
//! keyboard and mouse input paths (IRQ → read → GPU flush).

use compositor::state::IqeState;
use libfolk::sys::io::{write_str, write_char};
use crate::fmt_iqe_line;

/// Poll IQE telemetry events and update EWMA latency metrics.
/// Called once per main loop iteration.
pub fn poll_telemetry(iqe: &mut IqeState, tsc_per_us: u64) {
    if tsc_per_us == 0 { return; }

    let n = libfolk::sys::iqe_read(&mut iqe.buf, 12);

    // Debug: log IQE poll result (first 3 only)
    static mut IQE_DBG: u32 = 0;
    if n > 0 { unsafe {
        if IQE_DBG < 3 {
            write_str("[IQE-POLL] n=");
            write_char(b'0' + n as u8);
            write_str("\n");
            IQE_DBG += 1;
        }
    }}

    for i in 0..n {
        let base = i * 24;
        let etype = iqe.buf[base];
        let tsc = u64::from_le_bytes([
            iqe.buf[base+8], iqe.buf[base+9], iqe.buf[base+10], iqe.buf[base+11],
            iqe.buf[base+12], iqe.buf[base+13], iqe.buf[base+14], iqe.buf[base+15],
        ]);
        match etype {
            5 => { iqe.last_kbd_tsc = tsc; }       // KeyboardIrq
            0 => { iqe.last_mou_tsc = tsc; }       // MouseIrq
            6 => { iqe.last_kbd_read_tsc = tsc; }   // KeyboardRead
            7 => { iqe.last_mou_read_tsc = tsc; }   // MouseRead
            1 => {                               // GpuFlushSubmit
                // Keyboard split times
                if iqe.last_kbd_tsc > 0 && tsc > iqe.last_kbd_tsc {
                    let total = (tsc - iqe.last_kbd_tsc) / tsc_per_us;
                    if total < 100_000 {
                        iqe.ewma_kbd_us = iqe.ewma_kbd_us - (iqe.ewma_kbd_us >> 3) + (total >> 3);
                        let mut l = [0u8; 32];
                        let n = fmt_iqe_line(&mut l, b"KBD", total);
                        libfolk::sys::com3_write(&l[..n]);
                        // Split: wakeup (IRQ -> read)
                        if iqe.last_kbd_read_tsc > iqe.last_kbd_tsc {
                            let wake = (iqe.last_kbd_read_tsc - iqe.last_kbd_tsc) / tsc_per_us;
                            let rend = if tsc > iqe.last_kbd_read_tsc { (tsc - iqe.last_kbd_read_tsc) / tsc_per_us } else { 0 };
                            iqe.ewma_kbd_wake = iqe.ewma_kbd_wake - (iqe.ewma_kbd_wake >> 3) + (wake >> 3);
                            iqe.ewma_kbd_rend = iqe.ewma_kbd_rend - (iqe.ewma_kbd_rend >> 3) + (rend >> 3);
                            let mut l2 = [0u8; 32];
                            let n2 = fmt_iqe_line(&mut l2, b"KW", wake);
                            libfolk::sys::com3_write(&l2[..n2]);
                            let mut l3 = [0u8; 32];
                            let n3 = fmt_iqe_line(&mut l3, b"KR", rend);
                            libfolk::sys::com3_write(&l3[..n3]);
                        }
                    }
                    iqe.last_kbd_tsc = 0;
                    iqe.last_kbd_read_tsc = 0;
                }
                // Mouse split times
                if iqe.last_mou_tsc > 0 && tsc > iqe.last_mou_tsc {
                    let total = (tsc - iqe.last_mou_tsc) / tsc_per_us;
                    if total < 100_000 {
                        iqe.ewma_mou_us = iqe.ewma_mou_us - (iqe.ewma_mou_us >> 3) + (total >> 3);
                        let mut l = [0u8; 32];
                        let n = fmt_iqe_line(&mut l, b"MOU", total);
                        libfolk::sys::com3_write(&l[..n]);
                        if iqe.last_mou_read_tsc > iqe.last_mou_tsc {
                            let wake = (iqe.last_mou_read_tsc - iqe.last_mou_tsc) / tsc_per_us;
                            let rend = if tsc > iqe.last_mou_read_tsc { (tsc - iqe.last_mou_read_tsc) / tsc_per_us } else { 0 };
                            iqe.ewma_mou_wake = iqe.ewma_mou_wake - (iqe.ewma_mou_wake >> 3) + (wake >> 3);
                            iqe.ewma_mou_rend = iqe.ewma_mou_rend - (iqe.ewma_mou_rend >> 3) + (rend >> 3);
                            let mut l2 = [0u8; 32];
                            let n2 = fmt_iqe_line(&mut l2, b"MW", wake);
                            libfolk::sys::com3_write(&l2[..n2]);
                            let mut l3 = [0u8; 32];
                            let n3 = fmt_iqe_line(&mut l3, b"MR", rend);
                            libfolk::sys::com3_write(&l3[..n3]);
                        }
                    }
                    iqe.last_mou_tsc = 0;
                    iqe.last_mou_read_tsc = 0;
                }
            }
            _ => {}
        }
    }
}
