//! VirtIO Network Device Driver — Packet I/O
//!
//! Performs VirtIO legacy handshake, negotiates VIRTIO_NET_F_MAC feature,
//! reads MAC address, allocates RX/TX virtqueues, populates RX queue
//! with empty buffers, and provides `receive_packet()` / `transmit_packet()`
//! for raw Ethernet frame I/O.

use spin::Mutex;
use x86_64::instructions::port::Port;

use core::sync::atomic::{AtomicU32, Ordering};

use super::pci::{self, PciDevice, BarType};
use super::virtio::{Virtqueue, VRING_DESC_F_WRITE};

// ── VirtIO Legacy PCI Register Offsets (from BAR0) ───────────────────────────

const VIRTIO_PCI_DEVICE_FEATURES: u16 = 0x00;  // 32-bit, RO
const VIRTIO_PCI_DRIVER_FEATURES: u16 = 0x04;  // 32-bit, RW
const VIRTIO_PCI_QUEUE_PFN: u16 = 0x08;        // 32-bit, RW
const VIRTIO_PCI_QUEUE_SIZE: u16 = 0x0C;        // 16-bit, RO
const VIRTIO_PCI_QUEUE_SEL: u16 = 0x0E;         // 16-bit, RW
const VIRTIO_PCI_QUEUE_NOTIFY: u16 = 0x10;      // 16-bit, RW
const VIRTIO_PCI_DEVICE_STATUS: u16 = 0x12;     // 8-bit, RW

// ── VirtIO Device Status Bits ────────────────────────────────────────────────

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FAILED: u8 = 128;

// ── VirtIO Net Feature Bits ──────────────────────────────────────────────────

const VIRTIO_NET_F_MAC: u32 = 1 << 5;

// ── MSI-X Capability ID ─────────────────────────────────────────────────────

const PCI_CAP_ID_MSIX: u8 = 0x11;

// ── RX Buffer Constants ──────────────────────────────────────────────────────

const RX_BUF_COUNT: usize = 32;
const RX_BUF_SIZE: usize = 2048;  // Ethernet MTU 1514 + VirtIO net header 10 + margin
const TX_BUF_SIZE: usize = 2048;

/// VirtIO net header (legacy, 10 bytes — no mergeable rx buffers feature)
const VIRTIO_NET_HDR_SIZE: usize = 10;

/// Packet receive/transmit statistics
static RX_PACKET_COUNT: AtomicU32 = AtomicU32::new(0);
static TX_PACKET_COUNT: AtomicU32 = AtomicU32::new(0);

// ── Error Type ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    DeviceNotFound,
    BarNotIoSpace,
    DeviceFailed,
    NoMacFeature,
    QueueSetupFailed,
}

// ── Device State ─────────────────────────────────────────────────────────────

pub struct VirtIONet {
    io_base: u16,
    mac: [u8; 6],
    rx_queue: Virtqueue,
    tx_queue: Virtqueue,
    rx_bufs_phys: [usize; RX_BUF_COUNT],
    rx_bufs_virt: [usize; RX_BUF_COUNT],
}

static NET_DEVICE: Mutex<Option<VirtIONet>> = Mutex::new(None);

// ── I/O Helpers ──────────────────────────────────────────────────────────────

fn read_io8(base: u16, offset: u16) -> u8 {
    unsafe { Port::<u8>::new(base + offset).read() }
}

fn write_io8(base: u16, offset: u16, val: u8) {
    unsafe { Port::<u8>::new(base + offset).write(val); }
}

fn read_io16(base: u16, offset: u16) -> u16 {
    unsafe { Port::<u16>::new(base + offset).read() }
}

fn write_io16(base: u16, offset: u16, val: u16) {
    unsafe { Port::<u16>::new(base + offset).write(val); }
}

fn read_io32(base: u16, offset: u16) -> u32 {
    unsafe { Port::<u32>::new(base + offset).read() }
}

fn write_io32(base: u16, offset: u16, val: u32) {
    unsafe { Port::<u32>::new(base + offset).write(val); }
}

// ── MSI-X Detection ─────────────────────────────────────────────────────────

/// Check if the PCI device has MSI-X capability
fn has_msix(dev: &PciDevice) -> bool {
    let mut ptr = dev.capabilities_ptr;
    // Walk capability linked list
    while ptr != 0 {
        let cap_id = pci::pci_read8(dev.bus, dev.device, dev.function, ptr);
        if cap_id == PCI_CAP_ID_MSIX {
            // Check if MSI-X is actually enabled (bit 15 of Message Control at cap+2)
            let msg_ctrl = pci::pci_read16(dev.bus, dev.device, dev.function, ptr + 2);
            return msg_ctrl & 0x8000 != 0;
        }
        let next = pci::pci_read8(dev.bus, dev.device, dev.function, ptr + 1);
        ptr = next;
    }
    false
}

// ── Queue Setup Helper ──────────────────────────────────────────────────────

/// Set up a virtqueue for a given queue index.
/// Selects the queue, reads its size, allocates the Virtqueue, and writes the PFN.
fn setup_queue(io_base: u16, queue_idx: u16, name: &str) -> Result<Virtqueue, NetError> {
    // Select queue
    write_io16(io_base, VIRTIO_PCI_QUEUE_SEL, queue_idx);

    // Read queue size
    let queue_size = read_io16(io_base, VIRTIO_PCI_QUEUE_SIZE);
    crate::serial_str!("[VIRTIO_NET] ");
    crate::serial_str!(name);
    crate::serial_str!(" queue size: ");
    crate::drivers::serial::write_dec(queue_size as u32);
    crate::drivers::serial::write_newline();

    if queue_size == 0 {
        crate::serial_str!("[VIRTIO_NET] ERROR: ");
        crate::serial_str!(name);
        crate::serial_strln!(" queue size is 0");
        return Err(NetError::QueueSetupFailed);
    }

    // Allocate virtqueue
    let queue = Virtqueue::new(queue_size).ok_or_else(|| {
        crate::serial_str!("[VIRTIO_NET] ERROR: Failed to allocate ");
        crate::serial_str!(name);
        crate::serial_strln!(" queue");
        NetError::QueueSetupFailed
    })?;

    // Tell device the queue's physical page frame number
    let queue_pfn = (queue.queue_phys / 4096) as u32;
    write_io32(io_base, VIRTIO_PCI_QUEUE_PFN, queue_pfn);

    crate::serial_str!("[VIRTIO_NET] ");
    crate::serial_str!(name);
    crate::serial_str!(" queue setup: size=");
    crate::drivers::serial::write_dec(queue_size as u32);
    crate::serial_str!(", PFN=0x");
    crate::drivers::serial::write_hex(queue_pfn as u64);
    crate::drivers::serial::write_newline();

    Ok(queue)
}

// ── RX Buffer Population ────────────────────────────────────────────────────

/// Populate the RX queue with empty buffers for the device to write received packets into.
/// Allocates 16 pages (each holds 2 × 2048-byte buffers = 32 buffers total).
fn populate_rx_queue(
    rx_queue: &mut Virtqueue,
    rx_bufs_phys: &mut [usize; RX_BUF_COUNT],
    rx_bufs_virt: &mut [usize; RX_BUF_COUNT],
    io_base: u16,
) -> Result<(), NetError> {
    // 16 pages × 4096 bytes = 65536 bytes
    // 32 buffers × 2048 bytes = 65536 bytes — exactly fits
    let pages_needed = (RX_BUF_COUNT * RX_BUF_SIZE + 4095) / 4096; // = 16

    for page_idx in 0..pages_needed {
        let page_phys = crate::memory::physical::alloc_page().ok_or_else(|| {
            crate::serial_strln!("[VIRTIO_NET] ERROR: Failed to allocate RX buffer page");
            NetError::QueueSetupFailed
        })?;
        let page_virt = crate::phys_to_virt(page_phys);

        // Zero the page
        unsafe {
            core::ptr::write_bytes(page_virt as *mut u8, 0, 4096);
        }

        // Each page holds 2 buffers
        for slot in 0..2 {
            let buf_idx = page_idx * 2 + slot;
            if buf_idx >= RX_BUF_COUNT {
                break;
            }

            let offset = slot * RX_BUF_SIZE;
            let buf_phys = page_phys + offset;
            let buf_virt = page_virt + offset;

            rx_bufs_phys[buf_idx] = buf_phys;
            rx_bufs_virt[buf_idx] = buf_virt;

            // Allocate a descriptor and configure it as device-writable
            let desc_idx = rx_queue.alloc_desc().ok_or_else(|| {
                crate::serial_strln!("[VIRTIO_NET] ERROR: No free RX descriptors");
                NetError::QueueSetupFailed
            })?;

            unsafe {
                let desc = &mut *rx_queue.desc(desc_idx);
                desc.addr = buf_phys as u64;
                desc.len = RX_BUF_SIZE as u32;
                desc.flags = VRING_DESC_F_WRITE;
                desc.next = 0;
            }

            // Submit to available ring
            rx_queue.submit(desc_idx);
        }
    }

    // Notify device that RX buffers are available
    write_io16(io_base, VIRTIO_PCI_QUEUE_NOTIFY, 0);

    crate::serial_str!("[VIRTIO_NET] Populated RX queue with ");
    crate::drivers::serial::write_dec(RX_BUF_COUNT as u32);
    crate::serial_strln!(" buffers");

    Ok(())
}

// ── Initialization ──────────────────────────────────────────────────────────

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

    // ── VirtIO Handshake ─────────────────────────────────────────────────

    // Step 1: Reset
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, 0);

    // Step 2: ACKNOWLEDGE
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE);

    // Step 3: DRIVER
    write_io8(io_base, VIRTIO_PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    // ── Feature Negotiation ──────────────────────────────────────────────

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

    // ── Determine Device Config Offset (MSI-X defensive) ─────────────────

    let msix = has_msix(&pci_dev);
    let cfg_offset: u16 = if msix { 0x18 } else { 0x14 };

    if msix {
        crate::serial_strln!("[VIRTIO_NET] Config offset: 0x18 (MSI-X detected)");
    } else {
        crate::serial_strln!("[VIRTIO_NET] Config offset: 0x14 (no MSI-X)");
    }

    // ── Read MAC Address ─────────────────────────────────────────────────

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

    // ── Setup Virtqueues (BEFORE DRIVER_OK per VirtIO spec) ──────────────

    // Queue 0: RX
    let mut rx_queue = setup_queue(io_base, 0, "RX")?;

    // Queue 1: TX
    let tx_queue = setup_queue(io_base, 1, "TX")?;

    // ── Populate RX Queue with empty buffers ─────────────────────────────

    let mut rx_bufs_phys = [0usize; RX_BUF_COUNT];
    let mut rx_bufs_virt = [0usize; RX_BUF_COUNT];
    populate_rx_queue(&mut rx_queue, &mut rx_bufs_phys, &mut rx_bufs_virt, io_base)?;

    // ── Set DRIVER_OK ────────────────────────────────────────────────────

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

    // ── Store Device ─────────────────────────────────────────────────────

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

// ── Packet I/O ──────────────────────────────────────────────────────────────

/// Try to receive a packet from the RX queue.
/// Returns a copy of the Ethernet frame (without VirtIO header) if one is available.
/// The RX buffer is recycled back into the queue immediately.
fn receive_packet_inner(dev: &mut VirtIONet) -> Option<([u8; 1514], usize)> {
    let (desc_idx, total_len) = dev.rx_queue.pop_used()?;

    // The descriptor index tells us which buffer was filled
    // Read the physical address from the descriptor to find which buffer it is
    let buf_phys = unsafe { (*dev.rx_queue.desc(desc_idx)).addr as usize };

    // Find the matching virtual address
    let buf_virt = match dev.rx_bufs_phys.iter().position(|&p| p == buf_phys) {
        Some(idx) => dev.rx_bufs_virt[idx],
        None => {
            // Fallback: compute via HHDM
            crate::phys_to_virt(buf_phys)
        }
    };

    let total = total_len as usize;
    if total <= VIRTIO_NET_HDR_SIZE {
        // Packet too small — just the header, no payload. Recycle and skip.
        recycle_rx_buffer(dev, desc_idx, buf_phys);
        return None;
    }

    let frame_len = total - VIRTIO_NET_HDR_SIZE;
    let max_copy = frame_len.min(1514); // Ethernet max frame size

    let mut frame = [0u8; 1514];
    unsafe {
        let src = (buf_virt + VIRTIO_NET_HDR_SIZE) as *const u8;
        core::ptr::copy_nonoverlapping(src, frame.as_mut_ptr(), max_copy);
    }

    // Recycle buffer back into RX queue
    recycle_rx_buffer(dev, desc_idx, buf_phys);

    Some((frame, max_copy))
}

/// Recycle an RX buffer back into the queue so the device can reuse it.
fn recycle_rx_buffer(dev: &mut VirtIONet, desc_idx: u16, buf_phys: usize) {
    // Reconfigure the descriptor for device-write
    unsafe {
        let desc = &mut *dev.rx_queue.desc(desc_idx);
        desc.addr = buf_phys as u64;
        desc.len = RX_BUF_SIZE as u32;
        desc.flags = VRING_DESC_F_WRITE;
        desc.next = 0;
    }

    // Re-submit to available ring
    dev.rx_queue.submit(desc_idx);

    // Notify device
    write_io16(dev.io_base, VIRTIO_PCI_QUEUE_NOTIFY, 0);
}

/// Transmit a raw Ethernet frame.
/// Prepends the 10-byte VirtIO net header (zeroed) and sends via TX queue.
/// Returns Ok(()) on success, Err if the device is not ready or TX queue is full.
fn transmit_packet_inner(dev: &mut VirtIONet, frame: &[u8]) -> Result<(), NetError> {
    if frame.len() > TX_BUF_SIZE - VIRTIO_NET_HDR_SIZE {
        crate::serial_strln!("[VIRTIO_NET] TX: frame too large");
        return Err(NetError::DeviceFailed);
    }

    // Drain any completed TX descriptors first
    while let Some((done_idx, _)) = dev.tx_queue.pop_used() {
        dev.tx_queue.free_desc(done_idx);
    }

    // Allocate a descriptor
    let desc_idx = dev.tx_queue.alloc_desc().ok_or_else(|| {
        crate::serial_strln!("[VIRTIO_NET] TX: no free descriptors");
        NetError::DeviceFailed
    })?;

    // Allocate a physical page for the TX buffer
    let tx_page_phys = crate::memory::physical::alloc_page().ok_or_else(|| {
        crate::serial_strln!("[VIRTIO_NET] TX: failed to allocate buffer page");
        dev.tx_queue.free_desc(desc_idx);
        NetError::DeviceFailed
    })?;
    let tx_page_virt = crate::phys_to_virt(tx_page_phys);

    // Write VirtIO net header (10 bytes, all zeroes = no offloading)
    unsafe {
        core::ptr::write_bytes(tx_page_virt as *mut u8, 0, VIRTIO_NET_HDR_SIZE);
    }

    // Copy the Ethernet frame after the header
    let total_len = VIRTIO_NET_HDR_SIZE + frame.len();
    unsafe {
        let dst = (tx_page_virt + VIRTIO_NET_HDR_SIZE) as *mut u8;
        core::ptr::copy_nonoverlapping(frame.as_ptr(), dst, frame.len());
    }

    // Configure the descriptor (device-readable, no WRITE flag)
    unsafe {
        let desc = &mut *dev.tx_queue.desc(desc_idx);
        desc.addr = tx_page_phys as u64;
        desc.len = total_len as u32;
        desc.flags = 0; // device reads this buffer
        desc.next = 0;
    }

    // Submit to TX available ring
    dev.tx_queue.submit(desc_idx);

    // Notify device (queue 1 = TX)
    write_io16(dev.io_base, VIRTIO_PCI_QUEUE_NOTIFY, 1);

    TX_PACKET_COUNT.fetch_add(1, Ordering::Relaxed);

    Ok(())
}

// ── Public API ───────────────────────────────────────────────────────────────

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

    while let Some((frame, len)) = receive_packet_inner(dev) {
        let count = RX_PACKET_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

        // Log first 8 packets to serial for debugging
        if count <= 8 {
            crate::serial_str!("[NET RX] Packet #");
            crate::drivers::serial::write_dec(count);
            crate::serial_str!(", ");
            crate::drivers::serial::write_dec(len as u32);
            crate::serial_str!(" bytes, dst=");
            // Print destination MAC (first 6 bytes of Ethernet frame)
            for i in 0..6 {
                crate::drivers::serial::write_hex(frame[i] as u64);
                if i < 5 { crate::serial_str!(":"); }
            }
            crate::serial_str!(" src=");
            for i in 6..12 {
                crate::drivers::serial::write_hex(frame[i] as u64);
                if i < 11 { crate::serial_str!(":"); }
            }
            // EtherType at bytes 12-13
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
pub fn receive_raw() -> Option<([u8; 1514], usize)> {
    let mut guard = NET_DEVICE.lock();
    let dev = guard.as_mut()?;
    receive_packet_inner(dev)
}

/// Transmit a raw Ethernet frame. Public API.
pub fn transmit_packet(frame: &[u8]) -> Result<(), NetError> {
    let mut guard = NET_DEVICE.lock();
    let dev = guard.as_mut().ok_or(NetError::DeviceNotFound)?;
    transmit_packet_inner(dev, frame)
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
    // Ethernet header (14 bytes)
    let mut frame = [0u8; 42]; // 14 (eth) + 28 (ARP) = 42 bytes

    // Destination: broadcast FF:FF:FF:FF:FF:FF
    frame[0..6].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    // Source: our MAC
    frame[6..12].copy_from_slice(&our_mac);
    // EtherType: ARP (0x0806)
    frame[12] = 0x08;
    frame[13] = 0x06;

    // ARP payload (28 bytes)
    // Hardware type: Ethernet (1)
    frame[14] = 0x00; frame[15] = 0x01;
    // Protocol type: IPv4 (0x0800)
    frame[16] = 0x08; frame[17] = 0x00;
    // Hardware address length: 6
    frame[18] = 6;
    // Protocol address length: 4
    frame[19] = 4;
    // Operation: request (1)
    frame[20] = 0x00; frame[21] = 0x01;
    // Sender hardware address: our MAC
    frame[22..28].copy_from_slice(&our_mac);
    // Sender protocol address: 10.0.2.15 (SLIRP default guest IP)
    frame[28] = 10; frame[29] = 0; frame[30] = 2; frame[31] = 15;
    // Target hardware address: 00:00:00:00:00:00 (unknown)
    frame[32..38].copy_from_slice(&[0x00; 6]);
    // Target protocol address: 10.0.2.2 (SLIRP gateway)
    frame[38] = 10; frame[39] = 0; frame[40] = 2; frame[41] = 2;

    match transmit_packet(&frame) {
        Ok(()) => { crate::serial_strln!("[NET TEST] ARP request sent!"); }
        Err(_) => { crate::serial_strln!("[NET TEST] ERROR: Failed to send ARP request"); }
    }
}
