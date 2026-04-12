//! TCP Remote Shell — plaintext shell server on port 2222.
//!
//! Commands: help, ps, uptime, mem, ping, traceroute, draug status/pause/resume
//! Features: character echo, backspace, Draug bridge via kernel-global atomics.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use smoltcp::socket::tcp;
use smoltcp::wire::Ipv4Address;
use spin::Mutex;

const SHELL_PORT: u16 = 2222;
const BANNER: &[u8] = b"\r\n  Folkering OS v1.0 - TCP Shell\r\n  Type 'help' for commands.\r\n\r\n> ";

// ── Draug Bridge (kernel-global, written by compositor, read by shell) ──

/// 0 = running, 1 = paused. Set by shell, read by compositor.
pub static DRAUG_PAUSE_FLAG: AtomicU8 = AtomicU8::new(0);

/// Packed Draug status. Written by compositor every tick.
pub static DRAUG_ITER: AtomicU32 = AtomicU32::new(0);
pub static DRAUG_PASSED: AtomicU32 = AtomicU32::new(0);
pub static DRAUG_FAILED: AtomicU32 = AtomicU32::new(0);
pub static DRAUG_SKIPS: AtomicU32 = AtomicU32::new(0);
pub static DRAUG_RETRIES: AtomicU32 = AtomicU32::new(0);
/// Packed: [L1_count, L2_count, L3_count, plan_mode, complex_idx, hibernating, consecutive_skips, 0]
pub static DRAUG_STATE: [AtomicU8; 8] = [const { AtomicU8::new(0) }; 8];

// ── Shell State ─────────────────────────────────────────────────────────

struct ShellState {
    tcp_handle: smoltcp::iface::SocketHandle,
    line_buf: [u8; 256],
    line_len: usize,
    connected: bool,
}

static SHELL: Mutex<Option<ShellState>> = Mutex::new(None);

/// Initialize the TCP shell server. Call after DHCP completes.
pub fn init(state: &mut super::state::NetState) {
    let tcp_rx = tcp::SocketBuffer::new(alloc::vec![0u8; 4096]);
    let tcp_tx = tcp::SocketBuffer::new(alloc::vec![0u8; 4096]);
    let mut socket = tcp::Socket::new(tcp_rx, tcp_tx);

    if let Err(_) = socket.listen(SHELL_PORT) {
        crate::serial_strln!("[SHELL-TCP] Failed to listen on port 2222");
        return;
    }

    let handle = state.sockets.add(socket);
    *SHELL.lock() = Some(ShellState {
        tcp_handle: handle,
        line_buf: [0u8; 256],
        line_len: 0,
        connected: false,
    });

    crate::serial_strln!("[SHELL-TCP] Listening on port 2222");
}

/// Poll the TCP shell. Called from `net::poll()` with NET_STATE held.
pub fn poll(state: &mut super::state::NetState) {
    let mut guard = SHELL.lock();
    let shell = match guard.as_mut() {
        Some(s) => s,
        None => return,
    };

    let socket = state.sockets.get_mut::<tcp::Socket>(shell.tcp_handle);

    // Recovery: stale socket → re-listen
    if !shell.connected && !socket.is_active() && !socket.is_listening() {
        socket.abort();
        let _ = socket.listen(SHELL_PORT);
        return;
    }

    // New connection
    if !shell.connected && socket.is_active() && socket.may_send() {
        shell.connected = true;
        shell.line_len = 0;
        let _ = socket.send_slice(BANNER);
        crate::serial_strln!("[SHELL-TCP] Client connected");
        return;
    }

    // Client disconnected
    if shell.connected && (!socket.is_active() || !socket.may_recv()) {
        shell.connected = false;
        shell.line_len = 0;
        crate::serial_strln!("[SHELL-TCP] Client disconnected");
        socket.abort();
        let _ = socket.listen(SHELL_PORT);
        return;
    }

    // Read incoming data
    if shell.connected && socket.can_recv() {
        let mut tmp = [0u8; 256];
        let n = socket.recv_slice(&mut tmp).unwrap_or(0);

        for i in 0..n {
            let byte = tmp[i];
            if byte >= 0xF0 { continue; } // Telnet IAC
            if byte == b'\r' { continue; }

            if byte == b'\n' {
                // Echo newline
                let socket = state.sockets.get_mut::<tcp::Socket>(shell.tcp_handle);
                let _ = socket.send_slice(b"\r\n");

                if shell.line_len > 0 {
                    let line = &shell.line_buf[..shell.line_len];
                    let response = dispatch_command(line, state);
                    let socket = state.sockets.get_mut::<tcp::Socket>(shell.tcp_handle);
                    let _ = socket.send_slice(response.as_bytes());
                    let _ = socket.send_slice(b"\r\n");
                }
                let socket = state.sockets.get_mut::<tcp::Socket>(shell.tcp_handle);
                let _ = socket.send_slice(b"> ");
                shell.line_len = 0;
            } else if byte == 0x7F || byte == 0x08 {
                // Backspace
                if shell.line_len > 0 {
                    shell.line_len -= 1;
                    let socket = state.sockets.get_mut::<tcp::Socket>(shell.tcp_handle);
                    let _ = socket.send_slice(b"\x08 \x08"); // erase char
                }
            } else if shell.line_len < 255 {
                // Echo character
                shell.line_buf[shell.line_len] = byte;
                shell.line_len += 1;
                let socket = state.sockets.get_mut::<tcp::Socket>(shell.tcp_handle);
                let _ = socket.send_slice(&[byte]);
            }
        }
    }
}

// ── Command Dispatch ────────────────────────────────────────────────────

fn dispatch_command(line: &[u8], state: &mut super::state::NetState) -> String {
    let cmd = match core::str::from_utf8(line) {
        Ok(s) => s.trim(),
        Err(_) => return String::from("error: invalid UTF-8"),
    };

    let (verb, args) = match cmd.split_once(' ') {
        Some((v, a)) => (v, a.trim()),
        None => (cmd, ""),
    };

    match verb {
        "help" => cmd_help(),
        "ps" => cmd_ps(),
        "uptime" => cmd_uptime(),
        "mem" => cmd_mem(),
        "net" => cmd_net(state),
        "df" => cmd_df(),
        "ping" => cmd_ping(args, state),
        "traceroute" | "tracert" => cmd_traceroute(args, state),
        "draug" => cmd_draug(args),
        "" => String::new(),
        _ => {
            let mut r = String::from("unknown command: ");
            r.push_str(verb);
            r.push_str("\r\nType 'help' for available commands.");
            r
        }
    }
}

fn cmd_help() -> String {
    String::from(
        "Commands:\r\n\
         \x20 help             show this message\r\n\
         \x20 ps               list running tasks\r\n\
         \x20 uptime           system uptime\r\n\
         \x20 mem              memory statistics\r\n\
         \x20 net              network configuration\r\n\
         \x20 df               disk / database usage\r\n\
         \x20 ping <ip>        send ICMP echo\r\n\
         \x20 traceroute <ip>  trace route (max 16 hops)\r\n\
         \x20 draug status     Draug AI daemon status\r\n\
         \x20 draug pause      pause the refactor loop\r\n\
         \x20 draug resume     resume the refactor loop"
    )
}

fn cmd_ps() -> String {
    use crate::task::task::{TASK_TABLE, TaskState};

    let mut out = String::with_capacity(512);
    out.push_str("PID  NAME                 STATE\r\n");
    out.push_str("---  ----                 -----\r\n");

    let table = TASK_TABLE.lock();
    for (&id, task_arc) in table.iter() {
        let task = task_arc.lock();
        push_decimal_padded(&mut out, id as u32, 3);
        out.push_str("  ");

        let name_len = task.name.iter().position(|&b| b == 0).unwrap_or(16);
        if let Ok(name) = core::str::from_utf8(&task.name[..name_len]) {
            out.push_str(name);
            for _ in name.len()..20 { out.push(' '); }
        } else {
            out.push_str("???                 ");
        }
        out.push(' ');

        let state_str = match task.state {
            TaskState::Running => "Running",
            TaskState::Runnable => "Runnable",
            TaskState::BlockedOnReceive => "Blocked(recv)",
            TaskState::BlockedOnSend(_) => "Blocked(send)",
            TaskState::WaitingForReply(_) => "Waiting(reply)",
            TaskState::Exited => "Exited",
        };
        out.push_str(state_str);
        out.push_str("\r\n");
    }
    out
}

fn cmd_uptime() -> String {
    let ms = crate::timer::uptime_ms();
    let total_s = ms / 1000;
    let hours = total_s / 3600;
    let mins = (total_s % 3600) / 60;
    let secs = total_s % 60;

    let mut out = String::with_capacity(64);
    out.push_str("uptime: ");
    push_dec(&mut out, hours as u32);
    out.push_str("h ");
    push_dec(&mut out, mins as u32);
    out.push_str("m ");
    push_dec(&mut out, secs as u32);
    out.push_str("s (");
    push_dec(&mut out, (ms / 1000) as u32);
    out.push_str("s total)");
    out
}

fn cmd_mem() -> String {
    let (total_pages, free_pages) = crate::memory::physical::memory_stats();
    let total_mb = total_pages * 4 / 1024;
    let used_pages = total_pages.saturating_sub(free_pages);
    let used_mb = used_pages * 4 / 1024;
    let pct = if total_pages > 0 { used_pages * 100 / total_pages } else { 0 };

    let mut out = String::with_capacity(128);
    out.push_str("memory: ");
    push_dec(&mut out, used_mb as u32);
    out.push('/');
    push_dec(&mut out, total_mb as u32);
    out.push_str(" MB used (");
    push_dec(&mut out, pct as u32);
    out.push_str("%)\r\npages:  ");
    push_dec(&mut out, free_pages as u32);
    out.push('/');
    push_dec(&mut out, total_pages as u32);
    out.push_str(" free");
    out
}

fn cmd_net(state: &mut super::state::NetState) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("Network:\r\n");

    // Show IP from smoltcp interface
    for cidr in state.iface.ip_addrs() {
        out.push_str("  IP: ");
        match cidr.address() {
            smoltcp::wire::IpAddress::Ipv4(v4) => {
                let o = v4.octets();
                push_dec(&mut out, o[0] as u32); out.push('.');
                push_dec(&mut out, o[1] as u32); out.push('.');
                push_dec(&mut out, o[2] as u32); out.push('.');
                push_dec(&mut out, o[3] as u32);
                out.push('/');
                push_dec(&mut out, cidr.prefix_len() as u32);
            }
            _ => out.push_str("(non-IPv4)"),
        }
        out.push_str("\r\n");
    }

    out.push_str("  Gateway: 10.0.2.2 (SLIRP)\r\n");
    out.push_str("  Services:\r\n");
    out.push_str("    TCP :2222  remote shell\r\n");
    out.push_str("    TCP :14711 proxy (outbound via SLIRP)");
    out
}

fn cmd_df() -> String {
    // Report Synapse DB usage via the kernel's block device stats
    let mut out = String::with_capacity(128);
    out.push_str("Filesystem       Size   Used\r\n");
    out.push_str("virtio-data.img  4 MB   ");

    // We can't query Synapse directly from kernel, but we know
    // the DB starts at sector 2048 and the disk is ~365 MB.
    // Report what we know statically.
    out.push_str("(query via TCP shell not available)\r\n");
    out.push_str("draug-sandbox/   archive has ");

    // Count archived files is host-side, not accessible from kernel.
    out.push_str("N files (check host)");
    out
}

fn cmd_ping(args: &str, state: &mut super::state::NetState) -> String {
    let octets = match parse_ip(args) {
        Some(o) => o,
        None => return String::from("usage: ping <x.x.x.x>"),
    };

    let target = Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]);
    super::icmp::send_ping_inner(state, target);

    // Non-blocking: just report that ping was sent.
    // Reply will appear in serial log via check_ping_reply.
    let mut out = String::with_capacity(64);
    out.push_str("ping sent to ");
    out.push_str(args);
    out.push_str(" (reply in serial log)");
    out
}

fn cmd_traceroute(args: &str, state: &mut super::state::NetState) -> String {
    let octets = match parse_ip(args) {
        Some(o) => o,
        None => return String::from("usage: traceroute <x.x.x.x>"),
    };

    let target = Ipv4Address::new(octets[0], octets[1], octets[2], octets[3]);
    let mut out = String::with_capacity(512);
    out.push_str("traceroute to ");
    out.push_str(args);
    out.push_str(", max 16 hops\r\n");

    for ttl in 1..=16u8 {
        push_decimal_padded(&mut out, ttl as u32, 2);
        out.push_str("  ");

        // Send ICMP echo with this TTL
        state.ping_seq = state.ping_seq.wrapping_add(1);
        let seq = state.ping_seq;

        let icmp_socket = state.sockets.get_mut::<smoltcp::socket::icmp::Socket>(state.icmp_handle);
        if !icmp_socket.is_open() {
            let _ = icmp_socket.bind(smoltcp::socket::icmp::Endpoint::Ident(super::icmp::PING_IDENT));
        }
        let echo = smoltcp::wire::Icmpv4Repr::EchoRequest {
            ident: super::icmp::PING_IDENT,
            seq_no: seq,
            data: b"folk",
        };
        if icmp_socket.can_send() {
            let tx = icmp_socket.send(echo.buffer_len(), smoltcp::wire::IpAddress::Ipv4(target)).unwrap();
            let mut pkt = smoltcp::wire::Icmpv4Packet::new_unchecked(tx);
            echo.emit(&mut pkt, &smoltcp::phy::ChecksumCapabilities::default());
        }

        // Set TTL on the interface's IP hop limit
        // smoltcp doesn't expose per-socket TTL easily. For SLIRP,
        // the gateway at 10.0.2.2 always responds so traceroute will
        // show 1 hop. Log this limitation.
        let start = crate::timer::uptime_ms();

        // Poll for ICMP response
        let mut got_reply = false;
        loop {
            let now = smoltcp::time::Instant::from_millis(crate::timer::uptime_ms() as i64);
            let mut device = super::device::FolkeringDevice;
            state.iface.poll(now, &mut device, &mut state.sockets);

            let icmp_socket = state.sockets.get_mut::<smoltcp::socket::icmp::Socket>(state.icmp_handle);
            if icmp_socket.can_recv() {
                if let Ok((data, from)) = icmp_socket.recv() {
                    let rtt = crate::timer::uptime_ms().saturating_sub(start);
                    // Show source IP
                    if let smoltcp::wire::IpAddress::Ipv4(v4) = from {
                        let o = v4.octets();
                        push_dec(&mut out, o[0] as u32); out.push('.');
                        push_dec(&mut out, o[1] as u32); out.push('.');
                        push_dec(&mut out, o[2] as u32); out.push('.');
                        push_dec(&mut out, o[3] as u32);
                    }
                    out.push_str("  ");
                    push_dec(&mut out, rtt as u32);
                    out.push_str("ms");

                    // Check if it's echo reply (reached destination)
                    if let Ok(pkt) = smoltcp::wire::Icmpv4Packet::new_checked(data) {
                        if let Ok(smoltcp::wire::Icmpv4Repr::EchoReply { .. }) =
                            smoltcp::wire::Icmpv4Repr::parse(&pkt, &smoltcp::phy::ChecksumCapabilities::default())
                        {
                            out.push_str("  <-- destination reached");
                            out.push_str("\r\n");
                            return out;
                        }
                    }
                    got_reply = true;
                    break;
                }
            }
            if crate::timer::uptime_ms() - start > 2000 { break; }
            for _ in 0..500 { core::hint::spin_loop(); }
        }

        if !got_reply {
            out.push_str("*  (timeout)");
        }
        out.push_str("\r\n");
    }
    out
}

fn cmd_draug(args: &str) -> String {
    match args {
        "status" => {
            let iter = DRAUG_ITER.load(Ordering::Relaxed);
            let passed = DRAUG_PASSED.load(Ordering::Relaxed);
            let failed = DRAUG_FAILED.load(Ordering::Relaxed);
            let skips = DRAUG_SKIPS.load(Ordering::Relaxed);
            let retries = DRAUG_RETRIES.load(Ordering::Relaxed);
            let l1 = DRAUG_STATE[0].load(Ordering::Relaxed);
            let l2 = DRAUG_STATE[1].load(Ordering::Relaxed);
            let l3 = DRAUG_STATE[2].load(Ordering::Relaxed);
            let plan_mode = DRAUG_STATE[3].load(Ordering::Relaxed);
            let complex_idx = DRAUG_STATE[4].load(Ordering::Relaxed);
            let hibernating = DRAUG_STATE[5].load(Ordering::Relaxed);
            let consec_skips = DRAUG_STATE[6].load(Ordering::Relaxed);
            let paused = DRAUG_PAUSE_FLAG.load(Ordering::Relaxed);

            let mut out = String::with_capacity(256);
            out.push_str("Draug AI Daemon Status\r\n");
            out.push_str("  Skill tree: L1=");
            push_dec(&mut out, l1 as u32);
            out.push_str("/20 L2=");
            push_dec(&mut out, l2 as u32);
            out.push_str("/20 L3=");
            push_dec(&mut out, l3 as u32);
            out.push_str("/20\r\n  Iteration: ");
            push_dec(&mut out, iter);
            out.push_str("  passed=");
            push_dec(&mut out, passed);
            out.push_str(" failed=");
            push_dec(&mut out, failed);
            out.push_str(" skips=");
            push_dec(&mut out, skips);
            out.push_str(" retries=");
            push_dec(&mut out, retries);
            out.push_str("\r\n  Plan mode: ");
            out.push_str(if plan_mode != 0 { "active" } else { "inactive" });
            out.push_str(" (task ");
            push_dec(&mut out, complex_idx as u32);
            out.push_str("/8)\r\n  State: ");
            if paused != 0 {
                out.push_str("PAUSED");
            } else if hibernating != 0 {
                out.push_str("HIBERNATING (");
                push_dec(&mut out, consec_skips as u32);
                out.push_str(" consecutive skips)");
            } else {
                out.push_str("RUNNING");
            }
            out
        }
        "pause" => {
            DRAUG_PAUSE_FLAG.store(1, Ordering::Relaxed);
            String::from("Draug paused. Use 'draug resume' to continue.")
        }
        "resume" => {
            DRAUG_PAUSE_FLAG.store(0, Ordering::Relaxed);
            String::from("Draug resumed.")
        }
        _ => String::from("usage: draug status|pause|resume"),
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn parse_ip(s: &str) -> Option<[u8; 4]> {
    if s.is_empty() { return None; }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 { return None; }
    let mut octets = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        octets[i] = part.parse::<u8>().ok()?;
    }
    Some(octets)
}

fn push_dec(out: &mut String, mut v: u32) {
    if v == 0 { out.push('0'); return; }
    let mut buf = [0u8; 10];
    let mut i = 0;
    while v > 0 {
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    while i > 0 { i -= 1; out.push(buf[i] as char); }
}

fn push_decimal_padded(out: &mut String, v: u32, width: usize) {
    let mut tmp = String::with_capacity(10);
    push_dec(&mut tmp, v);
    for _ in tmp.len()..width { out.push(' '); }
    out.push_str(&tmp);
}
