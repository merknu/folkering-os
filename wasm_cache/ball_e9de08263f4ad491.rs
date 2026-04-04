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

static mut BALL_X: i32 = 100;
static mut BALL_Y: i32 = 100;
static mut VEL_X: i32 = 5;
static mut VEL_Y: i32 = 4;

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        let w = folk_screen_width();
        let h = folk_screen_height();
        let radius = 20;

        // Update position
        BALL_X += VEL_X;
        BALL_Y += VEL_Y;

        // Bounce off walls
        if BALL_X - radius < 0 || BALL_X + radius > w {
            VEL_X = -VEL_X;
        }
        if BALL_Y - radius < 0 || BALL_Y + radius > h {
            VEL_Y = -VEL_Y;
        }

        // Clamp to screen bounds
        if BALL_X < radius { BALL_X = radius; }
        if BALL_X > w - radius { BALL_X = w - radius; }
        if BALL_Y < radius { BALL_Y = radius; }
        if BALL_Y > h - radius { BALL_Y = h - radius; }

        // Render
        folk_fill_screen(0x00000000);
        folk_draw_circle(BALL_X, BALL_Y, radius, 0x00FF0000);
    }
}