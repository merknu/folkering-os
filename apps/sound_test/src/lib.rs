//! Sound Test — click a button to play audio beep through AC97

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);
    fn folk_screen_width() -> i32;
    fn folk_poll_event(event_ptr: i32) -> i32;
    fn folk_audio_beep(duration_ms: i32) -> i32;
    fn folk_audio_play(ptr: i32, sample_count: i32) -> i32;
}

const BG: i32 = 0x0D1117;
const FG: i32 = 0xC9D1D9;
const ACCENT: i32 = 0x58A6FF;
const BUTTON_BG: i32 = 0x238636;
const BUTTON_HOVER: i32 = 0x2EA043;

static mut EVT: [i32; 4] = [0i32; 4];
static mut LAST_BEEP_TIME: i32 = -999;
static mut FRAMES: i32 = 0;

// Pre-computed 440Hz sine-ish samples for "beep" button
// (Square wave 440Hz, stereo, ~100ms)
static mut BEEP_SAMPLES: [i16; 8820] = [0i16; 8820]; // 0.1s * 44100 * 2

unsafe fn init_beep_samples() {
    // Square wave: period = 44100/440 = 100.2 samples; half = 50
    let mut i = 0;
    while i < BEEP_SAMPLES.len() / 2 {
        let val: i16 = if (i / 50) % 2 == 0 { 6000 } else { -6000 };
        BEEP_SAMPLES[i * 2] = val;
        BEEP_SAMPLES[i * 2 + 1] = val;
        i += 1;
    }
}

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        FRAMES += 1;
        if FRAMES == 1 {
            init_beep_samples();
        }

        let sw = folk_screen_width();
        folk_fill_screen(BG);

        // Title
        let title = b"Sound Test - AC97 Audio";
        folk_draw_text(40, 40, title.as_ptr() as i32, title.len() as i32, ACCENT);

        // Info
        let info1 = b"Click buttons below to play audio.";
        folk_draw_text(40, 80, info1.as_ptr() as i32, info1.len() as i32, FG);
        let info2 = b"Requires MCP server restart for -device AC97 to take effect.";
        folk_draw_text(40, 100, info2.as_ptr() as i32, info2.len() as i32, 0x8B949E);

        // Button 1: Quick beep
        let btn1_x = 40;
        let btn1_y = 160;
        let btn1_w = 200;
        let btn1_h = 60;
        folk_draw_rect(btn1_x, btn1_y, btn1_w, btn1_h, BUTTON_BG);
        let label1 = b"BEEP (100ms)";
        folk_draw_text(btn1_x + 40, btn1_y + 24, label1.as_ptr() as i32, label1.len() as i32, 0xFFFFFF);

        // Button 2: Long beep
        let btn2_x = 260;
        let btn2_y = 160;
        folk_draw_rect(btn2_x, btn2_y, btn1_w, btn1_h, BUTTON_BG);
        let label2 = b"LONG BEEP (500ms)";
        folk_draw_text(btn2_x + 24, btn2_y + 24, label2.as_ptr() as i32, label2.len() as i32, 0xFFFFFF);

        // Button 3: Custom samples
        let btn3_x = 480;
        let btn3_y = 160;
        folk_draw_rect(btn3_x, btn3_y, btn1_w, btn1_h, BUTTON_BG);
        let label3 = b"PCM WAVE";
        folk_draw_text(btn3_x + 56, btn3_y + 24, label3.as_ptr() as i32, label3.len() as i32, 0xFFFFFF);

        // Status line
        if FRAMES - LAST_BEEP_TIME < 120 {
            let status = b"> Playing audio...";
            folk_draw_text(40, 260, status.as_ptr() as i32, status.len() as i32, 0x3FB950);
        }

        // Footer
        let footer = b"AC97 driver: BDL + 32 sample buffers allocated at boot";
        folk_draw_text(40, 320, footer.as_ptr() as i32, footer.len() as i32, 0x8B949E);

        // Handle input
        loop {
            let e = core::ptr::addr_of_mut!(EVT) as *mut i32;
            if folk_poll_event(e as i32) == 0 { break; }
            if *e.add(0) != 2 { continue; } // only mouse clicks

            let mx = *e.add(1);
            let my = *e.add(2);

            // Button 1
            if mx >= btn1_x && mx < btn1_x + btn1_w
                && my >= btn1_y && my < btn1_y + btn1_h {
                folk_audio_beep(100);
                LAST_BEEP_TIME = FRAMES;
            }
            // Button 2
            if mx >= btn2_x && mx < btn2_x + btn1_w
                && my >= btn2_y && my < btn2_y + btn1_h {
                folk_audio_beep(500);
                LAST_BEEP_TIME = FRAMES;
            }
            // Button 3
            if mx >= btn3_x && mx < btn3_x + btn1_w
                && my >= btn3_y && my < btn3_y + btn1_h {
                let ptr = core::ptr::addr_of!(BEEP_SAMPLES) as i32;
                folk_audio_play(ptr, BEEP_SAMPLES.len() as i32);
                LAST_BEEP_TIME = FRAMES;
            }
        }
    }
}
