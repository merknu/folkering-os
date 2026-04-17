//! Kernel-side TCP client for the a64-stream daemon on Raspberry Pi 5.
//!
//! Implements the framed wire protocol (HELLO/CODE/DATA/EXEC/RESULT)
//! directly from kernel space using the smoltcp TCP stack. No userspace
//! involvement — the kernel JIT-compiles WASM to AArch64 and streams
//! the result to the Pi for execution.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, Ipv4Address};

use super::device::FolkeringDevice;
use super::state::NET_STATE;
use super::tls::{next_port, tsc_ms};

const FRAME_HELLO: u8 = 0x01;
const FRAME_CODE: u8 = 0x02;
const FRAME_DATA: u8 = 0x03;
const FRAME_EXEC: u8 = 0x04;
const FRAME_RESULT: u8 = 0x05;
const FRAME_ERROR: u8 = 0x06;
const FRAME_BYE: u8 = 0x07;
const HEADER_LEN: usize = 5;

pub struct Hello {
    pub mem_base: u64,
    pub mem_size: u32,
}

pub struct A64Session {
    tcp_handle: smoltcp::iface::SocketHandle,
    pub hello: Hello,
}

impl A64Session {
    /// Connect to the Pi daemon, perform HELLO handshake, return session.
    pub fn connect(ip: [u8; 4], port: u16) -> Result<Self, &'static str> {
        let mut guard = {
            let mut attempts = 0u32;
            loop {
                if let Some(g) = NET_STATE.try_lock() { break g; }
                attempts += 1;
                if attempts > 1000 { return Err("NET_STATE locked"); }
                core::hint::spin_loop();
            }
        };
        let state = guard.as_mut().ok_or("no network")?;
        if !state.has_ip { return Err("no IP address"); }

        let tcp_rx = tcp::SocketBuffer::new(vec![0u8; 16384]);
        let tcp_tx = tcp::SocketBuffer::new(vec![0u8; 16384]);
        let tcp_socket = tcp::Socket::new(tcp_rx, tcp_tx);
        let tcp_handle = state.sockets.add(tcp_socket);

        let remote = IpAddress::Ipv4(Ipv4Address::new(ip[0], ip[1], ip[2], ip[3]));
        unsafe { core::arch::asm!("sti"); }

        crate::serial_str!("[A64] connecting to Pi daemon...\n");

        {
            let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
            socket.connect(state.iface.context(), (remote, port), next_port())
                .map_err(|_| "TCP connect failed")?;
        }

        let start = tsc_ms();
        loop {
            poll_net(state);
            let socket = state.sockets.get_mut::<tcp::Socket>(tcp_handle);
            if socket.may_send() { break; }
            if !socket.is_active() {
                state.sockets.remove(tcp_handle);
                return Err("TCP refused");
            }
            if tsc_ms() - start > 10_000 {
                state.sockets.remove(tcp_handle);
                return Err("connect timeout");
            }
            spin_short();
        }

        crate::serial_str!("[A64] connected, waiting for HELLO...\n");

        // Receive HELLO frame
        let hello_payload = recv_frame(state, tcp_handle, FRAME_HELLO, 5_000)?;
        if hello_payload.len() < 12 {
            state.sockets.remove(tcp_handle);
            return Err("HELLO too short");
        }
        let mem_base = u64::from_le_bytes([
            hello_payload[0], hello_payload[1], hello_payload[2], hello_payload[3],
            hello_payload[4], hello_payload[5], hello_payload[6], hello_payload[7],
        ]);
        let mem_size = u32::from_le_bytes([
            hello_payload[8], hello_payload[9], hello_payload[10], hello_payload[11],
        ]);

        crate::serial_str!("[A64] HELLO: mem_base=0x");
        crate::drivers::serial::write_hex(mem_base);
        crate::serial_str!(", mem_size=");
        crate::drivers::serial::write_dec(mem_size);
        crate::serial_str!("\n");

        Ok(A64Session {
            tcp_handle,
            hello: Hello { mem_base, mem_size },
        })
    }

    /// Send JIT-compiled code to the daemon.
    pub fn send_code(&self, code: &[u8]) -> Result<(), &'static str> {
        let mut guard = net_lock()?;
        let state = guard.as_mut().ok_or("no network")?;
        send_frame(state, self.tcp_handle, FRAME_CODE, code)
    }

    /// Send weight/input data to the daemon's linear memory.
    pub fn send_data(&self, offset: u32, data: &[u8]) -> Result<(), &'static str> {
        let mut payload = Vec::with_capacity(4 + data.len());
        payload.extend_from_slice(&offset.to_le_bytes());
        payload.extend_from_slice(data);
        let mut guard = net_lock()?;
        let state = guard.as_mut().ok_or("no network")?;
        send_frame(state, self.tcp_handle, FRAME_DATA, &payload)
    }

    /// Trigger execution and return the i32 result.
    pub fn exec(&self) -> Result<i32, &'static str> {
        let mut guard = net_lock()?;
        let state = guard.as_mut().ok_or("no network")?;
        send_frame(state, self.tcp_handle, FRAME_EXEC, &[])?;
        let payload = recv_frame(state, self.tcp_handle, FRAME_RESULT, 10_000)?;
        if payload.len() < 4 {
            return Err("RESULT too short");
        }
        Ok(i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]))
    }

    /// Gracefully close the session.
    pub fn close(self) -> Result<(), &'static str> {
        let mut guard = net_lock()?;
        let state = guard.as_mut().ok_or("no network")?;
        let _ = send_frame(state, self.tcp_handle, FRAME_BYE, &[]);
        state.sockets.remove(self.tcp_handle);
        crate::serial_str!("[A64] session closed\n");
        Ok(())
    }
}

// ── Internal helpers ────────────────────────────────────────────────

fn net_lock() -> Result<spin::MutexGuard<'static, Option<super::state::NetState>>, &'static str> {
    let mut attempts = 0u32;
    loop {
        if let Some(g) = NET_STATE.try_lock() { return Ok(g); }
        attempts += 1;
        if attempts > 1000 { return Err("NET_STATE locked"); }
        core::hint::spin_loop();
    }
}

fn poll_net(state: &mut super::state::NetState) {
    let now = Instant::from_millis(tsc_ms());
    let mut device = FolkeringDevice;
    state.iface.poll(now, &mut device, &mut state.sockets);
    super::tcp_shell::poll(state);
}

fn spin_short() {
    for _ in 0..200 { core::hint::spin_loop(); }
}

fn send_frame(
    state: &mut super::state::NetState,
    handle: smoltcp::iface::SocketHandle,
    frame_type: u8,
    payload: &[u8],
) -> Result<(), &'static str> {
    let mut header = [0u8; HEADER_LEN];
    header[0] = frame_type;
    let len = payload.len() as u32;
    header[1..5].copy_from_slice(&len.to_le_bytes());

    send_all(state, handle, &header)?;
    if !payload.is_empty() {
        send_all(state, handle, payload)?;
    }
    Ok(())
}

fn send_all(
    state: &mut super::state::NetState,
    handle: smoltcp::iface::SocketHandle,
    data: &[u8],
) -> Result<(), &'static str> {
    let mut sent = 0;
    let start = tsc_ms();
    while sent < data.len() {
        poll_net(state);
        let socket = state.sockets.get_mut::<tcp::Socket>(handle);
        if !socket.is_active() { return Err("connection lost"); }
        if socket.can_send() {
            match socket.send_slice(&data[sent..]) {
                Ok(n) => sent += n,
                Err(_) => return Err("send error"),
            }
        }
        if tsc_ms() - start > 15_000 { return Err("send timeout"); }
        spin_short();
    }
    // Flush
    poll_net(state);
    Ok(())
}

fn recv_frame(
    state: &mut super::state::NetState,
    handle: smoltcp::iface::SocketHandle,
    expected_type: u8,
    timeout_ms: u64,
) -> Result<Vec<u8>, &'static str> {
    let mut header = [0u8; HEADER_LEN];
    recv_exact(state, handle, &mut header, timeout_ms)?;

    if header[0] == FRAME_ERROR {
        return Err("daemon returned ERROR");
    }
    if header[0] != expected_type {
        return Err("unexpected frame type");
    }

    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > 1024 * 1024 {
        return Err("frame too large");
    }

    let mut payload = vec![0u8; len];
    if len > 0 {
        recv_exact(state, handle, &mut payload, timeout_ms)?;
    }
    Ok(payload)
}

fn recv_exact(
    state: &mut super::state::NetState,
    handle: smoltcp::iface::SocketHandle,
    buf: &mut [u8],
    timeout_ms: u64,
) -> Result<(), &'static str> {
    let mut received = 0;
    let start = tsc_ms();
    while received < buf.len() {
        poll_net(state);
        let socket = state.sockets.get_mut::<tcp::Socket>(handle);
        if !socket.is_active() && !socket.may_recv() {
            return Err("connection closed");
        }
        if socket.can_recv() {
            match socket.recv_slice(&mut buf[received..]) {
                Ok(n) => received += n,
                Err(_) => return Err("recv error"),
            }
        }
        if (tsc_ms() - start) as u64 > timeout_ms { return Err("recv timeout"); }
        spin_short();
    }
    Ok(())
}
