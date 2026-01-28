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

/// Initialize keyboard driver
pub fn init() {
    unsafe {
        // Enable keyboard IRQ (IRQ1) on PIC
        // The PIC was fully disabled by APIC init, we need to enable IRQ1
        let mut pic1_data = Port::<u8>::new(0x21);

        // First, reinitialize PIC1 to route IRQ0-7 to vectors 32-39
        let mut pic1_cmd = Port::<u8>::new(0x20);
        let mut pic2_cmd = Port::<u8>::new(0xA0);
        let mut pic2_data = Port::<u8>::new(0xA1);

        // ICW1: Initialize + ICW4 needed
        pic1_cmd.write(0x11);
        pic2_cmd.write(0x11);

        // ICW2: Vector offset (32 for PIC1, 40 for PIC2)
        pic1_data.write(32);
        pic2_data.write(40);

        // ICW3: Tell PICs about each other
        pic1_data.write(4);  // IRQ2 has slave
        pic2_data.write(2);  // Slave ID 2

        // ICW4: 8086 mode
        pic1_data.write(0x01);
        pic2_data.write(0x01);

        // Mask all interrupts except IRQ1 (keyboard)
        // Bit 0 = IRQ0 (timer) - masked
        // Bit 1 = IRQ1 (keyboard) - enabled (0)
        // Bit 2 = IRQ2 (cascade) - enabled for PIC2
        pic1_data.write(0b11111001);  // Only IRQ1 and IRQ2 enabled
        pic2_data.write(0xFF);         // All PIC2 interrupts masked

        // Clear any pending keyboard data
        let mut status = Port::<u8>::new(KEYBOARD_STATUS_PORT);
        let mut data = Port::<u8>::new(KEYBOARD_DATA_PORT);
        while status.read() & 1 != 0 {
            let _ = data.read();
        }

        crate::drivers::serial::write_str("[KEYBOARD] PS/2 keyboard driver initialized\n");
        crate::drivers::serial::write_str("[KEYBOARD] IRQ1 enabled (vector 33)\n");
    }

    KEYBOARD_INIT.store(true, Ordering::Relaxed);
}

/// Handle keyboard interrupt (called from IDT handler)
pub fn handle_interrupt() {
    // Always read scancode from port 0x60 to clear the keyboard controller
    let scancode = unsafe {
        let mut data_port = Port::<u8>::new(KEYBOARD_DATA_PORT);
        data_port.read()
    };

    // Send EOI to PIC immediately (must happen for every interrupt)
    unsafe {
        let mut pic1_cmd = Port::<u8>::new(0x20);
        pic1_cmd.write(0x20);
    }

    if !KEYBOARD_INIT.load(Ordering::Relaxed) {
        return;
    }

    // Handle key release (bit 7 set)
    if scancode & 0x80 != 0 {
        let released = scancode & 0x7F;
        if released == SCANCODE_LSHIFT || released == SCANCODE_RSHIFT {
            SHIFT_PRESSED.store(false, Ordering::Relaxed);
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
        _ => {}
    }

    // Convert scancode to ASCII
    let shift = SHIFT_PRESSED.load(Ordering::Relaxed);
    let caps = CAPS_LOCK.load(Ordering::Relaxed);

    let ascii = if shift {
        unsafe { SCANCODE_TO_ASCII_SHIFT[scancode as usize] }
    } else {
        unsafe { SCANCODE_TO_ASCII[scancode as usize] }
    };

    // Apply caps lock (only affects a-z)
    let ascii = if caps && ascii >= b'a' && ascii <= b'z' {
        ascii - 32 // To uppercase
    } else if caps && ascii >= b'A' && ascii <= b'Z' && !shift {
        ascii + 32 // To lowercase (caps + no shift = lowercase)
    } else {
        ascii
    };

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
