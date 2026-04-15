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

pub(crate) struct FolkeringRxToken {
    buffer: Vec<u8>,
}

impl phy::RxToken for FolkeringRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer)
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
                    let rx = FolkeringRxToken { buffer: data[..len].to_vec() };
                    drop(ring);
                    return Some((rx, FolkeringTxToken));
                }
                drop(ring);
                return None;
            }
        }
        // Fallback to VirtIO — loop to skip dropped packets
        loop {
            let (frame, len) = match virtio_net::receive_raw() {
                Some(f) => f,
                None => return None,
            };
            if firewall::filter_packet(&frame[..len]) == firewall::FirewallAction::Allow {
                let rx = FolkeringRxToken { buffer: frame[..len].to_vec() };
                return Some((rx, FolkeringTxToken));
            }
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
