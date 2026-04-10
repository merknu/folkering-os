//! Per-instance application state for the shell-hosted apps
//! (calculator, greeter, folkpad).

use libfolk::println;
use libfolk::sys::synapse::write_file;

/// Maximum number of simultaneously open app instances.
pub const MAX_APP_INSTANCES: usize = 8;

/// Per-instance application state — indexed by Compositor win_id.
/// Enables multiple calculators open simultaneously with independent state.
#[derive(Copy, Clone)]
pub struct AppState {
    pub win_id: u32,        // Compositor window ID (0 = slot unused)
    pub app_type: u8,       // 0=calculator, 1=greeter, 2=folkpad
    pub display: i64,       // Current display value (calc) or unused (greeter)
    pub accumulator: i64,   // Stored accumulator (calc) or unused
    pub operator: u8,       // 0=none, 1=+, 2=-, 3=*, 4=/
    pub fresh_digit: bool,  // True = next digit starts a new number
    // Greeter state: last submitted name
    pub greet_name: [u8; 32],
    pub greet_name_len: usize,
    // Folkpad state (app_type == 2)
    pub pad_lines: [[u8; 64]; 10],
    pub pad_line_lens: [usize; 10],
    pub pad_line_count: usize,
    pub pad_saved: bool,
}

impl AppState {
    pub const fn empty() -> Self {
        Self {
            win_id: 0, app_type: 0, display: 0, accumulator: 0, operator: 0,
            fresh_digit: true, greet_name: [0; 32], greet_name_len: 0,
            pad_lines: [[0; 64]; 10], pad_line_lens: [0; 10],
            pad_line_count: 0, pad_saved: false,
        }
    }

    pub fn new_calculator(win_id: u32) -> Self {
        Self { win_id, app_type: 0, ..Self::empty() }
    }

    pub fn new_folkpad(win_id: u32) -> Self {
        Self { win_id, app_type: 2, ..Self::empty() }
    }
}

/// Fixed-size app state registry (no alloc needed).
/// Lives as `static mut` in main.rs; this module exposes safe accessors.
pub static mut APP_STATES: [AppState; MAX_APP_INSTANCES] = [AppState::empty(); MAX_APP_INSTANCES];

/// Find or create an `AppState` for a given `win_id`.
/// Returns `None` if all slots are full.
pub fn get_app_state(win_id: u32) -> Option<&'static mut AppState> {
    unsafe {
        for state in APP_STATES.iter_mut() {
            if state.win_id == win_id {
                return Some(state);
            }
        }
        for state in APP_STATES.iter_mut() {
            if state.win_id == 0 {
                *state = AppState::new_calculator(win_id);
                return Some(state);
            }
        }
        None
    }
}

/// Binary entry size for serialized app state on disk:
/// `win_id(4) + display(8) + accumulator(8) + operator(1) + fresh_digit(1)`
pub const APP_STATE_ENTRY_SIZE: usize = 22;

/// Serialize all active app states and write to VFS for persistence
/// across reboots. Format: `[count: u8][entries...]`.
pub fn save_all_app_states() {
    let mut buf = [0u8; 1 + MAX_APP_INSTANCES * APP_STATE_ENTRY_SIZE];
    let mut count: u8 = 0;
    let mut pos = 1;

    unsafe {
        for state in APP_STATES.iter() {
            if state.win_id != 0 {
                buf[pos..pos+4].copy_from_slice(&state.win_id.to_le_bytes());
                buf[pos+4..pos+12].copy_from_slice(&state.display.to_le_bytes());
                buf[pos+12..pos+20].copy_from_slice(&state.accumulator.to_le_bytes());
                buf[pos+20] = state.operator;
                buf[pos+21] = state.fresh_digit as u8;
                pos += APP_STATE_ENTRY_SIZE;
                count += 1;
            }
        }
    }
    buf[0] = count;

    if count > 0 {
        match write_file("app_states.dat", &buf[..pos]) {
            Ok(()) => println!("[SHELL] Saved {} app state(s)", count),
            Err(_) => println!("[SHELL] Failed to save app states"),
        }
    } else {
        println!("[SHELL] No app states to save");
    }
}

// ── Command buffer for stdin input ────────────────────────────────────

pub const CMD_BUFFER_SIZE: usize = 256;

/// Command buffer for user input. Lives in BSS, accessed via volatile helpers.
pub static mut CMD_BUFFER: [u8; CMD_BUFFER_SIZE] = [0u8; CMD_BUFFER_SIZE];
pub static mut CMD_LEN: usize = 0;

#[inline]
pub fn get_cmd_len() -> usize {
    unsafe { core::ptr::read_volatile(&CMD_LEN) }
}

#[inline]
pub fn set_cmd_len(len: usize) {
    unsafe { core::ptr::write_volatile(&mut CMD_LEN, len) }
}

#[inline]
pub fn get_cmd_byte(idx: usize) -> u8 {
    unsafe { core::ptr::read_volatile(&CMD_BUFFER[idx]) }
}

#[inline]
pub fn set_cmd_byte(idx: usize, val: u8) {
    unsafe { core::ptr::write_volatile(&mut CMD_BUFFER[idx], val) }
}

pub fn clear_buffer() {
    set_cmd_len(0);
    for i in 0..CMD_BUFFER_SIZE {
        set_cmd_byte(i, 0);
    }
}
