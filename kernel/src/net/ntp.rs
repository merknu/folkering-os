//! Minimal SNTP (Simple NTP) client.
//!
//! Sends a single request to an NTP server and parses the response to get
//! current Unix timestamp. Auto-syncs RTC if drift exceeds 5 seconds.

use super::udp::udp_send_recv;

/// Query an NTP server for current time. Returns Unix timestamp (seconds
/// since 1970-01-01 UTC) or 0 on failure.
///
/// Uses pool.ntp.org by default (resolved via DNS first).
/// Caller should provide IP via dns_lookup().
pub fn ntp_query(server_ip: [u8; 4]) -> u64 {
    // NTP request packet: 48 bytes
    // byte 0 = LI(2) | VN(3) | Mode(3) = 0b00_011_011 = 0x1B (client mode, version 3)
    let mut request = [0u8; 48];
    request[0] = 0x1B;

    let mut response = [0u8; 48];
    let n = udp_send_recv(server_ip, 123, &request, &mut response, 3000);
    if n < 48 { return 0; }

    // NTP timestamp is 64 bits: 32-bit seconds + 32-bit fraction, starting 1900-01-01
    // Transmit Timestamp is at offset 40
    let ntp_seconds = u32::from_be_bytes([
        response[40], response[41], response[42], response[43],
    ]) as u64;

    if ntp_seconds == 0 { return 0; }

    // NTP epoch (1900-01-01) → Unix epoch (1970-01-01) = 2208988800 seconds
    if ntp_seconds < 2208988800 { return 0; }
    let unix = ntp_seconds - 2208988800;

    // Sync RTC if NTP time is significantly different (avoid jitter on every call)
    let current_rtc = crate::drivers::cmos::unix_timestamp();
    let diff = if unix > current_rtc { unix - current_rtc } else { current_rtc - unix };
    if diff > 5 {
        crate::drivers::cmos::set_unix_time(unix);
    }

    unix
}
