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
}

static SLOTS: Mutex<[AsyncSlot; MAX_ASYNC_SLOTS]> = Mutex::new([
    AsyncSlot { state: SlotState::Free, handle: None },
    AsyncSlot { state: SlotState::Free, handle: None },
    AsyncSlot { state: SlotState::Free, handle: None },
    AsyncSlot { state: SlotState::Free, handle: None },
]);

/// Create a non-blocking TCP connection.
///
/// Returns slot_id (0-3) on success, EAGAIN if connecting, MAX on error.
pub fn syscall_tcp_connect(ip_packed: u64, port: u64) -> u64 {
    let ip = [
        ((ip_packed >> 24) & 0xFF) as u8,
        ((ip_packed >> 16) & 0xFF) as u8,
        ((ip_packed >> 8) & 0xFF) as u8,
        (ip_packed & 0xFF) as u8,
    ];

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

    // Find a free slot or check existing connecting slot
    let mut free_slot = None;
    for (i, slot) in slots.iter_mut().enumerate() {
        match slot.state {
            SlotState::Connecting => {
                // Check if connection completed
                if let Some(h) = slot.handle {
                    let socket = state.sockets.get_mut::<tcp::Socket>(h);
                    if socket.may_send() {
                        slot.state = SlotState::Connected;
                        return i as u64;
                    }
                    if !socket.is_active() {
                        // Connection failed
                        state.sockets.remove(h);
                        slot.state = SlotState::Free;
                        slot.handle = None;
                        return u64::MAX;
                    }
                    return EAGAIN; // still connecting
                }
            }
            SlotState::Free if free_slot.is_none() => {
                free_slot = Some(i);
            }
            _ => {}
        }
    }

    // Allocate new connection
    let slot_idx = match free_slot {
        Some(i) => i,
        None => return u64::MAX, // no free slots
    };

    let tcp_rx = tcp::SocketBuffer::new(alloc::vec![0u8; 8192]);
    let tcp_tx = tcp::SocketBuffer::new(alloc::vec![0u8; 4096]);
    let tcp_socket = tcp::Socket::new(tcp_rx, tcp_tx);
    let tcp_handle = state.sockets.add(tcp_socket);

    let remote = IpAddress::Ipv4(Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]));

    // Enable interrupts for VirtIO-net
    unsafe { core::arch::asm!("sti"); }

    let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
    if socket.connect(state.iface.context(), (remote, port as u16), next_port()).is_err() {
        state.sockets.remove(tcp_handle);
        return u64::MAX;
    }

    slots[slot_idx].state = SlotState::Connecting;
    slots[slot_idx].handle = Some(tcp_handle);

    EAGAIN // connecting, check back next frame
}

/// Non-blocking send. Returns bytes written, EAGAIN, or MAX on error.
pub fn syscall_tcp_send(slot_id: u64, data_ptr: u64, data_len: u64) -> u64 {
    if slot_id as usize >= MAX_ASYNC_SLOTS { return u64::MAX; }

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
        (SlotState::Connecting, _) => return EAGAIN,
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

    let mut guard = match NET_STATE.try_lock() {
        Some(g) => g,
        None => return EAGAIN,
    };
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
    0
}

extern crate alloc;
