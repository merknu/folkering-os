//! PS/2 Keyboard Driver
//!
//! Simple keyboard driver for user input. Uses IRQ1 via legacy PIC.

use spin::Mutex;
use x86_64::instructions::port::Port;
use core::sync::atomic::{AtomicBool, Ordering};

/// PS/2 keyboard data port
const KEYBOARD_DATA_PORT: u16 = 0x60;

/// PS/2 keyboard status port
const KEYBOARD_STATUS_PORT: u16 = 0x64;

/// Key buffer size
const KEY_BUFFER_SIZE: usize = 64;

/// Circular key buffer
pub struct KeyBuffer {
    buffer: [u8; KEY_BUFFER_SIZE],
    read_pos: usize,
    write_pos: usize,
    count: usize,
}

impl KeyBuffer {
    const fn new() -> Self {
        Self {
            buffer: [0; KEY_BUFFER_SIZE],
            read_pos: 0,
            write_pos: 0,
            count: 0,
        }
    }

    fn push(&mut self, key: u8) {
        if self.count < KEY_BUFFER_SIZE {
            self.buffer[self.write_pos] = key;
            self.write_pos = (self.write_pos + 1) % KEY_BUFFER_SIZE;
            self.count += 1;
        }
        // Drop key if buffer full
    }

    fn pop(&mut self) -> Option<u8> {
        if self.count > 0 {
            let key = self.buffer[self.read_pos];
            self.read_pos = (self.read_pos + 1) % KEY_BUFFER_SIZE;
            self.count -= 1;
            Some(key)
        } else {
            None
        }
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Global key buffer
static KEY_BUFFER: Mutex<KeyBuffer> = Mutex::new(KeyBuffer::new());

/// Keyboard initialized flag
static KEYBOARD_INIT: AtomicBool = AtomicBool::new(false);

/// Shift key state
static SHIFT_PRESSED: AtomicBool = AtomicBool::new(false);

/// Caps lock state
static CAPS_LOCK: AtomicBool = AtomicBool::new(false);

/// Alt key state
static ALT_PRESSED: AtomicBool = AtomicBool::new(false);

/// Ctrl key state
static CTRL_PRESSED: AtomicBool = AtomicBool::new(false);

/// Extended scancode prefix received (0xE0)
static EXTENDED_PREFIX: AtomicBool = AtomicBool::new(false);

/// US keyboard scancode set 1 to ASCII mapping
const SCANCODE_TO_ASCII: [u8; 128] = [
    0, 27, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8',    // 0-9
    b'9', b'0', b'-', b'=', 8, b'\t',                          // 10-15
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', // 16-25
    b'[', b']', b'\n', 0, b'a', b's',                           // 26-31
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';',            // 32-39
    b'\'', b'`', 0, b'\\', b'z', b'x', b'c', b'v',             // 40-47
    b'b', b'n', b'm', b',', b'.', b'/', 0, b'*',               // 48-55
    0, b' ', 0, 0, 0, 0, 0, 0,                                  // 56-63 (alt, space, caps, F1-F5)
    0, 0, 0, 0, 0, 0, 0, b'7',                                  // 64-71 (F6-F10, numlock, scroll, numpad)
    b'8', b'9', b'-', b'4', b'5', b'6', b'+', b'1',            // 72-79
    b'2', b'3', b'0', b'.', 0, 0, 0, 0,                        // 80-87
    0, 0, 0, 0, 0, 0, 0, 0,                                     // 88-95
    0, 0, 0, 0, 0, 0, 0, 0,                                     // 96-103
    0, 0, 0, 0, 0, 0, 0, 0,                                     // 104-111
    0, 0, 0, 0, 0, 0, 0, 0,                                     // 112-119
    0, 0, 0, 0, 0, 0, 0, 0,                                     // 120-127
];

/// Shifted US keyboard scancode set 1 to ASCII mapping
const SCANCODE_TO_ASCII_SHIFT: [u8; 128] = [
    0, 27, b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*',     // 0-9
    b'(', b')', b'_', b'+', 8, b'\t',                          // 10-15
    b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I', b'O', b'P', // 16-25
    b'{', b'}', b'\n', 0, b'A', b'S',                           // 26-31
    b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':',            // 32-39
    b'"', b'~', 0, b'|', b'Z', b'X', b'C', b'V',               // 40-47
    b'B', b'N', b'M', b'<', b'>', b'?', 0, b'*',               // 48-55
    0, b' ', 0, 0, 0, 0, 0, 0,                                  // 56-63
    0, 0, 0, 0, 0, 0, 0, b'7',                                  // 64-71
    b'8', b'9', b'-', b'4', b'5', b'6', b'+', b'1',            // 72-79
    b'2', b'3', b'0', b'.', 0, 0, 0, 0,                        // 80-87
    0, 0, 0, 0, 0, 0, 0, 0,                                     // 88-95
    0, 0, 0, 0, 0, 0, 0, 0,                                     // 96-103
    0, 0, 0, 0, 0, 0, 0, 0,                                     // 104-111
    0, 0, 0, 0, 0, 0, 0, 0,                                     // 112-119
    0, 0, 0, 0, 0, 0, 0, 0,                                     // 120-127
];

/// Special scancodes
const SCANCODE_LSHIFT: u8 = 0x2A;
const SCANCODE_RSHIFT: u8 = 0x36;
const SCANCODE_CAPS_LOCK: u8 = 0x3A;
const SCANCODE_ESCAPE: u8 = 0x01;
const SCANCODE_BACKSPACE: u8 = 0x0E;
const SCANCODE_ENTER: u8 = 0x1C;
const SCANCODE_ALT: u8 = 0x38;
const SCANCODE_CTRL: u8 = 0x1D;
const SCANCODE_F12: u8 = 0x58;

/// Extended scancode prefix
const SCANCODE_EXTENDED: u8 = 0xE0;

/// Extended key scancodes (after 0xE0 prefix)
const EXT_LEFT_WINDOWS: u8 = 0x5B;
const EXT_RIGHT_WINDOWS: u8 = 0x5C;
const EXT_ARROW_UP: u8 = 0x48;
const EXT_ARROW_DOWN: u8 = 0x50;
const EXT_ARROW_LEFT: u8 = 0x4B;
const EXT_ARROW_RIGHT: u8 = 0x4D;
const EXT_HOME: u8 = 0x47;
const EXT_END: u8 = 0x4F;
const EXT_DELETE: u8 = 0x53;

/// Special key codes we emit (outside normal ASCII, userspace can detect these)
/// These are NOT scancodes - they are our own key codes
pub const KEY_LEFT_WINDOWS: u8 = 0xE5;
pub const KEY_RIGHT_WINDOWS: u8 = 0xE6;
pub const KEY_ARROW_UP: u8 = 0x80;
pub const KEY_ARROW_DOWN: u8 = 0x81;
pub const KEY_ARROW_LEFT: u8 = 0x82;
pub const KEY_ARROW_RIGHT: u8 = 0x83;
pub const KEY_HOME: u8 = 0x84;
pub const KEY_END: u8 = 0x85;
pub const KEY_DELETE: u8 = 0x86;
pub const KEY_SHIFT_TAB: u8 = 0x87;
pub const KEY_ALT_TAB: u8 = 0x88;
pub const KEY_CTRL_F12: u8 = 0x89;
pub const KEY_CTRL_C: u8 = 0x8A;
pub const KEY_CTRL_V: u8 = 0x8B;

/// Initialize keyboard driver
pub fn init() {
    unsafe {
        // Clear any pending keyboard data before enabling interrupt
        let mut status = Port::<u8>::new(KEYBOARD_STATUS_PORT);
        let mut data = Port::<u8>::new(KEYBOARD_DATA_PORT);
        while status.read() & 1 != 0 {
            let _ = data.read();
        }

        // Enable IRQ1 (keyboard) using centralized PIC module
        crate::arch::x86_64::pic::enable_irq(1);

        crate::drivers::serial::write_str("[KEYBOARD] PS/2 keyboard driver initialized\n");
    }

    KEYBOARD_INIT.store(true, Ordering::Relaxed);
}

/// Initialize keyboard driver without enabling PIC IRQ
/// Use this when IOAPIC handles interrupt routing instead of PIC
pub fn init_without_pic() {
    if KEYBOARD_INIT.load(Ordering::Relaxed) {
        return;
    }

    unsafe {
        // Clear any pending data in the keyboard buffer
        let mut status = Port::<u8>::new(KEYBOARD_STATUS_PORT);
        let mut data = Port::<u8>::new(KEYBOARD_DATA_PORT);
        while status.read() & 1 != 0 {
            let _ = data.read();
        }

        // Don't enable PIC IRQ - IOAPIC handles it
        crate::drivers::serial::write_str("[KEYBOARD] PS/2 keyboard driver initialized (IOAPIC mode)\n");
    }

    KEYBOARD_INIT.store(true, Ordering::Relaxed);
}

/// Handle keyboard interrupt (called from IDT handler)
pub fn handle_interrupt() {
    let irq_tsc = crate::drivers::iqe::rdtsc(); // IQE: capture TSC at IRQ1 entry

    // Always read scancode from port 0x60 to clear the keyboard controller
    let scancode = unsafe {
        let mut data_port = Port::<u8>::new(KEYBOARD_DATA_PORT);
        data_port.read()
    };

    // IQE: record keyboard event (all scancodes, userspace filters key-ups)
    crate::drivers::iqe::record(
        crate::drivers::iqe::IqeEventType::KeyboardIrq, irq_tsc, scancode as u64);

    // Debug: log first 5 keystrokes to serial
    static KEY_DBG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let kc = KEY_DBG.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if kc < 5 {
        crate::serial_str!("[KEY] sc=");
        crate::drivers::serial::write_dec(scancode as u32);
        crate::serial_str!(" iqe_avail=");
        crate::drivers::serial::write_dec(crate::drivers::iqe::available() as u32);
        crate::serial_strln!("");
    }

    // Send EOI to Local APIC (IOAPIC routes to Local APIC)
    crate::arch::x86_64::apic::send_eoi();

    if !KEYBOARD_INIT.load(Ordering::Relaxed) {
        return;
    }


    // Check if this is the extended prefix (0xE0)
    // Just ignore extended scancodes for now - they can cause issues
    if scancode == SCANCODE_EXTENDED {
        EXTENDED_PREFIX.store(true, Ordering::Relaxed);
        return;
    }

    // Handle extended scancode sequence (after 0xE0 prefix)
    let is_extended = EXTENDED_PREFIX.load(Ordering::Relaxed);
    if is_extended {
        EXTENDED_PREFIX.store(false, Ordering::Relaxed);

        // Ignore key releases in extended mode
        if scancode & 0x80 != 0 {
            return;
        }

        // Map extended scancodes to our key codes
        let key = match scancode {
            EXT_ARROW_UP => KEY_ARROW_UP,
            EXT_ARROW_DOWN => KEY_ARROW_DOWN,
            EXT_ARROW_LEFT => KEY_ARROW_LEFT,
            EXT_ARROW_RIGHT => KEY_ARROW_RIGHT,
            EXT_HOME => KEY_HOME,
            EXT_END => KEY_END,
            EXT_DELETE => KEY_DELETE,
            EXT_LEFT_WINDOWS => KEY_LEFT_WINDOWS,
            EXT_RIGHT_WINDOWS => KEY_RIGHT_WINDOWS,
            _ => return, // Unknown extended scancode
        };

        KEY_BUFFER.lock().push(key);
        return;
    }

    // Handle key release (bit 7 set) for non-extended keys
    if scancode & 0x80 != 0 {
        let released = scancode & 0x7F;
        if released == SCANCODE_LSHIFT || released == SCANCODE_RSHIFT {
            SHIFT_PRESSED.store(false, Ordering::Relaxed);
        }
        if released == SCANCODE_ALT {
            ALT_PRESSED.store(false, Ordering::Relaxed);
        }
        if released == SCANCODE_CTRL {
            CTRL_PRESSED.store(false, Ordering::Relaxed);
        }
        return;
    }

    // Handle special keys
    match scancode {
        SCANCODE_LSHIFT | SCANCODE_RSHIFT => {
            SHIFT_PRESSED.store(true, Ordering::Relaxed);
            return;
        }
        SCANCODE_CAPS_LOCK => {
            let current = CAPS_LOCK.load(Ordering::Relaxed);
            CAPS_LOCK.store(!current, Ordering::Relaxed);
            return;
        }
        SCANCODE_ALT => {
            ALT_PRESSED.store(true, Ordering::Relaxed);
            return;
        }
        SCANCODE_CTRL => {
            CTRL_PRESSED.store(true, Ordering::Relaxed);
            return;
        }
        _ => {}
    }

    // Ctrl+F12: emit special key code for UI dump
    if scancode == SCANCODE_F12 && CTRL_PRESSED.load(Ordering::Relaxed) {
        KEY_BUFFER.lock().push(KEY_CTRL_F12);
        return;
    }

    // Ctrl+C: emit clipboard copy key code (scancode 0x2E = 'c')
    if scancode == 0x2E && CTRL_PRESSED.load(Ordering::Relaxed) {
        KEY_BUFFER.lock().push(KEY_CTRL_C);
        return;
    }

    // Ctrl+V: emit clipboard paste key code (scancode 0x2F = 'v')
    if scancode == 0x2F && CTRL_PRESSED.load(Ordering::Relaxed) {
        KEY_BUFFER.lock().push(KEY_CTRL_V);
        return;
    }

    // Convert scancode to ASCII
    let shift = SHIFT_PRESSED.load(Ordering::Relaxed);
    let caps = CAPS_LOCK.load(Ordering::Relaxed);

    let ascii = if shift {
        SCANCODE_TO_ASCII_SHIFT[scancode as usize]
    } else {
        SCANCODE_TO_ASCII[scancode as usize]
    };

    // Apply caps lock (only affects a-z)
    let ascii = if caps && ascii >= b'a' && ascii <= b'z' {
        ascii - 32 // To uppercase
    } else if caps && ascii >= b'A' && ascii <= b'Z' && !shift {
        ascii + 32 // To lowercase (caps + no shift = lowercase)
    } else {
        ascii
    };

    // Special: Alt+Tab sends distinct keycode for window cycling
    if ascii == b'\t' && ALT_PRESSED.load(Ordering::Relaxed) {
        KEY_BUFFER.lock().push(KEY_ALT_TAB);
        return;
    }

    // Special: Shift+Tab sends distinct keycode for reverse navigation
    if ascii == b'\t' && shift {
        KEY_BUFFER.lock().push(KEY_SHIFT_TAB);
        return;
    }

    // Push to buffer if valid
    if ascii != 0 {
        KEY_BUFFER.lock().push(ascii);
    }
}

/// Read a key from the buffer (non-blocking)
pub fn read_key() -> Option<u8> {
    KEY_BUFFER.lock().pop()
}

/// Check if a key is available
pub fn key_available() -> bool {
    !KEY_BUFFER.lock().is_empty()
}

/// Read a key (blocking)
pub fn read_key_blocking() -> u8 {
    loop {
        if let Some(key) = read_key() {
            return key;
        }
        x86_64::instructions::hlt();
    }
}

/// Read a line into a buffer (blocking)
/// Returns the number of characters read (excluding newline)
pub fn read_line(buffer: &mut [u8]) -> usize {
    let mut pos = 0;

    loop {
        let key = read_key_blocking();

        match key {
            b'\n' | 13 => {
                // Enter pressed, return line
                return pos;
            }
            8 | 127 => {
                // Backspace
                if pos > 0 {
                    pos -= 1;
                    // Echo backspace (erase character)
                    crate::drivers::serial::write_str("\x08 \x08");
                }
            }
            27 => {
                // Escape - cancel line
                return 0;
            }
            _ if pos < buffer.len() => {
                buffer[pos] = key;
                pos += 1;
            }
            _ => {
                // Buffer full, ignore
            }
        }
    }
}
