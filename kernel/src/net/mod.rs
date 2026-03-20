//! Network Stack — smoltcp integration over VirtIO-net
//!
//! Provides DHCP-based IPv4 connectivity via smoltcp. The VirtIO-net driver
//! supplies raw Ethernet frames; this module wraps it as a smoltcp Device
//! and runs the TCP/IP stack. Supports ICMP echo (ping), DNS resolution, and TLS 1.3.

pub mod tls;

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{dhcpv4, dns, icmp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    DnsQueryType, EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr,
    Ipv4Address, Ipv4Cidr,
};

use crate::drivers::virtio_net;

// ── Constants ───────────────────────────────────────────────────────────────

/// ICMP echo identifier — "Fo" for Folkering
const PING_IDENT: u16 = 0x466F;

// ── smoltcp Device wrapper ──────────────────────────────────────────────────

struct FolkeringDevice;

struct FolkeringRxToken {
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

struct FolkeringTxToken;

impl phy::TxToken for FolkeringTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = vec![0u8; len];
        let result = f(&mut buffer);
        let _ = virtio_net::transmit_packet(&buffer);
        result
    }
}

impl Device for FolkeringDevice {
    type RxToken<'a> = FolkeringRxToken where Self: 'a;
    type TxToken<'a> = FolkeringTxToken where Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let (frame, len) = virtio_net::receive_raw()?;
        let rx = FolkeringRxToken {
            buffer: frame[..len].to_vec(),
        };
        let tx = FolkeringTxToken;
        Some((rx, tx))
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

// ── Network State ───────────────────────────────────────────────────────────

pub(crate) struct NetState {
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp_handle: SocketHandle,
    icmp_handle: SocketHandle,
    dns_handle: SocketHandle,
    has_ip: bool,
    ping_seq: u16,
    ping_send_at: Option<u64>,
    auto_ping_done: bool,
    // Async DNS auto-test state
    auto_dns_started: bool,
    auto_dns_query: Option<dns::QueryHandle>,
    // HTTPS auto-test
    auto_https_done: bool,
}

static NET_STATE: Mutex<Option<NetState>> = Mutex::new(None);

// ── Initialization ──────────────────────────────────────────────────────────

pub fn init() {
    let mac = match virtio_net::mac_address() {
        Some(m) => m,
        None => {
            crate::serial_strln!("[NET] No MAC address — skipping network stack init");
            return;
        }
    };

    crate::serial_str!("[NET] Initializing smoltcp stack, MAC=");
    for i in 0..6 {
        crate::drivers::serial::write_hex(mac[i] as u64);
        if i < 5 {
            crate::serial_str!(":");
        }
    }
    crate::drivers::serial::write_newline();

    let mut device = FolkeringDevice;

    let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
    let mut config = Config::new(hw_addr);
    // Seed PRNG from TSC — needed for DNS transaction IDs and source ports.
    // A zero seed causes xorshift to always return 0, breaking DNS.
    config.random_seed = {
        let lo: u32;
        let hi: u32;
        unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi); }
        ((hi as u64) << 32) | (lo as u64)
    };

    let now = Instant::from_millis(crate::timer::uptime_ms() as i64);
    let iface = Interface::new(config, &mut device, now);

    // DHCP socket
    let dhcp_socket = dhcpv4::Socket::new();

    // ICMP socket
    let icmp_rx_buf = icmp::PacketBuffer::new(
        vec![icmp::PacketMetadata::EMPTY; 4],
        vec![0; 1024],
    );
    let icmp_tx_buf = icmp::PacketBuffer::new(
        vec![icmp::PacketMetadata::EMPTY; 4],
        vec![0; 1024],
    );
    let icmp_socket = icmp::Socket::new(icmp_rx_buf, icmp_tx_buf);

    // DNS socket — pre-allocate 1 query slot (servers set from DHCP)
    let dns_socket = dns::Socket::new(&[], vec![None]);

    let mut sockets = SocketSet::new(vec![]);
    let dhcp_handle = sockets.add(dhcp_socket);
    let icmp_handle = sockets.add(icmp_socket);
    let dns_handle = sockets.add(dns_socket);

    *NET_STATE.lock() = Some(NetState {
        iface,
        sockets,
        dhcp_handle,
        icmp_handle,
        dns_handle,
        has_ip: false,
        ping_seq: 0,
        ping_send_at: None,
        auto_ping_done: false,
        auto_dns_started: false,
        auto_dns_query: None,
        auto_https_done: false,
    });

    crate::serial_strln!("[NET] Stack initialized, DHCP discovery starting...");
}

// ── Polling ─────────────────────────────────────────────────────────────────

pub fn poll() {
    let mut guard = NET_STATE.lock();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return,
    };

    let now = Instant::from_millis(crate::timer::uptime_ms() as i64);
    let mut device = FolkeringDevice;

    // Drive the stack
    state.iface.poll(now, &mut device, &mut state.sockets);

    // ── DHCP events ──────────────────────────────────────────────────────
    let event = state
        .sockets
        .get_mut::<dhcpv4::Socket>(state.dhcp_handle)
        .poll();
    match event {
        None => {}
        Some(dhcpv4::Event::Configured(config)) => {
            crate::serial_str!("[NET] DHCP: got IP ");
            print_ipv4_cidr(&config.address);
            crate::drivers::serial::write_newline();

            state.iface.update_ip_addrs(|addrs| {
                addrs.clear();
                addrs.push(IpCidr::Ipv4(config.address)).unwrap();
            });

            if let Some(router) = config.router {
                crate::serial_str!("[NET] DHCP: gateway ");
                print_ipv4(&router);
                crate::drivers::serial::write_newline();
                state
                    .iface
                    .routes_mut()
                    .add_default_ipv4_route(router)
                    .unwrap();
            }

            // Configure DNS servers from DHCP
            let mut dns_servers: Vec<IpAddress> = Vec::new();
            for (i, dns_ip) in config.dns_servers.iter().enumerate() {
                crate::serial_str!("[NET] DHCP: DNS");
                crate::drivers::serial::write_dec(i as u32);
                crate::serial_str!(" = ");
                print_ipv4(dns_ip);
                crate::drivers::serial::write_newline();
                dns_servers.push(IpAddress::Ipv4(*dns_ip));
            }
            state
                .sockets
                .get_mut::<dns::Socket>(state.dns_handle)
                .update_servers(&dns_servers);

            state.has_ip = true;
        }
        Some(dhcpv4::Event::Deconfigured) => {
            crate::serial_strln!("[NET] DHCP: lost configuration");
            state.iface.update_ip_addrs(|addrs| addrs.clear());
            state.iface.routes_mut().remove_default_ipv4_route();
            state.has_ip = false;
        }
    }

    // ── Auto-ping gateway after first DHCP ────────────────────────────────
    if state.has_ip && !state.auto_ping_done {
        state.auto_ping_done = true;
        let gateway = Ipv4Address::new(10, 0, 2, 2);
        send_ping_inner(state, gateway);
    }

    // ── Check for ICMP echo replies ──────────────────────────────────────
    check_ping_reply(state);

    // ── Auto-DNS test (async: start query in one tick, check in next) ────
    if state.has_ip && !state.auto_dns_started && state.auto_ping_done {
        if crate::timer::uptime_ms() > 8000 {
            state.auto_dns_started = true;
            crate::serial_strln!("[NET] DNS auto-test: resolving google.com...");
            let dns_socket = state.sockets.get_mut::<dns::Socket>(state.dns_handle);
            match dns_socket.start_query(state.iface.context(), "google.com", DnsQueryType::A) {
                Ok(h) => {
                    state.auto_dns_query = Some(h);
                }
                Err(_) => {
                    crate::serial_strln!("[NET] DNS auto-test: failed to start query");
                }
            }
        }
    }

    // Check auto-DNS result
    if let Some(qh) = state.auto_dns_query {
        let dns_socket = state.sockets.get_mut::<dns::Socket>(state.dns_handle);
        match dns_socket.get_query_result(qh) {
            Ok(addrs) => {
                state.auto_dns_query = None;
                for addr in addrs.iter() {
                    if let IpAddress::Ipv4(v4) = addr {
                        crate::serial_str!("[NET] DNS: google.com -> ");
                        print_ipv4(v4);
                        crate::drivers::serial::write_newline();
                        break;
                    }
                }
            }
            Err(dns::GetQueryResultError::Pending) => {
                // Still waiting — will check next poll tick
            }
            Err(dns::GetQueryResultError::Failed) => {
                state.auto_dns_query = None;
                crate::serial_strln!("[NET] DNS auto-test: failed (host DNS unreachable?)");
            }
        }
    }

}

// ── Ping Implementation ─────────────────────────────────────────────────────

/// Send an ICMP echo request (internal, called with lock held)
fn send_ping_inner(state: &mut NetState, target: Ipv4Address) {
    state.ping_seq = state.ping_seq.wrapping_add(1);
    let seq = state.ping_seq;

    let icmp_socket = state.sockets.get_mut::<icmp::Socket>(state.icmp_handle);

    // Bind to our echo identifier if not already bound
    if !icmp_socket.is_open() {
        icmp_socket
            .bind(icmp::Endpoint::Ident(PING_IDENT))
            .unwrap();
    }

    // Build echo request payload
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
fn check_ping_reply(state: &mut NetState) {
    let icmp_socket = state.sockets.get_mut::<icmp::Socket>(state.icmp_handle);

    while icmp_socket.can_recv() {
        let (data, from) = match icmp_socket.recv() {
            Ok(v) => v,
            Err(_) => break,
        };

        // Parse the ICMP packet
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
            _ => {
                // Other ICMP (e.g. destination unreachable) — ignore for now
            }
        }
    }
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Check if the network has an IP address
pub fn has_ip() -> bool {
    NET_STATE.lock().as_ref().map_or(false, |s| s.has_ip)
}

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

/// Resolve a domain name to an IPv4 address (blocking).
/// MUST be called from userspace syscall context (interrupts enabled).
/// Returns packed IPv4 (a | b<<8 | c<<16 | d<<24) on success, 0 on failure.
pub fn dns_lookup(name: &str) -> u64 {
    // Phase 1: Start the query (brief lock)
    let query_handle = {
        let mut guard = NET_STATE.lock();
        let state = match guard.as_mut() {
            Some(s) if s.has_ip => s,
            _ => {
                crate::serial_strln!("[NET] DNS: no network — ignoring");
                return 0;
            }
        };

        crate::serial_str!("[NET] DNS: resolving ");
        for &b in name.as_bytes() {
            crate::drivers::serial::write_byte(b);
        }
        crate::serial_strln!("...");

        let dns_socket = state.sockets.get_mut::<dns::Socket>(state.dns_handle);
        match dns_socket.start_query(state.iface.context(), name, DnsQueryType::A) {
            Ok(h) => h,
            Err(_) => {
                crate::serial_strln!("[NET] DNS: failed to start query");
                return 0;
            }
        }
        // Lock dropped here — timer ISR can poll the network
    };

    // Phase 2: Wait for result (release lock between checks so timer can poll)
    let start_ms = crate::timer::uptime_ms();
    let timeout_ms = 10_000u64;

    loop {
        // Brief yield to let timer tick poll the network
        x86_64::instructions::interrupts::enable();
        for _ in 0..1000 {
            core::hint::spin_loop();
        }

        // Check for result
        let mut guard = NET_STATE.lock();
        let state = match guard.as_mut() {
            Some(s) => s,
            None => return 0,
        };

        // Also poll the interface ourselves
        let now = Instant::from_millis(crate::timer::uptime_ms() as i64);
        let mut device = FolkeringDevice;
        state.iface.poll(now, &mut device, &mut state.sockets);

        let dns_socket = state.sockets.get_mut::<dns::Socket>(state.dns_handle);
        match dns_socket.get_query_result(query_handle) {
            Ok(addrs) => {
                for addr in addrs.iter() {
                    if let IpAddress::Ipv4(v4) = addr {
                        let o = v4.octets();
                        crate::serial_str!("[NET] DNS: resolved to ");
                        print_ipv4(v4);
                        crate::drivers::serial::write_newline();
                        return (o[0] as u64)
                            | ((o[1] as u64) << 8)
                            | ((o[2] as u64) << 16)
                            | ((o[3] as u64) << 24);
                    }
                }
                crate::serial_strln!("[NET] DNS: no IPv4 in response");
                return 0;
            }
            Err(dns::GetQueryResultError::Pending) => {
                if crate::timer::uptime_ms() - start_ms > timeout_ms {
                    crate::serial_strln!("[NET] DNS: timeout");
                    return 0;
                }
                // Drop lock and try again
            }
            Err(dns::GetQueryResultError::Failed) => {
                crate::serial_strln!("[NET] DNS: query failed (NXDOMAIN or no server)");
                return 0;
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn print_ipv4(addr: &Ipv4Address) {
    let octets = addr.octets();
    for i in 0..4 {
        crate::drivers::serial::write_dec(octets[i] as u32);
        if i < 3 {
            crate::serial_str!(".");
        }
    }
}

fn print_ipv4_cidr(cidr: &Ipv4Cidr) {
    print_ipv4(&cidr.address());
    crate::serial_str!("/");
    crate::drivers::serial::write_dec(cidr.prefix_len() as u32);
}
