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

// Kernel task creation removed - kernel runs in interrupt/syscall context
// and doesn't need a schedulable Task structure

/// Main kernel initialization function with extracted boot info
///
/// Called from the binary entry point in main.rs
/// Takes a BootInfo structure with data already extracted from Limine requests
pub fn kernel_main_with_boot_info(boot_info: &boot::BootInfo) -> ! {
    // Initialize HHDM offset from bootloader
    init_hhdm_offset(boot_info.hhdm_offset);

    unsafe {
        // BSS already cleared in kmain() before switching stacks

        // Initialize serial console driver
        drivers::serial::init();

        serial_println!("\n[KERNEL_MAIN] kernel_main_with_boot_info() started!");

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

        // Initialize CPU frequency scaling
        serial_println!("[INIT] Initializing CPU frequency scaling...");
        arch::x86_64::cpu_freq_init();
        serial_println!("[CPU_FREQ] Dynamic voltage and frequency scaling ready\n");

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

        // ===== Phase 3: IPC & Task Management =====

        serial_println!("[BOOT] Starting Phase 3: IPC & Task Management...\n");

        serial_println!("[DEBUG] About to call ipc::init()...");
        // Initialize IPC subsystem
        serial_println!("[INIT] Initializing IPC subsystem...");
        ipc::init();
        serial_println!("[DEBUG] ipc::init() returned OK");
        serial_println!("[IPC] IPC subsystem ready\n");

        serial_println!("[DEBUG] About to call scheduler_init()...");
        // Initialize scheduler
        serial_println!("[INIT] Initializing scheduler...");
        task::scheduler_init();
        serial_println!("[DEBUG] scheduler_init() returned OK");
        serial_println!("[SCHED] Scheduler ready\n");

        // Note: Kernel doesn't need its own Task structure
        // It runs in interrupt/syscall context, not as a schedulable task
        serial_println!("[INIT] Kernel running in interrupt context (no task structure needed)\n");

        serial_println!("[BOOT] ✅ Phase 3 COMPLETE - IPC & Task system operational\n");

        // Spawn IPC test tasks
        // Note: IPC_SENDER is hardcoded to send to task ID 2
        // So we spawn receiver first (ID 1), then sender (ID 2)
        // Sender will send to task 2, but wait - that's itself!
        // Actually, let's spawn receiver as task 2:
        // - Dummy task 1 (idle loop)
        // - Receiver as task 2
        // - Sender as task 3 (sends to task 2)

        serial_println!("[TEST] Spawning ONLY task 1 (yield loop) for debugging...\n");

        // TEST: Simple yield loop using SYSCALL instruction
        static YIELD_LOOP: [u8; 11] = [
            // loop_start:
            0x48, 0xc7, 0xc0, 0x07, 0x00, 0x00, 0x00,  // mov rax, 7 (YIELD syscall)
            0x0f, 0x05,                                 // syscall
            0xeb, 0xf7,                                 // jmp loop_start (back 9 bytes)
        ];
        serial_println!("[BOOT] Spawning infinite yield loop task...");
        match task::spawn_raw(&YIELD_LOOP, 0) {
            Ok(task_id) => {
                serial_println!("[BOOT] spawn_raw OK, task_id={}", task_id);
                serial_println!("[BOOT] Dummy task spawned (id={})\n", task_id);
            }
            Err(e) => {
                serial_println!("[BOOT] spawn_raw FAILED: {:?}\n", e);
                loop { x86_64::instructions::hlt(); }
            }
        }

        // DISABLED for testing - only spawning task 1
        /*
        // Spawn task 2 - IPC receive loop
        static IPC_RECEIVE_LOOP: [u8; 15] = [
            // loop_start:
            0x48, 0x31, 0xff,                           // xor rdi, rdi (from=0, receive from any)
            0x48, 0xc7, 0xc0, 0x01, 0x00, 0x00, 0x00,  // mov rax, 1 (IPC_RECEIVE)
            0x0f, 0x05,                                 // syscall
            0xeb, 0xf3,                                 // jmp loop_start
            0xf4,                                       // hlt
        ];
        serial_println!("[BOOT] Spawning task 2 (IPC receiver)...");
        match task::spawn_raw(&IPC_RECEIVE_LOOP, 0) {
            Ok(_) => { serial_println!("[BOOT] Task 2 spawned OK"); }
            Err(_) => {
                serial_println!("[BOOT] Task 2 spawn FAILED");
                loop { x86_64::instructions::hlt(); }
            }
        }

        // Spawn task 3 - TEST: IPC send (single, no loop)
        // Use static to avoid stack overflow (kernel stack is limited)
        static IPC_SEND_CODE: [u8; 27] = [
            0x48, 0xc7, 0xc7, 0x02, 0x00, 0x00, 0x00,  // mov rdi, 2 (target task 2)
            0x48, 0xc7, 0xc6, 0xAA, 0x00, 0x00, 0x00,  // mov rsi, 0xAA (payload0)
            0x48, 0x31, 0xd2,                           // xor rdx, rdx (payload1=0)
            0x48, 0xc7, 0xc0, 0x00, 0x00, 0x00, 0x00,  // mov rax, 0 (IPC_SEND)
            0x0f, 0x05,                                 // syscall
            0xf4,                                       // hlt
        ];
        serial_println!("[TEST] Spawning task 3 with IPC send (no loop)...");
        match task::spawn_raw(&IPC_SEND_CODE, 0) {
            Ok(_) => { serial_println!("[TEST] Task 3 (IPC send) spawned OK"); }
            Err(_) => {
                serial_println!("[TEST] Task 3 spawn FAILED");
                loop { x86_64::instructions::hlt(); }
            }
        }
        */

        serial_println!("[BOOT] All tasks spawned, starting scheduler...\n");
        serial_println!("==============================================\n");

        // Start scheduler (does not return)
        task::scheduler_start();
    }
}

// Old functions removed - now using kernel_main_with_requests()

extern crate alloc;

#[macro_use]
extern crate lazy_static;

pub mod arch;
pub mod bridge;
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
