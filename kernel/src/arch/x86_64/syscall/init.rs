//! Syscall subsystem initialization: enables SYSCALL/SYSRET, sets MSRs,
//! and installs the per-CPU kernel syscall stack with a guard page.

use x86_64::registers::model_specific::{Efer, EferFlags, LStar};
use x86_64::VirtAddr;

use super::debug::DEBUG_MARKER;
use super::entry::syscall_entry;
use super::state::{AlignedStack, SYSCALL_STACK};

/// Initialize syscall support
pub fn init() {
    // Set initialization marker to verify new code is loaded
    DEBUG_MARKER.store(0xBEEF, core::sync::atomic::Ordering::Relaxed);
    crate::serial_strln!("[SYSCALL_INIT] DEBUG_MARKER initialized to 0xBEEF");

    // Enable SYSCALL/SYSRET extensions
    unsafe {
        Efer::update(|flags| {
            flags.insert(EferFlags::SYSTEM_CALL_EXTENSIONS);
        });
    }

    // Verify SYSCALL is enabled
    let efer = Efer::read();
    crate::serial_str!("[SYSCALL_INIT] EFER.SCE = ");
    if efer.contains(EferFlags::SYSTEM_CALL_EXTENSIONS) {
        crate::serial_strln!("true");
    } else {
        crate::serial_strln!("false");
    }

    // Set syscall handler entry point
    let entry_addr = syscall_entry as u64;
    crate::serial_str!("[SYSCALL_INIT] syscall_entry address: ");
    crate::drivers::serial::write_hex(entry_addr);
    crate::drivers::serial::write_newline();
    LStar::write(VirtAddr::new(entry_addr));

    let lstar_check = LStar::read();
    crate::serial_str!("[SYSCALL_INIT] LSTAR set to ");
    crate::drivers::serial::write_hex(lstar_check.as_u64());
    crate::drivers::serial::write_newline();

    if lstar_check.as_u64() != entry_addr {
        crate::serial_strln!("[SYSCALL_INIT] WARNING: LSTAR mismatch!");
    }

    // Configure STAR MSR for SYSCALL/SYSRET
    // STAR[47:32] = kernel CS for SYSCALL
    // STAR[63:48] = base for SYSRET (SYSRET adds +16 for CS, +8 for SS)
    // With our GDT: user_data=0x18, user_code=0x20, so base must be 0x10
    // SYSRET will set: CS = (0x10 + 16) | 3 = 0x23, SS = (0x10 + 8) | 3 = 0x1B
    let kernel_cs = super::super::gdt::kernel_code_selector();
    let kernel_data = super::super::gdt::kernel_data_selector();  // 0x10

    let star_value: u64 =
        ((kernel_cs.0 as u64) << 32) |   // Kernel CS for SYSCALL
        ((kernel_data.0 as u64) << 48);  // Base for SYSRET

    crate::serial_str!("[SYSCALL_INIT] STAR = ");
    crate::drivers::serial::write_hex(star_value);
    crate::serial_str!(" (kernel_cs=");
    crate::drivers::serial::write_hex(kernel_cs.0 as u64);
    crate::serial_str!(", sysret_base=");
    crate::drivers::serial::write_hex(kernel_data.0 as u64);
    crate::serial_strln!(")");

    unsafe {
        use x86_64::registers::model_specific::Msr;
        let mut star = Msr::new(0xC0000081); // IA32_STAR
        star.write(star_value);

        // Set FMASK MSR - clear IF and DF on SYSCALL entry
        // This prevents interrupts during the critical section before kernel stack switch
        let mut fmask = Msr::new(0xC0000084); // IA32_FMASK
        fmask.write(0x600); // Clear IF (bit 9) and DF (bit 10)
        crate::serial_strln!("[SYSCALL_INIT] FMASK set to 0x600 (clear IF+DF on SYSCALL)");
    }

    // Initialise per-CPU local storage for the SWAPGS-based syscall entry.
    // CpuLocal.kernel_rsp is loaded by `mov rsp, gs:[0]` on every SYSCALL.
    let stack_top = unsafe {
        (&SYSCALL_STACK as *const AlignedStack as usize + core::mem::size_of::<AlignedStack>()) as u64
    };
    super::super::cpu_local::init(stack_top);

    // Stack Guard Page: map a non-present guard page one page below SYSCALL_STACK.
    // Any kernel stack overflow hitting this page will immediately raise #PF.
    let stack_base = unsafe { &SYSCALL_STACK as *const AlignedStack as u64 };
    let guard_vaddr = VirtAddr::new(stack_base.saturating_sub(4096));
    crate::memory::paging::map_guard_page(guard_vaddr);
}
