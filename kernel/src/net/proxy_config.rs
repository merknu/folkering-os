//! Build-time configurable proxy address.
//!
//! Phase 17 hardcoded `[192, 168, 68, 150]` (the demo Proxmox host's
//! LAN address) at every TCP-to-host call site — kernel syscall
//! handlers, the Gemini relay, the WebSocket bring-up in compositor,
//! and the daemon's async TCP path. That made local QEMU runs (which
//! use SLIRP / `[10, 0, 2, 2]`) impossible without source patches.
//!
//! This module exposes a single `PROXY_IP` resolved at compile time:
//! `FOLKERING_PROXY_IP` env var if set (e.g. `cargo build` with
//! `FOLKERING_PROXY_IP=192.168.68.150`), otherwise the SLIRP default.
//! `PROXY_PORT` follows the same pattern with a 14711 default.

/// Address of the host-side `folkering-proxy` listener.
pub const PROXY_IP: [u8; 4] = match option_env!("FOLKERING_PROXY_IP") {
    Some(s) => parse_ipv4(s),
    None => [10, 0, 2, 2],
};

/// TCP port the host-side proxy listens on.
pub const PROXY_PORT: u16 = match option_env!("FOLKERING_PROXY_PORT") {
    Some(s) => parse_u16(s),
    None => 14711,
};

/// TCP port the host-side Gemini relay listens on. Separate from the
/// main proxy because it's historically a different service
/// (`mcp/server.py` exposes both on different ports).
pub const GEMINI_PORT: u16 = match option_env!("FOLKERING_GEMINI_PORT") {
    Some(s) => parse_u16(s),
    None => 8080,
};

/// Compile-time IPv4 dotted-quad parser. Accepts strings like
/// `"10.0.2.2"` or `"192.168.68.150"`. Non-digit / non-dot bytes are
/// silently treated as zero — bad input panics later through normal
/// `[u8; 4]` use, which is fine for a build-time misconfiguration
/// signal. Each octet must fit in `u8`; values > 255 wrap (which is
/// also a misconfiguration the user will notice on first packet).
const fn parse_ipv4(s: &str) -> [u8; 4] {
    let bytes = s.as_bytes();
    let mut out = [0u8; 4];
    let mut octet: usize = 0;
    let mut acc: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'.' {
            if octet < 4 {
                out[octet] = acc as u8;
            }
            octet += 1;
            acc = 0;
        } else if b >= b'0' && b <= b'9' {
            acc = acc * 10 + (b - b'0') as u32;
        }
        i += 1;
    }
    if octet < 4 {
        out[octet] = acc as u8;
    }
    out
}

/// Compile-time decimal `u16` parser. Same lenient rules as
/// `parse_ipv4`.
const fn parse_u16(s: &str) -> u16 {
    let bytes = s.as_bytes();
    let mut acc: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b >= b'0' && b <= b'9' {
            acc = acc * 10 + (b - b'0') as u32;
        }
        i += 1;
    }
    acc as u16
}
