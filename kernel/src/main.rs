//! Folkering OS Kernel Entry Point

#![no_std]
#![no_main]

use limine::BaseRevision;
use limine::request::{
    RequestsStartMarker, RequestsEndMarker,
    FramebufferRequest, MemoryMapRequest, HhdmRequest, RsdpRequest
};

// Import kernel library
extern crate folkering_kernel;

// Limine base revision
#[used]
#[link_section = ".requests"]
static BASE_REVISION: BaseRevision = BaseRevision::new();

// Request framebuffer
#[used]
#[link_section = ".requests"]
static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

// Request memory map
#[used]
#[link_section = ".requests"]
static MEMORY_MAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();

// Request Higher Half Direct Map
#[used]
#[link_section = ".requests"]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

// Request RSDP (ACPI root table)
#[used]
#[link_section = ".requests"]
static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();

// Request markers
#[used]
#[link_section = ".requests_start_marker"]
static _START_MARKER: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[link_section = ".requests_end_marker"]
static _END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

/// IDT Entry structure
#[derive(Copy, Clone)]
#[repr(C, packed)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const fn new() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    fn set_handler(&mut self, handler: unsafe extern "C" fn()) {
        let addr = handler as u64;
        self.offset_low = (addr & 0xFFFF) as u16;
        self.offset_mid = ((addr >> 16) & 0xFFFF) as u16;
        self.offset_high = ((addr >> 32) & 0xFFFFFFFF) as u32;
        self.selector = 0x08; // Kernel code segment
        self.ist = 0;
        self.type_attr = 0x8E; // Present, DPL=0, Interrupt Gate
        self.reserved = 0;
    }
}

/// IDT Descriptor for LIDT instruction
#[repr(C, packed)]
struct IdtDescriptor {
    limit: u16,
    base: u64,
}

/// IDT with 256 entries
#[link_section = ".bss"]
static mut IDT: [IdtEntry; 256] = [IdtEntry::new(); 256];

/// Generic exception handler - halt on any exception
unsafe extern "C" fn exception_handler() {
    serial_write("\n[EXCEPTION] CPU exception occurred! Halting.\n");
    core::arch::asm!("cli");
    loop {
        core::arch::asm!("hlt", options(nomem, nostack));
    }
}

/// Write a string to COM1 serial port
unsafe fn serial_write(s: &str) {
    for &byte in s.as_bytes() {
        core::arch::asm!(
            "out dx, al",
            in("dx") 0x3F8u16,
            in("al") byte,
            options(nostack)
        );
    }
}

/// Initialize IDT with generic exception handlers
unsafe fn init_idt() {
    // Set all IDT entries to the generic exception handler
    for entry in &mut IDT {
        entry.set_handler(exception_handler);
    }

    // Create IDT descriptor
    let idt_desc = IdtDescriptor {
        limit: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
        base: IDT.as_ptr() as u64,
    };

    // Load IDT
    core::arch::asm!(
        "lidt [{}]",
        in(reg) &idt_desc,
        options(readonly, nostack, preserves_flags)
    );
}


/// Kernel entry point
#[no_mangle]
unsafe extern "C" fn kmain() -> ! {
    // Note: Using Limine's default stack (sufficient for kernel needs)
    // Custom 32KB stack defined in linker.ld reserved for future use

    // Disable interrupts
    core::arch::asm!("cli");

    // Write boot message
    serial_write("\n\n[Folkering OS] Kernel booted successfully!\n");

    // Initialize IDT first (critical for stability)
    serial_write("[Folkering OS] Setting up IDT...\n");
    init_idt();
    serial_write("[Folkering OS] IDT loaded\n");

    // Build BootInfo structure from Limine responses
    serial_write("[Folkering OS] Building boot information...\n");

    // Get HHDM offset
    let hhdm_offset = if let Some(hhdm) = HHDM_REQUEST.get_response() {
        hhdm.offset() as usize
    } else {
        serial_write("[ERROR] No HHDM response!\n");
        halt_loop();
    };

    // Get RSDP address
    let rsdp_addr = if let Some(rsdp) = RSDP_REQUEST.get_response() {
        rsdp.address() as usize
    } else {
        0
    };

    // Try to get memory map entries directly
    // The Limine crate returns the entries as a slice, which should be accessible
    // since Limine has already set up page tables with HHDM mapping
    let (memory_map_slice, total_mem, usable_mem) = if let Some(mmap_response) = MEMORY_MAP_REQUEST.get_response() {
        serial_write("[DEBUG] Got memory map response\n");

        // Get entries - this returns a slice
        let entries = mmap_response.entries();
        serial_write("[DEBUG] Got entries slice\n");

        // The memory map slice is already accessible through HHDM
        // Just pass it directly to BootInfo
        let mut total = 0u64;
        let mut usable = 0u64;

        // Try to calculate totals - access entries one by one
        let len = entries.len();
        serial_write("[DEBUG] Entry count: ");
        // Can't call write_number since it's deleted, just continue

        for entry in entries.iter() {
            total += entry.length;
            if entry.entry_type == limine::memory_map::EntryType::USABLE {
                usable += entry.length;
            }
        }

        serial_write("[DEBUG] Memory map parsed successfully\n");
        (entries, total as usize, usable as usize)
    } else {
        serial_write("[ERROR] No memory map response!\n");
        let empty: &[&limine::memory_map::Entry] = &[];
        (empty, 0, 0)
    };

    let boot_info = folkering_kernel::boot::BootInfo {
        bootloader_name: "Limine",
        bootloader_version: "8.7.0",
        memory_total: total_mem,
        memory_usable: usable_mem,
        kernel_phys_base: 0x1ff50000, // Approximate from Limine output
        kernel_virt_base: 0xFFFF_FFFF_8000_0000,
        hhdm_offset,
        rsdp_addr,
        memory_map: memory_map_slice,
    };

    serial_write("[Folkering OS] Boot info ready, calling kernel_main...\n\n");

    // Call main kernel initialization
    folkering_kernel::kernel_main_with_boot_info(&boot_info);
}

/// Halt loop for errors
fn halt_loop() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}
