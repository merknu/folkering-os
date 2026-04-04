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

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        let w = folk_screen_width();
        let h = folk_screen_height();
        
        // Calculate center and radius
        let cx = w / 2;
        let cy = h / 2;
        let radius = if w < h { w / 4 } else { h / 4 };

        // Clear background
        folk_fill_screen(0x000F0F0F);
        
        // Draw the circle
        folk_draw_circle(cx, cy, radius, 0x00FFD700);
    }
}