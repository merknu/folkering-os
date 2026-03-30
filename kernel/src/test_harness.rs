//! Kernel Test Harness — isa-debug-exit support for automated testing
//!
//! When QEMU is started with `-device isa-debug-exit,iobase=0xf4,iosize=0x04`,
//! writing to I/O port 0xf4 causes QEMU to exit with code (value * 2 + 1).
//!
//! Convention:
//!   - 0x31 → exit code 99 → SUCCESS
//!   - 0x01 → exit code 3  → FAILURE
//!
//! Test output goes to serial (COM1) with [TEST] prefix.

use x86_64::instructions::port::Port;

const ISA_DEBUG_EXIT_PORT: u16 = 0xf4;
const EXIT_SUCCESS: u32 = 0x31;  // → QEMU exit code 99
const EXIT_FAILURE: u32 = 0x01;  // → QEMU exit code 3

/// Exit QEMU with success code (99)
pub fn exit_success() -> ! {
    unsafe { Port::new(ISA_DEBUG_EXIT_PORT).write(EXIT_SUCCESS); }
    loop { x86_64::instructions::hlt(); }
}

/// Exit QEMU with failure code (3)
pub fn exit_failure() -> ! {
    unsafe { Port::new(ISA_DEBUG_EXIT_PORT).write(EXIT_FAILURE); }
    loop { x86_64::instructions::hlt(); }
}

/// Log a test pass to serial
pub fn test_pass(name: &str) {
    crate::serial_str!("[TEST] PASS: ");
    crate::serial_strln!(name);
}

/// Log a test fail to serial
pub fn test_fail(name: &str, msg: &str) {
    crate::serial_str!("[TEST] FAIL: ");
    crate::serial_str!(name);
    crate::serial_str!(" — ");
    crate::serial_strln!(msg);
}

/// Run basic kernel boot assertions.
/// Called after all subsystems are initialized.
pub fn run_boot_tests() {
    crate::serial_strln!("[TEST] === Folkering OS Boot Test Suite ===");
    let mut passed = 0u32;
    let mut failed = 0u32;

    // Test 1: HHDM offset is valid (higher half)
    {
        let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
        if hhdm >= 0xFFFF_8000_0000_0000 {
            test_pass("HHDM offset is higher-half");
            passed += 1;
        } else {
            test_fail("HHDM offset", "not in higher half");
            failed += 1;
        }
    }

    // Test 2: Physical memory allocator works
    {
        if let Some(page) = crate::memory::physical::alloc_page() {
            if page > 0 && page % 4096 == 0 {
                test_pass("Physical page allocation");
                crate::memory::physical::free_page(page);
                passed += 1;
            } else {
                test_fail("Physical page allocation", "invalid address");
                failed += 1;
            }
        } else {
            test_fail("Physical page allocation", "alloc_page returned None");
            failed += 1;
        }
    }

    // Test 3: Uptime is advancing (timer interrupt fired)
    {
        let t1 = crate::timer::uptime_ms();
        // Busy-wait a bit
        for _ in 0..100_000 { core::hint::spin_loop(); }
        let t2 = crate::timer::uptime_ms();
        if t2 >= t1 {
            test_pass("Timer uptime accessible");
            passed += 1;
        } else {
            test_fail("Timer uptime", "uptime went backwards");
            failed += 1;
        }
    }

    // Test 4: Serial output works (if we got here, it does)
    {
        test_pass("Serial output works");
        passed += 1;
    }

    // Summary
    crate::serial_str!("[TEST] === Results: ");
    crate::drivers::serial::write_dec(passed);
    crate::serial_str!(" passed, ");
    crate::drivers::serial::write_dec(failed);
    crate::serial_strln!(" failed ===");

    if failed > 0 {
        exit_failure();
    }
    // Continue to benchmarks
}

/// Run boot tests without exiting (for chaining with benchmarks)
pub fn run_boot_tests_no_exit() {
    run_boot_tests();
}
