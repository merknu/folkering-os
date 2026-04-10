//! PCI / port-I/O / IRQ binding syscalls (Phase 10 — WASM driver bridge).
//!
//! - PCI device enumeration to userspace
//! - Capability-gated port I/O (in/out × b/w/l)
//! - IRQ binding for WASM-driver tasks (signal_irq is called from IDT handlers)

/// Compact PCI device info for userspace (64 bytes, C-repr)
#[repr(C)]
#[derive(Clone, Copy)]
struct PciDeviceUserInfo {
    vendor_id: u16,       // 0
    device_id: u16,       // 2
    class_code: u8,       // 4
    subclass: u8,         // 5
    prog_if: u8,          // 6
    revision: u8,         // 7
    header_type: u8,      // 8
    interrupt_line: u8,   // 9
    interrupt_pin: u8,    // 10
    bus: u8,              // 11
    device: u8,           // 12
    function: u8,         // 13
    capabilities_ptr: u8, // 14
    _pad: u8,             // 15
    bar_addrs: [u64; 3],  // 16-39: BAR physical addresses (MMIO base, decoded)
    bar_sizes: [u32; 6],  // 40-63: BAR sizes in bytes
}

pub fn syscall_pci_enumerate(buf_ptr: u64, buf_size: u64) -> u64 {
    let entry_size = core::mem::size_of::<PciDeviceUserInfo>();
    let max_entries = (buf_size as usize) / entry_size;

    if buf_ptr < 0x200000 || buf_ptr >= 0xFFFF_8000_0000_0000 || max_entries == 0 {
        return u64::MAX;
    }

    let list = crate::drivers::pci::PCI_DEVICES.lock();
    let mut written = 0usize;

    for i in 0..list.count.min(max_entries) {
        if let Some(ref dev) = list.devices[i] {
            let mut bar_addrs = [0u64; 3];
            let mut bar_sizes = [0u32; 6];

            for b in 0..6 {
                bar_sizes[b] = crate::drivers::pci::bar_size(dev.bus, dev.device, dev.function, b as u8);
                match crate::drivers::pci::decode_bar(dev, b) {
                    crate::drivers::pci::BarType::Mmio32 { base, .. } => {
                        if b < 3 { bar_addrs[b] = base as u64; }
                    }
                    crate::drivers::pci::BarType::Mmio64 { base, .. } => {
                        if b < 3 { bar_addrs[b] = base; }
                    }
                    crate::drivers::pci::BarType::Io { base } => {
                        if b < 3 { bar_addrs[b] = base as u64 | 0x1_0000_0000; }
                    }
                    crate::drivers::pci::BarType::None => {}
                }
            }

            let info = PciDeviceUserInfo {
                vendor_id: dev.vendor_id,
                device_id: dev.device_id,
                class_code: dev.class_code,
                subclass: dev.subclass,
                prog_if: dev.prog_if,
                revision: dev.revision,
                header_type: dev.header_type,
                interrupt_line: dev.interrupt_line,
                interrupt_pin: dev.interrupt_pin,
                bus: dev.bus,
                device: dev.device,
                function: dev.function,
                capabilities_ptr: dev.capabilities_ptr,
                _pad: 0,
                bar_addrs,
                bar_sizes,
            };

            let dest = (buf_ptr as usize) + written * entry_size;
            unsafe {
                let src = &info as *const PciDeviceUserInfo as *const u8;
                let dst = dest as *mut u8;
                core::ptr::copy_nonoverlapping(src, dst, entry_size);
            }
            written += 1;
        }
    }

    crate::serial_str!("[PCI] Enumerated ");
    crate::drivers::serial::write_dec(written as u32);
    crate::serial_strln!(" devices to userspace");

    written as u64
}

// ── Capability-Gated Port I/O ──────────────────────────────────────────

/// Check if a port is within a known PCI device's I/O BAR range.
/// Returns true if the port is permitted for userspace access.
fn port_io_allowed(port: u16) -> bool {
    // Blocklist: kernel-critical ports
    match port {
        0x0020..=0x0021 => return false, // PIC1
        0x00A0..=0x00A1 => return false, // PIC2
        0x0040..=0x0043 => return false, // PIT
        0x0060 | 0x0064 => return false, // PS/2
        0x0070..=0x0071 => return false, // CMOS
        0x03F8..=0x03FF => return false, // COM1
        0x02F8..=0x02FF => return false, // COM2
        0x03E8..=0x03EF => return false, // COM3
        0x0CF8..=0x0CFF => return false, // PCI config
        _ => {}
    }

    // Allowlist: check PCI device I/O BARs
    let list = crate::drivers::pci::PCI_DEVICES.lock();
    for i in 0..list.count {
        if let Some(ref dev) = list.devices[i] {
            for bar_idx in 0..6u8 {
                let bar_val = dev.bars[bar_idx as usize];
                if bar_val & 1 != 0 {
                    let base = (bar_val & 0xFFFC) as u16;
                    let size = crate::drivers::pci::bar_size(
                        dev.bus, dev.device, dev.function, bar_idx
                    ) as u16;
                    if size > 0 && port >= base && port < base.saturating_add(size) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

pub fn syscall_port_inb(port: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u8>::new(port);
        p.read() as u64
    }
}

pub fn syscall_port_inw(port: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u16>::new(port);
        p.read() as u64
    }
}

pub fn syscall_port_inl(port: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u32>::new(port);
        p.read() as u64
    }
}

pub fn syscall_port_outb(port: u64, value: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u8>::new(port);
        p.write(value as u8);
    }
    0
}

pub fn syscall_port_outw(port: u64, value: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u16>::new(port);
        p.write(value as u16);
    }
    0
}

pub fn syscall_port_outl(port: u64, value: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u32>::new(port);
        p.write(value as u32);
    }
    0
}

// ── IRQ Routing for WASM Drivers ───────────────────────────────────────
//
// Binding table: maps IDT vector → task_id + pending flag.
// When an interrupt fires, the IDT handler sets the pending flag.
// Userspace polls via SYS_CHECK_IRQ (non-blocking) or uses HLT + poll.

const MAX_IRQ_BINDINGS: usize = 24;
const WASM_IRQ_BASE_VECTOR: u8 = 46;

struct IrqBinding {
    vector: u8,
    task_id: u32,
    pending: bool,
    active: bool,
}

static IRQ_BINDINGS: spin::Mutex<[IrqBinding; MAX_IRQ_BINDINGS]> = spin::Mutex::new({
    const EMPTY: IrqBinding = IrqBinding { vector: 0, task_id: 0, pending: false, active: false };
    [EMPTY; MAX_IRQ_BINDINGS]
});

/// Called from IDT handlers to signal a bound IRQ.
/// Sets the pending flag so userspace can detect it via poll.
pub fn signal_irq(vector: u8) {
    let idx = vector.wrapping_sub(WASM_IRQ_BASE_VECTOR) as usize;
    if idx < MAX_IRQ_BINDINGS {
        if let Some(mut bindings) = IRQ_BINDINGS.try_lock() {
            if bindings[idx].active && bindings[idx].vector == vector {
                bindings[idx].pending = true;
            }
        }
        // If lock fails (contention from nested IRQ), the signal is lost.
        // Acceptable: hardware will re-assert level-triggered interrupts.
    }
}

pub fn syscall_bind_irq(irq_line: u64, _reserved: u64) -> u64 {
    let irq = irq_line as u8;
    let task_id = crate::task::task::get_current_task();

    if irq >= MAX_IRQ_BINDINGS as u8 {
        crate::serial_strln!("[IRQ] Bind failed: IRQ line out of range");
        return u64::MAX;
    }

    let vector = WASM_IRQ_BASE_VECTOR + irq;
    let idx = irq as usize;

    {
        let mut bindings = IRQ_BINDINGS.lock();
        bindings[idx] = IrqBinding {
            vector,
            task_id,
            pending: false,
            active: true,
        };
    }

    crate::arch::x86_64::ioapic::enable_irq_level(irq, vector);

    crate::serial_str!("[IRQ] Bound IRQ");
    crate::drivers::serial::write_dec(irq as u32);
    crate::serial_str!(" -> vector ");
    crate::drivers::serial::write_dec(vector as u32);
    crate::serial_str!(" for task ");
    crate::drivers::serial::write_dec(task_id);
    crate::serial_strln!("");

    vector as u64
}

pub fn syscall_ack_irq(irq_line: u64) -> u64 {
    let irq = irq_line as u8;
    let idx = irq as usize;

    if idx >= MAX_IRQ_BINDINGS { return u64::MAX; }

    {
        let mut bindings = IRQ_BINDINGS.lock();
        if bindings[idx].active {
            bindings[idx].pending = false;
        }
    }

    let vector = WASM_IRQ_BASE_VECTOR + irq;
    crate::arch::x86_64::ioapic::enable_irq_level(irq, vector);

    0
}

pub fn syscall_check_irq(irq_line: u64) -> u64 {
    let idx = irq_line as usize;
    if idx >= MAX_IRQ_BINDINGS { return u64::MAX; }

    let bindings = IRQ_BINDINGS.lock();
    if !bindings[idx].active { return u64::MAX; }
    if bindings[idx].pending { 1 } else { 0 }
}
