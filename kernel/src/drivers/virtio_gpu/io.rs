//! VirtIO Modern MMIO transport for VirtIO-GPU.
//!
//! Defines the Common Config register offsets, status/feature bits, and the
//! `MmioTransport` struct used for memory-mapped I/O against the device.

// ── VirtIO Device Status Bits ──────────────────────────────────────────

pub(super) const STATUS_ACKNOWLEDGE: u8 = 1;
pub(super) const STATUS_DRIVER: u8 = 2;
pub(super) const STATUS_DRIVER_OK: u8 = 4;
pub(super) const STATUS_FEATURES_OK: u8 = 8;
pub(super) const STATUS_FAILED: u8 = 128;

// ── VirtIO GPU Feature Bits ────────────────────────────────────────────

pub(super) const VIRTIO_GPU_F_VIRGL: u32 = 1 << 0; // 3D/VirGL support
pub(super) const VIRTIO_GPU_F_EDID: u32 = 1 << 1;  // EDID display info

// ── VirtIO GPU Command Flags ───────────────────────────────────────────

pub(super) const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;

// ── Modern VirtIO Common Config Register Offsets (OASIS v1.0) ──────────
// Note: device_status (0x14) is u8, config_generation (0x15) is u8

pub(super) const VIRTIO_PCI_COMMON_DFSELECT: usize = 0x00;  // u32
pub(super) const VIRTIO_PCI_COMMON_DF: usize = 0x04;        // u32
pub(super) const VIRTIO_PCI_COMMON_GFSELECT: usize = 0x08;  // u32
pub(super) const VIRTIO_PCI_COMMON_GF: usize = 0x0C;        // u32
pub(super) const VIRTIO_PCI_COMMON_STATUS: usize = 0x14;    // u8
pub(super) const VIRTIO_PCI_COMMON_Q_SELECT: usize = 0x16;  // u16
pub(super) const VIRTIO_PCI_COMMON_Q_SIZE: usize = 0x18;    // u16
pub(super) const VIRTIO_PCI_COMMON_Q_ENABLE: usize = 0x1C;  // u16
pub(super) const VIRTIO_PCI_COMMON_Q_NOFF: usize = 0x1E;    // u16
pub(super) const VIRTIO_PCI_COMMON_Q_DESCLO: usize = 0x20;  // u32
pub(super) const VIRTIO_PCI_COMMON_Q_DESCHI: usize = 0x24;  // u32
pub(super) const VIRTIO_PCI_COMMON_Q_AVAILLO: usize = 0x28; // u32
pub(super) const VIRTIO_PCI_COMMON_Q_AVAILHI: usize = 0x2C; // u32
pub(super) const VIRTIO_PCI_COMMON_Q_USEDLO: usize = 0x30;  // u32
pub(super) const VIRTIO_PCI_COMMON_Q_USEDHI: usize = 0x34;  // u32

// ── MMIO Transport ─────────────────────────────────────────────────────

/// VirtIO Modern MMIO register access.
pub(super) struct MmioTransport {
    pub(super) common_base: usize,   // Virtual address of common config MMIO region
    pub(super) notify_base: usize,   // Virtual address of notify MMIO region
    pub(super) notify_mul: u32,      // Notify offset multiplier
    pub(super) notify_off: u16,      // Queue 0's notify offset (from Q_NOFF register)
    pub(super) isr_base: usize,      // Virtual address of ISR region
    pub(super) device_base: usize,   // Virtual address of device-specific config
}

impl MmioTransport {
    pub(super) fn read_common32(&self, off: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.common_base + off) as *const u32) }
    }
    pub(super) fn write_common32(&self, off: usize, val: u32) {
        unsafe { core::ptr::write_volatile((self.common_base + off) as *mut u32, val) }
    }
    pub(super) fn read_common16(&self, off: usize) -> u16 {
        unsafe { core::ptr::read_volatile((self.common_base + off) as *const u16) }
    }
    pub(super) fn write_common16(&self, off: usize, val: u16) {
        unsafe { core::ptr::write_volatile((self.common_base + off) as *mut u16, val) }
    }
    pub(super) fn read_common8(&self, off: usize) -> u8 {
        unsafe { core::ptr::read_volatile((self.common_base + off) as *const u8) }
    }
    pub(super) fn write_common8(&self, off: usize, val: u8) {
        unsafe { core::ptr::write_volatile((self.common_base + off) as *mut u8, val) }
    }
    pub(super) fn notify_queue(&self, _queue_idx: u16) {
        let off = self.notify_off as usize * self.notify_mul as usize;
        let addr = self.notify_base + off;
        unsafe { core::ptr::write_volatile(addr as *mut u32, 0) }
    }
}
