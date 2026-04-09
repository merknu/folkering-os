#![no_std]
#![no_main]

extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_circle(cx: i32, cy: i32, r: i32, color: i32);
    fn folk_screen_width() -> i32;
    fn folk_screen_height() -> i32;
    fn folk_get_time() -> i32;
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

static mut BALL_X: i32 = 100;
static mut BALL_Y: i32 = 100;
static mut VEL_X: i32 = 5;
static mut VEL_Y: i32 = 5;

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        let w = folk_screen_width();
        let h = folk_screen_height();
        
        // Prevent division by zero or negative screen dimensions
        let safe_w = if w < 10 { 100 } else { w };
        let safe_h = if h < 10 { 100 } else { h };
        
        let radius = 20;

        // Update position
        BALL_X += VEL_X;
        BALL_Y += VEL_Y;

        // Collision logic with edge safety
        if BALL_X - radius <= 0 {
            BALL_X = radius;
            VEL_X = VEL_X.abs();
        } else if BALL_X + radius >= safe_w {
            BALL_X = safe_w - radius;
            VEL_X = -VEL_X.abs();
        }

        if BALL_Y - radius <= 0 {
            BALL_Y = radius;
            VEL_Y = VEL_Y.abs();
        } else if BALL_Y + radius >= safe_h {
            BALL_Y = safe_h - radius;
            VEL_Y = -VEL_Y.abs();
        }

        // Limit velocities to prevent jitter/teleportation
        if VEL_X > 20 { VEL_X = 20; }
        if VEL_Y > 20 { VEL_Y = 20; }

        folk_fill_screen(0x001a1a2e);
        folk_draw_circle(BALL_X, BALL_Y, radius, 0x00FF0000);
    }
}