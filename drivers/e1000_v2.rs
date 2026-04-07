//! Folkering OS — Intel E1000 Driver v2 (DMA RX/TX)
//!
//! Upgrades the bootstrap driver with actual packet transfer:
//! - DMA descriptor rings for RX and TX
//! - Packet buffers allocated via folk_dma_alloc
//! - Sends an ARP announcement on startup (proof of network)
//! - Logs received packets in the IRQ loop
//!
//! This is a v2 baseline for AutoDream to improve further.

#![no_std]
#![no_main]
#![allow(unused)]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

extern "C" {
    fn folk_device_vendor_id() -> i32;
    fn folk_device_id() -> i32;
    fn folk_bar_size(bar: i32) -> i32;
    fn folk_mmio_read_u32(bar: i32, offset: i32) -> i32;
    fn folk_mmio_write_u32(bar: i32, offset: i32, value: i32);
    fn folk_wait_irq();
    fn folk_ack_irq();
    fn folk_log(ptr: i32, len: i32);
    // DMA slot-based API
    fn folk_dma_alloc(size: i32) -> i32;      // returns slot ID or -1
    fn folk_dma_phys(slot: i32) -> i64;        // returns physical address
    fn folk_dma_write_u32(slot: i32, offset: i32, value: i32);
    fn folk_dma_write_u64(slot: i32, offset: i32, value: i64);
    fn folk_dma_read_u32(slot: i32, offset: i32) -> i32;
    fn folk_dma_read_u64(slot: i32, offset: i32) -> i64;
    fn folk_dma_free(slot: i32);
    // DMA coherency fallback — read physical memory via kernel HHDM
    fn folk_dma_sync_read(slot: i32, offset: i32, len: i32) -> i32;
    fn folk_dma_sync_read_u32(slot: i32, offset: i32) -> i32;
    fn folk_dma_sync_write(slot: i32, offset: i32, len: i32) -> i32;
    // Kernel-assisted RX: reads descriptor + packet via HHDM, delivers to smoltcp
    fn folk_net_dma_rx(ring_slot: i32, buf_slot: i32, desc_idx: i32, buf_size: i32) -> i32;
    // Network stack bridge — connect to kernel smoltcp
    fn folk_net_register(m0: i32, m1: i32, m2: i32, m3: i32, m4: i32, m5: i32);
    fn folk_net_submit_rx(dma_slot: i32, offset: i32, length: i32) -> i32;
    fn folk_net_poll_tx(dma_slot: i32, offset: i32, max_len: i32) -> i32;
}

// ── E1000 Register Map (BAR0 MMIO) ─────────────────────────────────────

const CTRL: i32    = 0x0000;
const STATUS: i32  = 0x0008;
const EECD: i32    = 0x0010;
const EERD: i32    = 0x0014;  // EEPROM Read
const ICR: i32     = 0x00C0;
const IMS: i32     = 0x00D0;
const IMC: i32     = 0x00D8;

// Receive registers
const RCTL: i32    = 0x0100;
const RDBAL: i32   = 0x2800;  // RX Descriptor Base Low
const RDBAH: i32   = 0x2804;  // RX Descriptor Base High
const RDLEN: i32   = 0x2808;  // RX Descriptor Length (bytes)
const RDH: i32     = 0x2810;  // RX Descriptor Head
const RDT: i32     = 0x2818;  // RX Descriptor Tail

// Transmit registers
const TCTL: i32    = 0x0400;
const TDBAL: i32   = 0x3800;  // TX Descriptor Base Low
const TDBAH: i32   = 0x3804;  // TX Descriptor Base High
const TDLEN: i32   = 0x3808;  // TX Descriptor Length (bytes)
const TDH: i32     = 0x3810;  // TX Descriptor Head
const TDT: i32     = 0x3818;  // TX Descriptor Tail
const TIPG: i32    = 0x0410;  // TX Inter-Packet Gap

// Multicast Table Array (clear all)
const MTA_BASE: i32 = 0x5200;

// Receive Address (MAC address)
const RAL0: i32    = 0x5400;
const RAH0: i32    = 0x5404;

// CTRL bits
const CTRL_SLU: i32  = 1 << 6;
const CTRL_RST: i32  = 1 << 26;
const CTRL_ASDE: i32 = 1 << 5;

// RCTL bits
const RCTL_EN: i32   = 1 << 1;   // Receiver Enable
const RCTL_BAM: i32  = 1 << 15;  // Broadcast Accept Mode
const RCTL_BSIZE_2K: i32 = 0;    // Buffer Size 2048
const RCTL_SECRC: i32 = 1 << 26; // Strip Ethernet CRC

// TCTL bits
const TCTL_EN: i32   = 1 << 1;   // Transmitter Enable
const TCTL_PSP: i32  = 1 << 3;   // Pad Short Packets
const TCTL_CT_SHIFT: i32 = 4;    // Collision Threshold shift
const TCTL_COLD_SHIFT: i32 = 12; // Collision Distance shift

// Interrupt bits
const ICR_TXDW: i32  = 1 << 0;
const ICR_TXQE: i32  = 1 << 1;
const ICR_LSC: i32   = 1 << 2;
const ICR_RXT0: i32  = 1 << 7;

// ── Descriptor Layout ───────────────────────────────────────────────────
// E1000 RX descriptor: 16 bytes
//   [0..7]   buffer_addr (u64, physical)
//   [8..9]   length (u16)
//   [10]     checksum (u16)
//   [12]     status (u8) — bit 0 = DD (Descriptor Done)
//   [13]     errors (u8)
//   [14..15] special (u16)
//
// E1000 TX descriptor: 16 bytes (legacy format)
//   [0..7]   buffer_addr (u64, physical)
//   [8..9]   length (u16)
//   [10]     cso (u8)
//   [11]     cmd (u8) — bit 0=EOP, bit 1=IFCS, bit 3=RS
//   [12]     status (u8) — bit 0 = DD
//   [13]     css (u8)
//   [14..15] special (u16)

const DESC_SIZE: i32 = 16;
const NUM_RX_DESC: i32 = 2;   // Active descriptors (with DMA buffers)
const NUM_TX_DESC: i32 = 2;   // Active TX descriptors
const RING_SIZE: i32 = 8;     // RDLEN/TDLEN=128 means 8 slots in hardware
const PACKET_BUF_SIZE: i32 = 2048; // Must be >= MTU (1514) + headers

// TX command bits
const TCMD_EOP: u8 = 1 << 0;  // End of Packet
const TCMD_IFCS: u8 = 1 << 1; // Insert FCS (CRC)
const TCMD_RS: u8 = 1 << 3;   // Report Status

// RX/TX status bits
const RDESC_DD: u8 = 1 << 0;  // Descriptor Done

static mut IRQ_COUNT: u32 = 0;
static mut RX_COUNT: u32 = 0;
static mut TX_COUNT: u32 = 0;
static mut MAC: [u8; 6] = [0; 6];
static mut INITIALIZED: bool = false;

// DMA slot IDs (assigned at runtime)
static mut RX_RING_SLOT: i32 = -1;
static mut TX_RING_SLOT: i32 = -1;
static mut RX_BUF_SLOT: i32 = -1;
static mut TX_BUF_SLOT: i32 = -1;
static mut LAST_RDH: u32 = 0;        // Track last known RDH for RX detection
static mut NEEDS_DMA_SYNC: bool = false; // True if DMA readback requires kernel assist

fn log(msg: &[u8]) {
    unsafe { folk_log(msg.as_ptr() as i32, msg.len() as i32); }
}

/// Read a 16-bit word from E1000 EEPROM via EERD register.
/// QEMU uses the 82540 EEPROM interface: addr in bits [15:8], start=bit0, done=bit4.
/// Some E1000 variants use done=bit1 — we try both.
fn eeprom_read(addr: i32) -> u16 {
    unsafe {
        // Try 82540-style first: addr<<8, start=bit0, done=bit4
        folk_mmio_write_u32(0, EERD, (addr << 8) | 1);
        let mut val;
        let mut tries = 0i32;
        loop {
            val = folk_mmio_read_u32(0, EERD);
            if (val & (1 << 4)) != 0 { return ((val >> 16) & 0xFFFF) as u16; }
            if (val & (1 << 1)) != 0 { return ((val >> 16) & 0xFFFF) as u16; } // alt done bit
            tries += 1;
            if tries > 50000 { break; }
        }
        // Fallback: try 82541/82547-style: addr<<2, start=bit0, done=bit1
        folk_mmio_write_u32(0, EERD, (addr << 2) | 1);
        tries = 0;
        loop {
            val = folk_mmio_read_u32(0, EERD);
            if (val & (1 << 1)) != 0 { return ((val >> 16) & 0xFFFF) as u16; }
            tries += 1;
            if tries > 50000 { break; }
        }
        0xFFFF
    }
}

/// Read MAC address from E1000 EEPROM (words 0, 1, 2)
fn read_mac() {
    unsafe {
        let w0 = eeprom_read(0);
        let w1 = eeprom_read(1);
        let w2 = eeprom_read(2);
        MAC[0] = (w0 & 0xFF) as u8;
        MAC[1] = ((w0 >> 8) & 0xFF) as u8;
        MAC[2] = (w1 & 0xFF) as u8;
        MAC[3] = ((w1 >> 8) & 0xFF) as u8;
        MAC[4] = (w2 & 0xFF) as u8;
        MAC[5] = ((w2 >> 8) & 0xFF) as u8;

        // Fallback: if EEPROM returns all-ff (QEMU without EEPROM support),
        // use the QEMU default MAC convention (52:54:00:12:34:56)
        if MAC[0] == 0xFF && MAC[1] == 0xFF && MAC[2] == 0xFF {
            MAC = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        }

        // Write to RAL/RAH so hardware uses this MAC for filtering
        let ral_val = (MAC[0] as i32) | ((MAC[1] as i32) << 8)
            | ((MAC[2] as i32) << 16) | ((MAC[3] as i32) << 24);
        let rah_val = (MAC[4] as i32) | ((MAC[5] as i32) << 8)
            | (1 << 31); // AV (Address Valid) bit
        folk_mmio_write_u32(0, RAL0, ral_val);
        folk_mmio_write_u32(0, RAH0, rah_val);
    }
}

/// Initialize RX descriptor ring
fn init_rx_ring() -> bool {
    unsafe {
        // Allocate descriptor ring (128B = 8 slots, E1000 128-byte alignment requirement)
        RX_RING_SLOT = folk_dma_alloc(128);
        if RX_RING_SLOT < 0 { log(b"[E1000] RX ring alloc FAIL"); return false; }

        // Allocate packet buffers: 2 descriptors × 2048B = 4096B (one 4KB DMA page)
        RX_BUF_SLOT = folk_dma_alloc(NUM_RX_DESC * PACKET_BUF_SIZE);
        if RX_BUF_SLOT < 0 { log(b"[E1000] RX buf alloc FAIL"); return false; }

        let buf_phys = folk_dma_phys(RX_BUF_SLOT);
        if buf_phys < 0 { return false; }

        // Initialize ALL 8 ring slots (zero unused ones to prevent hardware confusion)
        for i in 0..RING_SIZE {
            let desc_off = i * DESC_SIZE;
            if i < NUM_RX_DESC {
                // Active descriptor: point to packet buffer
                let pkt_phys = buf_phys + (i as i64 * PACKET_BUF_SIZE as i64);
                folk_dma_write_u64(RX_RING_SLOT, desc_off, pkt_phys);
            } else {
                // Unused slot: zero buffer address
                folk_dma_write_u64(RX_RING_SLOT, desc_off, 0);
            }
            folk_dma_write_u64(RX_RING_SLOT, desc_off + 8, 0);
        }

        // Sync entire descriptor ring to physical memory (WHPX coherency)
        folk_dma_sync_write(RX_RING_SLOT, 0, RING_SIZE * DESC_SIZE);

        // Program MMIO registers
        let ring_phys = folk_dma_phys(RX_RING_SLOT);
        folk_mmio_write_u32(0, RDBAL, (ring_phys & 0xFFFFFFFF) as i32);
        folk_mmio_write_u32(0, RDBAH, ((ring_phys >> 32) & 0xFFFFFFFF) as i32);
        folk_mmio_write_u32(0, RDLEN, 128);
        folk_mmio_write_u32(0, RDH, 0);
        // DO NOT set RDT here — must be set AFTER RCTL.EN per E1000 spec
        // RDT is set in the main init sequence after RCTL enable.

        true
    }
}

/// Initialize TX descriptor ring
fn init_tx_ring() -> bool {
    unsafe {
        // Allocate TX descriptor ring — MUST be 128-byte aligned (E1000 spec)
        TX_RING_SLOT = folk_dma_alloc(128);
        if TX_RING_SLOT < 0 { log(b"[E1000] TX ring alloc FAIL"); return false; }

        // Allocate TX packet buffers
        TX_BUF_SLOT = folk_dma_alloc(NUM_TX_DESC * PACKET_BUF_SIZE);
        if TX_BUF_SLOT < 0 { log(b"[E1000] TX buf alloc FAIL"); return false; }

        // Initialize ALL ring slots to zero (128 bytes)
        for i in 0..RING_SIZE {
            let desc_off = i * DESC_SIZE;
            folk_dma_write_u64(TX_RING_SLOT, desc_off, 0);
            folk_dma_write_u64(TX_RING_SLOT, desc_off + 8, 0);
        }
        // Sync to physical memory
        folk_dma_sync_write(TX_RING_SLOT, 0, RING_SIZE * DESC_SIZE);

        // Program MMIO registers
        let ring_phys = folk_dma_phys(TX_RING_SLOT);
        folk_mmio_write_u32(0, TDBAL, (ring_phys & 0xFFFFFFFF) as i32);
        folk_mmio_write_u32(0, TDBAH, ((ring_phys >> 32) & 0xFFFFFFFF) as i32);
        folk_mmio_write_u32(0, TDLEN, 128); // Must be 128-byte aligned
        folk_mmio_write_u32(0, TDH, 0);
        folk_mmio_write_u32(0, TDT, 0); // Empty — nothing to transmit yet

        true
    }
}

/// Send a raw Ethernet frame via the TX ring
fn transmit(data_slot: i32, data_offset: i32, length: i32) -> bool {
    unsafe {
        let tdt = folk_mmio_read_u32(0, TDT) as u32;
        let idx = tdt % (NUM_TX_DESC as u32);
        let desc_off = (idx as i32) * DESC_SIZE;

        // Get physical address of the data in the TX buffer
        let buf_phys = folk_dma_phys(TX_BUF_SLOT);
        if buf_phys < 0 { return false; }
        let pkt_phys = buf_phys + (idx as i64 * PACKET_BUF_SIZE as i64);

        // Copy data from source DMA slot to TX buffer
        // (Since both are DMA slots, we write directly to TX buf)
        // The caller already wrote data to TX_BUF_SLOT at the right offset

        // Set up TX descriptor
        folk_dma_write_u64(TX_RING_SLOT, desc_off, pkt_phys);
        let cmd_len: u32 = (length as u32) | ((TCMD_EOP | TCMD_IFCS | TCMD_RS) as u32) << 24;
        folk_dma_write_u32(TX_RING_SLOT, desc_off + 8, cmd_len as i32);
        folk_dma_write_u32(TX_RING_SLOT, desc_off + 12, 0);
        // Sync descriptor + packet buffer to physical memory
        folk_dma_sync_write(TX_RING_SLOT, desc_off, DESC_SIZE);
        folk_dma_sync_write(TX_BUF_SLOT, (idx as i32) * PACKET_BUF_SIZE, length);

        // Bump TDT to tell hardware to send
        let new_tdt = (tdt + 1) % (NUM_TX_DESC as u32);
        folk_mmio_write_u32(0, TDT, new_tdt as i32);

        TX_COUNT = TX_COUNT.saturating_add(1);
        true
    }
}

/// Craft and send a gratuitous ARP announcement
fn send_arp_announce() {
    unsafe {
        // Write ARP packet into TX buffer slot 0
        let idx = folk_mmio_read_u32(0, TDT) as u32 % (NUM_TX_DESC as u32);
        let buf_off = (idx as i32) * PACKET_BUF_SIZE;

        // Ethernet header (14 bytes)
        // Destination: broadcast FF:FF:FF:FF:FF:FF
        folk_dma_write_u32(TX_BUF_SLOT, buf_off + 0, 0xFFFF_FFFFu32 as i32);
        folk_dma_write_u32(TX_BUF_SLOT, buf_off + 4,
            0xFFFF_u32 as i32 | ((MAC[0] as i32) << 16) | ((MAC[1] as i32) << 24));
        folk_dma_write_u32(TX_BUF_SLOT, buf_off + 8,
            (MAC[2] as i32) | ((MAC[3] as i32) << 8) | ((MAC[4] as i32) << 16) | ((MAC[5] as i32) << 24));
        // EtherType: ARP (0x0806) in network byte order
        folk_dma_write_u32(TX_BUF_SLOT, buf_off + 12, 0x0608_0001u32 as i32);
        // ARP: HTYPE=0x0001(Ethernet), PTYPE=0x0800(IPv4)
        // Already wrote HTYPE above as part of the u32
        folk_dma_write_u32(TX_BUF_SLOT, buf_off + 16, 0x0008_0604u32 as i32);
        // HLEN=6, PLEN=4, OPER=1(Request) — but we're doing announcement so OPER=2(Reply)
        folk_dma_write_u32(TX_BUF_SLOT, buf_off + 20, 0x0002_0000u32 as i32 |
            ((MAC[0] as i32) << 16) | ((MAC[1] as i32) << 24));
        // Sender MAC (continued) + Sender IP (10.0.2.15 — QEMU default)
        folk_dma_write_u32(TX_BUF_SLOT, buf_off + 24,
            (MAC[2] as i32) | ((MAC[3] as i32) << 8) | ((MAC[4] as i32) << 16) | ((MAC[5] as i32) << 24));
        folk_dma_write_u32(TX_BUF_SLOT, buf_off + 28, 0x0F02_000Au32 as i32); // 10.0.2.15

        // Target MAC (zeros for announcement) + Target IP
        folk_dma_write_u32(TX_BUF_SLOT, buf_off + 32, 0x0000_0000u32 as i32);
        folk_dma_write_u32(TX_BUF_SLOT, buf_off + 36, 0x0000_0000u32 as i32);
        folk_dma_write_u32(TX_BUF_SLOT, buf_off + 40, 0x0F02_000Au32 as i32); // 10.0.2.15

        // Total: 42 bytes (14 ethernet + 28 ARP)
        transmit(TX_BUF_SLOT, buf_off, 42);
        log(b"[E1000] ARP announce sent (10.0.2.15)");
    }
}

/// Check RX ring for received packets using RDH advancement detection.
/// WHPX may not expose DD bit writeback, so we use the hardware RDH register.
fn poll_rx() {
    unsafe {
        let rdh = folk_mmio_read_u32(0, RDH) as u32;
        let ring_size = 8u32; // 128 / DESC_SIZE

        // If RDH advanced past LAST_RDH, hardware completed descriptor(s)
        while LAST_RDH != rdh {
            let idx = LAST_RDH;
            let desc_off = (idx as i32) * DESC_SIZE;

            // Read packet length from descriptor (set by hardware)
            let len_word = folk_dma_read_u32(RX_RING_SLOT, desc_off + 8);
            let pkt_len = (len_word & 0xFFFF) as u16;

            if pkt_len > 0 && pkt_len <= PACKET_BUF_SIZE as u16 {
                RX_COUNT = RX_COUNT.saturating_add(1);
                if RX_COUNT <= 5 {
                    log(b"[E1000] RX packet!");
                }

                // Deliver to kernel smoltcp
                let buf_offset = (idx as i32) * PACKET_BUF_SIZE;
                folk_net_submit_rx(RX_BUF_SLOT, buf_offset, pkt_len as i32);
            }

            // Reset descriptor for reuse
            let buf_phys = folk_dma_phys(RX_BUF_SLOT);
            if buf_phys >= 0 && (idx as i32) < NUM_RX_DESC {
                folk_dma_write_u64(RX_RING_SLOT, desc_off,
                    buf_phys + (idx as i64 * PACKET_BUF_SIZE as i64));
            }
            folk_dma_write_u64(RX_RING_SLOT, desc_off + 8, 0);

            // Give descriptor back to hardware
            folk_mmio_write_u32(0, RDT, idx as i32);

            // Advance our tracking
            LAST_RDH = (LAST_RDH + 1) % ring_size;
        }
    }
}

#[no_mangle]
pub extern "C" fn driver_main() {
    unsafe {
        // Guard: driver_main is called on EVERY resume from folk_wait_irq().
        // Only initialize once; on subsequent calls, skip straight to work section.
        if INITIALIZED {
            // Skip init, fall through to RX/TX work below
        } else {
        INITIALIZED = true;

        log(b"[E1000] v2 DMA driver starting");

        // Verify device
        let vid = folk_device_vendor_id();
        let did = folk_device_id();
        if vid != 0x8086 || did != 0x100E {
            log(b"[E1000] Wrong device!");
            return;
        }

        // Step 1: Reset
        let ctrl = folk_mmio_read_u32(0, CTRL);
        folk_mmio_write_u32(0, CTRL, ctrl | CTRL_RST);
        let mut wait = 0i32;
        loop {
            wait += 1;
            if wait > 10000 { break; }
            if (folk_mmio_read_u32(0, CTRL) & CTRL_RST) == 0 { break; }
        }
        log(b"[E1000] Reset done");

        // Step 2: Set Link Up
        folk_mmio_write_u32(0, CTRL, folk_mmio_read_u32(0, CTRL) | CTRL_SLU | CTRL_ASDE);

        // Step 3: Clear interrupts
        folk_mmio_write_u32(0, IMC, 0x7FFF_FFFFu32 as i32);
        let _ = folk_mmio_read_u32(0, ICR);

        // Debug: check BAR0 size
        let bar0_size = folk_bar_size(0);
        // Log BAR size as decimal string
        if bar0_size == 0 {
            log(b"[E1000] BAR0 size=0 (NOT MAPPED!)");
        } else {
            log(b"[E1000] BAR0 mapped OK");
        }

        // Step 4: Read MAC address and register with kernel network stack
        read_mac();
        folk_net_register(
            MAC[0] as i32, MAC[1] as i32, MAC[2] as i32,
            MAC[3] as i32, MAC[4] as i32, MAC[5] as i32,
        );
        log(b"[E1000] MAC registered with kernel");

        // Step 5: Clear first 4 MTA entries (minimum — rest are already 0 from reset)
        folk_mmio_write_u32(0, MTA_BASE, 0);
        folk_mmio_write_u32(0, MTA_BASE + 4, 0);
        folk_mmio_write_u32(0, MTA_BASE + 8, 0);
        folk_mmio_write_u32(0, MTA_BASE + 12, 0);

        // Step 6: Initialize RX ring with DMA
        if !init_rx_ring() {
            log(b"[E1000] RX init FAILED");
            return;
        }
        log(b"[E1000] RX ring initialized");

        // Step 7: Initialize TX ring with DMA
        if !init_tx_ring() {
            log(b"[E1000] TX init FAILED");
            return;
        }
        log(b"[E1000] TX ring initialized");

        // Step 8: Enable RX
        // Enable RX with promiscuous mode for maximum compatibility
        // UPE (bit 3) = accept all unicast, MPE (bit 4) = accept all multicast
        let rctl_upe = 1 << 3;
        let rctl_mpe = 1 << 4;
        folk_mmio_write_u32(0, RCTL, RCTL_EN | RCTL_BAM | rctl_upe | rctl_mpe | RCTL_SECRC);
        // NOW set RDT — after RCTL.EN, triggers hardware to fetch descriptors
        folk_mmio_write_u32(0, RDT, NUM_RX_DESC - 1);
        log(b"[E1000] RX enabled + RDT set");

        // Step 9: Enable TX
        // TIPG: standard values for 1Gbit
        folk_mmio_write_u32(0, TIPG, 10 | (10 << 10) | (10 << 20));
        folk_mmio_write_u32(0, TCTL,
            TCTL_EN | TCTL_PSP | (0x10 << TCTL_CT_SHIFT) | (0x40 << TCTL_COLD_SHIFT));
        log(b"[E1000] TX enabled");

        // Step 10: Enable interrupts
        folk_mmio_write_u32(0, IMS, ICR_LSC | ICR_RXT0 | ICR_TXDW);

        // Step 11: Check link
        let status = folk_mmio_read_u32(0, STATUS);
        if (status & 2) != 0 {
            log(b"[E1000] Link UP - sending ARP");
            send_arp_announce();
        } else {
            log(b"[E1000] Link DOWN");
        }

        log(b"[E1000] v2 init complete");
        } // end else (init block)
    }

    // ── MAIN WORK: runs on EVERY call to driver_main() ──
    unsafe {
        let _ = folk_mmio_read_u32(0, ICR);

        // ── RX: Graceful Degradation (Fast Path + Exception Path) ──
        let rdh = folk_mmio_read_u32(0, RDH) as u32;
        let ring_size = RING_SIZE as u32;
        let mut rx_processed = 0u32;

        while LAST_RDH != rdh && rx_processed < NUM_RX_DESC as u32 {
            let idx = LAST_RDH;
            if (idx as i32) < NUM_RX_DESC {
                let desc_off = (idx as i32) * DESC_SIZE;

                // Fast Path: try reading descriptor from userspace DMA mapping
                let mut pkt_len: u16 = 0;
                let mut delivered = false;

                if !NEEDS_DMA_SYNC {
                    let len_word = folk_dma_read_u32(RX_RING_SLOT, desc_off + 8);
                    pkt_len = (len_word & 0xFFFF) as u16;

                    if pkt_len == 0 {
                        // Cache incoherent! Switch to kernel-assisted path.
                        log(b"[E1000] Cache incoherent -> kernel DMA RX");
                        NEEDS_DMA_SYNC = true;
                    } else if pkt_len <= PACKET_BUF_SIZE as u16 {
                        // Fast path: deliver from userspace DMA buffer
                        RX_COUNT = RX_COUNT.saturating_add(1);
                        let buf_offset = (idx as i32) * PACKET_BUF_SIZE;
                        folk_net_submit_rx(RX_BUF_SLOT, buf_offset, pkt_len as i32);
                        delivered = true;
                    }
                }

                // Exception Path: kernel reads descriptor + packet from physical RAM
                if NEEDS_DMA_SYNC && !delivered {
                    let rx_len = folk_net_dma_rx(
                        RX_RING_SLOT, RX_BUF_SLOT,
                        idx as i32, PACKET_BUF_SIZE
                    );
                    if rx_len > 0 {
                        pkt_len = rx_len as u16;
                        RX_COUNT = RX_COUNT.saturating_add(1);
                        delivered = true;
                    }
                }

                if delivered && RX_COUNT <= 3 {
                    log(b"[E1000] RX delivered!");
                }

                // Reset descriptor for reuse + sync to physical memory
                let buf_phys = folk_dma_phys(RX_BUF_SLOT);
                if buf_phys >= 0 {
                    folk_dma_write_u64(RX_RING_SLOT, desc_off,
                        buf_phys + (idx as i64 * PACKET_BUF_SIZE as i64));
                }
                folk_dma_write_u64(RX_RING_SLOT, desc_off + 8, 0);
                folk_dma_sync_write(RX_RING_SLOT, desc_off, DESC_SIZE);
                folk_mmio_write_u32(0, RDT, idx as i32);
            }
            LAST_RDH = (LAST_RDH + 1) % ring_size;
            rx_processed += 1;
        }
        LAST_RDH = rdh;

        // ── TX: Poll kernel for outgoing packets ──
        if TX_BUF_SLOT >= 0 {
            let tdt = folk_mmio_read_u32(0, TDT) as u32;
            let tx_idx = tdt % (NUM_TX_DESC as u32);
            let tx_buf_off = (tx_idx as i32) * PACKET_BUF_SIZE;
            let tx_len = folk_net_poll_tx(TX_BUF_SLOT, tx_buf_off, PACKET_BUF_SIZE);
            if tx_len > 0 {
                transmit(TX_BUF_SLOT, tx_buf_off, tx_len);
            }
        }

        folk_ack_irq();
        folk_wait_irq();
    }
}
