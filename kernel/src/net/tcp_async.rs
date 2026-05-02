//! Non-blocking TCP syscalls for Draug's async state machine.
//!
//! Instead of one blocking `tcp_request()` that holds the compositor
//! for 3-80 seconds, these syscalls return EAGAIN immediately if the
//! operation isn't complete. The compositor polls them every frame.
//!
//! Protocol:
//!   1. sys_tcp_connect(ip, port) → handle (or EAGAIN while connecting)
//!   2. sys_tcp_send(handle, ptr, len) → bytes_sent (or EAGAIN)
//!   3. sys_tcp_poll_recv(handle, ptr, max) → bytes_read (or EAGAIN)
//!   4. sys_tcp_close(handle) → frees socket
//!
//! The EAGAIN value is 0xFFFF_FFFE (distinct from 0xFFFF_FFFF = error).
//! iface.poll() is called on every syscall to drive the TCP state machine.

use core::sync::atomic::{AtomicU64, Ordering};

use smoltcp::socket::tcp;
use smoltcp::wire::{IpAddress, Ipv4Address};
use smoltcp::time::Instant;
use spin::Mutex;

use super::device::FolkeringDevice;
use super::state::NET_STATE;
use super::tls::{next_port, tsc_ms};

/// Return value meaning "not ready, try again next frame"
pub const EAGAIN: u64 = 0xFFFF_FFFE;

/// Maximum concurrent async TCP connections
const MAX_ASYNC_SLOTS: usize = 4;

/// Slot states
enum SlotState {
    Free,
    Connecting,
    Connected,
}

struct AsyncSlot {
    state: SlotState,
    handle: Option<smoltcp::iface::SocketHandle>,
    /// Task that owns this slot, for cleanup on task exit. `0` means
    /// "unowned" (slot is Free or was allocated before ownership
    /// tracking landed — legacy path).
    owner: u32,
    /// Wall-clock timestamp (ms via `tsc_ms`) when this slot last
    /// transitioned into `Connecting`. Used by the stuck-handshake
    /// diagnostic in `syscall_tcp_send` to throttle a one-line log
    /// per slot per stuck-window — see Issue #99 for the symptom
    /// (Draug-async TIMEOUT after 90s in Sending). `0` when the slot
    /// is Free or has already been promoted past Connecting.
    connecting_since_ms: u64,
}

static SLOTS: Mutex<[AsyncSlot; MAX_ASYNC_SLOTS]> = Mutex::new([
    AsyncSlot { state: SlotState::Free, handle: None, owner: 0, connecting_since_ms: 0 },
    AsyncSlot { state: SlotState::Free, handle: None, owner: 0, connecting_since_ms: 0 },
    AsyncSlot { state: SlotState::Free, handle: None, owner: 0, connecting_since_ms: 0 },
    AsyncSlot { state: SlotState::Free, handle: None, owner: 0, connecting_since_ms: 0 },
]);

/// Issue #99 diagnostic: last wall-clock ms we logged a stuck slot,
/// per slot. Throttle to one entry per 5 s so a 90 s timeout window
/// produces ~18 lines on the serial — enough to see the pattern,
/// not enough to drown anything else.
static LAST_STUCK_LOG_MS: [AtomicU64; MAX_ASYNC_SLOTS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Print one line describing why slot `slot_id` is still Connecting,
/// reading state straight off the smoltcp socket. Throttled to one
/// log per slot per 5 s. Caller must already hold `state.sockets`
/// borrows in a sane shape (we take a fresh `&` borrow here).
fn log_stuck_connecting(
    slot_id: usize,
    slot: &AsyncSlot,
    socket: &tcp::Socket,
    now_ms: u64,
) {
    let last = LAST_STUCK_LOG_MS[slot_id].load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < 5_000 {
        return;
    }
    LAST_STUCK_LOG_MS[slot_id].store(now_ms, Ordering::Relaxed);

    let elapsed = now_ms.saturating_sub(slot.connecting_since_ms);

    crate::serial_str!("[TCP_ASYNC] slot ");
    crate::drivers::serial::write_dec(slot_id as u32);
    crate::serial_str!(" stuck Connecting for ");
    crate::drivers::serial::write_dec((elapsed / 1000) as u32);
    crate::serial_str!("s (owner=");
    crate::drivers::serial::write_dec(slot.owner);
    crate::serial_str!(", state=");
    crate::serial_str!(state_name(socket.state()));
    crate::serial_str!(", may_send=");
    crate::serial_str!(if socket.may_send() { "1" } else { "0" });
    crate::serial_str!(", can_send=");
    crate::serial_str!(if socket.can_send() { "1" } else { "0" });
    crate::serial_str!(", is_active=");
    crate::serial_str!(if socket.is_active() { "1" } else { "0" });
    if let Some(remote) = socket.remote_endpoint() {
        crate::serial_str!(", remote=");
        crate::drivers::serial::write_dec(remote.port as u32);
    }
    if let Some(local) = socket.local_endpoint() {
        crate::serial_str!(", local_port=");
        crate::drivers::serial::write_dec(local.port as u32);
    }
    crate::serial_strln!(")");
}

fn state_name(s: tcp::State) -> &'static str {
    use smoltcp::socket::tcp::State;
    match s {
        State::Closed => "Closed",
        State::Listen => "Listen",
        State::SynSent => "SynSent",
        State::SynReceived => "SynReceived",
        State::Established => "Established",
        State::FinWait1 => "FinWait1",
        State::FinWait2 => "FinWait2",
        State::CloseWait => "CloseWait",
        State::Closing => "Closing",
        State::LastAck => "LastAck",
        State::TimeWait => "TimeWait",
    }
}

/// Create — or re-poll — a non-blocking TCP connection.
///
/// **Idempotent on `(ip, port, owner)`:** calling this syscall
/// repeatedly with the same destination from the same task returns
/// the same slot id each time. First call allocates, starts the
/// SYN handshake, returns the slot (state = Connecting). Subsequent
/// calls:
///   * while still handshaking → `EAGAIN`
///   * once `socket.may_send()` → slot promoted to Connected, return
///     same slot id
///   * when the socket has failed → slot freed, return `u64::MAX`
///
/// Only when no matching slot exists for this caller's (ip, port)
/// does the syscall allocate a fresh slot. This lets high-level
/// wrappers (e.g. a synchronous `TcpSession::connect`) poll via
/// `tcp_connect_async` without risk of silently starting a second
/// connection to the same destination — the previous version, which
/// only inspected `SlotState::Connecting` slots, fell through to
/// the "allocate new" path once a slot had been promoted to
/// `Connected`, giving the caller a brand-new (unrelated) slot id
/// and leaving the original connection orphaned.
pub fn syscall_tcp_connect(ip_packed: u64, port: u64) -> u64 {
    let ip = [
        ((ip_packed >> 24) & 0xFF) as u8,
        ((ip_packed >> 16) & 0xFF) as u8,
        ((ip_packed >> 8) & 0xFF) as u8,
        (ip_packed & 0xFF) as u8,
    ];
    let target_ip = IpAddress::Ipv4(Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]));
    let target_port = port as u16;
    let current_task = crate::task::task::get_current_task();

    let mut guard = match NET_STATE.try_lock() {
        Some(g) => g,
        None => return EAGAIN,
    };
    let state = match guard.as_mut() {
        Some(s) if s.has_ip => s,
        _ => return u64::MAX,
    };

    // Drive network stack
    let now = Instant::from_millis(tsc_ms());
    let mut device = FolkeringDevice;
    state.iface.poll(now, &mut device, &mut state.sockets);
    super::tcp_shell::poll(state);

    let mut slots = SLOTS.lock();

    // First pass: look for an existing slot owned by this task that
    // targets (target_ip, target_port). Matches on the smoltcp
    // socket's remote endpoint so multiple concurrent connections
    // to different destinations don't step on each other.
    for i in 0..MAX_ASYNC_SLOTS {
        // Filter cheap fields before touching the smoltcp socket.
        if slots[i].owner != current_task {
            continue;
        }
        if matches!(slots[i].state, SlotState::Free) {
            continue;
        }
        let h = match slots[i].handle {
            Some(h) => h,
            None => continue,
        };
        // Endpoint check — immutable borrow of sockets, released
        // before we re-borrow as mut below.
        let matches_remote = {
            let socket = state.sockets.get::<tcp::Socket>(h);
            match socket.remote_endpoint() {
                Some(ep) => ep.addr == target_ip && ep.port == target_port,
                None => false,
            }
        };
        if !matches_remote {
            continue;
        }
        // Matching slot. Drive its state and return.
        let socket = state.sockets.get_mut::<tcp::Socket>(h);
        if socket.may_send() {
            slots[i].state = SlotState::Connected;
            slots[i].connecting_since_ms = 0;
            return i as u64;
        }
        if !socket.is_active() {
            // Handshake failed (timeout, RST) — clean up and signal
            // the caller. They can retry by calling us again, which
            // will allocate a fresh slot since the entry is now Free.
            state.sockets.remove(h);
            slots[i].state = SlotState::Free;
            slots[i].handle = None;
            slots[i].connecting_since_ms = 0;
            return u64::MAX;
        }
        return EAGAIN;
    }

    // Second pass: no matching slot — allocate a fresh one from the
    // first Free entry.
    let free_slot = (0..MAX_ASYNC_SLOTS)
        .find(|&i| matches!(slots[i].state, SlotState::Free));
    let slot_idx = match free_slot {
        Some(i) => i,
        None => {
            // Issue #58 instrumentation: dump slot-pool census so we
            // can see the pool exhaustion pattern in the serial log.
            //
            // PR #64 review: snapshot the slot states first, then drop
            // both NET_STATE and SLOTS locks before serialising the
            // log. COM1 writes disable interrupts — keeping the locks
            // across ~16 serial writes turned a small lock contention
            // into a long critical section that could mask the wedge
            // we were trying to diagnose.
            let mut census: [(u8, u32); MAX_ASYNC_SLOTS] =
                [(0u8, 0u32); MAX_ASYNC_SLOTS];
            for i in 0..MAX_ASYNC_SLOTS {
                let code = match slots[i].state {
                    SlotState::Free => 0u8,
                    SlotState::Connecting => 1u8,
                    SlotState::Connected => 2u8,
                };
                census[i] = (code, slots[i].owner);
            }
            drop(slots);
            drop(guard);

            crate::serial_strln!("[TCP_CONNECT] no free slots! pool census:");
            for i in 0..MAX_ASYNC_SLOTS {
                crate::serial_str!("[TCP_CONNECT]   slot[");
                crate::drivers::serial::write_dec(i as u32);
                crate::serial_str!("] state=");
                let label = match census[i].0 {
                    0 => "Free",
                    1 => "Connecting",
                    _ => "Connected",
                };
                crate::serial_str!(label);
                crate::serial_str!(" owner=");
                crate::drivers::serial::write_dec(census[i].1);
                crate::serial_strln!("");
            }
            return u64::MAX; // no free slots
        }
    };

    // Issue #99 fix candidate: match tcp_plain.rs's 65536/8192. The
    // earlier 8192/4096 was the smallest of any TCP path in the
    // codebase — and tcp_plain.rs (which works) goes 8x bigger on RX.
    // Smoltcp's SYN handshake bookkeeping may need more headroom than
    // the tiny SYN packet itself; bumping eliminates that as a source
    // of the SynSent-forever symptom.
    let tcp_rx = tcp::SocketBuffer::new(alloc::vec![0u8; 65536]);
    let tcp_tx = tcp::SocketBuffer::new(alloc::vec![0u8; 8192]);
    let tcp_socket = tcp::Socket::new(tcp_rx, tcp_tx);
    let tcp_handle = state.sockets.add(tcp_socket);

    // Enable interrupts for VirtIO-net
    unsafe { core::arch::asm!("sti"); }

    let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
    if socket.connect(state.iface.context(), (target_ip, target_port), next_port()).is_err() {
        state.sockets.remove(tcp_handle);
        return u64::MAX;
    }

    slots[slot_idx].state = SlotState::Connecting;
    slots[slot_idx].handle = Some(tcp_handle);
    slots[slot_idx].owner = current_task;
    slots[slot_idx].connecting_since_ms = tsc_ms() as u64;
    LAST_STUCK_LOG_MS[slot_idx].store(0, Ordering::Relaxed);

    // Return slot_id immediately. Connection completes asynchronously.
    // Subsequent polls on the same (ip, port) will return this slot
    // until it's closed (or until a peer-side reset frees it).
    slot_idx as u64
}

/// Basic pointer sanity check: non-null, reasonable length, no wraparound.
/// Not a full user/kernel boundary check (everything runs in ring 0),
/// but catches null pointers and wild lengths that would page-fault.
#[inline]
fn validate_user_ptr(ptr: u64, len: u64) -> bool {
    if ptr == 0 || len > 1024 * 1024 {
        return false; // null or > 1MB
    }
    // Check that ptr + len doesn't wrap around
    (ptr as usize).checked_add(len as usize).is_some()
}

/// Non-blocking send. Returns bytes written, EAGAIN, or MAX on error.
pub fn syscall_tcp_send(slot_id: u64, data_ptr: u64, data_len: u64) -> u64 {
    if slot_id as usize >= MAX_ASYNC_SLOTS { return u64::MAX; }
    if data_len > 0 && !validate_user_ptr(data_ptr, data_len) { return u64::MAX; }

    let mut guard = match NET_STATE.try_lock() {
        Some(g) => g,
        None => return EAGAIN,
    };
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return u64::MAX,
    };

    // Poll network
    let now = Instant::from_millis(tsc_ms());
    let mut device = FolkeringDevice;
    state.iface.poll(now, &mut device, &mut state.sockets);
    super::tcp_shell::poll(state);

    let mut slots = SLOTS.lock();
    let slot = &mut slots[slot_id as usize];

    // Auto-promote Connecting → Connected if handshake done
    if let (SlotState::Connecting, Some(h)) = (&slot.state, slot.handle) {
        let socket = state.sockets.get_mut::<tcp::Socket>(h);
        if socket.may_send() {
            slot.state = SlotState::Connected;
            slot.connecting_since_ms = 0;
        } else if !socket.is_active() {
            state.sockets.remove(h);
            slot.state = SlotState::Free;
            slot.handle = None;
            slot.connecting_since_ms = 0;
            return u64::MAX;
        } else {
            // Issue #99 diagnostic: surface the stuck handshake on
            // serial so we can tell SynSent vs CloseWait vs other
            // without re-running with a debug build.
            log_stuck_connecting(slot_id as usize, slot, &*socket, tsc_ms() as u64);
            return EAGAIN; // still connecting
        }
    }

    let handle = match (&slot.state, slot.handle) {
        (SlotState::Connected, Some(h)) => h,
        _ => return u64::MAX,
    };

    let socket = state.sockets.get_mut::<tcp::Socket>(handle);
    if !socket.can_send() {
        return EAGAIN;
    }

    let data = unsafe {
        core::slice::from_raw_parts(data_ptr as *const u8, data_len as usize)
    };

    match socket.send_slice(data) {
        Ok(n) => n as u64,
        Err(_) => u64::MAX,
    }
}

/// Non-blocking receive. Returns bytes read, EAGAIN, or MAX on error.
/// Returns 0 when peer has closed and all data is drained.
pub fn syscall_tcp_poll_recv(slot_id: u64, buf_ptr: u64, buf_max: u64) -> u64 {
    if slot_id as usize >= MAX_ASYNC_SLOTS { return u64::MAX; }
    if buf_max > 0 && !validate_user_ptr(buf_ptr, buf_max) { return u64::MAX; }

    let mut guard = match NET_STATE.try_lock() {
        Some(g) => g,
        None => return EAGAIN,
    };
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return u64::MAX,
    };

    // Poll network
    let now = Instant::from_millis(tsc_ms());
    let mut device = FolkeringDevice;
    state.iface.poll(now, &mut device, &mut state.sockets);
    super::tcp_shell::poll(state);

    let slots = SLOTS.lock();
    let slot = &slots[slot_id as usize];

    let handle = match (&slot.state, slot.handle) {
        (SlotState::Connected, Some(h)) => h,
        _ => return u64::MAX,
    };

    let socket = state.sockets.get_mut::<tcp::Socket>(handle);

    if socket.can_recv() {
        let buf = unsafe {
            core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_max as usize)
        };
        match socket.recv_slice(buf) {
            Ok(n) => n as u64,
            Err(_) => u64::MAX,
        }
    } else if !socket.may_recv() {
        // Peer closed and buffer drained
        0
    } else {
        EAGAIN // no data yet, try next frame
    }
}

/// Close and free a TCP connection.
pub fn syscall_tcp_close(slot_id: u64) -> u64 {
    if slot_id as usize >= MAX_ASYNC_SLOTS { return u64::MAX; }

    // Issue #58 hypothesis #2: previously returned EAGAIN whenever
    // NET_STATE was already held, causing the slot to NEVER be freed.
    // With MAX_ASYNC_SLOTS = 4, after 4 contended close attempts the
    // pool is exhausted and Phase 17 can never connect again —
    // exactly the post-flood wedge.
    //
    // Note: timer::tick() does NOT poll the network stack — that ran
    // in earlier prototypes but was moved off the timer ISR. Real
    // contention sources today are `net::poll()` from the idle
    // syscall, other TCP syscalls calling `iface.poll()`, and (once
    // SMP lands) work on another core.
    //
    // Fix: retry the lock for up to 1000 short spins (~few µs at
    // 3 GHz) before giving up. Even under sustained NET_STATE
    // contention this typically wins on the first or second retry.
    let mut attempts = 0u32;
    let mut guard = loop {
        if let Some(g) = NET_STATE.try_lock() { break g; }
        attempts += 1;
        if attempts > 1000 {
            crate::serial_strln!("[TCP_CLOSE] NET_STATE locked after 1000 spins — slot NOT freed");
            return EAGAIN;
        }
        core::hint::spin_loop();
    };
    if attempts > 0 {
        crate::serial_str!("[TCP_CLOSE] won lock after ");
        crate::drivers::serial::write_dec(attempts);
        crate::serial_strln!(" spins");
    }
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return u64::MAX,
    };

    let mut slots = SLOTS.lock();
    let slot = &mut slots[slot_id as usize];

    if let Some(h) = slot.handle.take() {
        let socket = state.sockets.get_mut::<tcp::Socket>(h);
        socket.abort();
        state.sockets.remove(h);
    }
    slot.state = SlotState::Free;
    slot.owner = 0;
    0
}

/// Free every async TCP slot owned by `task_id`. Called from
/// `syscall_exit` so a crashed or dying task doesn't leak slots —
/// without this, after 4 exits of tasks that opened TCP connections
/// the pool is exhausted and new `tcp_connect` calls fail.
pub fn free_task_slots(task_id: u32) {
    let mut guard = match NET_STATE.try_lock() {
        Some(g) => g,
        None => return, // best-effort — if net state is busy, skip
    };
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return,
    };
    let mut slots = SLOTS.lock();
    for slot in slots.iter_mut() {
        if slot.owner != task_id { continue; }
        if let Some(h) = slot.handle.take() {
            let socket = state.sockets.get_mut::<tcp::Socket>(h);
            socket.abort();
            state.sockets.remove(h);
        }
        slot.state = SlotState::Free;
        slot.owner = 0;
    }
}

extern crate alloc;
