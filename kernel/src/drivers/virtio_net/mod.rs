//! VirtIO Network Device Driver — Packet I/O
//!
//! Performs VirtIO legacy handshake, negotiates VIRTIO_NET_F_MAC feature,
//! reads MAC address, allocates RX/TX virtqueues, populates RX queue
//! with empty buffers, and provides receive_packet/transmit_packet for
//! raw Ethernet frame I/O.
//!
//! Module structure:
//! - `mod.rs` (this file) — VirtIONet struct, init, public API
//! - `io.rs` — I/O port helpers + VirtIO PCI register offsets
//! - `queue.rs` — virtqueue setup
//! - `rx.rs` — RX buffer population, packet receive, recycling
//! - `tx.rs` — packet transmit

extern crate alloc;

mod io;
mod queue;
mod rx;
mod tx;

use spin::Mutex;
use core::sync::atomic::{AtomicU32, Ordering};

use super::pci::{self, PciDevice, BarType};
use super::virtio::Virtqueue;
use io::*;

// ── MSI-X Capability ID ────────────────────────────────────────────────

const PCI_CAP_ID_MSIX: u8 = 0x11;

/// Packet receive/transmit statistics
static RX_PACKET_COUNT: AtomicU32 = AtomicU32::new(0);
static TX_PACKET_COUNT: AtomicU32 = AtomicU32::new(0);

// ── Error Type ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    DeviceNotFound,
    BarNotIoSpace,
    DeviceFailed,
    NoMacFeature,
    QueueSetupFailed,
}

// ── Device State ───────────────────────────────────────────────────────

pub struct VirtIONet {
    pub(super) io_base: u16,
    pub(super) mac: [u8; 6],
    pub(super) rx_queue: Virtqueue,
    pub(super) tx_queue: Virtqueue,
    pub(super) rx_bufs_phys: [usize; RX_BUF_COUNT],
    pub(super) rx_bufs_virt: [usize; RX_BUF_COUNT],
}

static NET_DEVICE: Mutex<Option<VirtIONet>> = Mutex::new(None);

// ── MSI-X Detection ────────────────────────────────────────────────────

/// Check if the PCI device has MSI-X capability
///
/// Capped at 32 iterations to defend against a self-referential cap
/// pointer (real PCI devices have at most ~16 caps; the existing
/// `virtio_gpu/pci_setup.rs` uses the same 32-cap bound).
fn has_msix(dev: &PciDevice) -> bool {
    let mut ptr = dev.capabilities_ptr;
    let mut iterations = 0u8;
    while ptr != 0 && iterations < 32 {
        let cap_id = pci::pci_read8(dev.bus, dev.device, dev.function, ptr);
        if cap_id == PCI_CAP_ID_MSIX {
            // Check if MSI-X is actually enabled (bit 15 of Message Control at cap+2)
            let msg_ctrl = pci::pci_read16(dev.bus, dev.device, dev.function, ptr + 2);
            return msg_ctrl & 0x8000 != 0;
        }
        let next = pci::pci_read8(dev.bus, dev.device, dev.function, ptr + 1);
        ptr = next;
        iterations += 1;
    }
    false
}

// ── Initialization ─────────────────────────────────────────────────────

pub fn init() -> Result<(), NetError> {
    crate::serial_strln!("[VIRTIO_NET] Looking for VirtIO network device...");

    // Find the PCI device
    let pci_dev = pci::find_virtio_net().ok_or_else(|| {
        crate::serial_strln!("[VIRTIO_NET] No VirtIO network device found on PCI bus");
        NetError::DeviceNotFound
    })?;

    crate::serial_str!("[VIRTIO_NET] Found device ");
    crate::drivers::serial::write_hex(pci_dev.vendor_id as u64);
    crate::serial_str!(":");
    crate::drivers::serial::write_hex(pci_dev.device_id as u64);
    crate::serial_str!(" at ");
    crate::drivers::serial::write_dec(pci_dev.bus as u32);
    crate::serial_str!(":");
    crate::drivers::serial::write_dec(pci_dev.device as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(pci_dev.function as u32);
    crate::drivers::serial::write_newline();

    // Decode BAR0 (must be I/O space for legacy transport)
    let io_base = match pci::decode_bar(&pci_dev, 0) {
        BarType::Io { base } => base,
        _ => {
            crate::serial_strln!("[VIRTIO_NET] ERROR: BAR0 is not I/O space");
            return Err(NetError::BarNotIoSpace);
        }
    };

    crate::serial_str!("[VIRTIO_NET] BAR0 I/O base: ");
    crate::drivers::serial::write_hex(io_base as u64);
    crate::drivers::serial::write_newline();

    // Enable PCI bus mastering (required for DMA)
    pci::enable_bus_master(pci_dev.bus, pci_dev.device, pci_dev.function);

    // ── VirtIO Handshake ───────────────────────────────────────────────

    // Step 1: Reset
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, 0);

    // Step 2: ACKNOWLEDGE
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE);

    // Step 3: DRIVER
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    // ── Feature Negotiation ────────────────────────────────────────────

    let device_features = read_io32(io_base, VIRTIO_PCI_DEVICE_FEATURES);
    crate::serial_str!("[VIRTIO_NET] Device features: ");
    crate::drivers::serial::write_hex(device_features as u64);
    crate::drivers::serial::write_newline();

    // Check for MAC address feature
    if device_features & VIRTIO_NET_F_MAC == 0 {
        crate::serial_strln!("[VIRTIO_NET] ERROR: Device does not support VIRTIO_NET_F_MAC");
        write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_FAILED);
        return Err(NetError::NoMacFeature);
    }

    // Accept MAC feature
    write_io32(io_base, VIRTIO_PCI_DRIVER_FEATURES, VIRTIO_NET_F_MAC);

    // Legacy transport: skip FEATURES_OK

    // ── Determine Device Config Offset (MSI-X defensive) ───────────────

    let msix = has_msix(&pci_dev);
    let cfg_offset: u16 = if msix { 0x18 } else { 0x14 };

    if msix {
        crate::serial_strln!("[VIRTIO_NET] Config offset: 0x18 (MSI-X detected)");
    } else {
        crate::serial_strln!("[VIRTIO_NET] Config offset: 0x14 (no MSI-X)");
    }

    // ── Read MAC Address ───────────────────────────────────────────────

    let mut mac = [0u8; 6];
    for i in 0..6 {
        mac[i] = read_io8(io_base, cfg_offset + i as u16);
    }

    crate::serial_str!("[VIRTIO_NET] MAC: ");
    for i in 0..6 {
        crate::drivers::serial::write_hex(mac[i] as u64);
        if i < 5 {
            crate::serial_str!(":");
        }
    }
    crate::drivers::serial::write_newline();

    // ── Setup Virtqueues (BEFORE DRIVER_OK per VirtIO spec) ────────────

    // Queue 0: RX
    let mut rx_queue = queue::setup_queue(io_base, 0, "RX")?;

    // Queue 1: TX
    let tx_queue = queue::setup_queue(io_base, 1, "TX")?;

    // ── Populate RX Queue with empty buffers ───────────────────────────

    let mut rx_bufs_phys = [0usize; RX_BUF_COUNT];
    let mut rx_bufs_virt = [0usize; RX_BUF_COUNT];
    rx::populate_rx_queue(&mut rx_queue, &mut rx_bufs_phys, &mut rx_bufs_virt, io_base)?;

    // ── Set DRIVER_OK ──────────────────────────────────────────────────

    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS,
              STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);

    let status = read_io8(io_base, VIRTIO_PCI_DEVICE_STATUS);
    crate::serial_str!("[VIRTIO_NET] Device status after init: ");
    crate::drivers::serial::write_hex(status as u64);
    crate::drivers::serial::write_newline();

    if status & STATUS_FAILED != 0 {
        crate::serial_strln!("[VIRTIO_NET] ERROR: Device set FAILED bit!");
        return Err(NetError::DeviceFailed);
    }

    // ── Store Device ───────────────────────────────────────────────────

    *NET_DEVICE.lock() = Some(VirtIONet {
        io_base,
        mac,
        rx_queue,
        tx_queue,
        rx_bufs_phys,
        rx_bufs_virt,
    });

    crate::serial_strln!("[VIRTIO_NET] VirtIO network device initialized!");

    Ok(())
}

// ── Public API ─────────────────────────────────────────────────────────

/// Get the MAC address if device is initialized
pub fn mac_address() -> Option<[u8; 6]> {
    NET_DEVICE.lock().as_ref().map(|dev| dev.mac)
}

/// Check if the network device is initialized
pub fn is_initialized() -> bool {
    NET_DEVICE.lock().is_some()
}

/// Poll the RX queue for incoming packets. Call this periodically from the kernel loop.
/// Logs the first few packets for debugging, then goes quiet.
pub fn poll_rx() {
    let mut guard = NET_DEVICE.lock();
    let dev = match guard.as_mut() {
        Some(d) => d,
        None => return,
    };

    // Cap at 256 packets per poll. RX queue is 1024 deep, but a
    // continuous flood (broadcast storm, deliberate DoS) could keep
    // refilling faster than we drain — same Issue #49 pattern. Yields
    // back to the caller after 256 so other ISR-driven work makes
    // progress even under flood.
    // Bounded for-loop so the 257th packet is never dequeued under flood.
    for _ in 0..256 {
        let frame = match rx::receive_packet_inner(dev) {
            Some(packet) => packet,
            None => break,
        };
        let len = frame.len();
        let count = RX_PACKET_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

        // Log first 8 packets to serial for debugging
        if count <= 8 && len >= 14 {
            crate::serial_str!("[NET RX] Packet #");
            crate::drivers::serial::write_dec(count);
            crate::serial_str!(", ");
            crate::drivers::serial::write_dec(len as u32);
            crate::serial_str!(" bytes, dst=");
            for i in 0..6 {
                crate::drivers::serial::write_hex(frame[i] as u64);
                if i < 5 { crate::serial_str!(":"); }
            }
            crate::serial_str!(" src=");
            for i in 6..12 {
                crate::drivers::serial::write_hex(frame[i] as u64);
                if i < 11 { crate::serial_str!(":"); }
            }
            let ethertype = ((frame[12] as u16) << 8) | frame[13] as u16;
            crate::serial_str!(" type=0x");
            crate::drivers::serial::write_hex(ethertype as u64);
            crate::drivers::serial::write_newline();
        } else if count == 9 {
            crate::serial_strln!("[NET RX] (further packets suppressed)");
        }
    }
}

/// Receive a single raw Ethernet frame (without VirtIO header).
/// Returns None if no packet is available. Used by the smoltcp device wrapper.
///
/// The returned `Vec<u8>` is sized to the actual frame length (no
/// 1514-byte stack copy on the way out — see `receive_packet_inner`'s
/// comment for the allocation profile rationale).
pub fn receive_raw() -> Option<alloc::vec::Vec<u8>> {
    let mut guard = NET_DEVICE.lock();
    let dev = guard.as_mut()?;
    rx::receive_packet_inner(dev)
}

/// Transmit a raw Ethernet frame. Public API.
pub fn transmit_packet(frame: &[u8]) -> Result<(), NetError> {
    let mut guard = NET_DEVICE.lock();
    let dev = guard.as_mut().ok_or(NetError::DeviceNotFound)?;
    tx::transmit_packet_inner(dev, frame)
}

/// Get packet counters (rx, tx)
pub fn packet_counts() -> (u32, u32) {
    (
        RX_PACKET_COUNT.load(Ordering::Relaxed),
        TX_PACKET_COUNT.load(Ordering::Relaxed),
    )
}

/// Send a broadcast ARP "Who has 10.0.2.2?" to provoke SLIRP into responding.
/// This is a self-test: if packet I/O works, we should receive an ARP reply.
pub fn send_test_arp() {
    let our_mac = match mac_address() {
        Some(m) => m,
        None => return,
    };

    crate::serial_strln!("[NET TEST] Sending ARP Who-has 10.0.2.2...");

    // Build a raw ARP request Ethernet frame
    let mut frame = [0u8; 42]; // 14 (eth) + 28 (ARP) = 42 bytes

    // Destination: broadcast FF:FF:FF:FF:FF:FF
    frame[0..6].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    // Source: our MAC
    frame[6..12].copy_from_slice(&our_mac);
    // EtherType: ARP (0x0806)
    frame[12] = 0x08;
    frame[13] = 0x06;

    // ARP payload
    frame[14] = 0x00; frame[15] = 0x01;  // Hardware type: Ethernet
    frame[16] = 0x08; frame[17] = 0x00;  // Protocol type: IPv4
    frame[18] = 6;                        // Hardware address length
    frame[19] = 4;                        // Protocol address length
    frame[20] = 0x00; frame[21] = 0x01;  // Operation: request
    frame[22..28].copy_from_slice(&our_mac);
    frame[28] = 10; frame[29] = 0; frame[30] = 2; frame[31] = 15;
    frame[32..38].copy_from_slice(&[0x00; 6]);
    frame[38] = 10; frame[39] = 0; frame[40] = 2; frame[41] = 2;

    match transmit_packet(&frame) {
        Ok(()) => { crate::serial_strln!("[NET TEST] ARP request sent!"); }
        Err(_) => { crate::serial_strln!("[NET TEST] ERROR: Failed to send ARP request"); }
    }
}
