//! Folkering OS — Network Fetch Demo
//! Demonstrates folk_http_get: fetches live data from the internet
//! and displays it on screen.

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
    fn folk_http_get(url_ptr: i32, url_len: i32, buf_ptr: i32, max_len: i32) -> i32;
    fn folk_net_has_ip() -> i32;
    fn folk_get_time() -> i32;
}

static mut RESPONSE_BUF: [u8; 4096] = [0u8; 4096];
static mut FETCHED: bool = false;
static mut FETCH_LEN: usize = 0;
static mut LAST_FETCH: i32 = 0;

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        let sw = folk_screen_width();
        let sh = folk_screen_height();

        // Dark background
        folk_fill_screen(0x1a1a2e);

        // Title bar
        folk_draw_rect(0, 0, sw, 40, 0x16213e);
        let title = b"Network Fetch Demo";
        folk_draw_text(20, 12, title.as_ptr() as i32, title.len() as i32, 0x00e4ff);

        // Check network
        let online = folk_net_has_ip();
        if online == 0 {
            let msg = b"No network - waiting for DHCP...";
            folk_draw_text(20, 60, msg.as_ptr() as i32, msg.len() as i32, 0xff6666);
            return;
        }

        let now = folk_get_time();

        // Fetch every 30 seconds (or on first run)
        if !FETCHED || (now - LAST_FETCH > 30000) {
            let status = b"Fetching https://httpbin.org/get ...";
            folk_draw_text(20, 60, status.as_ptr() as i32, status.len() as i32, 0xaaaaaa);

            let url = b"https://httpbin.org/get";
            let n = folk_http_get(
                url.as_ptr() as i32,
                url.len() as i32,
                RESPONSE_BUF.as_mut_ptr() as i32,
                RESPONSE_BUF.len() as i32,
            );

            if n > 0 {
                FETCH_LEN = n as usize;
                FETCHED = true;
                LAST_FETCH = now;
            } else {
                let err = b"HTTP GET failed!";
                folk_draw_text(20, 80, err.as_ptr() as i32, err.len() as i32, 0xff4444);
                return;
            }
        }

        // Display response
        if FETCHED && FETCH_LEN > 0 {
            // Success header
            folk_draw_rect(10, 50, sw - 20, 30, 0x0a3d0a);
            let ok = b"LIVE DATA from httpbin.org:";
            folk_draw_text(20, 58, ok.as_ptr() as i32, ok.len() as i32, 0x44ff44);

            // Show response lines
            let data = &RESPONSE_BUF[..FETCH_LEN];
            let mut y = 90;
            let mut line_start = 0;
            for i in 0..data.len() {
                if data[i] == b'\n' || i == data.len() - 1 {
                    let end = if data[i] == b'\n' { i } else { i + 1 };
                    let line_len = (end - line_start).min(100);
                    if line_len > 0 && y < sh - 20 {
                        folk_draw_text(
                            20, y,
                            data[line_start..].as_ptr() as i32,
                            line_len as i32,
                            0xcccccc,
                        );
                        y += 18;
                    }
                    line_start = end + 1;
                }
            }

            // Footer
            let footer = b"Fetched via folk_http_get -> TCP proxy -> internet";
            folk_draw_text(20, sh - 25, footer.as_ptr() as i32, footer.len() as i32, 0x666699);
        }
    }
}
