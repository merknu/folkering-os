//! PS/2 Mouse Driver
//!
//! Handles PS/2 mouse input via IRQ12. Decodes 3-byte packets and
//! pushes mouse events to a shared input ring buffer for the Compositor.

use spin::Mutex;
use x86_64::instructions::port::Port;
use core::sync::atomic::{AtomicBool, Ordering};

/// PS/2 data port
const PS2_DATA_PORT: u16 = 0x60;

/// PS/2 command/status port
const PS2_CMD_PORT: u16 = 0x64;

/// Mouse packet state machine
#[derive(Debug, Clone, Copy, PartialEq)]
enum PacketState {
    WaitingByte1,
    WaitingByte2,
    WaitingByte3,
}

/// Current packet being assembled
struct MousePacket {
    state: PacketState,
    byte1: u8,  // Buttons + sign bits + overflow
    byte2: u8,  // X movement
    byte3: u8,  // Y movement
}

impl MousePacket {
    const fn new() -> Self {
        Self {
            state: PacketState::WaitingByte1,
            byte1: 0,
            byte2: 0,
            byte3: 0,
        }
    }

    fn reset(&mut self) {
        self.state = PacketState::WaitingByte1;
    }
}

/// Decoded mouse event
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MouseEvent {
    /// Button state: bit 0 = left, bit 1 = right, bit 2 = middle
    pub buttons: u8,
    /// X movement (signed)
    pub dx: i16,
    /// Y movement (signed, positive = up in PS/2)
    pub dy: i16,
    /// TSC timestamp when IRQ12 fired (for IQE latency measurement)
    pub timestamp_tsc: u64,
}

/// Mouse event ring buffer
const MOUSE_BUFFER_SIZE: usize = 64;

struct MouseBuffer {
    events: [MouseEvent; MOUSE_BUFFER_SIZE],
    read_pos: usize,
    write_pos: usize,
    count: usize,
}

impl MouseBuffer {
    const fn new() -> Self {
        Self {
            events: [MouseEvent { buttons: 0, dx: 0, dy: 0, timestamp_tsc: 0 }; MOUSE_BUFFER_SIZE],
            read_pos: 0,
            write_pos: 0,
            count: 0,
        }
    }

    fn push(&mut self, event: MouseEvent) {
        if self.count < MOUSE_BUFFER_SIZE {
            self.events[self.write_pos] = event;
            self.write_pos = (self.write_pos + 1) % MOUSE_BUFFER_SIZE;
            self.count += 1;
        }
        // Drop event if buffer full
    }

    fn pop(&mut self) -> Option<MouseEvent> {
        if self.count > 0 {
            let event = self.events[self.read_pos];
            self.read_pos = (self.read_pos + 1) % MOUSE_BUFFER_SIZE;
            self.count -= 1;
            Some(event)
        } else {
            None
        }
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Global mouse state
static MOUSE_PACKET: Mutex<MousePacket> = Mutex::new(MousePacket::new());
static MOUSE_BUFFER: Mutex<MouseBuffer> = Mutex::new(MouseBuffer::new());
static MOUSE_INIT: AtomicBool = AtomicBool::new(false);

/// Wait for PS/2 controller input buffer to be ready (can write)
unsafe fn wait_write() {
    for _ in 0..100_000 {
        let mut status = Port::<u8>::new(PS2_CMD_PORT);
        if status.read() & 0x02 == 0 {
            return;
        }
    }
}

/// Wait for PS/2 controller output buffer to have data (can read)
unsafe fn wait_read() {
    for _ in 0..100_000 {
        let mut status = Port::<u8>::new(PS2_CMD_PORT);
        if status.read() & 0x01 != 0 {
            return;
        }
    }
}

/// Send command to PS/2 controller
unsafe fn send_command(cmd: u8) {
    wait_write();
    let mut cmd_port = Port::<u8>::new(PS2_CMD_PORT);
    cmd_port.write(cmd);
}

/// Send data to PS/2 data port
unsafe fn send_data(data: u8) {
    wait_write();
    let mut data_port = Port::<u8>::new(PS2_DATA_PORT);
    data_port.write(data);
}

/// Read data from PS/2 data port
unsafe fn read_data() -> u8 {
    wait_read();
    let mut data_port = Port::<u8>::new(PS2_DATA_PORT);
    data_port.read()
}

/// Send command to mouse (via controller)
unsafe fn mouse_write(cmd: u8) -> u8 {
    send_command(0xD4);  // Send next byte to mouse
    send_data(cmd);
    read_data()  // Read ACK
}

/// Initialize PS/2 mouse
pub fn init() {
    unsafe {
        crate::serial_strln!("[MOUSE] Initializing PS/2 mouse...");

        // Enable auxiliary device (mouse) on PS/2 controller
        send_command(0xA8);

        // Get controller configuration byte
        send_command(0x20);
        let config = read_data();

        // Enable mouse interrupt (bit 1) and disable mouse clock inhibit (bit 5)
        let new_config = (config | 0x02) & !0x20;
        send_command(0x60);
        send_data(new_config);

        // Reset mouse
        let ack = mouse_write(0xFF);
        if ack == 0xFA {
            // Wait for self-test result
            let _test = read_data();  // Should be 0xAA (pass)
            let _id = read_data();    // Should be 0x00 (standard mouse)
        }

        // Set defaults
        mouse_write(0xF6);

        // Enable data reporting
        let enable_ack = mouse_write(0xF4);

        // Enable IRQ12 (mouse) using centralized PIC module
        crate::arch::x86_64::pic::enable_irq(12);

        crate::serial_str!("[MOUSE] Enable ACK: 0x");
        crate::drivers::serial::write_hex(enable_ack as u64);
        crate::serial_strln!("");
        crate::serial_strln!("[MOUSE] PS/2 mouse initialized");

        MOUSE_INIT.store(true, Ordering::Relaxed);
    }
}

/// Initialize PS/2 mouse without enabling PIC IRQ
/// Use this when IOAPIC handles interrupt routing instead of PIC
pub fn init_without_pic() {
    unsafe {
        crate::serial_strln!("[MOUSE] Initializing PS/2 mouse (IOAPIC mode)...");

        // Enable auxiliary device (mouse) on PS/2 controller
        send_command(0xA8);

        // Get controller configuration byte
        send_command(0x20);
        let config = read_data();

        // Enable mouse interrupt (bit 1) and disable mouse clock inhibit (bit 5)
        let new_config = (config | 0x02) & !0x20;
        send_command(0x60);
        send_data(new_config);

        // Reset mouse
        let ack = mouse_write(0xFF);
        if ack == 0xFA {
            // Wait for self-test result
            let _test = read_data();  // Should be 0xAA (pass)
            let _id = read_data();    // Should be 0x00 (standard mouse)
        }

        // Set defaults
        mouse_write(0xF6);

        // Enable data reporting
        let enable_ack = mouse_write(0xF4);

        // Don't enable PIC IRQ - IOAPIC handles it

        crate::serial_str!("[MOUSE] Enable ACK: 0x");
        crate::drivers::serial::write_hex(enable_ack as u64);
        crate::serial_strln!("");
        crate::serial_strln!("[MOUSE] PS/2 mouse initialized (IOAPIC mode)");

        MOUSE_INIT.store(true, Ordering::Relaxed);
    }
}

/// Handle mouse interrupt (called from IDT handler)
pub fn handle_interrupt() {
    let irq_tsc = crate::timer::tsc_now(); // IQE: capture TSC at IRQ entry

    // Read byte from mouse
    let byte = unsafe {
        let mut data_port = Port::<u8>::new(PS2_DATA_PORT);
        data_port.read()
    };

    // Send EOI to Local APIC (IOAPIC routes to Local APIC)
    crate::arch::x86_64::apic::send_eoi();

    if !MOUSE_INIT.load(Ordering::Relaxed) {
        return;
    }

    // Process byte through packet state machine
    let mut packet = MOUSE_PACKET.lock();

    match packet.state {
        PacketState::WaitingByte1 => {
            // Byte 1 must have bit 3 set (always 1 in standard packets)
            if byte & 0x08 != 0 {
                packet.byte1 = byte;
                packet.state = PacketState::WaitingByte2;
            }
            // Otherwise discard (out of sync)
        }
        PacketState::WaitingByte2 => {
            packet.byte2 = byte;
            packet.state = PacketState::WaitingByte3;
        }
        PacketState::WaitingByte3 => {
            packet.byte3 = byte;

            // Decode complete packet
            let buttons = packet.byte1 & 0x07;  // Bits 0-2: L, R, M buttons

            // X movement with sign extension
            let x_sign = (packet.byte1 & 0x10) != 0;
            let dx = if x_sign {
                // Negative: sign extend
                (packet.byte2 as i16) - 256
            } else {
                packet.byte2 as i16
            };

            // Y movement with sign extension (inverted: PS/2 positive = up)
            let y_sign = (packet.byte1 & 0x20) != 0;
            let dy = if y_sign {
                (packet.byte3 as i16) - 256
            } else {
                packet.byte3 as i16
            };

            // Check for overflow (discard if overflow)
            let x_overflow = (packet.byte1 & 0x40) != 0;
            let y_overflow = (packet.byte1 & 0x80) != 0;

            if !x_overflow && !y_overflow {
                let event = MouseEvent { buttons, dx, dy, timestamp_tsc: irq_tsc };
                let buf = &mut MOUSE_BUFFER.lock();
                let depth = buf.count as u64;
                buf.push(event);
                // IQE: record mouse IRQ with buffer depth
                crate::drivers::iqe::record(
                    crate::drivers::iqe::IqeEventType::MouseIrq,
                    irq_tsc,
                    depth,
                );
            }

            packet.reset();
        }
    }
}

/// Read a mouse event from the buffer (non-blocking)
pub fn read_event() -> Option<MouseEvent> {
    MOUSE_BUFFER.lock().pop()
}

/// Check if mouse events are available
pub fn event_available() -> bool {
    !MOUSE_BUFFER.lock().is_empty()
}

/// Get mouse event count in buffer
pub fn event_count() -> usize {
    MOUSE_BUFFER.lock().count
}
