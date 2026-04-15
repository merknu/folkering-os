//! WASM Network Driver Packet Ring
//!
//! Shared between the compositor (via syscalls) and smoltcp (via Device trait).
//! The compositor's E1000 host functions call submit_rx/poll_tx syscalls.
//! The kernel's FolkeringDevice reads from rx_ring and writes to tx_ring.

extern crate alloc;
use spin::Mutex;

/// Max packets in the WASM driver ring buffers
pub(crate) const WASM_NET_RING_SIZE: usize = 8;
/// Max Ethernet frame size
pub(crate) const MAX_FRAME_SIZE: usize = 1514;

pub(crate) struct PacketSlot {
    pub data: [u8; MAX_FRAME_SIZE],
    pub len: usize,
    pub used: bool,
}

impl PacketSlot {
    pub const EMPTY: Self = Self { data: [0; MAX_FRAME_SIZE], len: 0, used: false };
}

/// Packet ring for WASM network driver ↔ kernel smoltcp bridge
pub(crate) struct WasmNetRing {
    /// Packets received by E1000 hardware, waiting for smoltcp to process
    rx_ring: [PacketSlot; WASM_NET_RING_SIZE],
    rx_head: usize,
    rx_count: usize,
    /// Packets from smoltcp waiting for E1000 to transmit
    tx_ring: [PacketSlot; WASM_NET_RING_SIZE],
    tx_head: usize,
    tx_count: usize,
    /// MAC address provided by the WASM driver
    pub mac: [u8; 6],
    /// Whether the WASM net backend is active
    pub active: bool,
}

impl WasmNetRing {
    const fn new() -> Self {
        Self {
            rx_ring: [PacketSlot::EMPTY; WASM_NET_RING_SIZE],
            rx_head: 0, rx_count: 0,
            tx_ring: [PacketSlot::EMPTY; WASM_NET_RING_SIZE],
            tx_head: 0, tx_count: 0,
            mac: [0; 6],
            active: false,
        }
    }

    /// Submit a received packet (from E1000 DMA → kernel)
    pub fn submit_rx(&mut self, data: &[u8]) -> bool {
        if self.rx_count >= WASM_NET_RING_SIZE || data.len() > MAX_FRAME_SIZE {
            return false;
        }
        let idx = (self.rx_head + self.rx_count) % WASM_NET_RING_SIZE;
        self.rx_ring[idx].data[..data.len()].copy_from_slice(data);
        self.rx_ring[idx].len = data.len();
        self.rx_ring[idx].used = true;
        self.rx_count += 1;
        true
    }

    /// Pop a received packet (kernel smoltcp reads it)
    pub fn pop_rx(&mut self) -> Option<(&[u8], usize)> {
        if self.rx_count == 0 { return None; }
        let idx = self.rx_head;
        if !self.rx_ring[idx].used { return None; }
        let len = self.rx_ring[idx].len;
        self.rx_ring[idx].used = false;
        self.rx_head = (self.rx_head + 1) % WASM_NET_RING_SIZE;
        self.rx_count -= 1;
        Some((&self.rx_ring[idx].data[..len], len))
    }

    /// Queue a packet for transmission (kernel smoltcp → E1000)
    pub fn submit_tx(&mut self, data: &[u8]) -> bool {
        if self.tx_count >= WASM_NET_RING_SIZE || data.len() > MAX_FRAME_SIZE {
            return false;
        }
        let idx = (self.tx_head + self.tx_count) % WASM_NET_RING_SIZE;
        self.tx_ring[idx].data[..data.len()].copy_from_slice(data);
        self.tx_ring[idx].len = data.len();
        self.tx_ring[idx].used = true;
        self.tx_count += 1;
        true
    }

    /// Pop a packet to transmit (E1000 reads it)
    pub fn pop_tx(&mut self) -> Option<(&[u8], usize)> {
        if self.tx_count == 0 { return None; }
        let idx = self.tx_head;
        if !self.tx_ring[idx].used { return None; }
        let len = self.tx_ring[idx].len;
        self.tx_ring[idx].used = false;
        self.tx_head = (self.tx_head + 1) % WASM_NET_RING_SIZE;
        self.tx_count -= 1;
        Some((&self.tx_ring[idx].data[..len], len))
    }
}

/// Global WASM network ring — accessed by syscalls and smoltcp Device
pub(crate) static WASM_NET: Mutex<WasmNetRing> = Mutex::new(WasmNetRing::new());

// ── Public API for syscalls ─────────────────────────────────────────────────

/// Called from SYS_NET_SUBMIT_RX syscall (compositor → kernel)
pub fn wasm_net_submit_rx(data: &[u8]) -> bool {
    WASM_NET.lock().submit_rx(data)
}

/// Called from SYS_NET_POLL_TX syscall (kernel → compositor)
/// Returns number of bytes copied to buf, or None if no packet available
pub fn wasm_net_poll_tx(buf: &mut [u8]) -> Option<usize> {
    let mut ring = WASM_NET.lock();
    if let Some((data, len)) = ring.pop_tx() {
        let copy_len = len.min(buf.len());
        buf[..copy_len].copy_from_slice(&data[..copy_len]);
        Some(copy_len)
    } else {
        None
    }
}

/// Initialize the WASM network backend with the given MAC address.
/// Called from SYS_NET_REGISTER syscall when E1000 driver starts.
pub fn init_wasm_net(mac: [u8; 6]) {
    let mut ring = WASM_NET.lock();
    ring.mac = mac;
    ring.active = true;
    drop(ring);

    crate::serial_str!("[NET] WASM E1000 registered, MAC=");
    for i in 0..6 {
        crate::drivers::serial::write_hex(mac[i] as u64);
        if i < 5 { crate::serial_str!(":"); }
    }
    crate::serial_strln!("");

    // Initialize smoltcp with this MAC (same as VirtIO path but using WASM backend)
    super::init_with_mac(mac);
}

/// Check if WASM net backend is active
pub fn wasm_net_active() -> bool {
    WASM_NET.try_lock().map_or(false, |r| r.active)
}
