//! Folkering OS — VirtIO-Net WASM Driver v1
//!
//! Cloud-native network driver using virtqueues (shared memory rings).
//! No DMA descriptor coherency issues — uses explicit memory barriers.
//!
//! VirtIO Legacy PCI Transport:
//! - BAR0: I/O port space (registers at port offsets)
//! - Queue 0: RX (device → driver)
//! - Queue 1: TX (driver → device)
//! - 10-byte VirtIO net header prepended to all packets

#![no_std]
#![no_main]
#![allow(unused)]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }

extern "C" {
    // Port I/O (VirtIO legacy uses I/O ports, not MMIO)
    fn folk_inb(port: i32) -> i32;
    fn folk_inw(port: i32) -> i32;
    fn folk_inl(port: i32) -> i32;
    fn folk_outb(port: i32, value: i32);
    fn folk_outw(port: i32, value: i32);
    fn folk_outl(port: i32, value: i32);
    // DMA memory (for virtqueue pages + packet buffers)
    fn folk_dma_alloc(size: i32) -> i32;
    fn folk_dma_phys(slot: i32) -> i64;
    fn folk_dma_write_u32(slot: i32, offset: i32, value: i32);
    fn folk_dma_write_u64(slot: i32, offset: i32, value: i64);
    fn folk_dma_read_u32(slot: i32, offset: i32) -> i32;
    fn folk_dma_read_u64(slot: i32, offset: i32) -> i64;
    fn folk_dma_free(slot: i32);
    fn folk_dma_sync_write(slot: i32, offset: i32, len: i32) -> i32;
    // Device identity
    fn folk_device_vendor_id() -> i32;
    fn folk_device_id() -> i32;
    fn folk_device_irq() -> i32;
    fn folk_bar_size(bar: i32) -> i32;
    fn folk_device_io_base(bar: i32) -> i32;
    // Debug
    fn folk_log(ptr: i32, len: i32);
    // Network stack bridge
    fn folk_net_register(m0: i32, m1: i32, m2: i32, m3: i32, m4: i32, m5: i32);
    fn folk_net_submit_rx(dma_slot: i32, offset: i32, length: i32) -> i32;
    fn folk_net_poll_tx(dma_slot: i32, offset: i32, max_len: i32) -> i32;
    // Yield
    fn folk_wait_irq();
    fn folk_ack_irq();
}

// ── VirtIO PCI Register Offsets (BAR0 I/O port space) ───────────────────

const VIRTIO_PCI_DEVICE_FEATURES: i32 = 0x00;
const VIRTIO_PCI_DRIVER_FEATURES: i32 = 0x04;
const VIRTIO_PCI_QUEUE_PFN: i32       = 0x08;
const VIRTIO_PCI_QUEUE_SIZE: i32      = 0x0C;
const VIRTIO_PCI_QUEUE_SEL: i32       = 0x0E;
const VIRTIO_PCI_QUEUE_NOTIFY: i32    = 0x10;
const VIRTIO_PCI_DEVICE_STATUS: i32   = 0x12;
const VIRTIO_PCI_ISR_STATUS: i32      = 0x13;
const VIRTIO_PCI_CONFIG: i32          = 0x14; // Device-specific config (MAC at +0)

// Device status bits
const STATUS_RESET: i32       = 0x00;
const STATUS_ACKNOWLEDGE: i32 = 0x01;
const STATUS_DRIVER: i32      = 0x02;
const STATUS_DRIVER_OK: i32   = 0x04;

// VirtIO-Net feature bits
const VIRTIO_NET_F_MAC: i32 = 1 << 5;

// Descriptor flags
const VRING_DESC_F_NEXT: u16  = 0x0001;
const VRING_DESC_F_WRITE: u16 = 0x0002;

// VirtIO net header size (legacy, no mergeable buffers)
const NET_HDR_SIZE: i32 = 10;

// Queue configuration
const RX_QUEUE: i32 = 0;
const TX_QUEUE: i32 = 1;
const RX_BUF_COUNT: i32 = 8;   // Number of RX buffers (keep small for WASM fuel)
const BUF_SIZE: i32 = 2048;    // Per-buffer size (header + frame)

// ── State ───────────────────────────────────────────────────────────────

static mut IO_BASE: i32 = 0;        // BAR0 I/O port base
static mut MAC: [u8; 6] = [0; 6];
static mut INITIALIZED: bool = false;

// DMA slot assignments
static mut RX_QUEUE_SLOT: i32 = -1;  // Virtqueue memory (descriptors + avail + used)
static mut TX_QUEUE_SLOT: i32 = -1;
static mut RX_BUF_SLOT: i32 = -1;   // RX packet buffers
static mut TX_BUF_SLOT: i32 = -1;   // TX packet buffer (1 page for single TX)

// Queue state
static mut RX_QUEUE_SIZE: u16 = 0;
static mut TX_QUEUE_SIZE: u16 = 0;
static mut RX_NEXT_AVAIL: u16 = 0;
static mut RX_LAST_USED: u16 = 0;
static mut TX_NEXT_AVAIL: u16 = 0;
static mut TX_LAST_USED: u16 = 0;
static mut RX_COUNT: u32 = 0;

fn log(msg: &[u8]) {
    unsafe { folk_log(msg.as_ptr() as i32, msg.len() as i32); }
}

// ── I/O helpers ─────────────────────────────────────────────────────────

fn io_read8(off: i32) -> u8 {
    unsafe { folk_inb(IO_BASE + off) as u8 }
}
fn io_read16(off: i32) -> u16 {
    unsafe { folk_inw(IO_BASE + off) as u16 }
}
fn io_read32(off: i32) -> u32 {
    unsafe { folk_inl(IO_BASE + off) as u32 }
}
fn io_write8(off: i32, val: u8) {
    unsafe { folk_outb(IO_BASE + off, val as i32); }
}
fn io_write16(off: i32, val: u16) {
    unsafe { folk_outw(IO_BASE + off, val as i32); }
}
fn io_write32(off: i32, val: u32) {
    unsafe { folk_outl(IO_BASE + off, val as i32); }
}

// ── Virtqueue helpers ───────────────────────────────────────────────────

/// Calculate offsets within a virtqueue DMA page
/// Layout: [descriptors: 16*N] [avail_ring: 6+2*N] [padding] [used_ring: 6+8*N]
fn desc_offset(idx: u16) -> i32 {
    (idx as i32) * 16 // Each descriptor = 16 bytes
}

fn avail_ring_offset(queue_size: u16) -> i32 {
    (queue_size as i32) * 16 // Right after descriptors
}

fn used_ring_offset(queue_size: u16) -> i32 {
    // Must be page-aligned: round up (descs + avail) to page boundary
    let avail_end = avail_ring_offset(queue_size) + 6 + 2 * (queue_size as i32);
    (avail_end + 4095) & !4095 // Round up to 4096
}

fn queue_total_size(queue_size: u16) -> i32 {
    used_ring_offset(queue_size) + 6 + 8 * (queue_size as i32)
}

/// Write a descriptor to the queue's DMA slot
fn write_desc(slot: i32, idx: u16, addr: i64, len: u32, flags: u16, next: u16) {
    let off = desc_offset(idx);
    unsafe {
        folk_dma_write_u64(slot, off, addr);
        folk_dma_write_u32(slot, off + 8, len as i32);
        folk_dma_write_u32(slot, off + 12, (flags as i32) | ((next as i32) << 16));
    }
}

/// Submit a descriptor to the available ring
fn submit_avail(slot: i32, queue_size: u16, next_avail: &mut u16, desc_idx: u16) {
    let avail_off = avail_ring_offset(queue_size);
    let ring_idx = (*next_avail % queue_size) as i32;
    unsafe {
        // Write ring entry: avail_off + 4 + ring_idx * 2
        let entry_off = avail_off + 4 + ring_idx * 2;
        // Write desc_idx as u16 at this offset (pack into u32 with existing data)
        let existing = folk_dma_read_u32(slot, entry_off & !3);
        let shift = (entry_off & 3) * 8;
        let mask = !(0xFFFF << shift);
        let new_val = (existing & mask as i32) | ((desc_idx as i32) << shift);
        folk_dma_write_u32(slot, entry_off & !3, new_val);

        // Update avail.idx (at avail_off + 2)
        *next_avail = next_avail.wrapping_add(1);
        let idx_off = avail_off + 2;
        let existing2 = folk_dma_read_u32(slot, idx_off & !3);
        let shift2 = (idx_off & 3) * 8;
        let mask2 = !(0xFFFF << shift2);
        let new_val2 = (existing2 & mask2 as i32) | ((*next_avail as i32) << shift2);
        folk_dma_write_u32(slot, idx_off & !3, new_val2);
    }
}

/// Check used ring for completions
fn pop_used(slot: i32, queue_size: u16, last_used: &mut u16) -> Option<(u16, u32)> {
    let used_off = used_ring_offset(queue_size);
    unsafe {
        // Read used.idx (at used_off + 2)
        let idx_word = folk_dma_read_u32(slot, used_off);
        let device_idx = ((idx_word >> 16) & 0xFFFF) as u16;

        if device_idx == *last_used {
            return None; // No new completions
        }

        // Read used ring entry: used_off + 4 + (last_used % queue_size) * 8
        let ring_idx = (*last_used % queue_size) as i32;
        let elem_off = used_off + 4 + ring_idx * 8;
        let id = folk_dma_read_u32(slot, elem_off) as u16;
        let len = folk_dma_read_u32(slot, elem_off + 4) as u32;

        *last_used = last_used.wrapping_add(1);
        Some((id, len))
    }
}

// ── Queue Setup ─────────────────────────────────────────────────────────

fn setup_queue(queue_idx: i32) -> (i32, u16) {
    // Select queue
    io_write16(VIRTIO_PCI_QUEUE_SEL, queue_idx as u16);
    let size = io_read16(VIRTIO_PCI_QUEUE_SIZE);
    if size == 0 {
        log(b"[VIRTIO] Queue size = 0!");
        return (-1, 0);
    }

    // Cap at 16 for WASM fuel efficiency
    let use_size = if size > 16 { 16 } else { size };

    // Allocate DMA memory for the queue
    let total = queue_total_size(use_size);
    let slot = unsafe { folk_dma_alloc(total) };
    if slot < 0 {
        log(b"[VIRTIO] Queue DMA alloc failed");
        return (-1, 0);
    }

    // Zero the queue memory
    let pages = (total + 4095) / 4096;
    for p in 0..pages {
        for w in 0..1024 {
            unsafe { folk_dma_write_u32(slot, p * 4096 + w * 4, 0); }
        }
    }

    // Initialize descriptor free chain
    for i in 0..use_size {
        let next = if i < use_size - 1 { i + 1 } else { 0xFFFF };
        write_desc(slot, i, 0, 0, 0, next);
    }

    // Sync to physical memory
    unsafe { folk_dma_sync_write(slot, 0, total); }

    // Tell device where the queue is (PFN = phys_addr / 4096)
    let phys = unsafe { folk_dma_phys(slot) };
    let pfn = (phys / 4096) as u32;
    io_write32(VIRTIO_PCI_QUEUE_PFN, pfn);

    (slot, use_size)
}

// ── Main Driver ─────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn driver_main() {
    unsafe {
        if INITIALIZED {
            // Resume: process RX/TX then yield
        } else {
            INITIALIZED = true;
            init();
        }

        // ── Work section: poll RX + send TX ──
        // Poll RX: check used ring for received packets
        while let Some((desc_idx, total_len)) = pop_used(RX_QUEUE_SLOT, RX_QUEUE_SIZE, &mut RX_LAST_USED) {
            if total_len > NET_HDR_SIZE as u32 {
                let frame_len = total_len - NET_HDR_SIZE as u32;
                let buf_offset = (desc_idx as i32) * BUF_SIZE + NET_HDR_SIZE;
                folk_net_submit_rx(RX_BUF_SLOT, buf_offset, frame_len as i32);
                RX_COUNT = RX_COUNT.saturating_add(1);
                if RX_COUNT <= 3 {
                    log(b"[VIRTIO] RX packet delivered!");
                }
            }
            // Recycle RX buffer
            let buf_phys = folk_dma_phys(RX_BUF_SLOT) + (desc_idx as i64 * BUF_SIZE as i64);
            write_desc(RX_QUEUE_SLOT, desc_idx, buf_phys, BUF_SIZE as u32, VRING_DESC_F_WRITE, 0);
            folk_dma_sync_write(RX_QUEUE_SLOT, desc_offset(desc_idx), 16);
            submit_avail(RX_QUEUE_SLOT, RX_QUEUE_SIZE, &mut RX_NEXT_AVAIL, desc_idx);
            folk_dma_sync_write(RX_QUEUE_SLOT, avail_ring_offset(RX_QUEUE_SIZE), 6 + 2 * RX_QUEUE_SIZE as i32);
            io_write16(VIRTIO_PCI_QUEUE_NOTIFY, RX_QUEUE as u16);
        }

        // Poll TX: check if kernel has packets to send
        if TX_BUF_SLOT >= 0 {
            // Drain completed TX descriptors
            while let Some((desc_idx, _)) = pop_used(TX_QUEUE_SLOT, TX_QUEUE_SIZE, &mut TX_LAST_USED) {
                // TX complete — descriptor can be reused
            }

            let tx_len = folk_net_poll_tx(TX_BUF_SLOT, NET_HDR_SIZE, BUF_SIZE - NET_HDR_SIZE);
            if tx_len > 0 {
                // Zero the 10-byte VirtIO header
                folk_dma_write_u64(TX_BUF_SLOT, 0, 0);
                folk_dma_write_u32(TX_BUF_SLOT, 8, 0); // Clear last 2 bytes of header + padding

                // Sync header + packet to physical memory
                folk_dma_sync_write(TX_BUF_SLOT, 0, NET_HDR_SIZE + tx_len);

                // Write TX descriptor (desc 0, always use slot 0 for simplicity)
                let buf_phys = folk_dma_phys(TX_BUF_SLOT);
                write_desc(TX_QUEUE_SLOT, 0, buf_phys, (NET_HDR_SIZE + tx_len) as u32, 0, 0);
                folk_dma_sync_write(TX_QUEUE_SLOT, 0, 16);

                submit_avail(TX_QUEUE_SLOT, TX_QUEUE_SIZE, &mut TX_NEXT_AVAIL, 0);
                folk_dma_sync_write(TX_QUEUE_SLOT, avail_ring_offset(TX_QUEUE_SIZE), 6 + 2 * TX_QUEUE_SIZE as i32);

                // Notify device
                io_write16(VIRTIO_PCI_QUEUE_NOTIFY, TX_QUEUE as u16);
            }
        }

        // Yield until next tick
        folk_ack_irq();
        folk_wait_irq();
    }
}

fn init() {
    log(b"[VIRTIO-NET] v1 starting");

    unsafe {
        // Get BAR0 I/O port base from device capability
        // BAR0 is an I/O port BAR (bit 0 set in PCI BAR register)
        // The driver runtime provides this via folk_bar_size
        // For VirtIO, BAR0 is I/O space
        // We need the actual port base — read from the PCI config via a host query
        // For now, hardcode detection: scan known VirtIO port ranges
        // Actually, the port base is in the DriverCapability's io_bars[0]
        // which is accessed via folk_inb/outb with the correct port number.
        // The host function validates against io_bars.

        // HACK: We need the BAR0 I/O base. The driver_runtime provides it
        // through the DriverCapability. But the WASM driver doesn't have direct
        // access to the capability struct. We need a host function to query it.
        // For now: try common VirtIO port bases.
        // On QEMU, VirtIO devices typically get I/O ports 0xC000-0xC0FF range.

        // Actually, the folk_inb/outb host functions validate against the
        // DriverCapability's io_bars. If we use port 0, the host function
        // adjusts based on the BAR. Let me check...
        // NO — folk_inb takes an absolute port number. We need the real port.

        // Read the I/O base from PCI config space via a device identity trick:
        // The device IRQ gives us info, but not the port base.
        // We need a new host function: folk_device_io_base(bar)
        // For now, let's use a detection approach: try reading STATUS from
        // candidate port bases until we find one that responds.

        // VirtIO devices typically get BAR0 at 0xC000-0xC0FF.
        // We'll try ports until STATUS register reads a valid value.

        // ACTUALLY: the correct approach is to have the host function use
        // bar-relative offsets, just like MMIO. Let me check if there's
        // folk_port_inb(bar, offset) — there is NOT. folk_inb takes an
        // absolute port.

        // The simplest fix: the DriverCapability has io_bars[0] = (port_base, size).
        // I need a host function to retrieve this. For now, log it and use 0.
        // TODO: add folk_device_io_base(bar) host function

        // Get BAR0 I/O port base from driver capability
        IO_BASE = folk_device_io_base(0);
        if IO_BASE == 0 {
            log(b"[VIRTIO-NET] No I/O BAR0!");
            return;
        }

        // Step 1: Reset device
        io_write8(VIRTIO_PCI_DEVICE_STATUS, STATUS_RESET as u8);

        // Step 2: Acknowledge
        io_write8(VIRTIO_PCI_DEVICE_STATUS, STATUS_ACKNOWLEDGE as u8);

        // Step 3: Driver
        io_write8(VIRTIO_PCI_DEVICE_STATUS, (STATUS_ACKNOWLEDGE | STATUS_DRIVER) as u8);

        // Step 4: Feature negotiation
        let features = io_read32(VIRTIO_PCI_DEVICE_FEATURES);
        if (features & VIRTIO_NET_F_MAC as u32) == 0 {
            log(b"[VIRTIO-NET] No MAC feature!");
            return;
        }
        io_write32(VIRTIO_PCI_DRIVER_FEATURES, VIRTIO_NET_F_MAC as u32);

        // Step 5: Read MAC address (6 bytes at config offset)
        for i in 0..6 {
            MAC[i] = io_read8(VIRTIO_PCI_CONFIG + i as i32);
        }

        // Register with kernel network stack
        folk_net_register(
            MAC[0] as i32, MAC[1] as i32, MAC[2] as i32,
            MAC[3] as i32, MAC[4] as i32, MAC[5] as i32,
        );
        log(b"[VIRTIO-NET] MAC registered");

        // Step 6: Setup RX queue (queue 0)
        let (rx_slot, rx_size) = setup_queue(RX_QUEUE);
        if rx_slot < 0 { return; }
        RX_QUEUE_SLOT = rx_slot;
        RX_QUEUE_SIZE = rx_size;
        log(b"[VIRTIO-NET] RX queue ready");

        // Step 7: Setup TX queue (queue 1)
        let (tx_slot, tx_size) = setup_queue(TX_QUEUE);
        if tx_slot < 0 { return; }
        TX_QUEUE_SLOT = tx_slot;
        TX_QUEUE_SIZE = tx_size;
        log(b"[VIRTIO-NET] TX queue ready");

        // Step 8: Allocate RX buffers and populate queue
        RX_BUF_SLOT = folk_dma_alloc(RX_BUF_COUNT * BUF_SIZE);
        if RX_BUF_SLOT < 0 {
            log(b"[VIRTIO-NET] RX buf alloc failed");
            return;
        }

        let buf_phys = folk_dma_phys(RX_BUF_SLOT);
        for i in 0..RX_BUF_COUNT as u16 {
            let pkt_phys = buf_phys + (i as i64 * BUF_SIZE as i64);
            write_desc(RX_QUEUE_SLOT, i, pkt_phys, BUF_SIZE as u32, VRING_DESC_F_WRITE, 0);
            submit_avail(RX_QUEUE_SLOT, RX_QUEUE_SIZE, &mut RX_NEXT_AVAIL, i);
        }
        // Sync queue to physical memory
        folk_dma_sync_write(RX_QUEUE_SLOT, 0, queue_total_size(RX_QUEUE_SIZE));

        // Notify device of available RX buffers
        io_write16(VIRTIO_PCI_QUEUE_NOTIFY, RX_QUEUE as u16);
        log(b"[VIRTIO-NET] RX buffers populated");

        // Step 9: Allocate TX buffer (single page)
        TX_BUF_SLOT = folk_dma_alloc(BUF_SIZE);
        if TX_BUF_SLOT < 0 {
            log(b"[VIRTIO-NET] TX buf alloc failed");
            return;
        }

        // Step 10: Set DRIVER_OK — device is now live
        io_write8(VIRTIO_PCI_DEVICE_STATUS,
            (STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK) as u8);

        // Verify status
        let final_status = io_read8(VIRTIO_PCI_DEVICE_STATUS);
        if (final_status & 0x80) != 0 {
            log(b"[VIRTIO-NET] Device FAILED!");
            return;
        }

        log(b"[VIRTIO-NET] v1 init complete, DRIVER_OK");
    }
}
