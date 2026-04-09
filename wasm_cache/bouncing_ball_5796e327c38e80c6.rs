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

static mut X: i32 = 100;
static mut Y: i32 = 100;
static mut DX: i32 = 5;
static mut DY: i32 = 7;
static RADIUS: i32 = 20;

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        let w = folk_screen_width();
        let h = folk_screen_height();

        // Update position
        X += DX;
        Y += DY;

        // Bounce off walls
        if X - RADIUS < 0 || X + RADIUS > w {
            DX = -DX;
        }
        if Y - RADIUS < 0 || Y + RADIUS > h {
            DY = -DY;
        }

        // Clamp to prevent getting stuck
        if X < RADIUS { X = RADIUS; }
        if X > w - RADIUS { X = w - RADIUS; }
        if Y < RADIUS { Y = RADIUS; }
        if Y > h - RADIUS { Y = h - RADIUS; }

        // Render
        folk_fill_screen(0x00000000);
        folk_draw_circle(X, Y, RADIUS, 0x00FF00FF);
    }
}