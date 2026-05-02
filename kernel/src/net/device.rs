//! smoltcp Device wrapper — routes packets between WASM/VirtIO backends and smoltcp.
//!
//! Implements `phy::Device` so smoltcp can transmit/receive frames via either
//! the WASM E1000 driver (preferred if active) or the VirtIO-net kernel driver.

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

use super::firewall;
use super::wasm_ring::WASM_NET;
use crate::drivers::virtio_net;

pub(crate) struct FolkeringDevice;

/// RX token variants. The virtio path now defaults to true zero-copy
/// (Delayed Requeueing) — we hand smoltcp a reference to the DMA
/// buffer directly and defer the descriptor recycle until the token
/// is dropped. The WASM-net path stays on the owned-buffer model
/// because its underlying ring already gives us a `Vec<u8>`.
pub(crate) enum FolkeringRxToken {
    /// Zero-copy: holds the descriptor checkout for a virtio frame.
    /// On Drop (whether via `consume` returning or smoltcp dropping
    /// without consuming), the descriptor is recycled into the avail
    /// ring so the virtio device can reuse the page.
    VirtioZeroCopy {
        desc_idx: u16,
        buf_phys: usize,
        buf_virt: usize,
        frame_offset: usize,
        len: usize,
    },
    /// Owned bytes (WASM-net path that doesn't have virtio
    /// descriptors to recycle).
    Owned(Vec<u8>),
}

impl phy::RxToken for FolkeringRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        match &self {
            FolkeringRxToken::VirtioZeroCopy {
                buf_virt, frame_offset, len, ..
            } => {
                // SAFETY: the descriptor is currently checked out
                // (not in the avail ring) — virtio device cannot
                // write to this buffer until our Drop puts it back.
                // The buf_virt + frame_offset slice is therefore
                // exclusive read access for the duration of `f`.
                let ptr = (*buf_virt + *frame_offset) as *const u8;
                let slice = unsafe { core::slice::from_raw_parts(ptr, *len) };
                f(slice)
                // Drop runs after this returns and recycles the
                // descriptor — see `Drop for FolkeringRxToken`.
            }
            FolkeringRxToken::Owned(bytes) => f(bytes),
        }
    }
}

impl Drop for FolkeringRxToken {
    fn drop(&mut self) {
        // Zero-copy tokens recycle the virtio descriptor whether
        // smoltcp consumed it (consume drops self at end of scope)
        // or dropped it without consuming (e.g., routing rejected
        // the frame). Owned tokens drop their Vec<u8> normally —
        // no descriptor to return.
        if let FolkeringRxToken::VirtioZeroCopy { desc_idx, buf_phys, .. } = *self {
            virtio_net::recycle_rx_descriptor(desc_idx, buf_phys);
        }
    }
}

pub(crate) struct FolkeringTxToken;

impl phy::TxToken for FolkeringTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        // Route to WASM driver if active, otherwise VirtIO
        let sent = WASM_NET.try_lock().map_or(false, |mut ring| {
            if ring.active { ring.submit_tx(&buffer) } else { false }
        });
        if sent {
            static TX_LOG_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            let c = TX_LOG_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if c < 5 {
                crate::serial_str!("[NET-TX] ");
                crate::drivers::serial::write_dec(len as u32);
                crate::serial_strln!("B queued for WASM driver");
            }
        } else {
            let _ = virtio_net::transmit_packet(&buffer);
        }
        result
    }
}

impl Device for FolkeringDevice {
    type RxToken<'a> = FolkeringRxToken where Self: 'a;
    type TxToken<'a> = FolkeringTxToken where Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Try WASM backend first (use try_lock to avoid deadlock with timer tick)
        if let Some(mut ring) = WASM_NET.try_lock() {
            if ring.active {
                if let Some((data, len)) = ring.pop_rx() {
                    // Firewall: inspect before passing to smoltcp
                    if firewall::filter_packet(&data[..len]) == firewall::FirewallAction::Drop {
                        drop(ring);
                        return None;
                    }
                    let rx = FolkeringRxToken::Owned(data[..len].to_vec());
                    drop(ring);
                    return Some((rx, FolkeringTxToken));
                }
                drop(ring);
                return None;
            }
        }
        // VirtIO path — true zero-copy via Delayed Requeueing.
        // virtio_net::receive_raw_zero_copy returns the descriptor
        // checkout info; the FolkeringRxToken's Drop impl (or
        // explicit recycle below for filter-drop) returns the
        // descriptor to the avail ring.
        //
        // 256-frame skip cap so a flood of firewall-denied packets
        // can't pin smoltcp's poll (Issue #49 pattern).
        let mut skipped = 0u32;
        loop {
            let info = match virtio_net::receive_raw_zero_copy() {
                Some(f) => f,
                None => return None,
            };
            // Firewall check needs to read the slice while holding
            // the descriptor checkout. Build a temporary view (no
            // copy — same pointer math as the token uses).
            let slice = unsafe {
                core::slice::from_raw_parts(
                    (info.buf_virt + info.frame_offset) as *const u8,
                    info.len,
                )
            };
            if firewall::filter_packet(slice) == firewall::FirewallAction::Allow {
                let rx = FolkeringRxToken::VirtioZeroCopy {
                    desc_idx: info.desc_idx,
                    buf_phys: info.buf_phys,
                    buf_virt: info.buf_virt,
                    frame_offset: info.frame_offset,
                    len: info.len,
                };
                return Some((rx, FolkeringTxToken));
            }
            // Filter dropped the frame — recycle immediately so the
            // descriptor goes straight back into the avail ring.
            virtio_net::recycle_rx_descriptor(info.desc_idx, info.buf_phys);
            skipped += 1;
            if skipped >= 256 { return None; }
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(FolkeringTxToken)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1500;
        caps.max_burst_size = Some(1);
        caps
    }
}
