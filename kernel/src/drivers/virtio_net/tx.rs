//! Transmit path for VirtIO-net.

use core::sync::atomic::Ordering;
use super::{NetError, VirtIONet, TX_PACKET_COUNT};
use super::io::*;

/// Transmit a raw Ethernet frame.
/// Prepends the 10-byte VirtIO net header (zeroed) and sends via TX queue.
/// Returns Ok(()) on success, Err if the device is not ready or TX queue is full.
pub(super) fn transmit_packet_inner(dev: &mut VirtIONet, frame: &[u8]) -> Result<(), NetError> {
    if frame.len() > TX_BUF_SIZE - VIRTIO_NET_HDR_SIZE {
        crate::serial_strln!("[VIRTIO_NET] TX: frame too large");
        return Err(NetError::DeviceFailed);
    }

    // Drain any completed TX descriptors first.
    // Each descriptor has a 4 KB physical page bound to its `addr`
    // field — `free_desc` only releases the descriptor index back
    // to the queue's free list. We must free the page too, otherwise
    // every outbound packet leaks 4 KB. Under sustained TCP/UDP
    // flood at 200 pps, the leak compounds to ~70 MB/min — see #54
    // for the live-Proxmox repro that surfaced this.
    while let Some((done_idx, _)) = dev.tx_queue.pop_used() {
        let desc_addr = unsafe { (*dev.tx_queue.desc(done_idx)).addr } as usize;
        dev.tx_queue.free_desc(done_idx);
        if desc_addr != 0 {
            crate::memory::physical::free_page(desc_addr);
        }
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
