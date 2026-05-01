//! Build-time configurable proxy address — userspace mirror of
//! `kernel::net::proxy_config`.
//!
//! Userspace tasks that connect to the host-side `folkering-proxy` /
//! Gemini relay (compositor's host_api WebSocket bring-up,
//! draug-daemon's PATCH/LLM TCP path) read these constants instead
//! of hardcoding the demo address. Same `FOLKERING_PROXY_IP` /
//! `FOLKERING_PROXY_PORT` env vars compile into both crates so kernel
//! and userspace agree.
//!
//! Default is the SLIRP `[10, 0, 2, 2]:14711` so local QEMU runs work
//! out of the box; set `FOLKERING_PROXY_IP=192.168.68.150` for the
//! Proxmox / bridged-LAN demo.

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
