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
// const_mut_refs is stable since 1.83, panic_info_message since 1.81, naked_functions since 1.88

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

        serial_strln!("\n[KERNEL_MAIN] kernel_main_with_boot_info() started!");

        serial_strln!("\n==============================================");
        serial_strln!("   Folkering OS v0.1.0 - Microkernel        ");
        serial_strln!("==============================================\n");

        // Display boot information using bypass functions
        serial_strln!("[BOOT] Boot information:");
        serial_strln!("[BOOT] Bootloader: Limine 8.7.0");
        serial_str!("[BOOT] Kernel physical base: ");
        drivers::serial::write_hex(boot_info.kernel_phys_base as u64);
        drivers::serial::write_newline();
        serial_str!("[BOOT] Kernel virtual base:  ");
        drivers::serial::write_hex(boot_info.kernel_virt_base as u64);
        drivers::serial::write_newline();
        serial_str!("[BOOT] HHDM offset:          ");
        drivers::serial::write_hex(boot_info.hhdm_offset as u64);
        drivers::serial::write_newline();
        if boot_info.rsdp_addr != 0 {
            serial_str!("[BOOT] RSDP address:         ");
            drivers::serial::write_hex(boot_info.rsdp_addr as u64);
            drivers::serial::write_newline();
        }
        serial_strln!("[BOOT] Boot information complete!");

        // Initialize physical memory manager
        serial_strln!("\n[PMM] Initializing physical memory manager...");
        memory::physical::init(boot_info);

        // Get memory stats
        let stats = memory::physical::stats();
        serial_strln!("\n[PMM] Initialization complete!");
        serial_str!("[PMM] Total memory: ");
        drivers::serial::write_dec((stats.total_bytes / (1024 * 1024)) as u32);
        serial_strln!(" MB");
        serial_str!("[PMM] Free memory:  ");
        drivers::serial::write_dec((stats.free_bytes / (1024 * 1024)) as u32);
        serial_strln!(" MB\n");

        // Initialize GDT and TSS
        serial_strln!("[INIT] Initializing GDT and TSS...");
        arch::x86_64::gdt_init();
        serial_strln!("[GDT] Done\n");

        // Initialize syscall support (requires GDT to be loaded first)
        serial_strln!("[INIT] Initializing SYSCALL/SYSRET support...");
        arch::x86_64::syscall_init();
        serial_strln!("[SYSCALL] Fast system calls enabled\n");

        // Initialize CPU frequency scaling
        serial_strln!("[INIT] Initializing CPU frequency scaling...");
        arch::x86_64::cpu_freq_init();
        serial_strln!("[CPU_FREQ] Dynamic voltage and frequency scaling ready\n");

        // Initialize paging subsystem (needed before APIC init for MMIO mapping)
        serial_strln!("[INIT] Initializing page table mapper...");
        memory::paging::init(boot_info);
        serial_strln!("[PAGING] Page table mapper ready\n");

        // Initialize kernel heap
        serial_strln!("[INIT] Initializing kernel heap...");
        memory::heap::init();
        serial_strln!("[HEAP] Kernel heap ready (16 MB allocated)\n");

        // Initialize APIC timer for preemptive scheduling (after paging for MMIO mapping)
        serial_strln!("[INIT] Initializing Local APIC...");
        arch::x86_64::apic_init();
        serial_strln!("[APIC] Timer interrupts enabled\n");

        // Verify dynamic allocations work
        use alloc::vec::Vec;
        use alloc::string::String;

        let mut v = Vec::new();
        v.push(1);
        v.push(2);
        v.push(3);
        let _s = String::from("Folkering OS");

        serial_strln!("[TEST] Dynamic allocations verified (Vec, String)\n");

        serial_strln!("\n[BOOT] ✅ Phase 1 COMPLETE - Memory subsystem operational");
        serial_strln!("[BOOT] ✅ Phase 2 COMPLETE - User mode infrastructure ready\n");

        // ===== Phase 3: IPC & Task Management =====

        serial_strln!("[BOOT] Starting Phase 3: IPC & Task Management...\n");

        serial_strln!("[DEBUG] About to call ipc::init()...");
        // Initialize IPC subsystem
        serial_strln!("[INIT] Initializing IPC subsystem...");
        ipc::init();
        serial_strln!("[DEBUG] ipc::init() returned OK");
        serial_strln!("[IPC] IPC subsystem ready\n");

        serial_strln!("[DEBUG] About to call scheduler_init()...");
        // Initialize scheduler
        serial_strln!("[INIT] Initializing scheduler...");
        task::scheduler_init();
        serial_strln!("[DEBUG] scheduler_init() returned OK");
        serial_strln!("[SCHED] Scheduler ready\n");

        // Note: Kernel doesn't need its own Task structure
        // It runs in interrupt/syscall context, not as a schedulable task
        serial_strln!("[INIT] Kernel running in interrupt context (no task structure needed)\n");

        serial_strln!("[BOOT] ✅ Phase 3 COMPLETE - IPC & Task system operational\n");

        // ===== IPC Test Setup =====
        // Task layout:
        // - Task 1: Dummy yield loop (to occupy ID 1)
        // - Task 2: IPC Receiver (receives messages and replies)
        // - Task 3: IPC Sender (sends to task 2)

        serial_strln!("[TEST] Spawning IPC test tasks (sender + receiver)...\n");

        // Simple yield loop for Task 1 (dummy)
        static YIELD_LOOP: [u8; 11] = [
            0x48, 0xc7, 0xc0, 0x07, 0x00, 0x00, 0x00,  // mov rax, 7 (YIELD)
            0x0f, 0x05,                                 // syscall
            0xeb, 0xf5,                                 // jmp -11
        ];

        // Spawn Task 1 - dummy yield loop (occupies ID 1)
        serial_strln!("[BOOT] Spawning Task 1 (dummy yield)...");
        match task::spawn_raw(&YIELD_LOOP, 0) {
            Ok(task_id) => {
                serial_str!("[BOOT] Task 1 spawned, id=");
                drivers::serial::write_dec(task_id);
                serial_strln!("");
            }
            Err(_e) => {
                serial_strln!("[BOOT] Task 1 spawn FAILED\n");
                loop { x86_64::instructions::hlt(); }
            }
        }

        // Spawn Task 2 - IPC Receiver (must be ID 2 because sender targets task 2)
        serial_strln!("[BOOT] Spawning Task 2 (IPC receiver)...");
        let receiver_id = match task::spawn_raw(&userspace_test::IPC_RECEIVER.code[..userspace_test::IpcReceiverProgram::code_size()], 0) {
            Ok(task_id) => {
                serial_str!("[BOOT] Task 2 (receiver) spawned, id=");
                drivers::serial::write_dec(task_id);
                serial_strln!("");
                task_id
            }
            Err(_e) => {
                serial_strln!("[BOOT] Task 2 spawn FAILED\n");
                loop { x86_64::instructions::hlt(); }
            }
        };

        // Spawn Task 3 - IPC Sender (sends to task 2)
        serial_strln!("[BOOT] Spawning Task 3 (IPC sender)...");
        let sender_id = match task::spawn_raw(&userspace_test::IPC_SENDER.code[..userspace_test::IpcSenderProgram::code_size()], 0) {
            Ok(task_id) => {
                serial_str!("[BOOT] Task 3 (sender) spawned, id=");
                drivers::serial::write_dec(task_id);
                serial_strln!("");
                task_id
            }
            Err(_e) => {
                serial_strln!("[BOOT] Task 3 spawn FAILED\n");
                loop { x86_64::instructions::hlt(); }
            }
        };

        // Grant IPC capabilities
        serial_strln!("[BOOT] Granting IPC capabilities...");
        // Sender needs capability to send to receiver
        if let Err(_e) = capability::grant_ipc_send(sender_id, receiver_id) {
            serial_strln!("[BOOT] WARNING: Failed to grant IPC capability to sender");
        }
        // Receiver needs capability to reply (send back to sender)
        if let Err(_e) = capability::grant_ipc_send(receiver_id, sender_id) {
            serial_strln!("[BOOT] WARNING: Failed to grant IPC capability to receiver");
        }
        serial_strln!("[BOOT] IPC capabilities granted");

        serial_strln!("[BOOT] All IPC test tasks spawned, starting scheduler...\n");

        // Enable timer interrupts for preemption
        serial_strln!("[BOOT] Enabling APIC timer for preemption...");
        arch::x86_64::enable_timer();

        serial_strln!("==============================================\n");

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

/// Serial print macro (uses format_args - may hang on toolchain bug)
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::drivers::serial::_print(format_args!($($arg)*))
    };
}

/// Serial println macro (uses format_args - may hang on toolchain bug)
#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($($arg:tt)*) => ($crate::serial_print!("{}\n", format_args!($($arg)*)));
}

/// Simple string print (bypasses format! - toolchain safe)
#[macro_export]
macro_rules! serial_str {
    ($s:expr) => {
        $crate::drivers::serial::write_str($s)
    };
}

/// Simple string print with newline (bypasses format! - toolchain safe)
#[macro_export]
macro_rules! serial_strln {
    ($s:expr) => {
        $crate::drivers::serial::write_str($s);
        $crate::drivers::serial::write_newline();
    };
}
