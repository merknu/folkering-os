//! Virtqueue setup helpers for VirtIO-net.
//!
//! Handles queue selection, allocation, and PFN registration with the device.

use crate::drivers::virtio::Virtqueue;
use super::NetError;
use super::io::*;

/// Set up a virtqueue for a given queue index.
/// Selects the queue, reads its size, allocates the Virtqueue, and writes the PFN.
pub(super) fn setup_queue(io_base: u16, queue_idx: u16, name: &str) -> Result<Virtqueue, NetError> {
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
