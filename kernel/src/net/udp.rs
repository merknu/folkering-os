//! Simple UDP send/recv for userspace.
//!
//! Each call creates a transient socket, sends/receives one packet, then closes.
//! This is stateless from userspace's perspective — no socket handles to track.
//! Suitable for DNS, NTP, mDNS, etc.

extern crate alloc;

use smoltcp::socket::udp;
use smoltcp::storage::PacketMetadata;
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

use super::device::FolkeringDevice;
use super::state::NET_STATE;
use super::tls;

/// Send a UDP packet to a target IP:port.
/// Returns true on success, false on error.
pub fn udp_send(target_ip: [u8; 4], target_port: u16, data: &[u8]) -> bool {
    if data.len() > 1472 { return false; }

    let mut guard = NET_STATE.lock();
    let state = match guard.as_mut() {
        Some(s) if s.has_ip => s,
        _ => return false,
    };

    let rx_meta: alloc::vec::Vec<PacketMetadata<udp::UdpMetadata>> = alloc::vec![PacketMetadata::EMPTY; 4];
    let rx_buf: alloc::vec::Vec<u8> = alloc::vec![0u8; 2048];
    let tx_meta: alloc::vec::Vec<PacketMetadata<udp::UdpMetadata>> = alloc::vec![PacketMetadata::EMPTY; 4];
    let tx_buf: alloc::vec::Vec<u8> = alloc::vec![0u8; 2048];
    let udp_rx = udp::PacketBuffer::new(rx_meta, rx_buf);
    let udp_tx = udp::PacketBuffer::new(tx_meta, tx_buf);
    let mut sock = udp::Socket::new(udp_rx, udp_tx);

    let local_port = tls::next_port();
    if sock.bind(local_port).is_err() { return false; }

    let endpoint = IpEndpoint {
        addr: IpAddress::Ipv4(Ipv4Address::new(target_ip[0], target_ip[1], target_ip[2], target_ip[3])),
        port: target_port,
    };

    if sock.send_slice(data, endpoint).is_err() { return false; }

    let handle = state.sockets.add(sock);
    let now = Instant::from_millis(tls::tsc_ms());
    let mut dev = FolkeringDevice;
    let _ = state.iface.poll(now, &mut dev, &mut state.sockets);
    state.sockets.remove(handle);
    true
}

/// Send a UDP packet and wait for a single response. Times out after `timeout_ms`.
/// Returns number of bytes received (0 on timeout/error).
pub fn udp_send_recv(
    target_ip: [u8; 4],
    target_port: u16,
    data: &[u8],
    response: &mut [u8],
    timeout_ms: u32,
) -> usize {
    if data.len() > 1472 { return 0; }

    let mut guard = NET_STATE.lock();
    let state = match guard.as_mut() {
        Some(s) if s.has_ip => s,
        _ => return 0,
    };

    let rx_meta: alloc::vec::Vec<PacketMetadata<udp::UdpMetadata>> = alloc::vec![PacketMetadata::EMPTY; 4];
    let rx_buf: alloc::vec::Vec<u8> = alloc::vec![0u8; 4096];
    let tx_meta: alloc::vec::Vec<PacketMetadata<udp::UdpMetadata>> = alloc::vec![PacketMetadata::EMPTY; 4];
    let tx_buf: alloc::vec::Vec<u8> = alloc::vec![0u8; 4096];
    let udp_rx = udp::PacketBuffer::new(rx_meta, rx_buf);
    let udp_tx = udp::PacketBuffer::new(tx_meta, tx_buf);
    let mut sock = udp::Socket::new(udp_rx, udp_tx);

    let local_port = tls::next_port();
    if sock.bind(local_port).is_err() { return 0; }

    let endpoint = IpEndpoint {
        addr: IpAddress::Ipv4(Ipv4Address::new(target_ip[0], target_ip[1], target_ip[2], target_ip[3])),
        port: target_port,
    };

    if sock.send_slice(data, endpoint).is_err() { return 0; }

    let handle = state.sockets.add(sock);
    let start = tls::tsc_ms();
    let mut received = 0usize;
    let mut dev = FolkeringDevice;

    loop {
        let now = Instant::from_millis(tls::tsc_ms());
        let _ = state.iface.poll(now, &mut dev, &mut state.sockets);

        let sock = state.sockets.get_mut::<udp::Socket>(handle);
        if let Ok((n, _src)) = sock.recv_slice(response) {
            if n > 0 {
                received = n;
                break;
            }
        }

        let elapsed = (tls::tsc_ms() - start) as u32;
        if elapsed >= timeout_ms { break; }

        for _ in 0..1000 { core::hint::spin_loop(); }
    }

    state.sockets.remove(handle);
    received
}
