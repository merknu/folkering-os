//! Network Stack — smoltcp integration over VirtIO-net or WASM E1000 driver
//!
//! Provides DHCP-based IPv4 connectivity via smoltcp. Supports two backends:
//! 1. VirtIO-net (kernel-internal driver)
//! 2. WASM E1000 driver (userspace, via packet ring IPC)
//!
//! The WASM backend uses shared memory ring buffers for zero-copy packet transfer
//! between the compositor's E1000 driver and the kernel's smoltcp stack.

pub mod firewall;
pub mod gemini;
pub mod github;
pub mod json;
pub mod tls;
pub mod websocket;

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

/// Max packets in the WASM driver ring buffers
const WASM_NET_RING_SIZE: usize = 8;
/// Max Ethernet frame size
const MAX_FRAME_SIZE: usize = 1514;

// ── WASM Network Driver Packet Ring ─────────────────────────────────────────
//
// Shared between the compositor (via syscalls) and smoltcp (via Device trait).
// The compositor's E1000 host functions call submit_rx/poll_tx syscalls.
// The kernel's FolkeringDevice reads from rx_ring and writes to tx_ring.

struct PacketSlot {
    data: [u8; MAX_FRAME_SIZE],
    len: usize,
    used: bool,
}

impl PacketSlot {
    const EMPTY: Self = Self { data: [0; MAX_FRAME_SIZE], len: 0, used: false };
}

/// Packet ring for WASM network driver ↔ kernel smoltcp bridge
pub(crate) struct WasmNetRing {
    /// Packets received by E1000 hardware, waiting for smoltcp to process
    rx_ring: [PacketSlot; WASM_NET_RING_SIZE],
    rx_head: usize,
    rx_count: usize,
    /// Packets from smoltcp waiting for E1000 to transmit
    tx_ring: [PacketSlot; WASM_NET_RING_SIZE],
    tx_head: usize,
    tx_count: usize,
    /// MAC address provided by the WASM driver
    mac: [u8; 6],
    /// Whether the WASM net backend is active
    active: bool,
}

impl WasmNetRing {
    const fn new() -> Self {
        Self {
            rx_ring: [PacketSlot::EMPTY; WASM_NET_RING_SIZE],
            rx_head: 0, rx_count: 0,
            tx_ring: [PacketSlot::EMPTY; WASM_NET_RING_SIZE],
            tx_head: 0, tx_count: 0,
            mac: [0; 6],
            active: false,
        }
    }

    /// Submit a received packet (from E1000 DMA → kernel)
    fn submit_rx(&mut self, data: &[u8]) -> bool {
        if self.rx_count >= WASM_NET_RING_SIZE || data.len() > MAX_FRAME_SIZE {
            return false;
        }
        let idx = (self.rx_head + self.rx_count) % WASM_NET_RING_SIZE;
        self.rx_ring[idx].data[..data.len()].copy_from_slice(data);
        self.rx_ring[idx].len = data.len();
        self.rx_ring[idx].used = true;
        self.rx_count += 1;
        true
    }

    /// Pop a received packet (kernel smoltcp reads it)
    fn pop_rx(&mut self) -> Option<(&[u8], usize)> {
        if self.rx_count == 0 { return None; }
        let idx = self.rx_head;
        if !self.rx_ring[idx].used { return None; }
        let len = self.rx_ring[idx].len;
        self.rx_ring[idx].used = false;
        self.rx_head = (self.rx_head + 1) % WASM_NET_RING_SIZE;
        self.rx_count -= 1;
        Some((&self.rx_ring[idx].data[..len], len))
    }

    /// Queue a packet for transmission (kernel smoltcp → E1000)
    fn submit_tx(&mut self, data: &[u8]) -> bool {
        if self.tx_count >= WASM_NET_RING_SIZE || data.len() > MAX_FRAME_SIZE {
            return false;
        }
        let idx = (self.tx_head + self.tx_count) % WASM_NET_RING_SIZE;
        self.tx_ring[idx].data[..data.len()].copy_from_slice(data);
        self.tx_ring[idx].len = data.len();
        self.tx_ring[idx].used = true;
        self.tx_count += 1;
        true
    }

    /// Pop a packet to transmit (E1000 reads it)
    fn pop_tx(&mut self) -> Option<(&[u8], usize)> {
        if self.tx_count == 0 { return None; }
        let idx = self.tx_head;
        if !self.tx_ring[idx].used { return None; }
        let len = self.tx_ring[idx].len;
        self.tx_ring[idx].used = false;
        self.tx_head = (self.tx_head + 1) % WASM_NET_RING_SIZE;
        self.tx_count -= 1;
        Some((&self.tx_ring[idx].data[..len], len))
    }
}

/// Global WASM network ring — accessed by syscalls and smoltcp Device
pub(crate) static WASM_NET: Mutex<WasmNetRing> = Mutex::new(WasmNetRing::new());

// ── Public API for syscalls ─────────────────────────────────────────────────

/// Called from SYS_NET_SUBMIT_RX syscall (compositor → kernel)
pub fn wasm_net_submit_rx(data: &[u8]) -> bool {
    WASM_NET.lock().submit_rx(data)
}

/// Called from SYS_NET_POLL_TX syscall (kernel → compositor)
/// Returns (data_ptr, length) or None
pub fn wasm_net_poll_tx(buf: &mut [u8]) -> Option<usize> {
    let mut ring = WASM_NET.lock();
    if let Some((data, len)) = ring.pop_tx() {
        let copy_len = len.min(buf.len());
        buf[..copy_len].copy_from_slice(&data[..copy_len]);
        Some(copy_len)
    } else {
        None
    }
}

/// Initialize the WASM network backend with the given MAC address.
/// Called from SYS_NET_REGISTER syscall when E1000 driver starts.
pub fn init_wasm_net(mac: [u8; 6]) {
    let mut ring = WASM_NET.lock();
    ring.mac = mac;
    ring.active = true;
    drop(ring);

    crate::serial_str!("[NET] WASM E1000 registered, MAC=");
    for i in 0..6 {
        crate::drivers::serial::write_hex(mac[i] as u64);
        if i < 5 { crate::serial_str!(":"); }
    }
    crate::serial_strln!("");

    // Initialize smoltcp with this MAC (same as VirtIO path but using WASM backend)
    init_with_mac(mac);
}

/// Check if WASM net backend is active
pub fn wasm_net_active() -> bool {
    WASM_NET.try_lock().map_or(false, |r| r.active)
}

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
        // Route to WASM driver if active, otherwise VirtIO
        let sent = WASM_NET.try_lock().map_or(false, |mut ring| {
            if ring.active { ring.submit_tx(&buffer) } else { false }
        });
        if sent {
            static TX_LOG_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            let c = TX_LOG_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if c < 5 { // Log first 5 TX packets
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
                    // ── Firewall: inspect before passing to smoltcp ──
                    if firewall::filter_packet(&data[..len]) == firewall::FirewallAction::Drop {
                        drop(ring);
                        return None; // Dropped by firewall
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
            // ── Firewall: inspect before passing to smoltcp ──
            if firewall::filter_packet(&frame[..len]) == firewall::FirewallAction::Allow {
                let rx = FolkeringRxToken { buffer: frame[..len].to_vec() };
                return Some((rx, FolkeringTxToken));
            }
            // Packet dropped, try next from VirtIO queue
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

// ── Network State ───────────────────────────────────────────────────────────

pub(crate) struct NetState {
    pub(crate) iface: Interface,
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

pub(crate) static NET_STATE: Mutex<Option<NetState>> = Mutex::new(None);

// ── Initialization ──────────────────────────────────────────────────────────

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
fn init_with_mac(mac: [u8; 6]) {
    // Don't re-initialize if already running
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
    let icmp_rx_buf = icmp::PacketBuffer::new(
        vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]);
    let icmp_tx_buf = icmp::PacketBuffer::new(
        vec![icmp::PacketMetadata::EMPTY; 4], vec![0; 1024]);
    let icmp_socket = icmp::Socket::new(icmp_rx_buf, icmp_tx_buf);
    let dns_socket = dns::Socket::new(&[], vec![None]);

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

    // ── Blocking DHCP boot loop ──
    // Drive the network stack until DHCP assigns an IP.
    // This replaces timer-ISR polling (which caused #GP from stack misalignment).
    // Reuses the poll() function which handles DHCP events properly.
    let dhcp_start = crate::timer::uptime_ms();
    loop {
        poll(); // calls try_lock + iface.poll + DHCP event handling
        {
            let g = NET_STATE.lock();
            if g.as_ref().map_or(false, |s| s.has_ip) {
                break; // DHCP complete!
            }
        }
        if crate::timer::uptime_ms() - dhcp_start > 10_000 {
            crate::serial_strln!("[NET] DHCP: timeout (10s), continuing without IP");
            break;
        }
        for _ in 0..10_000 { core::hint::spin_loop(); }
    }
}

// ── Polling ─────────────────────────────────────────────────────────────────

pub fn poll() {
    // Use try_lock: this is called from the timer ISR (tick()),
    // so we MUST NOT spin if the lock is held by a syscall (e.g., TLS/Gemini).
    // Spinning in ISR context would deadlock (ISR can't be preempted).
    let mut guard = match NET_STATE.try_lock() {
        Some(g) => g,
        None => return, // Lock held — skip this poll, next tick will retry
    };
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

    // ── Auto-ping after first DHCP ─────────────────────────────────────
    // Disabled: hardcoded gateway doesn't work with bridge networking.
    // Ping can be triggered manually via the `ping` omnibar command.

    // ── Check for ICMP echo replies ──────────────────────────────────────
    check_ping_reply(state);

    // DNS auto-test DISABLED — caused system deadlock after completion.
    // The DNS query completes successfully but something in smoltcp's
    // socket cleanup path causes the system to hang permanently.
    // This prevented Draug from ever reaching 15min idle for AutoDream.
    // (Disabled 2026-04-03, see commit history for original code)

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

// ── Non-blocking HTTPS test ─────────────────────────────────────────────
// Instead of blocking the compositor with dns_lookup + https_get,
// we use a kernel-side flag that poll() processes incrementally.

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
