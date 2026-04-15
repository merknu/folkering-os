//! DNS resolution via smoltcp.

extern crate alloc;

use smoltcp::socket::dns;
use smoltcp::time::Instant;
use smoltcp::wire::{DnsQueryType, IpAddress};

use super::state::NET_STATE;
use super::device::FolkeringDevice;
use super::print_ipv4;

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
