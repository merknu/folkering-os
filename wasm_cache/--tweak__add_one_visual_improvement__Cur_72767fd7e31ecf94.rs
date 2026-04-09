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

static mut BALL_X: i32 = 645;
static mut BALL_Y: i32 = 405;
static mut VEL_X: i32 = 5;
static mut VEL_Y: i32 = 3;

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        let w = folk_screen_width();
        let h = folk_screen_height();
        let r = 20;

        // Update physics
        BALL_X += VEL_X;
        BALL_Y += VEL_Y;

        // Bounce
        if BALL_X - r <= 0 || BALL_X + r >= w { VEL_X *= -1; }
        if BALL_Y - r <= 0 || BALL_Y + r >= h { VEL_Y *= -1; }

        // Draw
        folk_fill_screen(0x00101010);
        
        // Visual improvement: trailing shadow effect
        folk_draw_circle(BALL_X + 4, BALL_Y + 4, r, 0x00330e00);
        
        // Main ball
        folk_draw_circle(BALL_X, BALL_Y, r, 0x00FF4500);
    }
}