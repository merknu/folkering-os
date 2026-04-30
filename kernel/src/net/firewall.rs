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

/// Ring buffer for suspicious packet metadata.
/// Protected by a spin mutex for correctness. The push path only fires
/// on dropped SYN packets (~0.1/sec), so contention is negligible.
pub struct SuspiciousQueue {
    entries: spin::Mutex<[SuspiciousPacket; QUEUE_SIZE]>,
    write_head: AtomicUsize,
    pub count: AtomicU32,
}

// Safety: inner Mutex provides synchronization
unsafe impl Sync for SuspiciousQueue {}

impl SuspiciousQueue {
    pub const fn new() -> Self {
        Self {
            entries: spin::Mutex::new([SuspiciousPacket::EMPTY; QUEUE_SIZE]),
            write_head: AtomicUsize::new(0),
            count: AtomicU32::new(0),
        }
    }

    /// Push a suspicious packet (overwrites oldest if full)
    pub fn push(&self, pkt: SuspiciousPacket) {
        let idx = self.write_head.fetch_add(1, Ordering::Release) % QUEUE_SIZE;
        if let Some(mut entries) = self.entries.try_lock() {
            entries[idx] = pkt;
        }
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Read the N most recent entries (for diagnostics).
    /// Returns count of entries copied into `out`.
    pub fn recent(&self, out: &mut [SuspiciousPacket]) -> usize {
        let head = self.write_head.load(Ordering::Acquire);
        let n = out.len().min(QUEUE_SIZE).min(head);
        if let Some(entries) = self.entries.try_lock() {
            for i in 0..n {
                // Walk backwards from most recent
                let idx = (head.wrapping_sub(1).wrapping_sub(i)) % QUEUE_SIZE;
                out[i] = entries[idx];
            }
            n
        } else {
            0
        }
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

        // Rule 5.1: Allow non-SYN TCP (i.e. packets with ACK set).
        // These are replies to traffic *we* initiated — SYN-ACK for
        // outbound connects, ACK + data for established sessions,
        // FIN/RST for tear-downs. Smoltcp's per-connection state
        // machine rejects any stray packets that don't match an open
        // 4-tuple, so the firewall doesn't need to re-check source
        // IP here. Previously this branch only allowed src_ip =
        // 10.0.2.2 (SLIRP gateway), which silently dropped every
        // reply from arbitrary LAN addresses — breaking outbound
        // connections to anything the gateway proxies for us.
        //
        // The SYN-only check above already blocks unsolicited
        // inbound connection attempts from any non-whitelisted port,
        // so this relaxed rule preserves the ingress policy while
        // unblocking legitimate replies.
        ALLOWS.fetch_add(1, Ordering::Relaxed);
        return FirewallAction::Allow;
    }

    // Default: allow unknown protocols
    ALLOWS.fetch_add(1, Ordering::Relaxed);
    FirewallAction::Allow
}

// ── AI Anomaly Detection (Digital Immune System) ────────────────────────

/// Dynamic blocklist: IPs that have been caught SYN-scanning repeatedly.
/// After 3 SYN attempts from the same IP, ALL packets from that IP are dropped.
const MAX_BLOCKLIST: usize = 16;

/// Issue #58 root cause: blocklist entries used to be permanent. After a
/// SYN flood from any IP — including a host we legitimately talk to —
/// the IP would be auto-blocked forever, dropping every subsequent
/// SYN-ACK reply from that host. Time-out the block so post-flood
/// traffic recovers naturally.
///
/// 120 s gives Draug's hibernation cycle (60 s wake-period) two
/// chances to find the proxy reachable after a flood ends.
const BLOCK_DURATION_MS: u64 = 120_000;

/// Tuple: (ip, syn_count, last_seen_ms). `last_seen_ms` is the
/// monotonic uptime timestamp from `crate::timer::uptime_ms()` at the
/// most recent SYN attempt (NOT a wall-clock value); if
/// `now - last_seen_ms` exceeds `BLOCK_DURATION_MS` the entry is
/// treated as expired.
static BLOCKLIST: spin::Mutex<([([u8; 4], u8, u64); MAX_BLOCKLIST], usize)> =
    spin::Mutex::new(([([0u8; 4], 0u8, 0u64); MAX_BLOCKLIST], 0));

/// Check if an IP is in the dynamic blocklist
fn is_blocked(ip: [u8; 4]) -> bool {
    let now = crate::timer::uptime_ms();
    if let Some(list) = BLOCKLIST.try_lock() {
        for i in 0..list.1 {
            if list.0[i].0 == ip && list.0[i].1 >= 3 {
                // Issue #58 fix: only honour the block if it hasn't
                // expired. If `now - last_seen >= BLOCK_DURATION_MS`,
                // the IP has been quiet long enough that we let it
                // back in. Re-blocking happens automatically on the
                // next 3 SYN attempts.
                let last_seen = list.0[i].2;
                if now.saturating_sub(last_seen) < BLOCK_DURATION_MS {
                    return true;
                }
            }
        }
    }
    false
}

/// Record a SYN attempt from an IP. After 3 attempts, auto-block.
fn record_syn_attempt(ip: [u8; 4]) {
    let now = crate::timer::uptime_ms();
    if let Some(mut list) = BLOCKLIST.try_lock() {
        // Check if already tracked
        for i in 0..list.1 {
            if list.0[i].0 == ip {
                // Issue #58: if the prior block expired, reset the
                // counter so the IP gets a fresh chance. Otherwise
                // keep counting up.
                if list.0[i].1 >= 3
                    && now.saturating_sub(list.0[i].2) >= BLOCK_DURATION_MS
                {
                    list.0[i].1 = 1;
                    list.0[i].2 = now;
                    return;
                }
                list.0[i].1 = list.0[i].1.saturating_add(1);
                list.0[i].2 = now;
                if list.0[i].1 == 3 {
                    // Auto-blocked! Log it. Format the expiry window
                    // from BLOCK_DURATION_MS so the log stays accurate
                    // if the constant changes.
                    crate::serial_str!("[FW-AI] AUTO-BLOCKED ");
                    crate::drivers::serial::write_dec(ip[0] as u32);
                    crate::serial_str!(".");
                    crate::drivers::serial::write_dec(ip[1] as u32);
                    crate::serial_str!(".");
                    crate::drivers::serial::write_dec(ip[2] as u32);
                    crate::serial_str!(".");
                    crate::drivers::serial::write_dec(ip[3] as u32);
                    crate::serial_str!(" (3 SYN attempts, expires in ");
                    crate::drivers::serial::write_dec((BLOCK_DURATION_MS / 1000) as u32);
                    crate::serial_strln!("s)");
                }
                return;
            }
        }
        // New IP — add to tracker
        let idx = list.1;
        if idx < MAX_BLOCKLIST {
            list.0[idx] = (ip, 1, now);
            list.1 = idx + 1;
            return;
        }
        // Table full. Without eviction an attacker could spoof 16
        // source IPs with one SYN each (16 packets, well under the
        // block threshold of 3) to fill the slot array, after which
        // their real attack IP slips through without tracking.
        //
        // Eligible victims:
        //  * unblocked entries (count < 3) — pick the lowest count
        //    so we evict the least-invested tracker.
        //  * entries whose block has already expired (`is_blocked`
        //    won't honour them anyway). Rank these at 0 so they're
        //    always preferred over an unblocked tracker with count
        //    > 0 — keeps PR #64 review feedback from going stale:
        //    if all 16 slots filled with old expired blocks, the
        //    previous logic refused every eviction and stopped
        //    tracking new IPs.
        let mut victim: Option<usize> = None;
        let mut victim_rank: u8 = u8::MAX;
        for i in 0..MAX_BLOCKLIST {
            let c = list.0[i].1;
            let is_expired_block =
                c >= 3 && now.saturating_sub(list.0[i].2) >= BLOCK_DURATION_MS;
            if c < 3 || is_expired_block {
                let rank = if is_expired_block { 0 } else { c };
                if rank < victim_rank {
                    victim_rank = rank;
                    victim = Some(i);
                }
            }
        }
        if let Some(i) = victim {
            list.0[i] = (ip, 1, now);
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
