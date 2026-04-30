//! Network Stack — smoltcp integration over VirtIO-net or WASM E1000 driver
//!
//! Provides DHCP-based IPv4 connectivity via smoltcp. Supports two backends:
//! 1. VirtIO-net (kernel-internal driver)
//! 2. WASM E1000 driver (userspace, via packet ring IPC)
//!
//! The WASM backend uses shared memory ring buffers for zero-copy packet
//! transfer between the compositor's E1000 driver and the kernel's smoltcp stack.

// ── Module declarations ────────────────────────────────────────────────

// Public submodules (existing top-level network protocols)
pub mod firewall;
pub mod gemini;
pub mod github;
pub mod json;
pub mod tls;
pub mod tls_verify;
pub mod tcp_plain;
pub mod websocket;

// Internal submodules (refactored from former mod.rs body)
mod device;
mod state;
pub mod wasm_ring;
pub mod icmp;
pub mod dns;
pub mod udp;
pub mod ntp;
pub mod tcp_shell;
pub mod tcp_async;
pub mod a64_stream;

// ── Re-exports for the public API ──────────────────────────────────────
//
// Other crates/modules import these via `crate::net::*` — keep names stable.

pub use wasm_ring::{wasm_net_submit_rx, wasm_net_poll_tx, init_wasm_net, wasm_net_active};
pub use icmp::send_ping;
pub use dns::dns_lookup;
pub use udp::{udp_send, udp_send_recv};
pub use ntp::ntp_query;

/// Issue #58 hypothesis #3 — flush smoltcp's ARP/neighbor cache.
///
/// smoltcp 0.12 doesn't expose a direct `flush_neighbor_cache` on
/// the `Interface` API; the only public path that triggers it is
/// `update_ip_addrs`. We use a no-op closure so the IP set is left
/// alone but the post-call `flush_neighbor_cache` side-effect runs.
///
/// Called from the Draug hibernation wake path as an experimental
/// recovery step: if a stale neighbor entry is what's pinning the
/// post-flood TCP wedge, this should let the next connect attempt
/// re-resolve via fresh ARP. If TCP still wedges after the flush,
/// the bug isn't in the ARP cache.
pub fn reset_neighbor_cache() {
    let mut attempts = 0u32;
    let mut guard = loop {
        if let Some(g) = NET_STATE.try_lock() { break g; }
        attempts += 1;
        if attempts > 1000 {
            crate::serial_strln!("[NET] reset_neighbor_cache: NET_STATE locked, skipping");
            return;
        }
        core::hint::spin_loop();
    };
    if let Some(state) = guard.as_mut() {
        state.iface.update_ip_addrs(|_addrs| { /* no-op — only here for the flush side-effect */ });
        crate::serial_strln!("[NET] neighbor cache flushed");
    }
}

// Internal re-exports used by submodules
pub(crate) use state::{NetState, NET_STATE};

// ── Imports ────────────────────────────────────────────────────────────

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::{dhcpv4, dns as smoltcp_dns, icmp as smoltcp_icmp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address, Ipv4Cidr,
};

use crate::drivers::virtio_net;
use device::FolkeringDevice;

// ── Initialization ─────────────────────────────────────────────────────

/// Initialize network stack from VirtIO-net (existing path)
pub fn init() {
    let mac = match virtio_net::mac_address() {
        Some(m) => m,
        None => {
            crate::serial_strln!("[NET] No VirtIO MAC — will wait for WASM driver");
            return;
        }
    };
    init_with_mac(mac);
}

/// Initialize smoltcp stack with a given MAC address.
/// Called by both VirtIO init and WASM E1000 registration.
pub(crate) fn init_with_mac(mac: [u8; 6]) {
    if NET_STATE.lock().is_some() {
        crate::serial_strln!("[NET] Stack already initialized, skipping re-init");
        return;
    }

    crate::serial_str!("[NET] Initializing smoltcp stack, MAC=");
    for i in 0..6 {
        crate::drivers::serial::write_hex(mac[i] as u64);
        if i < 5 { crate::serial_str!(":"); }
    }
    crate::drivers::serial::write_newline();

    let mut device = FolkeringDevice;

    let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
    let mut config = Config::new(hw_addr);
    config.random_seed = {
        let lo: u32;
        let hi: u32;
        unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi); }
        ((hi as u64) << 32) | (lo as u64)
    };

    let now = Instant::from_millis(crate::timer::uptime_ms() as i64);
    let iface = Interface::new(config, &mut device, now);

    let dhcp_socket = dhcpv4::Socket::new();
    let icmp_rx_buf = smoltcp_icmp::PacketBuffer::new(
        vec![smoltcp_icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]);
    let icmp_tx_buf = smoltcp_icmp::PacketBuffer::new(
        vec![smoltcp_icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]);
    let icmp_socket = smoltcp_icmp::Socket::new(icmp_rx_buf, icmp_tx_buf);
    let dns_socket = smoltcp_dns::Socket::new(&[], vec![None]);

    let mut sockets = SocketSet::new(vec![]);
    let dhcp_handle = sockets.add(dhcp_socket);
    let icmp_handle = sockets.add(icmp_socket);
    let dns_handle = sockets.add(dns_socket);

    *NET_STATE.lock() = Some(NetState {
        iface, sockets, dhcp_handle, icmp_handle, dns_handle,
        has_ip: false, ping_seq: 0, ping_send_at: None,
        auto_ping_done: false, auto_dns_started: false,
        auto_dns_query: None, auto_https_done: false,
    });

    crate::serial_strln!("[NET] Stack initialized, DHCP discovery starting...");

    // Drive the network stack until DHCP assigns an IP.
    let dhcp_start = crate::timer::uptime_ms();
    loop {
        poll();
        {
            let g = NET_STATE.lock();
            if g.as_ref().map_or(false, |s| s.has_ip) {
                break;
            }
        }
        if crate::timer::uptime_ms() - dhcp_start > 10_000 {
            crate::serial_strln!("[NET] DHCP: timeout (10s), continuing without IP");
            break;
        }
        for _ in 0..10_000 { core::hint::spin_loop(); }
    }
}

// ── Polling ────────────────────────────────────────────────────────────

pub fn poll() {
    // Use try_lock: this is called from the timer ISR (tick()),
    // so we MUST NOT spin if the lock is held by a syscall (e.g., TLS/Gemini).
    let mut guard = match NET_STATE.try_lock() {
        Some(g) => g,
        None => return,
    };
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return,
    };

    let now = Instant::from_millis(crate::timer::uptime_ms() as i64);
    let mut device = FolkeringDevice;

    state.iface.poll(now, &mut device, &mut state.sockets);

    // ── DHCP events ────────────────────────────────────────────────────
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
                if addrs.push(IpCidr::Ipv4(config.address)).is_err() {
                    crate::serial_strln!("[NET] DHCP: WARN address list full");
                }
            });

            if let Some(router) = config.router {
                crate::serial_str!("[NET] DHCP: gateway ");
                print_ipv4(&router);
                crate::drivers::serial::write_newline();
                if state
                    .iface
                    .routes_mut()
                    .add_default_ipv4_route(router)
                    .is_err()
                {
                    crate::serial_strln!("[NET] DHCP: WARN route table full");
                }
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
                .get_mut::<smoltcp_dns::Socket>(state.dns_handle)
                .update_servers(&dns_servers);

            state.has_ip = true;

            // Start TCP shell server once we have an IP
            tcp_shell::init(state);
        }
        Some(dhcpv4::Event::Deconfigured) => {
            crate::serial_strln!("[NET] DHCP: lost configuration");
            state.iface.update_ip_addrs(|addrs| addrs.clear());
            state.iface.routes_mut().remove_default_ipv4_route();
            state.has_ip = false;
        }
    }

    // Check for ICMP echo replies
    icmp::check_ping_reply(state);

    // TCP remote shell
    tcp_shell::poll(state);
}

// ── Public API ─────────────────────────────────────────────────────────

/// Check if the network has an IP address
pub fn has_ip() -> bool {
    NET_STATE.lock().as_ref().map_or(false, |s| s.has_ip)
}

// ── Helpers (used by submodules) ───────────────────────────────────────

pub(crate) fn print_ipv4(addr: &Ipv4Address) {
    let octets = addr.octets();
    for i in 0..4 {
        crate::drivers::serial::write_dec(octets[i] as u32);
        if i < 3 {
            crate::serial_str!(".");
        }
    }
}

pub(crate) fn print_ipv4_cidr(cidr: &Ipv4Cidr) {
    print_ipv4(&cidr.address());
    crate::serial_str!("/");
    crate::drivers::serial::write_dec(cidr.prefix_len() as u32);
}
