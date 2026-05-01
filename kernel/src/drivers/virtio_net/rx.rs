//! Receive path for VirtIO-net: buffer population, packet receive, recycling.

extern crate alloc;
use alloc::vec::Vec;

use crate::drivers::virtio::{Virtqueue, VRING_DESC_F_WRITE};
use super::{NetError, VirtIONet};
use super::io::*;

/// Populate the RX queue with empty buffers for the device to write packets into.
/// Allocates 16 pages (each holds 2 × 2048-byte buffers = 32 buffers total).
pub(super) fn populate_rx_queue(
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

/// Try to receive a packet from the RX queue.
///
/// Returns the Ethernet frame (without the 10-byte VirtIO header) as a
/// `Vec<u8>` sized to the actual frame length. The descriptor's
/// physical buffer is recycled back into the queue before we return.
///
/// Allocation profile vs. the pre-2026-05-01 path: we used to memcpy
/// the buffer into a stack-resident `[u8; 1514]` then immediately
/// `to_vec(&[..len])` it inside the smoltcp device wrapper. That was
/// two copies and a 1514-byte zero-init per packet, regardless of
/// frame size. The current path zero-inits nothing and copies once
/// directly into a `Vec` of exactly `frame_len` bytes — halving RX
/// memory bandwidth and dropping the 1514-byte stack pressure.
///
/// The buffer is still recycled synchronously, so the RX queue has the
/// same depth-budget behaviour as before; no semantic change to the
/// virtio descriptor lifecycle, just a tighter copy path. A future
/// pass can replace this with an RAII guard that defers recycling
/// until smoltcp's `RxToken::consume` runs (true zero-copy), but that
/// requires reworking `FolkeringDevice::receive` to thread a borrow
/// through smoltcp's `Device` trait — out of scope here.
pub(super) fn receive_packet_inner(dev: &mut VirtIONet) -> Option<Vec<u8>> {
    let (desc_idx, total_len) = dev.rx_queue.pop_used()?;

    // The descriptor index tells us which buffer was filled
    // Read the physical address from the descriptor to find which buffer it is
    let buf_phys = unsafe { (*dev.rx_queue.desc(desc_idx)).addr as usize };

    // Find the matching virtual address
    let buf_virt = match dev.rx_bufs_phys.iter().position(|&p| p == buf_phys) {
        Some(idx) => dev.rx_bufs_virt[idx],
        None => crate::phys_to_virt(buf_phys),
    };

    let total = total_len as usize;
    if total <= VIRTIO_NET_HDR_SIZE {
        // Packet too small — just the header, no payload. Recycle and skip.
        recycle_rx_buffer(dev, desc_idx, buf_phys);
        return None;
    }

    let frame_len = total - VIRTIO_NET_HDR_SIZE;
    // 1514 is the standard non-jumbo Ethernet payload cap (1500 MTU
    // + 14 header). Same upper bound the old `[u8; 1514]` path
    // enforced — anything beyond that came from a misconfigured peer
    // or a truncation bug, and we'd rather drop the tail than
    // surprise the upper stack.
    let max_copy = frame_len.min(1514);

    let mut frame: Vec<u8> = Vec::with_capacity(max_copy);
    unsafe {
        let src = (buf_virt + VIRTIO_NET_HDR_SIZE) as *const u8;
        core::ptr::copy_nonoverlapping(src, frame.as_mut_ptr(), max_copy);
        frame.set_len(max_copy);
    }

    // Recycle buffer back into RX queue
    recycle_rx_buffer(dev, desc_idx, buf_phys);

    Some(frame)
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
