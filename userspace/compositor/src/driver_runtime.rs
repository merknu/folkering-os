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
const DRIVER_FUEL: u64 = 500_000;

/// Maximum MMIO BARs tracked per device
const MAX_BARS: usize = 6;

// ── DriverCapability: The SFI Boundary ──────────────────────────────────

/// Hardware capability tree for a single PCI device.
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

struct DriverState {
    cap: DriverCapability,
    irq_pending: bool,
    log_buf: [u8; 128],
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
}

// ── MMIO BAR Mapping ────────────────────────────────────────────────────

/// Map a device's MMIO BARs into the current process's virtual address space.
/// Uses SYS_MAP_PHYSICAL with Uncacheable attributes for correct MMIO semantics.
/// Returns the number of BARs successfully mapped.
pub fn map_device_bars(cap: &mut DriverCapability) -> usize {
    use libfolk::sys::map_physical::{map_physical, MapFlags};

    let mut mapped = 0;
    // Virtual addresses for BAR mapping (well above userspace heap)
    let base_vaddr: usize = 0x4000_0000;

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
}

impl WasmDriver {
    /// Create a new WASM driver for a PCI device.
    pub fn new(wasm_bytes: &[u8], cap: DriverCapability) -> Result<Self, String> {
        let engine = Engine::default();
        let mut store = Store::new(&engine, DriverState {
            cap: cap.clone(),
            irq_pending: false,
            log_buf: [0; 128],
        });
        store.set_fuel(DRIVER_FUEL).unwrap_or(());

        let module = Module::new(&engine, wasm_bytes)
            .map_err(|e| format!("Module load: {:?}", e))?;

        let mut linker = <Linker<DriverState>>::new(&engine);
        register_driver_functions(&mut linker);

        let instance = linker.instantiate(&mut store, &module)
            .map_err(|e| format!("Instantiate: {:?}", e))?
            .ensure_no_start(&mut store)
            .map_err(|e| format!("Start trap: {:?}", e))?;

        Ok(Self {
            store, instance,
            capability: cap,
            bound_irq: None,
            waiting_for_irq: false,
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
        self.store.set_fuel(DRIVER_FUEL).unwrap_or(());

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
        self.store.set_fuel(DRIVER_FUEL).unwrap_or(());

        // Re-execute driver_main
        self.start()
    }

    /// Resume after fuel exhaustion (preemption). Just refuel and restart.
    pub fn resume_after_fuel(&mut self) -> DriverResult {
        self.store.set_fuel(DRIVER_FUEL).unwrap_or(());
        self.start()
    }
}

/// Tick all active drivers: poll IRQs and resume suspended drivers.
/// Called from the compositor's main loop.
/// Returns the number of drivers that were resumed.
pub fn tick_drivers(drivers: &mut Vec<WasmDriver>) -> usize {
    let mut resumed = 0;
    let mut to_remove = Vec::new();

    for (i, driver) in drivers.iter_mut().enumerate() {
        if driver.waiting_for_irq {
            // Poll for hardware interrupt
            if driver.poll_irq() {
                libfolk::println!("[DRV:{}] IRQ fired — resuming",
                    driver.capability.driver_name());
                match driver.resume_after_irq() {
                    DriverResult::WaitingForIrq => {
                        // Driver processed IRQ and is waiting for next one — good
                    }
                    DriverResult::Completed => {
                        libfolk::println!("[DRV:{}] Completed", driver.capability.driver_name());
                        to_remove.push(i);
                    }
                    DriverResult::OutOfFuel => {
                        // Preempted during IRQ handling — resume next tick
                        let _ = driver.resume_after_fuel();
                    }
                    DriverResult::Trapped(msg) => {
                        libfolk::println!("[DRV:{}] TRAP: {}", driver.capability.driver_name(),
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
    format!(
        "Generate a Rust no_std WASM device driver for Folkering OS.\n\n\
         TARGET DEVICE:\n\
         - PCI Vendor: 0x{:04X}, Device: 0x{:04X}\n\
         - Class: {}\n\n\
         CONSTRAINTS:\n\
         - #![no_std] #![no_main]\n\
         - No allocation (no Vec, String, Box)\n\
         - No crate imports\n\
         - Export: #[no_mangle] pub extern \"C\" fn driver_main()\n\n\
         AVAILABLE HOST FUNCTIONS (extern \"C\"):\n\
         // Port I/O\n\
         fn folk_inb(port: i32) -> i32;\n\
         fn folk_inw(port: i32) -> i32;\n\
         fn folk_inl(port: i32) -> i32;\n\
         fn folk_outb(port: i32, value: i32);\n\
         fn folk_outw(port: i32, value: i32);\n\
         fn folk_outl(port: i32, value: i32);\n\n\
         // MMIO (offset relative to BAR base)\n\
         fn folk_mmio_read_u8(bar: i32, offset: i32) -> i32;\n\
         fn folk_mmio_read_u16(bar: i32, offset: i32) -> i32;\n\
         fn folk_mmio_read_u32(bar: i32, offset: i32) -> i32;\n\
         fn folk_mmio_write_u8(bar: i32, offset: i32, value: i32);\n\
         fn folk_mmio_write_u16(bar: i32, offset: i32, value: i32);\n\
         fn folk_mmio_write_u32(bar: i32, offset: i32, value: i32);\n\n\
         // Interrupt lifecycle\n\
         fn folk_wait_irq();  // yields until hardware interrupt\n\
         fn folk_ack_irq();   // acknowledge interrupt (unmask)\n\n\
         // Device identity\n\
         fn folk_device_vendor_id() -> i32;\n\
         fn folk_device_id() -> i32;\n\
         fn folk_device_irq() -> i32;\n\
         fn folk_bar_size(bar: i32) -> i32;\n\n\
         // Debug\n\
         fn folk_log(ptr: i32, len: i32);\n\n\
         DRIVER PATTERN:\n\
         1. Read device identity via folk_device_vendor_id()\n\
         2. Read BAR sizes via folk_bar_size(0..5)\n\
         3. Initialize device via MMIO/port writes\n\
         4. Enter loop: folk_wait_irq() → process data → folk_ack_irq()\n\n\
         Return ONLY the Rust code.",
        vendor_id, device_id, class_name
    )
}
