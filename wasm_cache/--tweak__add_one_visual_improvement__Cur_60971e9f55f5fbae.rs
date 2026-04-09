#![no_std]
#![no_main]

extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_circle(cx: i32, cy: i32, r: i32, color: i32);
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_get_time() -> i32;
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        let w = folk_screen_width();
        let h = folk_screen_height();
        let time = folk_get_time();

        // Background: Solid black
        folk_fill_screen(0x00000000);

        // Visual Improvement: Add pulsing decorative rects in the corners
        let pulse = (time / 10) % 50;
        folk_draw_rect(20, 20, 50 + pulse, 50 + pulse, 0x00333333);
        folk_draw_rect(w - 70 - pulse, h - 70 - pulse, 50 + pulse, 50 + pulse, 0x00333333);

        // Main circle: r=200 at (640, 400), color=red
        // Scaled slightly for dynamic screens
        folk_draw_circle(640, 400, 200, 0x00FF0000);
    }
}