//! ICMP echo (ping) implementation.

use smoltcp::socket::icmp;
use smoltcp::wire::{IpAddress, Icmpv4Packet, Icmpv4Repr, Ipv4Address};

use super::state::{NetState, NET_STATE};
use super::print_ipv4;

/// ICMP echo identifier — "Fo" for Folkering
pub(crate) const PING_IDENT: u16 = 0x466F;

/// Send a ping to a target IPv4 address (called from syscall handler)
pub fn send_ping(a: u8, b: u8, c: u8, d: u8) {
    let mut guard = NET_STATE.lock();
    let state = match guard.as_mut() {
        Some(s) if s.has_ip => s,
        _ => {
            crate::serial_strln!("[NET] Ping: no network — ignoring");
            return;
        }
    };

    let target = Ipv4Address::new(a, b, c, d);
    send_ping_inner(state, target);
}

/// Send an ICMP echo request (internal, called with lock held)
pub(crate) fn send_ping_inner(state: &mut NetState, target: Ipv4Address) {
    state.ping_seq = state.ping_seq.wrapping_add(1);
    let seq = state.ping_seq;

    let icmp_socket = state.sockets.get_mut::<icmp::Socket>(state.icmp_handle);

    if !icmp_socket.is_open() {
        icmp_socket
            .bind(icmp::Endpoint::Ident(PING_IDENT))
            .unwrap();
    }

    let payload = b"folkering";
    let echo = Icmpv4Repr::EchoRequest {
        ident: PING_IDENT,
        seq_no: seq,
        data: payload,
    };

    let packet_size = echo.buffer_len();

    if icmp_socket.can_send() {
        let tx_buf = icmp_socket
            .send(packet_size, IpAddress::Ipv4(target))
            .unwrap();
        let mut packet = Icmpv4Packet::new_unchecked(tx_buf);
        echo.emit(&mut packet, &smoltcp::phy::ChecksumCapabilities::default());

        state.ping_send_at = Some(crate::timer::uptime_ms());

        crate::serial_str!("[NET] Ping: sending to ");
        print_ipv4(&target);
        crate::serial_str!(" seq=");
        crate::drivers::serial::write_dec(seq as u32);
        crate::serial_strln!("...");
    } else {
        crate::serial_strln!("[NET] Ping: ICMP socket not ready to send");
    }
}

/// Check for incoming ICMP echo replies
pub(crate) fn check_ping_reply(state: &mut NetState) {
    let icmp_socket = state.sockets.get_mut::<icmp::Socket>(state.icmp_handle);

    while icmp_socket.can_recv() {
        let (data, from) = match icmp_socket.recv() {
            Ok(v) => v,
            Err(_) => break,
        };

        let packet = Icmpv4Packet::new_checked(data);
        let packet = match packet {
            Ok(p) => p,
            Err(_) => continue,
        };

        let repr = Icmpv4Repr::parse(&packet, &smoltcp::phy::ChecksumCapabilities::default());
        match repr {
            Ok(Icmpv4Repr::EchoReply {
                ident,
                seq_no,
                data: _,
            }) if ident == PING_IDENT => {
                let now_ms = crate::timer::uptime_ms();
                let rtt = match state.ping_send_at.take() {
                    Some(sent) => now_ms.saturating_sub(sent),
                    None => 0,
                };

                crate::serial_str!("[NET] Ping: reply from ");
                if let IpAddress::Ipv4(v4) = from {
                    print_ipv4(&v4);
                }
                crate::serial_str!(" seq=");
                crate::drivers::serial::write_dec(seq_no as u32);
                crate::serial_str!(" time=");
                crate::drivers::serial::write_dec(rtt as u32);
                crate::serial_strln!("ms");
            }
            _ => {}
        }
    }
}
