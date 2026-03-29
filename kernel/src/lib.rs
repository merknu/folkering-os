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

        // Initialize hardware RNG and RTC early (needed for TLS later)
        drivers::rng::init();
        drivers::cmos::init();

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

        // Enable AVX on BSP: set CR4.OSXSAVE, then configure XCR0
        {
            let mut cr4: u64;
            core::arch::asm!("mov {}, cr4", out(reg) cr4);
            cr4 |= (1 << 9) | (1 << 10) | (1 << 18); // OSFXSR + OSXMMEXCPT + OSXSAVE
            core::arch::asm!("mov cr4, {}", in(reg) cr4);
            // Enable x87 + SSE + AVX in XCR0
            core::arch::asm!(
                "xor ecx, ecx",
                "xgetbv",
                "or eax, 7",
                "xsetbv",
                out("eax") _, out("ecx") _, out("edx") _,
            );
            serial_strln!("[INIT] AVX/SSE enabled via CR4.OSXSAVE + XCR0");
        }

        // Initialize GDT and TSS
        serial_strln!("[INIT] Initializing GDT and TSS...");
        arch::x86_64::gdt_init();
        serial_strln!("[GDT] Done\n");

        // DEBUG: Verify SS is 0x10 right after GDT init
        let ss_after_gdt: u16;
        unsafe { core::arch::asm!("mov {0:x}, ss", out(reg) ss_after_gdt); }
        serial_str!("[GDT] SS after init: 0x");
        drivers::serial::write_hex(ss_after_gdt as u64);
        serial_strln!("");

        // Initialize PAT for Write-Combining support (before MMIO/framebuffer mapping)
        serial_strln!("[INIT] Initializing PAT for Write-Combining...");
        arch::x86_64::pat_init();

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

        // Initialize IOAPIC for device interrupt routing
        // Must be done after APIC but before keyboard/mouse drivers
        serial_strln!("[INIT] Initializing I/O APIC...");
        arch::x86_64::ioapic_init();

        // Disable PIC completely - IOAPIC handles all device interrupts now
        // Mask all PIC interrupts to avoid conflicts with IOAPIC
        serial_strln!("[INIT] Disabling legacy PIC (using IOAPIC instead)...");
        unsafe {
            use x86_64::instructions::port::Port;
            let mut pic1_data = Port::<u8>::new(0x21);
            let mut pic2_data = Port::<u8>::new(0xA1);
            pic1_data.write(0xFF); // Mask all IRQs on PIC1
            pic2_data.write(0xFF); // Mask all IRQs on PIC2
        }
        serial_strln!("[INIT] PIC disabled (all IRQs masked)");

        // Initialize ACPI (for future use)
        arch::x86_64::acpi_init(boot_info.rsdp_addr);

        // Boot APs via Limine SMP for parallel GEMM
        if let Some(smp) = boot_info.smp_response {
            serial_strln!("[INIT] Booting APs via Limine SMP...");
            arch::x86_64::smp::boot_aps_limine(smp);
        }

        // Initialize PCI bus enumeration
        serial_strln!("[INIT] Enumerating PCI bus...");
        drivers::pci::init();

        // Initialize VirtIO block device (if present)
        serial_strln!("[INIT] Looking for VirtIO block device...");
        match drivers::virtio_blk::init() {
            Ok(()) => {
                serial_strln!("[INIT] VirtIO block device ready");
                // Initialize disk layout (format if needed, run self-test)
                match drivers::virtio_blk::init_disk() {
                    Ok(()) => { serial_strln!("[INIT] Disk initialized"); }
                    Err(_) => { serial_strln!("[INIT] WARNING: Disk initialization failed"); }
                }
            }
            Err(_) => {
                serial_strln!("[INIT] No VirtIO block device (running without persistent storage)");
            }
        }

        // Initialize VirtIO network device
        serial_strln!("[INIT] Looking for VirtIO network device...");
        match drivers::virtio_net::init() {
            Ok(()) => {
                serial_strln!("[INIT] VirtIO network device ready");
                net::init();
            }
            Err(_) => { serial_strln!("[INIT] No VirtIO network device (running without networking)"); }
        }

        // VirtIO GPU (2D scanout — Limine framebuffer fallback on failure)
        serial_strln!("[INIT] Looking for VirtIO GPU device...");
        match drivers::virtio_gpu::init() {
            Ok(()) => { serial_strln!("[INIT] VirtIO GPU active!"); }
            Err(e) => {
                serial_str!("[INIT] VirtIO GPU: ");
                serial_strln!(e);
                serial_strln!("[INIT] Using Limine framebuffer (fallback)");
            }
        }

        // Initialize keyboard driver (uses IRQ1 via IOAPIC)
        // NOTE: keyboard::init() will try to enable PIC IRQ1, but it's masked
        serial_strln!("[INIT] Initializing PS/2 keyboard driver...");
        drivers::keyboard::init_without_pic();
        // Route keyboard IRQ1 to vector 33 via IOAPIC
        arch::x86_64::ioapic_enable_irq(1, 33);

        // Initialize mouse driver (uses IRQ12 via IOAPIC)
        // NOTE: mouse::init() will try to enable PIC IRQ12, but it's masked
        serial_strln!("[INIT] Initializing PS/2 mouse driver...");
        drivers::mouse::init_without_pic();
        // Route mouse IRQ12 to vector 44 via IOAPIC
        arch::x86_64::ioapic_enable_irq(12, 44);

        // Initialize boot info for userspace handoff
        serial_strln!("[INIT] Initializing boot info page...");
        if let Err(e) = boot_info::init() {
            serial_str!("[BOOT_INFO] Init failed: ");
            serial_strln!(e);
        }

        // Set RSDP address in boot info
        boot_info::set_rsdp(boot_info.rsdp_addr as u64);

        // Get framebuffer info from main.rs statics and set in boot info
        extern "Rust" {
            fn get_framebuffer_info() -> (usize, usize, usize, usize, usize, usize, usize, usize);
        }
        let (fb_addr, fb_width, fb_height, fb_pitch, fb_bpp, red_shift, green_shift, blue_shift) =
            get_framebuffer_info();

        if fb_addr != 0 {
            // Convert from HHDM virtual address to physical address
            let fb_phys = virt_to_phys(fb_addr).unwrap_or(fb_addr);
            let fb_config = boot_info::FramebufferConfig {
                physical_address: fb_phys as u64,
                width: fb_width as u32,
                height: fb_height as u32,
                pitch: fb_pitch as u32,
                bpp: fb_bpp as u16,
                memory_model: 1, // RGB
                red_mask_size: 8,
                red_mask_shift: red_shift as u8,
                green_mask_size: 8,
                green_mask_shift: green_shift as u8,
                blue_mask_size: 8,
                blue_mask_shift: blue_shift as u8,
                _reserved: [0; 3],
            };
            boot_info::set_framebuffer(fb_config);
        }

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

        // ===== Userspace Task Setup =====
        // Task layout:
        // - Task 1: Idle task (simple yield loop)
        // - Task 2: Synapse (Data Kernel - IPC service)
        // - Task 3+: Shell and other applications
        //
        // Note: Synapse MUST be Task 2 per the protocol (SYNAPSE_TASK_ID = 2)

        serial_strln!("[BOOT] Setting up userspace tasks...\n");

        // Task 1: Simple idle loop (occupies ID 1)
        static IDLE_LOOP: [u8; 11] = [
            0x48, 0xc7, 0xc0, 0x07, 0x00, 0x00, 0x00,  // mov rax, 7 (YIELD)
            0x0f, 0x05,                                 // syscall
            0xeb, 0xf5,                                 // jmp -11
        ];

        serial_strln!("[BOOT] Spawning Task 1 (idle)...");
        match task::spawn_raw(&IDLE_LOOP, 0) {
            Ok(task_id) => {
                // Set task name
                if let Some(task_arc) = task::task::get_task(task_id) {
                    task_arc.lock().set_name("idle");
                }
                serial_str!("[BOOT] Task 1 (idle) spawned, id=");
                drivers::serial::write_dec(task_id);
                serial_strln!("");
            }
            Err(_e) => {
                serial_strln!("[BOOT] Task 1 spawn FAILED");
            }
        }

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

        // ===== Spawn Core Services =====
        // Task 2: Synapse (Data Kernel) - MUST be spawned first for correct task ID
        // Task 3: Shell

        if let Some(rd) = fs::ramdisk() {
            // Spawn Synapse first (Task 2)
            if let Some(entry) = rd.find("synapse") {
                serial_strln!("[BOOT] Spawning Task 2 (Synapse - Data Kernel)...");
                let synapse_elf = rd.read(entry);
                serial_str!("[BOOT] Synapse ELF size: ");
                drivers::serial::write_dec(synapse_elf.len() as u32);
                serial_strln!(" bytes");
                match task::spawn(synapse_elf, &[]) {
                    Ok(task_id) => {
                        if let Some(task_arc) = task::task::get_task(task_id) {
                            task_arc.lock().set_name("synapse");
                        }
                        serial_str!("[BOOT] Synapse spawned, id=");
                        drivers::serial::write_dec(task_id);
                        serial_strln!("");
                    }
                    Err(e) => {
                        serial_str!("[BOOT] Synapse spawn FAILED: ");
                        match e {
                            task::SpawnError::InvalidElf(_) => { serial_strln!("InvalidElf"); }
                            task::SpawnError::OutOfMemory => { serial_strln!("OutOfMemory"); }
                            task::SpawnError::PermissionDenied => { serial_strln!("PermissionDenied"); }
                            task::SpawnError::NotFound => { serial_strln!("NotFound"); }
                        }
                    }
                }
            } else {
                serial_strln!("[BOOT] WARNING: Synapse not found in ramdisk!");
            }

            // Spawn Shell (Task 3)
            if let Some(entry) = rd.find("shell") {
                serial_strln!("[BOOT] Spawning Task 3 (Shell)...");
                let shell_elf = rd.read(entry);
                serial_str!("[BOOT] Shell ELF size: ");
                drivers::serial::write_dec(shell_elf.len() as u32);
                serial_strln!(" bytes");
                match task::spawn(shell_elf, &[]) {
                    Ok(task_id) => {
                        if let Some(task_arc) = task::task::get_task(task_id) {
                            task_arc.lock().set_name("shell");
                        }
                        serial_str!("[BOOT] Shell spawned, id=");
                        drivers::serial::write_dec(task_id);
                        serial_strln!("");
                        // Grant IPC send-to-any so shell can communicate with all tasks
                        let _ = capability::grant_ipc_send_any(task_id);
                        serial_strln!("[BOOT] Granted IPC capability to shell");
                    }
                    Err(e) => {
                        serial_str!("[BOOT] Shell spawn FAILED: ");
                        match e {
                            task::SpawnError::InvalidElf(_) => { serial_strln!("InvalidElf"); }
                            task::SpawnError::OutOfMemory => { serial_strln!("OutOfMemory"); }
                            task::SpawnError::PermissionDenied => { serial_strln!("PermissionDenied"); }
                            task::SpawnError::NotFound => { serial_strln!("NotFound"); }
                        }
                    }
                }
            } else {
                serial_strln!("[BOOT] WARNING: Shell not found in ramdisk!");
            }

            // Spawn any additional ELF entries (skip synapse and shell)
            for entry in rd.entries() {
                let name = entry.name_str();
                // Use byte comparison to avoid potential str comparison issues
                let is_shell = name.as_bytes() == b"shell";
                let is_synapse = name.as_bytes() == b"synapse";
                let is_compositor = name.as_bytes() == b"compositor";
                let is_inference = name.as_bytes() == b"inference";
                if is_shell || is_synapse {
                    continue;
                }
                // Phase 5 Hybrid AI: skip built-in inference server to save ~400MB RAM.
                // AI runs on host via LM Studio/llama.cpp, proxied through COM2.
                if is_inference {
                    serial_strln!("[BOOT] Skipping inference server (Phase 5 Hybrid AI mode)");
                    continue;
                }
                if entry.is_elf() {
                    serial_str!("[BOOT] Spawning \"");
                    serial_str!(name);
                    serial_strln!("\" from ramdisk...");
                    let elf_data = rd.read(entry);
                    match task::spawn(elf_data, &[]) {
                        Ok(task_id) => {
                            // Set task name from ramdisk entry name
                            if let Some(task_arc) = task::task::get_task(task_id) {
                                task_arc.lock().set_name(name);
                            }
                            serial_str!("[BOOT] Spawned \"");
                            serial_str!(name);
                            serial_str!("\", id=");
                            drivers::serial::write_dec(task_id);
                            serial_strln!("");

                            // Grant IPC send-to-any for all spawned tasks
                            let _ = capability::grant_ipc_send_any(task_id);

                            // Grant framebuffer capability to compositor
                            if is_compositor && fb_addr != 0 {
                                let fb_size = fb_pitch * fb_height;
                                // Convert HHDM virtual address to physical address
                                let fb_phys = virt_to_phys(fb_addr).unwrap_or(fb_addr);
                                serial_str!("[BOOT] FB cap: phys=");
                                drivers::serial::write_hex(fb_phys as u64);
                                serial_str!(" size=");
                                drivers::serial::write_hex(fb_size as u64);
                                drivers::serial::write_newline();
                                if capability::grant_framebuffer(task_id, fb_phys as u64, fb_size as u64).is_ok() {
                                    serial_str!("[BOOT] Granted framebuffer capability to compositor (task ");
                                    drivers::serial::write_dec(task_id);
                                    serial_strln!(")");

                                    // Also map the boot info page into compositor's address space
                                    if let Err(e) = boot_info::map_for_task(task_id) {
                                        serial_str!("[BOOT] Failed to map boot info for compositor: ");
                                        serial_strln!(e);
                                    } else {
                                        serial_strln!("[BOOT] Boot info page mapped for compositor");

                                        // DEBUG: Verify boot info content before compositor runs
                                        let bi = boot_info::get_boot_info();
                                        serial_str!("[BOOT_DEBUG] Kernel sees: magic=");
                                        drivers::serial::write_hex(bi.magic);
                                        serial_str!(", flags=");
                                        drivers::serial::write_hex(bi.flags);
                                        drivers::serial::write_newline();
                                        serial_str!("[BOOT_DEBUG] FB: ");
                                        drivers::serial::write_dec(bi.framebuffer.width);
                                        serial_str!("x");
                                        drivers::serial::write_dec(bi.framebuffer.height);
                                        serial_str!(" @ ");
                                        drivers::serial::write_hex(bi.framebuffer.physical_address);
                                        drivers::serial::write_newline();
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            serial_str!("[BOOT] Failed to spawn \"");
                            serial_str!(name);
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
        } else {
            serial_strln!("[BOOT] ERROR: No ramdisk available!");
        }

        serial_strln!("[BOOT] All tasks spawned, starting scheduler...\n");

        // TEMPORARY DIAGNOSTIC: wait 3 seconds so TCP serial logger can connect
        // before the scheduler starts (TCP logger misses early boot output).
        // Remove after debugging is complete.
        serial_strln!("[BOOT] Waiting 3s for serial logger to connect...");
        unsafe { core::arch::asm!("sti"); }
        arch::x86_64::enable_timer();
        let wait_start = timer::uptime_ms();
        while timer::uptime_ms() - wait_start < 3000 {
            core::hint::spin_loop();
        }
        unsafe { core::arch::asm!("cli"); }
        serial_strln!("[BOOT] Wait done. Boot output above should be visible.");

        // Grant IPC capabilities to all userspace tasks
        // Task 2 = Synapse, Task 3 = Shell, Task 4 = Compositor, Task 5 = Intent Service
        serial_strln!("[BOOT] Granting IPC capabilities...");
        for task_id in 2..=8 {
            if capability::grant_ipc_send_any(task_id).is_ok() {
                serial_str!("[BOOT] Task ");
                drivers::serial::write_dec(task_id);
                serial_strln!(" granted IpcSendAny");
            }
        }

        // ============================================
        // KERNEL-MODE TIMER TEST
        // Test that timer interrupt works in CPL 0 before switching to user mode
        // ============================================
        serial_strln!("[TIMER-TEST] Testing timer interrupt in kernel mode (CPL 0)...");

        // DEBUG: Check SS register value - it should be 0x10 (kernel data segment)
        let ss_value: u16;
        unsafe {
            core::arch::asm!("mov {0:x}, ss", out(reg) ss_value);
        }
        serial_str!("[TIMER-TEST] SS register before timer enable: 0x");
        drivers::serial::write_hex(ss_value as u64);
        serial_strln!("");

        if ss_value != 0x10 {
            serial_strln!("[TIMER-TEST] ERROR: SS is NOT 0x10 (kernel data segment)!");
            serial_strln!("[TIMER-TEST] SS should be 0x10, but it got corrupted!");
            serial_strln!("[TIMER-TEST] Investigating SS corruption...");

            // Print DS, ES, FS, GS too
            let ds_value: u16;
            let es_value: u16;
            unsafe {
                core::arch::asm!("mov {0:x}, ds", out(reg) ds_value);
                core::arch::asm!("mov {0:x}, es", out(reg) es_value);
            }
            serial_str!("[TIMER-TEST] DS=0x");
            drivers::serial::write_hex(ds_value as u64);
            serial_str!(", ES=0x");
            drivers::serial::write_hex(es_value as u64);
            serial_strln!("");
        }

        // Get initial tick count - use timer::uptime_ms() because that's what irq_timer calls
        let ticks_before = timer::uptime_ms();
        serial_str!("[TIMER-TEST] Initial uptime_ms: ");
        drivers::serial::write_dec(ticks_before as u32);
        serial_strln!("");

        // Enable interrupts and timer
        serial_strln!("[TIMER-TEST] Enabling interrupts (STI)...");
        unsafe { core::arch::asm!("sti"); }

        serial_strln!("[TIMER-TEST] Enabling APIC timer...");
        arch::x86_64::enable_timer();

        // Spin for a while to let timer fire (busy wait ~50ms)
        serial_strln!("[TIMER-TEST] Waiting for timer interrupts (busy loop)...");
        for i in 0..100_000u64 {
            // Prevent optimizer from removing this loop
            core::hint::spin_loop();
            if i % 1_000_000 == 0 {
                serial_str!(".");
            }
        }
        serial_strln!(" done");

        // Disable interrupts to check result safely
        unsafe { core::arch::asm!("cli"); }

        // Check tick count - use timer::uptime_ms() (what irq_timer updates)
        let ticks_after = timer::uptime_ms();
        serial_str!("[TIMER-TEST] Final uptime_ms: ");
        drivers::serial::write_dec(ticks_after as u32);
        serial_strln!("");

        let tick_delta = ticks_after - ticks_before;
        serial_str!("[TIMER-TEST] Uptime delta (ms): ");
        drivers::serial::write_dec(tick_delta as u32);
        serial_strln!("");

        if tick_delta > 0 {
            serial_strln!("[TIMER-TEST] SUCCESS: Timer interrupt works in kernel mode!");
            serial_str!("[TIMER-TEST] ");
            drivers::serial::write_dec(tick_delta as u32);
            serial_strln!("ms elapsed - timer is firing correctly!");
        } else {
            serial_strln!("[TIMER-TEST] FAILURE: No timer interrupts in kernel mode!");
            serial_strln!("[TIMER-TEST] Check IDT[32] setup and APIC configuration");
        }

        // Keep timer enabled for preemptive scheduling!
        serial_strln!("[TIMER-TEST] Keeping timer ENABLED for preemptive scheduling...");
        // Timer stays enabled - no disable_timer() call

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
pub mod boot_info;
pub mod bridge;
pub mod capability;
pub mod drivers;
pub mod fs;
pub mod ipc;
pub mod memory;
pub mod net;
pub mod panic;
pub mod task;
pub mod timer;
// userspace_test removed (cleanup)

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
        /// Limine SMP response (if available)
        pub smp_response: Option<&'static limine::response::SmpResponse>,
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
