//! WASM Driver Runtime — Sandboxed hardware driver execution
//!
//! Implements the Autonomous Driver Generation architecture from the blueprint.
//! WASM drivers execute in wasmi with a `DriverCapability` context that
//! enforces Software Fault Isolation (SFI) at every host function boundary.
//!
//! # Architecture
//!
//! ```text
//! LLM generates driver.rs → compile → driver.wasm
//!                                         ↓
//! wasmi::Store<DriverCapability> ← driver_main()
//!   ├─ folk_mmio_read_u32(bar, offset)  → validated volatile read
//!   ├─ folk_mmio_write_u32(bar, offset, val) → validated volatile write
//!   ├─ folk_inb(port) / folk_outb(port, val) → capability-gated I/O
//!   ├─ folk_wait_irq() → yields via call_resumable
//!   ├─ folk_ack_irq() → unmasks interrupt
//!   └─ folk_log(ptr, len) → serial debug output
//! ```
//!
//! # Security Model (seL4-inspired)
//!
//! Each driver's `DriverCapability` is locked to ONE PCI device.
//! Host functions reject any access outside the device's BARs/ports.
//! WASM sandbox prevents code execution, memory escape, and stack smashing.
//! Fuel metering prevents CPU monopolization.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use wasmi::*;

/// Fuel budget per driver execution slice (less than apps — drivers should be efficient)
/// Driver fuel: unlimited. Drivers yield cooperatively via folk_wait_irq().
/// Fuel metering is NOT used for drivers (they have DMA/MMIO which is expensive in wasmi).
const DRIVER_FUEL: u64 = 0; // 0 = skip set_fuel entirely

/// Maximum MMIO BARs tracked per device
const MAX_BARS: usize = 6;

// ── DriverCapability: The SFI Boundary ──────────────────────────────────

/// Hardware capability tree for a single PCI device.
// ── Driver Version Control ──────────────────────────────────────────────

/// How a driver was created
#[derive(Clone, Copy, PartialEq)]
pub enum DriverSource {
    Jit,        // LLM-generated on demand
    AutoDream,  // improved by Draug daemon
    Bootstrap,  // handwritten baseline
}

/// Stability metadata for a stored driver version
#[derive(Clone)]
pub struct DriverMeta {
    pub vendor_id: u16,
    pub device_id: u16,
    pub version: u16,
    pub stability_score: u16,   // 0-1000, higher = more stable
    pub irq_count: u32,         // total IRQs processed
    pub uptime_ticks: u32,      // compositor frames alive without fault
    pub fault_count: u16,       // total faults/traps
    pub source: DriverSource,
}

impl DriverMeta {
    pub fn new(vendor_id: u16, device_id: u16, version: u16, source: DriverSource) -> Self {
        Self { vendor_id, device_id, version, stability_score: 0,
               irq_count: 0, uptime_ticks: 0, fault_count: 0, source }
    }

    /// Recalculate stability: (uptime * 10) / (faults + 1), capped at 1000
    pub fn recalc_stability(&mut self) {
        let score = (self.uptime_ticks as u64 * 10) / (self.fault_count as u64 + 1);
        self.stability_score = score.min(1000) as u16;
    }

    /// Format VFS filename: "driver_8086_100e_v1.wasm"
    pub fn vfs_filename(&self) -> alloc::string::String {
        alloc::format!("driver_{:04x}_{:04x}_v{}.wasm",
            self.vendor_id, self.device_id, self.version)
    }
}

/// Format a driver VFS filename from vendor/device/version
pub fn driver_vfs_filename(vendor_id: u16, device_id: u16, version: u16) -> alloc::string::String {
    alloc::format!("driver_{:04x}_{:04x}_v{}.wasm", vendor_id, device_id, version)
}

/// Find the latest version number for a device in Synapse VFS.
/// Probes v1, v2, ... until not found. Returns 0 if no versions exist.
pub fn find_latest_version(vendor_id: u16, device_id: u16) -> u16 {
    let mut latest: u16 = 0;
    for v in 1..=32u16 {
        let fname = driver_vfs_filename(vendor_id, device_id, v);
        match libfolk::sys::synapse::read_file_by_name(fname.as_str()) {
            Ok(_) => latest = v,
            Err(_) => break,
        }
    }
    latest
}

/// Store a driver WASM binary in Synapse VFS with intent metadata.
pub fn store_driver_vfs(vendor_id: u16, device_id: u16, version: u16,
                        wasm_bytes: &[u8], source: DriverSource) -> bool {
    let fname = driver_vfs_filename(vendor_id, device_id, version);
    match libfolk::sys::synapse::write_file(fname.as_str(), wasm_bytes) {
        Ok(()) => libfolk::println!("[DRV-VFS] write_file OK: {}", fname),
        Err(e) => libfolk::println!("[DRV-VFS] write_file FAILED: {} (err={:?})", fname, e),
    }
    // Tag with driver intent metadata
    let source_str = match source {
        DriverSource::Jit => "jit",
        DriverSource::AutoDream => "autodream",
        DriverSource::Bootstrap => "bootstrap",
    };
    let intent_json = alloc::format!(
        r#"{{"type":"driver","vendor":"{:04x}","device":"{:04x}","version":{},"source":"{}"}}"#,
        vendor_id, device_id, version, source_str
    );
    // Best-effort intent write (file_id may not match exactly, use 0)
    let _ = libfolk::sys::synapse::write_intent(0, "application/wasm-driver", &intent_json);
    true
}

/// Load a driver WASM binary from Synapse VFS.
pub fn load_driver_vfs(vendor_id: u16, device_id: u16, version: u16) -> Option<alloc::vec::Vec<u8>> {
    let fname = driver_vfs_filename(vendor_id, device_id, version);
    match libfolk::sys::synapse::read_file_shmem(fname.as_str()) {
        Ok(resp) => {
            const DRV_VFS_VADDR: usize = 0x5008_0000;
            if libfolk::sys::shmem_map(resp.shmem_handle, DRV_VFS_VADDR).is_ok() {
                let data = unsafe {
                    core::slice::from_raw_parts(DRV_VFS_VADDR as *const u8, resp.size as usize)
                };
                let result = alloc::vec::Vec::from(data);
                let _ = libfolk::sys::shmem_unmap(resp.shmem_handle, DRV_VFS_VADDR);
                let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
                Some(result)
            } else {
                let _ = libfolk::sys::shmem_destroy(resp.shmem_handle);
                None
            }
        }
        Err(_) => None,
    }
}

// ── Bootstrap Drivers ───────────────────────────────────────────────────

/// Compiled E1000 bootstrap driver v1 (1026 bytes — basic reset/IRQ loop)
const E1000_V1_WASM: &[u8] = include_bytes!("../../../drivers/e1000_bootstrap_v1.wasm");
/// Compiled E1000 v2 driver — DMA RX/TX with ARP announce
const E1000_V2_WASM: &[u8] = include_bytes!("../../../drivers/e1000_v2.wasm");
/// Compiled VirtIO-Net v1 driver — virtqueue-based, cloud-native
const VIRTIO_NET_V1_WASM: &[u8] = include_bytes!("../../../drivers/virtio_net_v1.wasm");

/// Bootstrap drivers: (vendor, device, versions)
const BOOTSTRAP_DRIVERS: &[(u16, u16, &[(&[u8], DriverSource)])] = &[
    (0x8086, 0x100E, &[
        (E1000_V1_WASM, DriverSource::Bootstrap),
        (E1000_V2_WASM, DriverSource::Bootstrap),
    ]),
    (0x1AF4, 0x1000, &[ // VirtIO-Net (legacy/transitional)
        (VIRTIO_NET_V1_WASM, DriverSource::Bootstrap),
    ]),
];

/// Get a built-in driver for a specific PCI device (fallback when VFS fails).
/// Returns the latest compiled WASM for the device, or None.
pub fn get_builtin_driver(vendor_id: u16, device_id: u16) -> Option<&'static [u8]> {
    for &(vid, did, versions) in BOOTSTRAP_DRIVERS {
        if vid == vendor_id && did == device_id {
            // Return the LAST (latest) version
            return versions.last().map(|&(wasm, _)| wasm);
        }
    }
    None
}

/// Seed Synapse VFS with bootstrap drivers for detected hardware.
/// Called once at compositor startup. Seeds ALL versions that don't exist yet.
pub fn seed_bootstrap_drivers(pci_devices: &[libfolk::sys::pci::PciDeviceInfo], count: usize) {
    for &(vid, did, versions) in BOOTSTRAP_DRIVERS {
        let present = pci_devices[..count].iter().any(|d| d.vendor_id == vid && d.device_id == did);
        if !present { continue; }

        let existing = find_latest_version(vid, did);
        if existing >= versions.len() as u16 {
            libfolk::println!("[DRV-BOOT] {:04x}:{:04x} has v{} (up to date)", vid, did, existing);
            continue;
        }

        // Seed missing versions
        for (i, &(wasm, source)) in versions.iter().enumerate() {
            let v = (i + 1) as u16;
            if v <= existing { continue; } // Already in VFS
            store_driver_vfs(vid, did, v, wasm, source);
            libfolk::println!("[DRV-BOOT] Seeded {:04x}:{:04x} v{} ({} bytes)", vid, did, v, wasm.len());
        }
    }
}

// ── Driver Capability (SFI Boundary) ────────────────────────────────────

/// Stored in wasmi::Store — invisible and immutable to WASM bytecode.
/// Every host function validates access against these bounds.
#[derive(Clone)]
pub struct DriverCapability {
    // PCI identity
    pub vendor_id: u16,
    pub device_id: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub irq_line: u8,

    // MMIO regions: (physical_base, size) per BAR
    // Physical addresses are mapped into kernel virtual space for volatile access
    pub mmio_bars: [(u64, u32); MAX_BARS],

    // I/O port ranges: (base_port, size) per BAR
    pub io_bars: [(u16, u16); MAX_BARS],

    // Virtual addresses where MMIO BARs are mapped (set by map_device_bars)
    pub mmio_vaddrs: [usize; MAX_BARS],

    // Driver name (for logging)
    pub name: [u8; 32],
    pub name_len: usize,
}

impl DriverCapability {
    /// Create from a PciDeviceInfo (userspace struct)
    pub fn from_pci(dev: &libfolk::sys::pci::PciDeviceInfo) -> Self {
        let mut cap = Self {
            vendor_id: dev.vendor_id,
            device_id: dev.device_id,
            bus: dev.bus,
            device: dev.device_num,
            function: dev.function,
            irq_line: dev.interrupt_line,
            mmio_bars: [(0, 0); MAX_BARS],
            io_bars: [(0, 0); MAX_BARS],
            mmio_vaddrs: [0; MAX_BARS],
            name: [0; 32],
            name_len: 0,
        };

        for i in 0..3 {
            let size = dev.bar_sizes[i];
            if size == 0 { continue; }

            if dev.bar_is_io(i) {
                cap.io_bars[i] = (dev.bar_io_port(i), size as u16);
            } else {
                cap.mmio_bars[i] = (dev.bar_mmio_base(i), size);
            }
        }
        // BARs 3-5 only have sizes (no decoded addrs in PciDeviceInfo.bar_addrs[3])
        for i in 3..6 {
            cap.io_bars[i] = (0, 0);
            cap.mmio_bars[i] = (0, 0);
        }

        cap
    }

    /// Set driver name for logging
    pub fn set_name(&mut self, name: &str) {
        let n = name.len().min(31);
        self.name[..n].copy_from_slice(&name.as_bytes()[..n]);
        self.name_len = n;
    }

    /// Get driver name
    pub fn driver_name(&self) -> &str {
        unsafe { ::core::str::from_utf8_unchecked(&self.name[..self.name_len]) }
    }

    /// Validate MMIO access: bar_index valid, offset+size within BAR bounds
    fn validate_mmio(&self, bar: u8, offset: u32, size: u32) -> Option<usize> {
        let idx = bar as usize;
        if idx >= MAX_BARS { return None; }

        let (_, bar_size) = self.mmio_bars[idx];
        if bar_size == 0 { return None; }

        // Bounds check: offset + size must not exceed BAR size
        if offset.checked_add(size).map_or(true, |end| end > bar_size) {
            return None;
        }

        // Alignment check for multi-byte access
        if size > 1 && (offset % size) != 0 {
            return None;
        }

        let vaddr = self.mmio_vaddrs[idx];
        if vaddr == 0 { return None; }

        Some(vaddr + offset as usize)
    }

    /// Validate I/O port access: port within a known BAR range
    fn validate_port(&self, port: u16) -> bool {
        for (base, size) in &self.io_bars {
            if *size > 0 && port >= *base && port < base.saturating_add(*size) {
                return true;
            }
        }
        false
    }
}

// ── Driver State (wraps DriverCapability + runtime flags) ───────────────

/// Max DMA allocations per driver (rings + buffers)
const MAX_DMA_SLOTS: usize = 8;
/// Virtual address base for DMA slots: 0x6000_0000 + slot * 0x0001_0000 (64KB each)
/// Must not conflict with shmem (0x3000_0000), WASM linear memory, or framebuffer.
const DMA_VADDR_BASE: usize = 0x6000_0000;
const DMA_SLOT_SIZE: usize = 0x0001_0000; // 64KB per slot

/// A DMA buffer allocation tracked by the host
#[derive(Clone, Copy)]
struct DmaSlot {
    phys: u64,    // physical address (for MMIO register setup)
    vaddr: usize, // mapped virtual address (for host read/write)
    size: usize,  // allocated size in bytes
    active: bool,
}

impl DmaSlot {
    const EMPTY: Self = Self { phys: 0, vaddr: 0, size: 0, active: false };
}

struct DriverState {
    cap: DriverCapability,
    irq_pending: bool,
    log_buf: [u8; 128],
    /// DMA buffer pool: each slot mapped at a fixed vaddr range
    dma_slots: [DmaSlot; MAX_DMA_SLOTS],
}

// ── Host Function Registration ──────────────────────────────────────────

/// Register all driver host functions into a wasmi Linker.
/// These are the folk_* ABI functions from the blueprint's Section 7.
fn register_driver_functions(linker: &mut Linker<DriverState>) {
    // ── 7.1: Legacy Port I/O ──────────────────────────────────────────

    let _ = linker.func_wrap("env", "folk_inb",
        |caller: Caller<DriverState>, port: i32| -> i32 {
            let p = port as u16;
            if !caller.data().cap.validate_port(p) {
                return -1; // SFI violation
            }
            match libfolk::sys::pci::port_inb(p) {
                Ok(v) => v as i32,
                Err(_) => -1,
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_inw",
        |caller: Caller<DriverState>, port: i32| -> i32 {
            let p = port as u16;
            if !caller.data().cap.validate_port(p) { return -1; }
            match libfolk::sys::pci::port_inw(p) {
                Ok(v) => v as i32,
                Err(_) => -1,
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_inl",
        |caller: Caller<DriverState>, port: i32| -> i32 {
            let p = port as u16;
            if !caller.data().cap.validate_port(p) { return -1; }
            match libfolk::sys::pci::port_inl(p) {
                Ok(v) => v as i32,
                Err(_) => -1,
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_outb",
        |caller: Caller<DriverState>, port: i32, value: i32| {
            let p = port as u16;
            if caller.data().cap.validate_port(p) {
                let _ = libfolk::sys::pci::port_outb(p, value as u8);
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_outw",
        |caller: Caller<DriverState>, port: i32, value: i32| {
            let p = port as u16;
            if caller.data().cap.validate_port(p) {
                let _ = libfolk::sys::pci::port_outw(p, value as u16);
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_outl",
        |caller: Caller<DriverState>, port: i32, value: i32| {
            let p = port as u16;
            if caller.data().cap.validate_port(p) {
                let _ = libfolk::sys::pci::port_outl(p, value as u32);
            }
        },
    );

    // ── 7.2: MMIO Interface (Host Proxy — Approach A) ─────────────────

    let _ = linker.func_wrap("env", "folk_mmio_read_u8",
        |caller: Caller<DriverState>, bar: i32, offset: i32| -> i32 {
            match caller.data().cap.validate_mmio(bar as u8, offset as u32, 1) {
                Some(vaddr) => unsafe {
                    ::core::ptr::read_volatile(vaddr as *const u8) as i32
                },
                None => -1,
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_mmio_read_u16",
        |caller: Caller<DriverState>, bar: i32, offset: i32| -> i32 {
            match caller.data().cap.validate_mmio(bar as u8, offset as u32, 2) {
                Some(vaddr) => unsafe {
                    ::core::ptr::read_volatile(vaddr as *const u16) as i32
                },
                None => -1,
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_mmio_read_u32",
        |caller: Caller<DriverState>, bar: i32, offset: i32| -> i32 {
            match caller.data().cap.validate_mmio(bar as u8, offset as u32, 4) {
                Some(vaddr) => unsafe {
                    ::core::ptr::read_volatile(vaddr as *const u32) as i32
                },
                None => -1,
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_mmio_write_u8",
        |caller: Caller<DriverState>, bar: i32, offset: i32, value: i32| {
            if let Some(vaddr) = caller.data().cap.validate_mmio(bar as u8, offset as u32, 1) {
                unsafe { ::core::ptr::write_volatile(vaddr as *mut u8, value as u8); }
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_mmio_write_u16",
        |caller: Caller<DriverState>, bar: i32, offset: i32, value: i32| {
            if let Some(vaddr) = caller.data().cap.validate_mmio(bar as u8, offset as u32, 2) {
                unsafe { ::core::ptr::write_volatile(vaddr as *mut u16, value as u16); }
            }
        },
    );

    let _ = linker.func_wrap("env", "folk_mmio_write_u32",
        |caller: Caller<DriverState>, bar: i32, offset: i32, value: i32| {
            if let Some(vaddr) = caller.data().cap.validate_mmio(bar as u8, offset as u32, 4) {
                unsafe { ::core::ptr::write_volatile(vaddr as *mut u32, value as u32); }
            }
        },
    );

    // ── 7.4: Interrupt Lifecycle ──────────────────────────────────────

    // folk_wait_irq: yields WASM execution back to host (via error)
    // The host catches this error and puts the driver to sleep.
    // When the IRQ fires, host resumes the driver via call_resumable.
    let _ = linker.func_wrap("env", "folk_wait_irq",
        |_caller: Caller<DriverState>| -> Result<(), Error> {
            // Return an error to yield execution — host matches on the message
            Err(Error::new("__YIELD_IRQ__"))
        },
    );

    // folk_ack_irq: acknowledge interrupt (unmask at APIC)
    let _ = linker.func_wrap("env", "folk_ack_irq",
        |mut caller: Caller<DriverState>| {
            caller.data_mut().irq_pending = false;
            // TODO: When IRQ routing is implemented (Phase 4),
            // this will send a syscall to unmask the APIC line.
        },
    );

    // ── Utility: Debug logging ────────────────────────────────────────

    let _ = linker.func_wrap("env", "folk_log",
        |mut caller: Caller<DriverState>, ptr: i32, len: i32| {
            if len <= 0 || len > 120 { return; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return,
            };
            let mut buf = [0u8; 120];
            let n = (len as usize).min(120);
            if mem.read(&caller, ptr as usize, &mut buf[..n]).is_ok() {
                if let Ok(s) = ::core::str::from_utf8(&buf[..n]) {
                    libfolk::println!("[DRV:{}] {}", caller.data().cap.driver_name(), s);
                }
            }
        },
    );

    // ── Device identity query ─────────────────────────────────────────

    let _ = linker.func_wrap("env", "folk_device_vendor_id",
        |caller: Caller<DriverState>| -> i32 {
            caller.data().cap.vendor_id as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_device_id",
        |caller: Caller<DriverState>| -> i32 {
            caller.data().cap.device_id as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_device_irq",
        |caller: Caller<DriverState>| -> i32 {
            caller.data().cap.irq_line as i32
        },
    );

    // folk_device_io_base(bar) -> i32: returns the I/O port base for a BAR
    let _ = linker.func_wrap("env", "folk_device_io_base",
        |caller: Caller<DriverState>, bar: i32| -> i32 {
            let idx = bar as usize;
            if idx >= MAX_BARS { return 0; }
            let (port, _) = caller.data().cap.io_bars[idx];
            port as i32
        },
    );

    let _ = linker.func_wrap("env", "folk_bar_size",
        |caller: Caller<DriverState>, bar: i32| -> i32 {
            let idx = bar as usize;
            if idx >= MAX_BARS { return 0; }
            let (_, size) = caller.data().cap.mmio_bars[idx];
            if size > 0 { return size as i32; }
            let (_, iosize) = caller.data().cap.io_bars[idx];
            iosize as i32
        },
    );

    // ── 7.3: DMA Memory Allocation (Slot-Based) ──────────────────────

    // folk_dma_alloc(size) -> i32
    // Allocates contiguous physical DMA memory.
    // Returns slot ID (0..7) or -1 on error.
    // Max 64KB per slot, 8 slots per driver.
    let _ = linker.func_wrap("env", "folk_dma_alloc",
        |mut caller: Caller<DriverState>, size: i32| -> i32 {
            if size <= 0 || size as usize > DMA_SLOT_SIZE { return -1; }

            // Find free slot
            let slot_idx = {
                let state = caller.data();
                state.dma_slots.iter().position(|s| !s.active)
            };
            let slot_idx = match slot_idx {
                Some(i) => i,
                None => return -1, // All slots used
            };

            let vaddr = DMA_VADDR_BASE + slot_idx * DMA_SLOT_SIZE;
            match libfolk::sys::pci::dma_alloc(size as usize, vaddr) {
                Ok(phys) => {
                    let state = caller.data_mut();
                    state.dma_slots[slot_idx] = DmaSlot {
                        phys, vaddr, size: size as usize, active: true,
                    };
                    libfolk::println!("[DMA] Slot {} alloc: {}B phys=0x{:08x} vaddr=0x{:08x}",
                        slot_idx, size, phys, vaddr);
                    slot_idx as i32
                }
                Err(_) => -1,
            }
        },
    );

    // folk_dma_phys(slot) -> i64
    // Returns physical address of a DMA slot (for MMIO register setup).
    let _ = linker.func_wrap("env", "folk_dma_phys",
        |caller: Caller<DriverState>, slot: i32| -> i64 {
            let idx = slot as usize;
            let state = caller.data();
            if idx < MAX_DMA_SLOTS && state.dma_slots[idx].active {
                state.dma_slots[idx].phys as i64
            } else {
                -1
            }
        },
    );

    // folk_dma_write_u32(slot, offset, value)
    // Write a u32 to DMA buffer (e.g., descriptor ring entry fields).
    let _ = linker.func_wrap("env", "folk_dma_write_u32",
        |caller: Caller<DriverState>, slot: i32, offset: i32, value: i32| {
            let idx = slot as usize;
            let off = offset as usize;
            let state = caller.data();
            if idx < MAX_DMA_SLOTS && state.dma_slots[idx].active && off + 4 <= state.dma_slots[idx].size {
                let ptr = (state.dma_slots[idx].vaddr + off) as *mut u32;
                unsafe { core::ptr::write_volatile(ptr, value as u32); }
            }
        },
    );

    // folk_dma_write_u64(slot, offset, value)
    // Write a u64 to DMA buffer (e.g., 64-bit buffer address in descriptor).
    let _ = linker.func_wrap("env", "folk_dma_write_u64",
        |caller: Caller<DriverState>, slot: i32, offset: i32, value: i64| {
            let idx = slot as usize;
            let off = offset as usize;
            let state = caller.data();
            if idx < MAX_DMA_SLOTS && state.dma_slots[idx].active && off + 8 <= state.dma_slots[idx].size {
                let ptr = (state.dma_slots[idx].vaddr + off) as *mut u64;
                unsafe { core::ptr::write_volatile(ptr, value as u64); }
            }
        },
    );

    // folk_dma_read_u32(slot, offset) -> i32
    // Read a u32 from DMA buffer (e.g., check descriptor status).
    let _ = linker.func_wrap("env", "folk_dma_read_u32",
        |caller: Caller<DriverState>, slot: i32, offset: i32| -> i32 {
            let idx = slot as usize;
            let off = offset as usize;
            let state = caller.data();
            if idx < MAX_DMA_SLOTS && state.dma_slots[idx].active && off + 4 <= state.dma_slots[idx].size {
                let ptr = (state.dma_slots[idx].vaddr + off) as *const u32;
                unsafe { core::ptr::read_volatile(ptr) as i32 }
            } else {
                -1
            }
        },
    );

    // folk_dma_read_u64(slot, offset) -> i64
    let _ = linker.func_wrap("env", "folk_dma_read_u64",
        |caller: Caller<DriverState>, slot: i32, offset: i32| -> i64 {
            let idx = slot as usize;
            let off = offset as usize;
            let state = caller.data();
            if idx < MAX_DMA_SLOTS && state.dma_slots[idx].active && off + 8 <= state.dma_slots[idx].size {
                let ptr = (state.dma_slots[idx].vaddr + off) as *const u64;
                unsafe { core::ptr::read_volatile(ptr) as i64 }
            } else {
                -1
            }
        },
    );

    // folk_dma_free(slot) — release a DMA slot
    let _ = linker.func_wrap("env", "folk_dma_free",
        |mut caller: Caller<DriverState>, slot: i32| {
            let idx = slot as usize;
            if idx < MAX_DMA_SLOTS {
                caller.data_mut().dma_slots[idx].active = false;
            }
        },
    );

    // folk_dma_sync_read(slot, offset, len) -> i32
    // Bulk-copy from physical DMA memory via kernel HHDM to the DMA buffer.
    let _ = linker.func_wrap("env", "folk_dma_sync_read",
        |caller: Caller<DriverState>, slot: i32, offset: i32, len: i32| -> i32 {
            let idx = slot as usize;
            let off = offset as usize;
            let sz = len as usize;
            let state = caller.data();
            if idx >= MAX_DMA_SLOTS || !state.dma_slots[idx].active
                || off + sz > state.dma_slots[idx].size {
                return -1;
            }
            let phys = state.dma_slots[idx].phys + off as u64;
            let dest = unsafe {
                core::slice::from_raw_parts_mut(
                    (state.dma_slots[idx].vaddr + off) as *mut u8, sz)
            };
            match libfolk::sys::pci::dma_sync_read(phys, dest) {
                Ok(_) => 0,
                Err(_) => -1,
            }
        },
    );

    // folk_dma_sync_read_u32(slot, offset) -> i32
    // Read a u32 directly from physical memory via kernel HHDM.
    // Returns the value WITHOUT touching the DMA buffer cache. Zero-overhead on real HW.
    let _ = linker.func_wrap("env", "folk_dma_sync_read_u32",
        |caller: Caller<DriverState>, slot: i32, offset: i32| -> i32 {
            let idx = slot as usize;
            let off = offset as usize;
            let state = caller.data();
            if idx >= MAX_DMA_SLOTS || !state.dma_slots[idx].active || off + 4 > state.dma_slots[idx].size {
                return -1;
            }
            let phys = state.dma_slots[idx].phys + off as u64;
            let val = libfolk::sys::pci::dma_sync_read_u64(phys);
            (val & 0xFFFFFFFF) as i32
        },
    );

    // folk_dma_sync_write(slot, offset, len) -> i32
    // Write DMA buffer content to physical memory via kernel HHDM.
    // Ensures DMA devices see the written data on WHPX/buggy hardware.
    let _ = linker.func_wrap("env", "folk_dma_sync_write",
        |caller: Caller<DriverState>, slot: i32, offset: i32, len: i32| -> i32 {
            let idx = slot as usize;
            let off = offset as usize;
            let sz = len as usize;
            let state = caller.data();
            if idx >= MAX_DMA_SLOTS || !state.dma_slots[idx].active || off + sz > state.dma_slots[idx].size {
                return -1;
            }
            let phys = state.dma_slots[idx].phys + off as u64;
            let src = unsafe {
                core::slice::from_raw_parts((state.dma_slots[idx].vaddr + off) as *const u8, sz)
            };
            match libfolk::sys::pci::dma_sync_write(phys, src) {
                Ok(_) => 0,
                Err(_) => -1,
            }
        },
    );

    // folk_net_dma_rx(ring_slot, buf_slot, desc_idx, buf_size) -> i32
    // Kernel-assisted RX: reads DMA descriptor + packet from physical memory
    // and delivers directly to smoltcp. Returns packet length or 0.
    let _ = linker.func_wrap("env", "folk_net_dma_rx",
        |caller: Caller<DriverState>, ring_slot: i32, buf_slot: i32, desc_idx: i32, buf_size: i32| -> i32 {
            let ri = ring_slot as usize;
            let bi = buf_slot as usize;
            let state = caller.data();
            if ri >= MAX_DMA_SLOTS || !state.dma_slots[ri].active
                || bi >= MAX_DMA_SLOTS || !state.dma_slots[bi].active {
                return 0;
            }
            let ring_phys = state.dma_slots[ri].phys;
            let buf_phys = state.dma_slots[bi].phys;
            libfolk::sys::pci::net_dma_rx(ring_phys, desc_idx as u16, buf_phys, buf_size as u16) as i32
        },
    );

    // folk_iommu_status() -> i32: 1 if IOMMU available, 0 if not
    let _ = linker.func_wrap("env", "folk_iommu_status",
        |_caller: Caller<DriverState>| -> i32 {
            let (available, _) = libfolk::sys::pci::iommu_status();
            if available { 1 } else { 0 }
        },
    );

    // ── 7.4: Network Stack Bridge ────────���───────────────────────────

    // folk_net_register(mac0, mac1, mac2, mac3, mac4, mac5)
    // Register this driver as the OS network interface. Starts smoltcp + DHCP.
    let _ = linker.func_wrap("env", "folk_net_register",
        |caller: Caller<DriverState>, m0: i32, m1: i32, m2: i32, m3: i32, m4: i32, m5: i32| {
            let mac = [m0 as u8, m1 as u8, m2 as u8, m3 as u8, m4 as u8, m5 as u8];
            let _ = libfolk::sys::pci::net_register(&mac);
            libfolk::println!("[NET-DRV] Registered MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
        },
    );

    // folk_net_submit_rx(dma_slot, offset, length)
    // Deliver a received Ethernet frame to the kernel network stack.
    let _ = linker.func_wrap("env", "folk_net_submit_rx",
        |caller: Caller<DriverState>, slot: i32, offset: i32, length: i32| -> i32 {
            let idx = slot as usize;
            let off = offset as usize;
            let len = length as usize;
            let state = caller.data();
            if idx >= MAX_DMA_SLOTS || !state.dma_slots[idx].active || off + len > state.dma_slots[idx].size {
                return -1;
            }
            // Read packet data from DMA buffer
            let src = (state.dma_slots[idx].vaddr + off) as *const u8;
            let data = unsafe { core::slice::from_raw_parts(src, len) };
            // Submit to kernel via syscall
            match libfolk::sys::pci::net_submit_rx(data) {
                Ok(()) => 0,
                Err(()) => -1,
            }
        },
    );

    // folk_net_poll_tx(dma_slot, offset, max_len) -> i32
    // Check if the kernel has a packet to transmit. If so, copies it to the DMA buffer.
    // Returns packet length (> 0) or 0 if no packet.
    let _ = linker.func_wrap("env", "folk_net_poll_tx",
        |caller: Caller<DriverState>, slot: i32, offset: i32, max_len: i32| -> i32 {
            let idx = slot as usize;
            let off = offset as usize;
            let max = max_len as usize;
            let state = caller.data();
            if idx >= MAX_DMA_SLOTS || !state.dma_slots[idx].active || off + max > state.dma_slots[idx].size {
                return 0;
            }
            // Poll kernel for outgoing packet via syscall
            let dst = (state.dma_slots[idx].vaddr + off) as *mut u8;
            let buf = unsafe { core::slice::from_raw_parts_mut(dst, max) };
            libfolk::sys::pci::net_poll_tx(buf) as i32
        },
    );
}

// ── MMIO BAR Mapping ────────────────────────────────────────────────────

/// Map a device's MMIO BARs into the current process's virtual address space.
/// Uses SYS_MAP_PHYSICAL with Uncacheable attributes for correct MMIO semantics.
/// Returns the number of BARs successfully mapped.
pub fn map_device_bars(cap: &mut DriverCapability) -> usize {
    use libfolk::sys::map_physical::{map_physical, MapFlags};

    let mut mapped = 0;
    // Virtual addresses for BAR mapping — use high userspace region
    // 0x40000000 may conflict with WASM memory. Use 0x70000000 instead.
    let base_vaddr: usize = 0x7000_0000;

    for i in 0..MAX_BARS {
        let (phys, size) = cap.mmio_bars[i];
        if phys == 0 || size == 0 { continue; }

        let vaddr = base_vaddr + i * 0x10_0000; // 1MB apart per BAR

        // Map with READ + WRITE + CACHE_UC (Uncacheable for MMIO)
        let flags = MapFlags::READ | MapFlags::WRITE | MapFlags::CACHE_UC;
        match map_physical(phys, vaddr as u64, size as u64, flags) {
            Ok(()) => {
                cap.mmio_vaddrs[i] = vaddr;
                mapped += 1;
                libfolk::println!("[DRV] BAR{} mapped: phys={:#x} size={} vaddr={:#x}",
                    i, phys, size, vaddr);
            }
            Err(_) => {
                libfolk::println!("[DRV] BAR{} map FAILED: phys={:#x} size={}", i, phys, size);
            }
        }
    }

    mapped
}

// ── Driver Instantiation & Execution ────────────────────────────────────

/// Result of driver execution
#[derive(Debug)]
pub enum DriverResult {
    /// Driver completed normally (returned from driver_main)
    Completed,
    /// Driver yielded via folk_wait_irq (waiting for interrupt)
    WaitingForIrq,
    /// Driver hit fuel limit (preempted)
    OutOfFuel,
    /// Driver crashed (trap, OOB, div-by-zero, etc.)
    Trapped(String),
    /// Driver failed to load
    LoadError(String),
}

/// A running WASM driver instance with resumable execution.
///
/// Lifecycle:
/// 1. `new()` — load WASM, link host functions, bind IRQ
/// 2. `start()` — initial `call_resumable` of `driver_main`
/// 3. Driver calls `folk_wait_irq()` → Error("__YIELD_IRQ__") → returns `WaitingForIrq`
/// 4. Compositor polls `check_irq()` in main loop
/// 5. IRQ fires → `resume()` refuels + re-calls `driver_main`
/// 6. Repeat 3-5 until driver completes or traps
pub struct WasmDriver {
    store: Store<DriverState>,
    instance: Instance,
    pub capability: DriverCapability,
    /// IRQ line this driver is bound to (for polling)
    pub bound_irq: Option<u8>,
    /// True if driver is suspended waiting for IRQ
    pub waiting_for_irq: bool,
    /// Version control metadata (stability tracking)
    pub meta: DriverMeta,
}

impl WasmDriver {
    /// Create a new WASM driver for a PCI device.
    pub fn new(wasm_bytes: &[u8], cap: DriverCapability) -> Result<Self, String> {
        let engine = Engine::default();
        let mut store = Store::new(&engine, DriverState {
            cap: cap.clone(),
            irq_pending: false,
            log_buf: [0; 128],
            dma_slots: [DmaSlot::EMPTY; MAX_DMA_SLOTS],
        });
        store.set_fuel(10_000_000).unwrap_or(());

        let module = Module::new(&engine, wasm_bytes)
            .map_err(|e| format!("Module load: {:?}", e))?;

        let mut linker = <Linker<DriverState>>::new(&engine);
        register_driver_functions(&mut linker);

        let instance = linker.instantiate_and_start(&mut store, &module)
            .map_err(|e| format!("Instantiate: {:?}", e))?;

        let meta = DriverMeta::new(cap.vendor_id, cap.device_id, 0, DriverSource::Jit);
        Ok(Self {
            store, instance,
            capability: cap,
            bound_irq: None,
            waiting_for_irq: false,
            meta,
        })
    }

    /// Bind this driver to its PCI device's IRQ line.
    /// Must be called before start() for interrupt-driven drivers.
    pub fn bind_irq(&mut self) -> Result<u8, ()> {
        let irq = self.capability.irq_line;
        if irq == 0 || irq == 0xFF {
            return Err(()); // No IRQ assigned
        }
        match libfolk::sys::pci::bind_irq(irq) {
            Ok(vector) => {
                self.bound_irq = Some(irq);
                libfolk::println!("[DRV:{}] Bound IRQ {} → vector {}",
                    self.capability.driver_name(), irq, vector);
                Ok(vector)
            }
            Err(_) => Err(()),
        }
    }

    /// Start driver execution. Returns immediately if driver yields.
    pub fn start(&mut self) -> DriverResult {
        self.store.set_fuel(10_000_000).unwrap_or(());

        let func = match self.instance.get_typed_func::<(), ()>(&self.store, "driver_main") {
            Ok(f) => f,
            Err(_) => match self.instance.get_typed_func::<(), ()>(&self.store, "run") {
                Ok(f) => f,
                Err(_) => return DriverResult::LoadError(
                    String::from("No driver_main or run export")
                ),
            }
        };

        self.execute_func(func)
    }

    /// Core execution: call function and classify the result.
    fn execute_func(&mut self, func: TypedFunc<(), ()>) -> DriverResult {
        match func.call(&mut self.store, ()) {
            Ok(()) => {
                self.waiting_for_irq = false;
                DriverResult::Completed
            }
            Err(err) => {
                let msg = format!("{:?}", err);
                if msg.contains("__YIELD_IRQ__") {
                    self.waiting_for_irq = true;
                    DriverResult::WaitingForIrq
                } else if msg.contains("fuel") || msg.contains("Fuel") {
                    DriverResult::OutOfFuel
                } else {
                    self.waiting_for_irq = false;
                    DriverResult::Trapped(msg)
                }
            }
        }
    }

    /// Check if this driver's IRQ has fired (non-blocking).
    pub fn poll_irq(&self) -> bool {
        if let Some(irq) = self.bound_irq {
            libfolk::sys::pci::check_irq(irq).unwrap_or(false)
        } else {
            false
        }
    }

    /// Resume driver after IRQ. Refuels and re-executes driver_main.
    /// With full call_resumable (wasmi 0.38), the driver resumes at
    /// the instruction after folk_wait_irq(). For now, we re-execute
    /// driver_main which is idempotent for well-structured drivers.
    pub fn resume_after_irq(&mut self) -> DriverResult {
        if let Some(irq) = self.bound_irq {
            // Clear pending flag + unmask at IOAPIC
            let _ = libfolk::sys::pci::ack_irq(irq);
        }
        self.store.data_mut().irq_pending = true;
        self.waiting_for_irq = false;

        // Refuel for next execution slice
        self.store.set_fuel(10_000_000).unwrap_or(());

        // Re-execute driver_main
        self.start()
    }

    /// Resume after fuel exhaustion (preemption). Just refuel and restart.
    pub fn resume_after_fuel(&mut self) -> DriverResult {
        self.store.set_fuel(10_000_000).unwrap_or(());
        self.start()
    }
}

/// Tick all active drivers: poll IRQs and resume suspended drivers.
/// Called from the compositor's main loop.
/// Returns the number of drivers that were resumed.
pub fn tick_drivers(drivers: &mut Vec<WasmDriver>) -> usize {
    let mut resumed = 0;
    let mut to_remove = Vec::new();

    // Debug: log driver count and state periodically
    static TICK_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let tc = TICK_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if tc < 3 {
        libfolk::println!("[tick_drivers] {} drivers, tick #{}", drivers.len(), tc);
        for (i, d) in drivers.iter().enumerate() {
            libfolk::println!("[tick_drivers]   [{}] waiting={} uptime={}", i, d.waiting_for_irq, d.meta.uptime_ticks);
        }
    }

    for (i, driver) in drivers.iter_mut().enumerate() {
        // Track uptime for all active drivers
        driver.meta.uptime_ticks = driver.meta.uptime_ticks.saturating_add(1);

        if driver.waiting_for_irq {
            // Wake driver periodically for TX polling (every 5 ticks ≈ every frame)
            // The kernel smoltcp generates packets on a timer that the driver must transmit.
            let tx_poll_due = (driver.meta.uptime_ticks % 5) == 0;

            // Poll for hardware interrupt OR periodic TX poll
            if driver.poll_irq() || tx_poll_due {
                driver.meta.irq_count = driver.meta.irq_count.saturating_add(1);
                libfolk::println!("[DRV:{}] IRQ #{} — resuming",
                    driver.capability.driver_name(), driver.meta.irq_count);
                match driver.resume_after_irq() {
                    DriverResult::WaitingForIrq => {
                        // Driver processed IRQ and is waiting for next one — good
                    }
                    DriverResult::Completed => {
                        driver.meta.recalc_stability();
                        libfolk::println!("[DRV:{}] Completed (stability={})",
                            driver.capability.driver_name(), driver.meta.stability_score);
                        to_remove.push(i);
                    }
                    DriverResult::OutOfFuel => {
                        // Preempted during IRQ handling — resume next tick
                        let _ = driver.resume_after_fuel();
                    }
                    DriverResult::Trapped(msg) => {
                        driver.meta.fault_count = driver.meta.fault_count.saturating_add(1);
                        driver.meta.recalc_stability();
                        libfolk::println!("[DRV:{}] TRAP (faults={}, stability={}): {}",
                            driver.capability.driver_name(),
                            driver.meta.fault_count, driver.meta.stability_score,
                            &msg[..msg.len().min(60)]);
                        to_remove.push(i);
                    }
                    DriverResult::LoadError(_) => { to_remove.push(i); }
                }
                resumed += 1;
            }
        }
    }

    // Remove crashed/completed drivers (reverse order to preserve indices)
    for i in to_remove.into_iter().rev() {
        let name = String::from(drivers[i].capability.driver_name());
        drivers.remove(i);
        libfolk::println!("[DRV] Removed driver: {}", name);
    }

    resumed
}

// ── LLM Prompt Template ─────────────────────────────────────────────────

/// Generate the system prompt for LLM driver synthesis.
/// Includes the complete folk_* ABI definition and constraints.
pub fn driver_generation_prompt(vendor_id: u16, device_id: u16, class_name: &str) -> String {
    // The actual prompt is in the proxy (tools/serial-gemini-proxy.py).
    // This function just provides the device context for the __DRIVER_GEN__ marker.
    format!(
        "__DRIVER_GEN__{:04x}:{:04x}:{}",
        vendor_id, device_id, class_name
    )
}
