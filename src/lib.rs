//! Folkering OS Microkernel
//!
//! A capability-based microkernel operating system written in Rust.
//!
//! # Architecture
//!
//! - **Microkernel design**: Only essential services in kernel space
//! - **Capability-based security**: Unforgeable 128-bit capability tokens
//! - **IPC-centric**: Fast message passing (<1000 cycles target)
//! - **Higher-half kernel**: Mapped at 0xFFFFFFFF80000000
//!
//! # Performance Targets
//!
//! - Boot time: <10 seconds
//! - IPC latency: <1000 CPU cycles
//! - Context switch: <500 cycles
//! - Scheduling decision: <10,000 cycles

#![no_std]
#![feature(abi_x86_interrupt)]
#![feature(allocator_api)]
#![feature(alloc_error_handler)]
#![feature(const_mut_refs)]
#![feature(panic_info_message)]
#![feature(naked_functions)]

// Text section anchor - workaround for lld orphaned section bug
// Forces .text section creation so ltext sections get proper permissions
// See: https://github.com/llvm/llvm-project/issues/92864
core::arch::global_asm!(
    ".section .text.anchor,\"ax\",@progbits",
    ".global __text_anchor",
    "__text_anchor:",
    "ret"
);

/// Main kernel initialization function with extracted boot info
///
/// Called from the binary entry point in main.rs
/// Takes a BootInfo structure with data already extracted from Limine requests
pub fn kernel_main_with_boot_info(boot_info: &boot::BootInfo) -> ! {
    // Initialize HHDM offset from bootloader
    init_hhdm_offset(boot_info.hhdm_offset);

    unsafe {
        // Clear BSS section first
        extern "C" {
            static mut __bss_start: u8;
            static mut __bss_end: u8;
        }

        let bss_start = &raw mut __bss_start;
        let bss_end = &raw mut __bss_end;
        let bss_size = bss_end as usize - bss_start as usize;
        core::ptr::write_bytes(bss_start, 0, bss_size);

        // Initialize serial
        use x86_64::instructions::port::Port;
        const PORT: u16 = 0x3F8;
        let mut ier_port: Port<u8> = Port::new(PORT + 1);
        ier_port.write(0x00);
        let mut lcr_port: Port<u8> = Port::new(PORT + 3);
        lcr_port.write(0x80);
        let mut dll_port: Port<u8> = Port::new(PORT + 0);
        let mut dlh_port: Port<u8> = Port::new(PORT + 1);
        dll_port.write(0x03);
        dlh_port.write(0x00);
        lcr_port.write(0x03);
        let mut fcr_port: Port<u8> = Port::new(PORT + 2);
        fcr_port.write(0xC7);
        let mut mcr_port: Port<u8> = Port::new(PORT + 4);
        mcr_port.write(0x0B);

        serial_println!("\n==============================================");
        serial_println!("   Folkering OS v0.1.0 - Microkernel        ");
        serial_println!("==============================================\n");

        // Display boot information
        serial_println!("[BOOT] Boot information:");
        serial_println!("[BOOT] Bootloader: {} {}", boot_info.bootloader_name, boot_info.bootloader_version);
        serial_println!("[BOOT] Kernel physical base: {:#x}", boot_info.kernel_phys_base);
        serial_println!("[BOOT] Kernel virtual base:  {:#x}", boot_info.kernel_virt_base);
        serial_println!("[BOOT] HHDM offset:          {:#x}", boot_info.hhdm_offset);
        if boot_info.rsdp_addr != 0 {
            serial_println!("[BOOT] RSDP address:         {:#x}", boot_info.rsdp_addr);
        }

        serial_println!("\n[BOOT] Boot information complete!");

        // Initialize physical memory manager
        serial_println!("\n[PMM] Initializing physical memory manager...");
        memory::physical::init(boot_info);

        // Get memory stats
        let stats = memory::physical::stats();
        serial_println!("\n[PMM] Initialization complete!");
        serial_println!("[PMM] Total memory: {} MB", stats.total_bytes / (1024 * 1024));
        serial_println!("[PMM] Free memory:  {} MB", stats.free_bytes / (1024 * 1024));
        serial_println!("[PMM] Used memory:  {} MB\n", stats.used_bytes / (1024 * 1024));

        // Initialize GDT and TSS
        serial_println!("[INIT] Initializing GDT and TSS...");
        arch::x86_64::gdt_init();
        serial_println!("[GDT] Global Descriptor Table and Task State Segment loaded\n");

        // Initialize syscall support (requires GDT to be loaded first)
        serial_println!("[INIT] Initializing SYSCALL/SYSRET support...");
        arch::x86_64::syscall_init();
        serial_println!("[SYSCALL] Fast system calls enabled (8 syscalls registered)\n");

        // Initialize paging subsystem
        serial_println!("[INIT] Initializing page table mapper...");
        memory::paging::init(boot_info);
        serial_println!("[PAGING] Page table mapper ready\n");

        // Initialize kernel heap
        serial_println!("[INIT] Initializing kernel heap...");
        memory::heap::init();
        serial_println!("[HEAP] Kernel heap ready (16 MB allocated)\n");

        // Verify dynamic allocations work
        use alloc::vec::Vec;
        use alloc::string::String;

        let mut v = Vec::new();
        v.push(1);
        v.push(2);
        v.push(3);
        let _s = String::from("Folkering OS");

        serial_println!("[TEST] Dynamic allocations verified (Vec, String)\n");

        serial_println!("\n[BOOT] ✅ Phase 1 COMPLETE - Memory subsystem operational");
        serial_println!("[BOOT] ✅ Phase 2 COMPLETE - User mode infrastructure ready\n");

        // Load and execute test user program
        serial_println!("[BOOT] Starting user-mode test program...\n");

        // Get user program code
        let user_code = &userspace_test::USER_PROGRAM.code[..userspace_test::UserProgram::code_size()];

        // Map and load user code at user-accessible address
        let entry_point = arch::x86_64::usermode::map_and_load_user_code(user_code);

        // Allocate user stack
        let user_stack = arch::x86_64::usermode::allocate_user_stack();

        // Jump to user mode (does not return!)
        arch::x86_64::usermode::jump_to_usermode(entry_point, user_stack);
    }
}

// Old functions removed - now using kernel_main_with_requests()

extern crate alloc;

#[macro_use]
extern crate lazy_static;

pub mod arch;
pub mod capability;
pub mod drivers;
pub mod ipc;
pub mod memory;
pub mod panic;
pub mod task;
pub mod timer;
pub mod userspace_test;

// Boot information structure (moved from boot module)
pub mod boot {
    use limine::memory_map::Entry;

    /// Boot information structure
    pub struct BootInfo {
        pub bootloader_name: &'static str,
        pub bootloader_version: &'static str,
        pub memory_total: usize,
        pub memory_usable: usize,
        pub kernel_phys_base: usize,
        pub kernel_virt_base: usize,
        pub hhdm_offset: usize,
        pub rsdp_addr: usize,
        pub memory_map: &'static [&'static Entry],
    }
}

/// Kernel version information
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BUILD_DATE: &str = "2026-01-21";

/// Higher-half kernel virtual base address
pub const KERNEL_VIRT_BASE: usize = 0xFFFF_FFFF_8000_0000;

/// Higher-half direct map offset (HHDM) - Default fallback
pub const HHDM_OFFSET_DEFAULT: usize = 0xFFFF_8000_0000_0000;

/// Actual HHDM offset from bootloader (initialized at boot)
static HHDM_OFFSET: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(HHDM_OFFSET_DEFAULT);

/// Initialize HHDM offset from boot info (called once at boot)
pub fn init_hhdm_offset(offset: usize) {
    HHDM_OFFSET.store(offset, core::sync::atomic::Ordering::Relaxed);
}

/// Convert physical address to virtual address via HHDM
#[inline]
pub fn phys_to_virt(phys: usize) -> usize {
    HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed) + phys
}

/// Convert virtual address to physical address via HHDM
#[inline]
pub fn virt_to_phys(virt: usize) -> Option<usize> {
    let offset = HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    if virt >= offset {
        Some(virt - offset)
    } else {
        None
    }
}

/// Serial print macro
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::drivers::serial::_print(format_args!($($arg)*))
    };
}

/// Serial println macro
#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($($arg:tt)*) => ($crate::serial_print!("{}\n", format_args!($($arg)*)));
}
