#![no_std]
#![no_main]

extern "C" {
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_draw_circle(cx: i32, cy: i32, r: i32, color: i32);
    fn folk_fill_screen(color: i32);
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
        let time = folk_get_time();
        let sw = folk_screen_width();
        let sh = folk_screen_height();

        // Clear screen
        folk_fill_screen(0x00000000);

        // Visual improvement: pulse the circle radius based on time
        let pulse = (time / 10 % 50) as i32;
        let radius = 200 + pulse;

        // Rectangle 0
        folk_draw_rect(20, 20, 50, 50, 0x00333333);

        // Rectangle 1 (using screen width/height for responsive positioning)
        folk_draw_rect(sw - 70, sh - 70, 50, 50, 0x00333333);

        // Circle at center
        folk_draw_circle(sw / 2, sh / 2, radius, 0x00FF0000);
    }
}