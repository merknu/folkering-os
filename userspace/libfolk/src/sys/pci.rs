//! PCI Device Enumeration — Userspace interface to kernel PCI discovery
//!
//! Provides safe access to PCI device information discovered at boot.
//! This is the foundation for WASM-sandboxed device drivers:
//! the compositor reads PCI devices, constructs DriverCapability structs,
//! and injects them into wasmi Store contexts.

use crate::syscall::{syscall1, syscall2};

/// Syscall number for PCI enumeration
const SYS_PCI_ENUMERATE: u64 = 0xA0;

// Port I/O syscalls (capability-gated by kernel against PCI BARs)
const SYS_PORT_INB: u64 = 0xA1;
const SYS_PORT_INW: u64 = 0xA2;
const SYS_PORT_INL: u64 = 0xA3;
const SYS_PORT_OUTB: u64 = 0xA4;
const SYS_PORT_OUTW: u64 = 0xA5;
const SYS_PORT_OUTL: u64 = 0xA6;

/// PCI device info as seen from userspace (64 bytes, matches kernel layout)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PciDeviceInfo {
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub header_type: u8,
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
    pub bus: u8,
    pub device_num: u8,
    pub function: u8,
    pub capabilities_ptr: u8,
    _pad: u8,
    /// BAR physical addresses (up to 3 decoded BARs)
    /// Bit 32 set = I/O port BAR (lower 16 bits = port base)
    pub bar_addrs: [u64; 3],
    /// BAR sizes in bytes (all 6 BARs)
    pub bar_sizes: [u32; 6],
}

impl PciDeviceInfo {
    /// Check if this is a VirtIO device
    pub fn is_virtio(&self) -> bool {
        self.vendor_id == 0x1AF4
    }

    /// Get the PCI class name
    pub fn class_name(&self) -> &'static str {
        match self.class_code {
            0x00 => "Unclassified",
            0x01 => "Mass Storage",
            0x02 => "Network",
            0x03 => "Display",
            0x04 => "Multimedia",
            0x05 => "Memory",
            0x06 => "Bridge",
            0x07 => "Communication",
            0x08 => "System Peripheral",
            0x09 => "Input Device",
            0x0C => "Serial Bus",
            0x0D => "Wireless",
            0xFF => "Unassigned",
            _ => "Other",
        }
    }

    /// Check if BAR n is an I/O port BAR (bit 32 flag)
    pub fn bar_is_io(&self, index: usize) -> bool {
        index < 3 && (self.bar_addrs[index] & 0x1_0000_0000) != 0
    }

    /// Get BAR physical base address (MMIO)
    pub fn bar_mmio_base(&self, index: usize) -> u64 {
        if index < 3 && !self.bar_is_io(index) {
            self.bar_addrs[index]
        } else {
            0
        }
    }

    /// Get BAR I/O port base
    pub fn bar_io_port(&self, index: usize) -> u16 {
        if index < 3 && self.bar_is_io(index) {
            (self.bar_addrs[index] & 0xFFFF) as u16
        } else {
            0
        }
    }
}

/// Enumerate all PCI devices discovered at boot.
/// Returns the number of devices found (up to `buf.len()`).
pub fn enumerate(buf: &mut [PciDeviceInfo]) -> usize {
    let ptr = buf.as_mut_ptr() as u64;
    let size = (buf.len() * core::mem::size_of::<PciDeviceInfo>()) as u64;

    let ret = unsafe { syscall2(SYS_PCI_ENUMERATE, ptr, size) };

    if ret == u64::MAX { 0 } else { ret as usize }
}

/// Get PCI device count
pub fn device_count() -> usize {
    let mut buf: [PciDeviceInfo; 32] = unsafe { core::mem::zeroed() };
    enumerate(&mut buf)
}

// ============================================================================
// Capability-Gated Port I/O
//
// These functions perform x86 IN/OUT instructions via kernel syscalls.
// The kernel validates each port against PCI device I/O BAR ranges.
// Ports not belonging to any PCI device are REJECTED (returns u64::MAX).
// Kernel-reserved ports (PIC, PIT, PS/2, COM, PCI config) are BLOCKED.
// ============================================================================

/// Read byte from I/O port. Returns u64::MAX if port is not permitted.
pub fn port_inb(port: u16) -> Result<u8, ()> {
    let ret = unsafe { syscall1(SYS_PORT_INB, port as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as u8) }
}

/// Read 16-bit word from I/O port.
pub fn port_inw(port: u16) -> Result<u16, ()> {
    let ret = unsafe { syscall1(SYS_PORT_INW, port as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as u16) }
}

/// Read 32-bit dword from I/O port.
pub fn port_inl(port: u16) -> Result<u32, ()> {
    let ret = unsafe { syscall1(SYS_PORT_INL, port as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as u32) }
}

/// Write byte to I/O port. Returns Err if port is not permitted.
pub fn port_outb(port: u16, value: u8) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_PORT_OUTB, port as u64, value as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Write 16-bit word to I/O port.
pub fn port_outw(port: u16, value: u16) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_PORT_OUTW, port as u64, value as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Write 32-bit dword to I/O port.
pub fn port_outl(port: u16, value: u32) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_PORT_OUTL, port as u64, value as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

// ============================================================================
// IRQ Routing for WASM Drivers
// ============================================================================

const SYS_BIND_IRQ: u64 = 0xA7;
const SYS_ACK_IRQ: u64 = 0xA8;
const SYS_CHECK_IRQ: u64 = 0xA9;

/// Bind an IRQ line to the current task for interrupt notification.
/// Returns the IDT vector number assigned, or Err if binding failed.
pub fn bind_irq(irq_line: u8) -> Result<u8, ()> {
    let ret = unsafe { syscall1(SYS_BIND_IRQ, irq_line as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as u8) }
}

/// Acknowledge an IRQ (clear pending flag + unmask at IOAPIC).
/// Call this after the driver has finished processing the interrupt.
pub fn ack_irq(irq_line: u8) -> Result<(), ()> {
    let ret = unsafe { syscall1(SYS_ACK_IRQ, irq_line as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Allocate a contiguous DMA buffer.
/// Returns the physical address of the buffer, or Err if allocation failed.
/// The buffer is mapped at `vaddr` in the caller's address space with UC attributes.
pub fn dma_alloc(size: usize, vaddr: usize) -> Result<u64, ()> {
    let ret = unsafe { syscall2(0xAA, size as u64, vaddr as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(ret) }
}

/// Query IOMMU availability.
/// Returns (available, base_address).
pub fn iommu_status() -> (bool, u64) {
    let ret = unsafe { syscall1(0xAB, 0) };
    let available = (ret & 1) != 0;
    let base = ret & 0xFFFFFFFF_00000000;
    (available, base)
}

// ── WASM Network Driver Bridge (Phase 11) ──────────────────────────────

/// Register a WASM network driver with the kernel.
/// Initializes the smoltcp stack with the provided MAC address.
pub fn net_register(mac: &[u8; 6]) -> Result<(), ()> {
    let mac_lo = (mac[0] as u64) | ((mac[1] as u64) << 8)
        | ((mac[2] as u64) << 16) | ((mac[3] as u64) << 24);
    let mac_hi = (mac[4] as u64) | ((mac[5] as u64) << 8);
    let ret = unsafe { syscall2(0xAC, mac_lo, mac_hi) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Submit a received Ethernet frame to the kernel network stack.
/// `data` must be a valid slice in the caller's address space.
pub fn net_submit_rx(data: &[u8]) -> Result<(), ()> {
    let ret = unsafe { syscall2(0xAD, data.as_ptr() as u64, data.len() as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Poll for a packet to transmit from the kernel network stack.
/// Returns the number of bytes written to `buf`, or 0 if no packet available.
pub fn net_poll_tx(buf: &mut [u8]) -> usize {
    let ret = unsafe { syscall2(0xAE, buf.as_mut_ptr() as u64, buf.len() as u64) };
    if ret == u64::MAX { 0 } else { ret as usize }
}

/// Read physical memory via kernel HHDM (DMA coherency fallback).
/// Copies `len` bytes from `phys_addr` to `dest` buffer.
pub fn dma_sync_read(phys_addr: u64, dest: &mut [u8]) -> Result<usize, ()> {
    let len = dest.len().min(4096);
    let packed = (dest.as_mut_ptr() as u64) | ((len as u64) << 32);
    let ret = unsafe { syscall2(0xAF, phys_addr, packed) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

/// Read a u64 directly from physical memory via kernel HHDM.
pub fn dma_sync_read_u64(phys_addr: u64) -> u64 {
    unsafe { syscall2(0xAF, phys_addr, 0) }
}

/// Write to physical memory via kernel HHDM (DMA coherency for writes).
pub fn dma_sync_write(phys_addr: u64, src: &[u8]) -> Result<usize, ()> {
    let len = src.len().min(4096);
    let packed = (src.as_ptr() as u64) | ((len as u64) << 32);
    let ret = unsafe { syscall2(0xB1, phys_addr, packed) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

// ── OS Metrics for AI Introspection (Phase 11) ─────────────────────────

/// Query OS metrics from the kernel. Used by Draug, WASM apps, and AI agents.
/// metric_id: 0=network, 1=firewall, 2=uptime, 3=suspicious_count
pub fn os_metric(metric_id: u32) -> u64 {
    unsafe { syscall2(0xB2, metric_id as u64, 0) }
}

/// Get network status: (has_ip, ip_a, ip_b, ip_c, ip_d)
pub fn net_status() -> (bool, u8, u8, u8, u8) {
    let v = os_metric(0);
    let has_ip = (v & 1) != 0;
    let a = ((v >> 8) & 0xFF) as u8;
    let b = ((v >> 16) & 0xFF) as u8;
    let c = ((v >> 24) & 0xFF) as u8;
    let d = ((v >> 32) & 0xFF) as u8;
    (has_ip, a, b, c, d)
}

/// Get firewall stats: (allows, drops)
pub fn firewall_stats() -> (u32, u32) {
    let v = os_metric(1);
    ((v & 0xFFFFFFFF) as u32, ((v >> 32) & 0xFFFFFFFF) as u32)
}

/// Get uptime in milliseconds
pub fn uptime_ms() -> u64 {
    os_metric(2)
}

/// Get suspicious packet count
pub fn suspicious_count() -> u32 {
    os_metric(3) as u32
}

/// Kernel-assisted DMA RX: reads descriptor + packet from physical memory and
/// delivers directly to smoltcp. Bypasses ALL cache coherency issues.
pub fn net_dma_rx(ring_phys: u64, desc_idx: u16, buf_phys: u64, buf_size: u16) -> usize {
    let arg1 = ring_phys | ((desc_idx as u64) << 48);
    let arg2 = buf_phys | ((buf_size as u64) << 48);
    let ret = unsafe { syscall2(0xB0, arg1, arg2) };
    if ret == u64::MAX { 0 } else { ret as usize }
}

/// Check if a bound IRQ has fired (non-blocking).
/// Returns true if an interrupt is pending, false if not.
pub fn check_irq(irq_line: u8) -> Result<bool, ()> {
    let ret = unsafe { syscall1(SYS_CHECK_IRQ, irq_line as u64) };
    match ret {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(()),
    }
}
