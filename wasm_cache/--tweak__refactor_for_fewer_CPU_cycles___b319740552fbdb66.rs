#![no_std]
#![no_main]

extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_circle(cx: i32, cy: i32, r: i32, color: i32);
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

static mut FRAME: i32 = 0;

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        let w = folk_screen_width();
        let h = folk_screen_height();
        let center_x = w >> 1;
        let center_y = h >> 1;
        let radius = if h < w { h >> 2 } else { w >> 2 };

        folk_fill_screen(0x00000000);
        
        let color = (0x00FF0000) | ((FRAME % 255) << 8);
        folk_draw_circle(center_x, center_y, radius, color);
        
        FRAME = FRAME.wrapping_add(1);
    }
}