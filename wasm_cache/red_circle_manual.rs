#![no_std]
#![no_main]
extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_circle(cx: i32, cy: i32, r: i32, color: i32);
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
}
#[panic_handler] fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
#[no_mangle] pub extern "C" fn run() {
    unsafe {
        let w = folk_screen_width();
        let h = folk_screen_height();
        folk_fill_screen(0x001a1a2e);
        folk_draw_circle(w/2, h/2, h/4, 0x00FF0000);
    }
}
