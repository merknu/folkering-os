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
        // Increment frame counter for animation
        FRAME = FRAME.wrapping_add(1);

        let w = folk_screen_width();
        let h = folk_screen_height();

        // Edge case: Prevent division by zero or invalid dimensions
        if w <= 0 || h <= 0 {
            return;
        }

        // Calculate dynamic radius: smaller of half width or half height
        // Constrain to positive values to prevent undefined behavior in drawing
        let base_dim = if w < h { w } else { h };
        let radius = (base_dim / 4).clamp(1, 1000);

        // Center calculation
        let cx = w / 2;
        let cy = h / 2;

        // Clear screen
        folk_fill_screen(0x00050505);

        // Draw multiple circles to create a pulse effect
        // Use wrapping arithmetic to handle frame counter overflow
        let pulse = (FRAME % 60) * (radius / 30);
        let current_radius = radius + pulse;

        // Ensure we don't draw with extreme negative/overflowing dimensions
        if current_radius > 0 {
            folk_draw_circle(cx, cy, current_radius, 0x00FF8800);
            folk_draw_circle(cx, cy, radius, 0x0000AAFF);
        }
    }
}