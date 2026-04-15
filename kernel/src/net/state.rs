//! Shared smoltcp interface state.
//!
//! `NetState` wraps the smoltcp `Interface`, `SocketSet`, and the various
//! socket handles (DHCP/ICMP/DNS) used by the rest of the network stack.

extern crate alloc;

use spin::Mutex;
use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::socket::dns;

pub(crate) struct NetState {
    pub(crate) iface: Interface,
    pub(crate) sockets: SocketSet<'static>,
    pub(crate) dhcp_handle: SocketHandle,
    pub(crate) icmp_handle: SocketHandle,
    pub(crate) dns_handle: SocketHandle,
    pub(crate) has_ip: bool,
    pub(crate) ping_seq: u16,
    pub(crate) ping_send_at: Option<u64>,
    pub(crate) auto_ping_done: bool,
    // Async DNS auto-test state
    pub(crate) auto_dns_started: bool,
    pub(crate) auto_dns_query: Option<dns::QueryHandle>,
    // HTTPS auto-test
    pub(crate) auto_https_done: bool,
}

pub(crate) static NET_STATE: Mutex<Option<NetState>> = Mutex::new(None);
