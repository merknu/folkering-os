//! Network Test — demonstrates the new kernel UDP + NTP infrastructure

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);
    fn folk_poll_event(event_ptr: i32) -> i32;
    fn folk_ntp_query(a: i32, b: i32, c: i32, d: i32) -> i64;
}

const BG: i32 = 0x0D1117;
const FG: i32 = 0xC9D1D9;
const ACCENT: i32 = 0x58A6FF;
const SUCCESS: i32 = 0x3FB950;
const ERROR: i32 = 0xF85149;
const BTN: i32 = 0x238636;

static mut EVT: [i32; 4] = [0i32; 4];
static mut NTP_RESULT: i64 = -1;
static mut LAST_QUERY_TICK: i32 = 0;
static mut FRAMES: i32 = 0;

// Simple decimal formatter (no_std)
fn write_dec(buf: &mut [u8], mut n: u64) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 24];
    let mut i = 0;
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    for j in 0..i {
        buf[j] = tmp[i - 1 - j];
    }
    i
}

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        FRAMES += 1;
        folk_fill_screen(BG);

        // Title
        let title = b"Network Test - Kernel UDP/NTP Demo";
        folk_draw_text(40, 40, title.as_ptr() as i32, title.len() as i32, ACCENT);

        let info1 = b"Click button to query NTP server (Cloudflare time)";
        folk_draw_text(40, 80, info1.as_ptr() as i32, info1.len() as i32, FG);
        let info2 = b"Uses syscall 0x59 (UDP send_recv) - no proxy needed";
        folk_draw_text(40, 100, info2.as_ptr() as i32, info2.len() as i32, 0x8B949E);

        // Query button
        let bx = 40;
        let by = 160;
        let bw = 280;
        let bh = 50;
        folk_draw_rect(bx, by, bw, bh, BTN);
        let label = b"Query NTP (162.159.200.123)";
        folk_draw_text(bx + 20, by + 18, label.as_ptr() as i32, label.len() as i32, 0xFFFFFF);

        // Result display
        if NTP_RESULT > 0 {
            let result_label = b"Unix timestamp:";
            folk_draw_text(40, 240, result_label.as_ptr() as i32, result_label.len() as i32, FG);

            let mut buf = [0u8; 24];
            let n = write_dec(&mut buf, NTP_RESULT as u64);
            folk_draw_text(220, 240, buf.as_ptr() as i32, n as i32, SUCCESS);

            // Convert to year approximation (rough)
            let years_since_1970 = NTP_RESULT / (365 * 86400);
            let year = 1970 + years_since_1970;
            let year_label = b"Year (approx):";
            folk_draw_text(40, 270, year_label.as_ptr() as i32, year_label.len() as i32, FG);
            let mut buf2 = [0u8; 8];
            let n2 = write_dec(&mut buf2, year as u64);
            folk_draw_text(220, 270, buf2.as_ptr() as i32, n2 as i32, SUCCESS);

            let kernel_label = b"[OK] Direct kernel UDP path works!";
            folk_draw_text(40, 320, kernel_label.as_ptr() as i32, kernel_label.len() as i32, SUCCESS);
        } else if NTP_RESULT == 0 {
            let err_label = b"NTP query failed (no network or timeout)";
            folk_draw_text(40, 240, err_label.as_ptr() as i32, err_label.len() as i32, ERROR);
        } else {
            let hint = b"No query yet. Click the button.";
            folk_draw_text(40, 240, hint.as_ptr() as i32, hint.len() as i32, 0x8B949E);
        }

        // Footer
        let footer = b"Folkering OS now has: UDP, NTP, IPv6, AC97, TLS hostname check";
        folk_draw_text(40, 380, footer.as_ptr() as i32, footer.len() as i32, 0x8B949E);

        // Handle clicks
        loop {
            let e = core::ptr::addr_of_mut!(EVT) as *mut i32;
            if folk_poll_event(e as i32) == 0 { break; }
            if *e.add(0) != 2 { continue; }

            let mx = *e.add(1);
            let my = *e.add(2);
            if mx >= bx && mx < bx + bw && my >= by && my < by + bh {
                NTP_RESULT = folk_ntp_query(162, 159, 200, 123);
                LAST_QUERY_TICK = FRAMES;
            }
        }
    }
}
