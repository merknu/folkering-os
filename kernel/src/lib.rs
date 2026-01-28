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

        // Initialize keyboard driver (after IDT and PIC setup)
        serial_strln!("[INIT] Initializing PS/2 keyboard driver...");
        drivers::keyboard::init();

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

        // Initialize IPC subsystem
        serial_strln!("[INIT] Initializing IPC subsystem...");
        ipc::init();
        serial_strln!("[IPC] IPC subsystem ready\n");

        // Initialize scheduler
        serial_strln!("[INIT] Initializing scheduler...");
        task::scheduler_init();
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

        // Spawn Task 2 - IPC Receiver
        serial_strln!("[BOOT] Spawning Task 2 (IPC receiver)...");
        match task::spawn_raw(&userspace_test::IPC_RECEIVER.code[..userspace_test::IpcReceiverProgram::code_size()], 0) {
            Ok(task_id) => {
                serial_str!("[BOOT] Task 2 (IPC receiver) spawned, id=");
                drivers::serial::write_dec(task_id);
                serial_strln!("");
            }
            Err(_e) => {
                serial_strln!("[BOOT] Task 2 (IPC receiver) spawn FAILED");
            }
        }

        // Spawn Task 3 - IPC Sender
        serial_strln!("[BOOT] Spawning Task 3 (IPC sender)...");
        match task::spawn_raw(&userspace_test::IPC_SENDER.code[..userspace_test::IpcSenderProgram::code_size()], 0) {
            Ok(task_id) => {
                serial_str!("[BOOT] Task 3 (IPC sender) spawned, id=");
                drivers::serial::write_dec(task_id);
                serial_strln!("");
            }
            Err(_e) => {
                serial_strln!("[BOOT] Task 3 (IPC sender) spawn FAILED");
            }
        }

        // Simple assembly shell disabled — competes for keyboard buffer with Rust shell
        // TODO: Route keyboard input to focused task only

        // ===== Ramdisk Initialization =====
        // Try to load Folk-Pack initrd from boot module
        let has_ramdisk = if boot_info.initrd_size > 0 {
            serial_strln!("[RAMDISK] Parsing Folk-Pack initrd...");
            serial_str!("[RAMDISK] Address: ");
            drivers::serial::write_hex(boot_info.initrd_start as u64);
            serial_str!(", size: ");
            drivers::serial::write_dec(boot_info.initrd_size as u32);
            serial_strln!(" bytes");

            match fs::ramdisk::Ramdisk::from_memory(boot_info.initrd_start, boot_info.initrd_size) {
                Ok(rd) => {
                    serial_str!("[RAMDISK] Found Folk-Pack image: ");
                    drivers::serial::write_dec(rd.entry_count() as u32);
                    serial_strln!(" entries");

                    for entry in rd.entries() {
                        serial_str!("[RAMDISK] Entry ");
                        drivers::serial::write_dec(entry.id as u32);
                        serial_str!(": \"");
                        serial_str!(entry.name_str());
                        serial_str!("\" (");
                        if entry.is_elf() { serial_str!("ELF"); } else { serial_str!("DATA"); }
                        serial_str!(", ");
                        drivers::serial::write_dec(entry.size as u32);
                        serial_strln!(" bytes)");
                    }

                    // Store ramdisk globally for syscall access
                    fs::init_ramdisk(rd);
                    true
                }
                Err(e) => {
                    serial_str!("[RAMDISK] Failed to parse initrd: ");
                    match e {
                        fs::ramdisk::RamdiskError::TooSmall => { serial_strln!("TooSmall"); }
                        fs::ramdisk::RamdiskError::BadMagic => { serial_strln!("BadMagic"); }
                        fs::ramdisk::RamdiskError::BadVersion => { serial_strln!("BadVersion"); }
                        fs::ramdisk::RamdiskError::EntryTableOverflow => { serial_strln!("EntryTableOverflow"); }
                        fs::ramdisk::RamdiskError::EntryDataOverflow => { serial_strln!("EntryDataOverflow"); }
                    }
                    false
                }
            }
        } else {
            serial_strln!("[RAMDISK] No initrd provided, using embedded fallback");
            false
        };

        // Spawn Task 5 - Rust Shell (libfolk-based, ELF binary)
        // Try ramdisk first, fall back to include_bytes! if not available
        serial_strln!("[BOOT] Spawning Task 5 (Rust shell from libfolk)...");

        let shell_elf: &[u8] = if let Some(rd) = fs::ramdisk() {
            if let Some(entry) = rd.find("shell") {
                serial_strln!("[BOOT] Loading shell from ramdisk...");
                rd.read(entry)
            } else {
                serial_strln!("[BOOT] Shell not found in ramdisk, using embedded fallback");
                include_bytes!("../../userspace/target/x86_64-folkering-userspace/release/shell")
            }
        } else {
            include_bytes!("../../userspace/target/x86_64-folkering-userspace/release/shell")
        };

        serial_str!("[BOOT] Rust shell ELF size: ");
        drivers::serial::write_dec(shell_elf.len() as u32);
        serial_strln!(" bytes");
        match task::spawn(shell_elf, &[]) {
            Ok(task_id) => {
                serial_str!("[BOOT] Task 5 (Rust shell) spawned, id=");
                drivers::serial::write_dec(task_id);
                serial_strln!("");
            }
            Err(e) => {
                serial_str!("[BOOT] Task 5 (Rust shell) spawn FAILED: ");
                match e {
                    task::SpawnError::InvalidElf(_) => { serial_strln!("InvalidElf"); }
                    task::SpawnError::OutOfMemory => { serial_strln!("OutOfMemory"); }
                    task::SpawnError::PermissionDenied => { serial_strln!("PermissionDenied"); }
                    task::SpawnError::NotFound => { serial_strln!("NotFound"); }
                }
            }
        }

        // Spawn any additional ELF entries from the ramdisk
        if let Some(rd) = fs::ramdisk() {
            for entry in rd.entries() {
                // Skip "shell" — already spawned above
                let name = entry.name_str();
                let is_shell = name.len() == 5
                    && name.as_bytes()[0] == b's'
                    && name.as_bytes()[1] == b'h'
                    && name.as_bytes()[2] == b'e'
                    && name.as_bytes()[3] == b'l'
                    && name.as_bytes()[4] == b'l';
                if is_shell {
                    continue;
                }
                if entry.is_elf() {
                    serial_str!("[BOOT] Spawning \"");
                    serial_str!(entry.name_str());
                    serial_strln!("\" from ramdisk...");
                    let elf_data = rd.read(entry);
                    match task::spawn(elf_data, &[]) {
                        Ok(task_id) => {
                            serial_str!("[BOOT] Spawned \"");
                            serial_str!(entry.name_str());
                            serial_str!("\", id=");
                            drivers::serial::write_dec(task_id);
                            serial_strln!("");
                        }
                        Err(e) => {
                            serial_str!("[BOOT] Failed to spawn \"");
                            serial_str!(entry.name_str());
                            serial_str!("\": ");
                            match e {
                                task::SpawnError::InvalidElf(_) => { serial_strln!("InvalidElf"); }
                                task::SpawnError::OutOfMemory => { serial_strln!("OutOfMemory"); }
                                task::SpawnError::PermissionDenied => { serial_strln!("PermissionDenied"); }
                                task::SpawnError::NotFound => { serial_strln!("NotFound"); }
                            }
                        }
                    }
                }
            }
        }

        serial_strln!("[BOOT] All tasks spawned, starting scheduler...\n");

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
pub mod fs;
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
        /// Physical address of the initrd (Folk-Pack image), 0 if none
        pub initrd_start: usize,
        /// Size of the initrd in bytes
        pub initrd_size: usize,
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

/// Serial println macro (uses format_args)
#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($fmt:expr) => ($crate::drivers::serial::_print(format_args!(concat!($fmt, "\n"))));
    ($fmt:expr, $($arg:tt)*) => ($crate::drivers::serial::_print(format_args!(concat!($fmt, "\n"), $($arg)*)));
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
