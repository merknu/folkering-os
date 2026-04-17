//! TCP Remote Shell — plaintext shell server on port 2222.
//!
//! Commands: help, ps, uptime, mem, net, df, ping, draug status/pause/resume
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
pub static DRAUG_RETRIES: AtomicU32 = AtomicU32::new(0);
/// [L1_count, L2_count, L3_count, plan_mode, complex_idx, hibernating, consecutive_skips, 0]
pub static DRAUG_STATE: [AtomicU8; 8] = [const { AtomicU8::new(0) }; 8];
/// Current task name (written by compositor, 31 bytes + NUL)
pub static DRAUG_CURRENT_TASK: spin::Mutex<[u8; 32]> = spin::Mutex::new([0u8; 32]);

// ── Shell State ─────────────────────────────────────────────────────────

struct ShellState {
    tcp_handle: smoltcp::iface::SocketHandle,
    line_buf: [u8; 256],
    line_len: usize,
    connected: bool,
    /// Uptime (ms) when client connected — for idle timeout.
    connected_at_ms: u64,
    /// Uptime (ms) of last received byte — for idle timeout.
    last_recv_ms: u64,
    /// True if line_buf is full (bytes being dropped).
    overflow: bool,
}

/// Idle client timeout: 5 minutes. Frees socket for other users.
const CLIENT_IDLE_TIMEOUT_MS: u64 = 300_000;

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
        connected_at_ms: 0,
        last_recv_ms: 0,
        overflow: false,
    });

    crate::serial_strln!("[SHELL-TCP] Listening on port 2222");
}

/// Poll the TCP shell. Called from `net::poll()` with NET_STATE held.
pub fn poll(state: &mut super::state::NetState) {
    // try_lock: avoid spinning if held (e.g. recursive call from tcp_plain)
    let mut guard = match SHELL.try_lock() {
        Some(g) => g,
        None => return,
    };
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
        shell.overflow = false;
        let now = crate::timer::uptime_ms();
        shell.connected_at_ms = now;
        shell.last_recv_ms = now;
        let _ = socket.send_slice(BANNER);
        crate::serial_strln!("[SHELL-TCP] Client connected");
        return;
    }

    // Idle timeout: disconnect clients that send nothing for 5 minutes
    if shell.connected {
        let now = crate::timer::uptime_ms();
        if now.saturating_sub(shell.last_recv_ms) > CLIENT_IDLE_TIMEOUT_MS {
            crate::serial_strln!("[SHELL-TCP] Client idle timeout — disconnecting");
            shell.connected = false;
            shell.line_len = 0;
            socket.abort();
            let _ = socket.listen(SHELL_PORT);
            return;
        }
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
        let n = match socket.recv_slice(&mut tmp) {
            Ok(n) => n,
            Err(_) => {
                // Recv error — disconnect client cleanly
                crate::serial_strln!("[SHELL-TCP] recv error — disconnecting");
                shell.connected = false;
                shell.line_len = 0;
                socket.abort();
                let _ = socket.listen(SHELL_PORT);
                return;
            }
        };

        if n > 0 {
            shell.last_recv_ms = crate::timer::uptime_ms();
        }

        for i in 0..n {
            let byte = tmp[i];
            if byte >= 0xF0 { continue; } // Telnet IAC
            if byte == b'\r' { continue; }

            if byte == b'\n' {
                let socket = state.sockets.get_mut::<tcp::Socket>(shell.tcp_handle);
                let _ = socket.send_slice(b"\r\n");

                if shell.overflow {
                    // Line was truncated — warn user
                    let socket = state.sockets.get_mut::<tcp::Socket>(shell.tcp_handle);
                    let _ = socket.send_slice(b"(line truncated at 255 bytes)\r\n");
                    shell.overflow = false;
                }

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
                if shell.line_len > 0 {
                    shell.line_len -= 1;
                    shell.overflow = false;
                    let socket = state.sockets.get_mut::<tcp::Socket>(shell.tcp_handle);
                    let _ = socket.send_slice(b"\x08 \x08");
                }
            } else if shell.line_len < 255 {
                shell.line_buf[shell.line_len] = byte;
                shell.line_len += 1;
                let socket = state.sockets.get_mut::<tcp::Socket>(shell.tcp_handle);
                let _ = socket.send_slice(&[byte]);
            } else {
                // Buffer full — silently drop but mark overflow
                shell.overflow = true;
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
        "jit" => cmd_jit(args),
        "draug" => cmd_draug(args),
        "clear" => String::from("\x1b[2J\x1b[H"),
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
         \x20 df               disk usage\r\n\
         \x20 ping <ip>        send ICMP echo\r\n\
         \x20 jit <wasm> [<data>] <ip>  JIT a ramdisk WASM → run on Pi\r\n\
         \x20 jit <ip>         (legacy) run built-in MLP demo\r\n\
         \x20 clear            clear screen\r\n\
         \x20 draug status     AI daemon status + current task\r\n\
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
    let mut out = String::with_capacity(384);
    out.push_str("Filesystem          Size       Used       Notes\r\n");
    out.push_str("---                 ----       ----       -----\r\n");
    // Physical memory stats (real data)
    let (total_pages, free_pages) = crate::memory::physical::memory_stats();
    let total_mb = (total_pages * 4) / 1024;
    let used_mb = ((total_pages - free_pages) * 4) / 1024;
    out.push_str("physical RAM        ");
    push_dec(&mut out, total_mb as u32);
    out.push_str(" MB    ");
    push_dec(&mut out, used_mb as u32);
    out.push_str(" MB    kernel + userspace\r\n");
    // Static info for disk (kernel has no live disk usage stats yet)
    out.push_str("virtio-data.img     365 MB     ~368 MB  model + synapse DB\r\n");
    out.push_str("draug archives      host       -        ~/folkering/draug-sandbox/archive/");
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

/// Push a signed i32 as decimal text into `out`. Reuses the
/// existing unsigned `push_dec` helper defined later in this file.
fn push_dec_signed(out: &mut String, val: i32) {
    if val < 0 {
        out.push('-');
        push_dec(out, (val as i64).unsigned_abs() as u32);
    } else {
        push_dec(out, val as u32);
    }
}

const USAGE_JIT: &str =
    "usage: jit <wasm-name> [<data-name>] <pi-ip>\r\n  \
     wasm-name and data-name must be present in the bundled ramdisk.\r\n  \
     example: jit attention.wasm weights.bin 192.168.68.72\r\n  \
     legacy:  jit <pi-ip>   (runs the built-in MLP demo)";

/// Look up a file in the FPK ramdisk by name and return its bytes.
fn ramdisk_read(name: &str) -> Option<&'static [u8]> {
    let rd = crate::fs::ramdisk()?;
    let entry = rd.find(name)?;
    Some(rd.read(entry))
}

fn cmd_jit(args: &str) -> String {
    let trimmed = args.trim();

    // Legacy form: just an IP → run hardcoded MLP. Kept so existing
    // smoke tests keep working without a ramdisk WASM file.
    if let Some(ip) = parse_ip(trimmed) {
        return cmd_jit_mlp_legacy(ip);
    }

    // Generic form: <wasm-name> [<data-name>] <ip>
    let mut tokens = trimmed.split_ascii_whitespace();
    let wasm_name = match tokens.next() { Some(t) => t, None => return String::from(USAGE_JIT) };
    let mid = tokens.next();
    let last = tokens.next();
    if tokens.next().is_some() {
        return String::from(USAGE_JIT);
    }

    let (data_name, ip_str): (Option<&str>, &str) = match (mid, last) {
        (Some(ip_str), None) => (None, ip_str),
        (Some(data), Some(ip_str)) => (Some(data), ip_str),
        _ => return String::from(USAGE_JIT),
    };

    let ip = match parse_ip(ip_str) {
        Some(o) => o,
        None => return String::from(USAGE_JIT),
    };

    let wasm_bytes = match ramdisk_read(wasm_name) {
        Some(b) => b,
        None => {
            let mut e = String::from("error: wasm file not found in ramdisk: ");
            e.push_str(wasm_name);
            return e;
        }
    };

    let data_bytes = match data_name {
        Some(name) => match ramdisk_read(name) {
            Some(b) => Some(b),
            None => {
                let mut e = String::from("error: data file not found in ramdisk: ");
                e.push_str(name);
                return e;
            }
        },
        None => None,
    };

    let mut out = String::with_capacity(256);
    out.push_str("JIT: ");
    out.push_str(wasm_name);
    out.push_str(" (");
    push_dec(&mut out, wasm_bytes.len() as u32);
    out.push_str(" B");
    if let Some(name) = data_name {
        out.push_str(", data=");
        out.push_str(name);
        out.push_str(" (");
        push_dec(&mut out, data_bytes.unwrap().len() as u32);
        out.push_str(" B)");
    }
    out.push_str(") → ");
    out.push_str(ip_str);
    out.push_str("\r\n");

    match crate::jit::jit_run_wasm(
        wasm_bytes,
        data_bytes,
        crate::jit::DEFAULT_DATA_BASE,
        ip,
        7700,
    ) {
        Ok(result) => {
            out.push_str("Pi result: ");
            push_dec_signed(&mut out, result.exit_code);
            out.push_str("\r\nCode: ");
            push_dec(&mut out, result.code_bytes as u32);
            out.push_str(" B AArch64, compile ~");
            push_dec(&mut out, result.compile_us as u32);
            out.push_str(" us");
        }
        Err(e) => {
            out.push_str("error: ");
            out.push_str(e);
        }
    }
    out
}

/// Backwards-compatible MLP demo: kept callable as `jit <ip>` (no
/// wasm path). New work should go through the generic `jit_run_wasm`
/// pathway by packaging its module + data into the ramdisk.
fn cmd_jit_mlp_legacy(ip: [u8; 4]) -> String {
    let mut out = String::with_capacity(128);
    out.push_str("JIT-compiling built-in MLP (4→4→4→1) to AArch64...\r\n");
    match crate::jit::run_mlp_on_pi(ip, 7700) {
        Ok(result) => {
            out.push_str("Pi result: ");
            push_dec_signed(&mut out, result.exit_code);
            out.push_str("\r\nCode: ");
            push_dec(&mut out, result.code_bytes as u32);
            out.push_str(" B AArch64");
        }
        Err(e) => {
            out.push_str("error: ");
            out.push_str(e);
        }
    }
    out
}

fn cmd_draug(args: &str) -> String {
    match args {
        "status" => {
            // Acquire: synchronize with compositor's Release stores
            let iter = DRAUG_ITER.load(Ordering::Acquire);
            let passed = DRAUG_PASSED.load(Ordering::Acquire);
            let failed = DRAUG_FAILED.load(Ordering::Acquire);
            let retries = DRAUG_RETRIES.load(Ordering::Acquire);
            let l1 = DRAUG_STATE[0].load(Ordering::Acquire);
            let l2 = DRAUG_STATE[1].load(Ordering::Acquire);
            let l3 = DRAUG_STATE[2].load(Ordering::Acquire);
            let plan_mode = DRAUG_STATE[3].load(Ordering::Acquire);
            let complex_idx = DRAUG_STATE[4].load(Ordering::Acquire);
            let hibernating = DRAUG_STATE[5].load(Ordering::Acquire);
            let consec_skips = DRAUG_STATE[6].load(Ordering::Acquire);
            let paused = DRAUG_PAUSE_FLAG.load(Ordering::Acquire);

            let mut out = String::with_capacity(512);
            out.push_str("Draug AI Daemon\r\n");

            // State line
            out.push_str("  State: ");
            if paused != 0 {
                out.push_str("\x1b[33mPAUSED\x1b[0m");
            } else if hibernating != 0 {
                out.push_str("\x1b[31mHIBERNATING\x1b[0m (");
                push_dec(&mut out, consec_skips as u32);
                out.push_str(" skips)");
            } else {
                out.push_str("\x1b[32mRUNNING\x1b[0m");
            }

            // Current task
            out.push_str("\r\n  Current: ");
            {
                let task_buf = DRAUG_CURRENT_TASK.lock();
                let len = task_buf.iter().position(|&b| b == 0).unwrap_or(32);
                if len > 0 {
                    if let Ok(s) = core::str::from_utf8(&task_buf[..len]) {
                        out.push_str(s);
                    }
                } else {
                    out.push_str("(idle)");
                }
            }

            out.push_str("\r\n  Skill tree: L1=");
            push_dec(&mut out, l1 as u32);
            out.push_str("/20 L2=");
            push_dec(&mut out, l2 as u32);
            out.push_str("/20 L3=");
            push_dec(&mut out, l3 as u32);
            out.push_str("/20");

            if plan_mode != 0 {
                out.push_str("\r\n  Plan mode: task ");
                push_dec(&mut out, complex_idx as u32);
                out.push_str("/8");
            }

            out.push_str("\r\n  Stats: iter=");
            push_dec(&mut out, iter);
            out.push_str(" pass=");
            push_dec(&mut out, passed);
            out.push_str(" fail=");
            push_dec(&mut out, failed);
            out.push_str(" skip=");
            push_dec(&mut out, consec_skips as u32);
            out.push_str(" retry=");
            push_dec(&mut out, retries);

            // Success rate
            let total = passed + failed;
            if total > 0 {
                let rate = passed * 100 / total;
                out.push_str("\r\n  Success rate: ");
                push_dec(&mut out, rate);
                out.push_str("%");
            }
            out
        }
        "pause" => {
            DRAUG_PAUSE_FLAG.store(1, Ordering::Release);
            String::from("Draug paused. Use 'draug resume' to continue.")
        }
        "resume" => {
            DRAUG_PAUSE_FLAG.store(0, Ordering::Release);
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
