//! Low-level I/O port helpers + VirtIO PCI register offsets.

use x86_64::instructions::port::Port;

// ── VirtIO Legacy PCI Register Offsets (from BAR0) ─────────────────────

pub(super) const VIRTIO_PCI_DEVICE_FEATURES: u16 = 0x00;  // 32-bit, RO
pub(super) const VIRTIO_PCI_DRIVER_FEATURES: u16 = 0x04;  // 32-bit, RW
pub(super) const VIRTIO_PCI_QUEUE_PFN: u16 = 0x08;        // 32-bit, RW
pub(super) const VIRTIO_PCI_QUEUE_SIZE: u16 = 0x0C;       // 16-bit, RO
pub(super) const VIRTIO_PCI_QUEUE_SEL: u16 = 0x0E;        // 16-bit, RW
pub(super) const VIRTIO_PCI_QUEUE_NOTIFY: u16 = 0x10;     // 16-bit, RW
pub(super) const VIRTIO_PCI_DEVICE_STATUS: u16 = 0x12;    // 8-bit, RW

// ── VirtIO Device Status Bits ──────────────────────────────────────────

pub(super) const STATUS_ACKNOWLEDGE: u8 = 1;
pub(super) const STATUS_DRIVER: u8 = 2;
pub(super) const STATUS_DRIVER_OK: u8 = 4;
pub(super) const STATUS_FAILED: u8 = 128;

// ── VirtIO Net Feature Bits ────────────────────────────────────────────

pub(super) const VIRTIO_NET_F_MAC: u32 = 1 << 5;

// ── Buffer constants ───────────────────────────────────────────────────

pub(super) const RX_BUF_COUNT: usize = 32;
pub(super) const RX_BUF_SIZE: usize = 2048;  // Ethernet MTU 1514 + VirtIO net header 10 + margin
pub(super) const TX_BUF_SIZE: usize = 2048;

/// VirtIO net header (legacy, 10 bytes — no mergeable rx buffers feature)
pub(super) const VIRTIO_NET_HDR_SIZE: usize = 10;

// ── I/O Port Helpers ───────────────────────────────────────────────────

pub(super) fn read_io8(base: u16, offset: u16) -> u8 {
    unsafe { Port::<u8>::new(base + offset).read() }
}

pub(super) fn write_io8(base: u16, offset: u16, val: u8) {
    unsafe { Port::<u8>::new(base + offset).write(val); }
}

pub(super) fn read_io16(base: u16, offset: u16) -> u16 {
    unsafe { Port::<u16>::new(base + offset).read() }
}

pub(super) fn write_io16(base: u16, offset: u16, val: u16) {
    unsafe { Port::<u16>::new(base + offset).write(val); }
}

pub(super) fn read_io32(base: u16, offset: u16) -> u32 {
    unsafe { Port::<u32>::new(base + offset).read() }
}

pub(super) fn write_io32(base: u16, offset: u16, val: u32) {
    unsafe { Port::<u32>::new(base + offset).write(val); }
}
