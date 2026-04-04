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

static mut BALL_X: i32 = 0;
static mut BALL_Y: i32 = 0;
static mut VEL_X: i32 = 5;
static mut VEL_Y: i32 = 5;

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        let w = folk_screen_width();
        let h = folk_screen_height();
        let r = 20;

        if BALL_X == 0 {
            BALL_X = w / 2;
            BALL_Y = h / 2;
        }

        BALL_X += VEL_X;
        BALL_Y += VEL_Y;

        if BALL_X - r <= 0 || BALL_X + r >= w {
            VEL_X = -VEL_X;
        }
        if BALL_Y - r <= 0 || BALL_Y + r >= h {
            VEL_Y = -VEL_Y;
        }

        folk_fill_screen(0x00101010);
        folk_draw_circle(BALL_X, BALL_Y, r, 0x00FF4500);
    }
}