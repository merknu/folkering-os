//! Piano — click keys on screen to play notes via AC97

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

extern "C" {
    fn folk_fill_screen(color: i32);
    fn folk_draw_rect(x: i32, y: i32, w: i32, h: i32, color: i32);
    fn folk_draw_text(x: i32, y: i32, ptr: i32, len: i32, color: i32);
    fn folk_poll_event(event_ptr: i32) -> i32;
    fn folk_audio_play(ptr: i32, sample_count: i32) -> i32;
}

const BG: i32 = 0x0D1117;
const FG: i32 = 0xC9D1D9;
const ACCENT: i32 = 0x58A6FF;
const KEY_WHITE: i32 = 0xF5F5F5;
const KEY_WHITE_PRESSED: i32 = 0xFFD700;
const KEY_BLACK: i32 = 0x202020;
const KEY_BLACK_PRESSED: i32 = 0x404040;
const BORDER: i32 = 0x555555;

static mut EVT: [i32; 4] = [0i32; 4];
static mut PRESSED_KEY: i32 = -1;
static mut FRAMES: i32 = 0;

// Sample buffer: 0.2 second at 44100Hz stereo = 17640 samples
const SAMPLE_COUNT: usize = 17640;
static mut SAMPLES: [i16; SAMPLE_COUNT] = [0i16; SAMPLE_COUNT];

// Note frequencies (Hz) for C4 to B4
// C4 = 261.63, D4 = 293.66, E4 = 329.63, F4 = 349.23
// G4 = 392.00, A4 = 440.00, B4 = 493.88
// C5 = 523.25
//
// Half-period in samples (44100 / freq / 2):
// C4 = 84, D4 = 75, E4 = 67, F4 = 63
// G4 = 56, A4 = 50, B4 = 45, C5 = 42
const HALF_PERIODS: [u32; 8] = [84, 75, 67, 63, 56, 50, 45, 42];
const NOTE_NAMES: &[&[u8]] = &[
    b"C", b"D", b"E", b"F", b"G", b"A", b"B", b"C5",
];

unsafe fn play_note(note_idx: usize) {
    if note_idx >= HALF_PERIODS.len() { return; }
    let half_period = HALF_PERIODS[note_idx];

    // Generate square wave with amplitude envelope (decay for piano-like sound)
    let amplitude_max: i32 = 8000;
    let frame_count = SAMPLE_COUNT / 2;

    let mut i: u32 = 0;
    while i < frame_count as u32 {
        // Decay envelope: amplitude drops linearly from max to 0
        let envelope = amplitude_max - (amplitude_max * i as i32 / frame_count as i32);
        let val: i16 = if (i / half_period) % 2 == 0 {
            envelope as i16
        } else {
            -envelope as i16
        };

        let stereo_idx = (i * 2) as usize;
        if stereo_idx + 1 < SAMPLE_COUNT {
            SAMPLES[stereo_idx] = val;
            SAMPLES[stereo_idx + 1] = val;
        }
        i += 1;
    }

    let ptr = core::ptr::addr_of!(SAMPLES) as i32;
    folk_audio_play(ptr, SAMPLE_COUNT as i32);
}

#[no_mangle]
pub extern "C" fn run() {
    unsafe {
        FRAMES += 1;
        folk_fill_screen(BG);

        // Title
        let title = b"Piano - Click keys to play notes";
        folk_draw_text(40, 30, title.as_ptr() as i32, title.len() as i32, ACCENT);

        let info = b"Uses folk_audio_play with generated PCM samples + decay envelope";
        folk_draw_text(40, 55, info.as_ptr() as i32, info.len() as i32, 0x8B949E);

        // Draw 8 white keys (C major scale)
        let key_w = 70;
        let key_h = 200;
        let key_y = 100;
        let start_x = 40;

        for i in 0..8 {
            let kx = start_x + i * key_w;
            let color = if PRESSED_KEY == i as i32 && FRAMES - LAST_PRESSED_FRAME < 10 {
                KEY_WHITE_PRESSED
            } else {
                KEY_WHITE
            };
            folk_draw_rect(kx, key_y, key_w - 4, key_h, color);
            folk_draw_rect(kx, key_y, key_w - 4, 2, BORDER); // top
            folk_draw_rect(kx, key_y + key_h - 2, key_w - 4, 2, BORDER); // bottom
            folk_draw_rect(kx, key_y, 2, key_h, BORDER); // left
            folk_draw_rect(kx + key_w - 6, key_y, 2, key_h, BORDER); // right

            // Note label
            let name = NOTE_NAMES[i as usize];
            folk_draw_text(kx + 24, key_y + key_h - 30, name.as_ptr() as i32, name.len() as i32, 0x202020);
        }

        // Footer with status
        if PRESSED_KEY >= 0 && FRAMES - LAST_PRESSED_FRAME < 30 {
            let status = b"> Playing note";
            folk_draw_text(40, 340, status.as_ptr() as i32, status.len() as i32, 0x3FB950);
        } else {
            let hint = b"Click any key";
            folk_draw_text(40, 340, hint.as_ptr() as i32, hint.len() as i32, 0x8B949E);
        }

        let footer = b"AC97 audio output - 44100Hz stereo PCM";
        folk_draw_text(40, 380, footer.as_ptr() as i32, footer.len() as i32, 0x8B949E);

        // Handle clicks
        loop {
            let e = core::ptr::addr_of_mut!(EVT) as *mut i32;
            if folk_poll_event(e as i32) == 0 { break; }
            if *e.add(0) != 2 { continue; }

            let mx = *e.add(1);
            let my = *e.add(2);

            if my >= key_y && my < key_y + key_h {
                let key_idx = (mx - start_x) / key_w;
                if key_idx >= 0 && key_idx < 8 {
                    PRESSED_KEY = key_idx;
                    LAST_PRESSED_FRAME = FRAMES;
                    play_note(key_idx as usize);
                }
            }
        }
    }
}

static mut LAST_PRESSED_FRAME: i32 = -100;
