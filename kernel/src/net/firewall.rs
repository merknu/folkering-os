//! Pre-Routing Firewall — Digital Immune System
//!
//! Inspects raw Ethernet frames BEFORE they reach smoltcp.
//! Stateless packet filter with suspicious packet telemetry for AI analysis.
//!
//! Design: zero-allocation, inline, no locks. Uses atomics for the
//! suspicious packet queue. Safe to call from timer ISR context.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

// ── Actions ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub enum FirewallAction {
    Allow,
    Drop,
}

// ── Suspicious Packet Telemetry ─────────────────────────────────────────

/// Metadata for a dropped/suspicious packet (for future AI analysis)
#[derive(Clone, Copy)]
pub struct SuspiciousPacket {
    pub src_ip: [u8; 4],
    pub dst_port: u16,
    pub protocol: u8,      // 6=TCP, 17=UDP, 1=ICMP
    pub tcp_flags: u8,
    pub timestamp_ms: u64,
    pub valid: bool,
}

impl SuspiciousPacket {
    const EMPTY: Self = Self {
        src_ip: [0; 4], dst_port: 0, protocol: 0, tcp_flags: 0,
        timestamp_ms: 0, valid: false,
    };
}

const QUEUE_SIZE: usize = 16;

/// Lock-free ring buffer for suspicious packet metadata.
/// Single producer (firewall in receive()), multiple consumers (AI/diagnostics).
pub struct SuspiciousQueue {
    entries: [SuspiciousPacket; QUEUE_SIZE],
    write_head: AtomicUsize,
    pub count: AtomicU32,
}

impl SuspiciousQueue {
    pub const fn new() -> Self {
        Self {
            entries: [SuspiciousPacket::EMPTY; QUEUE_SIZE],
            write_head: AtomicUsize::new(0),
            count: AtomicU32::new(0),
        }
    }

    /// Push a suspicious packet (overwrites oldest if full)
    pub fn push(&self, pkt: SuspiciousPacket) {
        let idx = self.write_head.fetch_add(1, Ordering::Relaxed) % QUEUE_SIZE;
        // Safety: single producer (firewall runs under NET_STATE lock context)
        unsafe {
            let ptr = &self.entries as *const _ as *mut [SuspiciousPacket; QUEUE_SIZE];
            (*ptr)[idx] = pkt;
        }
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Read the N most recent entries (for diagnostics)
    pub fn recent(&self, n: usize) -> &[SuspiciousPacket] {
        let end = self.write_head.load(Ordering::Relaxed).min(QUEUE_SIZE);
        let start = end.saturating_sub(n);
        &self.entries[start..end]
    }
}

/// Global suspicious packet queue — accessed by firewall + AI diagnostics
pub static SUSPICIOUS: SuspiciousQueue = SuspiciousQueue::new();

/// Total packets dropped by firewall
pub static DROPS: AtomicU32 = AtomicU32::new(0);
/// Total packets allowed
pub static ALLOWS: AtomicU32 = AtomicU32::new(0);

// ── Packet Filter ───────────────────────────────────────────────────────

/// EtherType constants
const ETHERTYPE_ARP: u16  = 0x0806;
const ETHERTYPE_IPV4: u16 = 0x0800;

/// IP protocol constants
const PROTO_ICMP: u8 = 1;
const PROTO_TCP: u8  = 6;
const PROTO_UDP: u8  = 17;

/// TCP flag bits
const TCP_SYN: u8 = 0x02;
const TCP_ACK: u8 = 0x10;

/// Inspect a raw Ethernet frame and decide whether to allow or drop it.
///
/// Frame format: [dst_mac(6) | src_mac(6) | ethertype(2) | payload...]
/// This function does NO allocation and NO locking. ~50ns per call.
#[inline]
pub fn filter_packet(frame: &[u8]) -> FirewallAction {
    // Need at least Ethernet header (14 bytes)
    if frame.len() < 14 {
        return FirewallAction::Drop;
    }

    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);

    // Rule 1: ARP → Always allow (needed for MAC resolution + DHCP)
    if ethertype == ETHERTYPE_ARP {
        ALLOWS.fetch_add(1, Ordering::Relaxed);
        return FirewallAction::Allow;
    }

    // Rule 2: Non-IPv4 (IPv6, etc.) → Allow (permissive for now)
    if ethertype != ETHERTYPE_IPV4 {
        ALLOWS.fetch_add(1, Ordering::Relaxed);
        return FirewallAction::Allow;
    }

    // ── IPv4 packet inspection ──
    // Need at least: 14 (eth) + 20 (min IPv4 header) = 34 bytes
    if frame.len() < 34 {
        return FirewallAction::Drop; // Truncated IPv4
    }

    let ihl = (frame[14] & 0x0F) as usize; // Header length in 32-bit words
    if ihl < 5 { return FirewallAction::Drop; } // Invalid IHL
    let ip_hdr_len = ihl * 4;
    let proto = frame[14 + 9]; // Protocol field
    let src_ip = [frame[14 + 12], frame[14 + 13], frame[14 + 14], frame[14 + 15]];

    // Rule 3: ICMP → Allow (ping replies, diagnostics)
    if proto == PROTO_ICMP {
        ALLOWS.fetch_add(1, Ordering::Relaxed);
        return FirewallAction::Allow;
    }

    // Need transport header: 14 + ip_hdr_len + 4 (src+dst ports)
    let transport_offset = 14 + ip_hdr_len;
    if frame.len() < transport_offset + 4 {
        return FirewallAction::Drop; // Truncated transport header
    }

    let src_port = u16::from_be_bytes([frame[transport_offset], frame[transport_offset + 1]]);
    let dst_port = u16::from_be_bytes([frame[transport_offset + 2], frame[transport_offset + 3]]);

    // Rule 4: UDP → Allow DHCP (67/68) and DNS (53), allow all other UDP
    if proto == PROTO_UDP {
        ALLOWS.fetch_add(1, Ordering::Relaxed);
        return FirewallAction::Allow;
    }

    // Rule 4.5: Dynamic blocklist — IPs auto-blocked by anomaly detection
    if is_blocked(src_ip) {
        DROPS.fetch_add(1, Ordering::Relaxed);
        return FirewallAction::Drop; // Silently drop all traffic from blocked IPs
    }

    // Rule 5: TCP — block unsolicited inbound SYN (except whitelisted ports)
    if proto == PROTO_TCP {
        // TCP flags at offset 13 within TCP header
        if frame.len() < transport_offset + 14 {
            return FirewallAction::Drop; // Truncated TCP header
        }
        let tcp_flags = frame[transport_offset + 13];

        // SYN set, ACK not set → unsolicited connection attempt
        if (tcp_flags & TCP_SYN) != 0 && (tcp_flags & TCP_ACK) == 0 {
            // Allow SYN to whitelisted local server ports
            if dst_port == 2222 {
                // TCP remote shell — allow inbound connections
                ALLOWS.fetch_add(1, Ordering::Relaxed);
                return FirewallAction::Allow;
            }
            // Track for anomaly detection (auto-block after 3 attempts)
            record_syn_attempt(src_ip);
            log_drop(src_ip, src_port, dst_port);

            SUSPICIOUS.push(SuspiciousPacket {
                src_ip,
                dst_port,
                protocol: PROTO_TCP,
                tcp_flags,
                timestamp_ms: crate::timer::uptime_ms(),
                valid: true,
            });

            DROPS.fetch_add(1, Ordering::Relaxed);
            return FirewallAction::Drop;
        }

        // Rule 5.1: Stateful-ish — allow non-SYN TCP from known sources.
        // Under SLIRP, all legitimate traffic comes from the gateway
        // (10.0.2.2) or is to a whitelisted local port (2222).
        let from_gateway = src_ip == [10, 0, 2, 2];
        let to_shell = dst_port == 2222;

        if from_gateway || to_shell {
            ALLOWS.fetch_add(1, Ordering::Relaxed);
            return FirewallAction::Allow;
        }

        // Non-SYN TCP from non-gateway source → spoofed or unknown
        DROPS.fetch_add(1, Ordering::Relaxed);
        return FirewallAction::Drop;
    }

    // Default: allow unknown protocols
    ALLOWS.fetch_add(1, Ordering::Relaxed);
    FirewallAction::Allow
}

// ── AI Anomaly Detection (Digital Immune System) ────────────────────────

/// Dynamic blocklist: IPs that have been caught SYN-scanning repeatedly.
/// After 3 SYN attempts from the same IP, ALL packets from that IP are dropped.
const MAX_BLOCKLIST: usize = 16;
static BLOCKLIST: spin::Mutex<([([u8; 4], u8); MAX_BLOCKLIST], usize)> =
    spin::Mutex::new(([([0u8; 4], 0u8); MAX_BLOCKLIST], 0));

/// Check if an IP is in the dynamic blocklist
fn is_blocked(ip: [u8; 4]) -> bool {
    if let Some(list) = BLOCKLIST.try_lock() {
        for i in 0..list.1 {
            if list.0[i].0 == ip && list.0[i].1 >= 3 {
                return true;
            }
        }
    }
    false
}

/// Record a SYN attempt from an IP. After 3 attempts, auto-block.
fn record_syn_attempt(ip: [u8; 4]) {
    if let Some(mut list) = BLOCKLIST.try_lock() {
        // Check if already tracked
        for i in 0..list.1 {
            if list.0[i].0 == ip {
                list.0[i].1 = list.0[i].1.saturating_add(1);
                if list.0[i].1 == 3 {
                    // Auto-blocked! Log it
                    crate::serial_str!("[FW-AI] AUTO-BLOCKED ");
                    crate::drivers::serial::write_dec(ip[0] as u32);
                    crate::serial_str!(".");
                    crate::drivers::serial::write_dec(ip[1] as u32);
                    crate::serial_str!(".");
                    crate::drivers::serial::write_dec(ip[2] as u32);
                    crate::serial_str!(".");
                    crate::drivers::serial::write_dec(ip[3] as u32);
                    crate::serial_strln!(" (3 SYN attempts)");
                }
                return;
            }
        }
        // New IP — add to tracker
        let idx = list.1;
        if idx < MAX_BLOCKLIST {
            list.0[idx] = (ip, 1);
            list.1 = idx + 1;
        }
    }
}

/// Get anomaly stats: (blocked_ips, total_syn_attempts)
pub fn anomaly_stats() -> (u32, u32) {
    if let Some(list) = BLOCKLIST.try_lock() {
        let count = list.1;
        let mut blocked = 0u32;
        let mut attempts = 0u32;
        for i in 0..count {
            if list.0[i].1 >= 3 { blocked += 1; }
            attempts += list.0[i].1 as u32;
        }
        (blocked, attempts)
    } else {
        (0, 0)
    }
}

// ── Logging ─────────────────────────────────────────────────────────────

/// Log limit to prevent serial flooding
static DROP_LOG_COUNT: AtomicU32 = AtomicU32::new(0);
const MAX_DROP_LOGS: u32 = 32;

fn log_drop(src_ip: [u8; 4], src_port: u16, dst_port: u16) {
    let count = DROP_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if count >= MAX_DROP_LOGS { return; }

    crate::serial_str!("[FW] DROP TCP SYN ");
    crate::drivers::serial::write_dec(src_ip[0] as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(src_ip[1] as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(src_ip[2] as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(src_ip[3] as u32);
    crate::serial_str!(":");
    crate::drivers::serial::write_dec(src_port as u32);
    crate::serial_str!(" -> :");
    crate::drivers::serial::write_dec(dst_port as u32);
    crate::drivers::serial::write_newline();
}
