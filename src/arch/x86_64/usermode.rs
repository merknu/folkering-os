//! User Mode Transition
//!
//! Functions for transitioning from kernel mode (Ring 0) to user mode (Ring 3).

use x86_64::VirtAddr;
use x86_64::structures::gdt::SegmentSelector;

/// Jump to user mode (Ring 3)
///
/// This function sets up the IRET stack frame and transitions to user mode.
/// It does not return - execution continues in userspace.
///
/// # Arguments
/// * `entry_point` - Virtual address of user code to execute
/// * `user_stack` - Virtual address of user stack (stack grows down, so pass top of stack)
///
/// # Safety
/// - User code and stack must be mapped and accessible
/// - Entry point must point to valid user code
/// - Stack must have sufficient space
#[no_mangle]
pub unsafe fn jump_to_usermode(entry_point: VirtAddr, user_stack: VirtAddr) -> ! {
    let user_cs = super::gdt::user_code_selector();
    let user_ss = super::gdt::user_data_selector();

    crate::serial_println!("[USERMODE] Jumping to Ring 3...");
    crate::serial_println!("[USERMODE]   Entry point: {:#x}", entry_point.as_u64());
    crate::serial_println!("[USERMODE]   Stack: {:#x}", user_stack.as_u64());
    crate::serial_println!("[USERMODE]   CS: {:#x}", user_cs.0);
    crate::serial_println!("[USERMODE]   SS: {:#x}", user_ss.0);
    crate::serial_println!("[USERMODE] About to execute IRETQ...");

    // IRET stack frame (pushed in reverse order):
    // [SS, RSP, RFLAGS, CS, RIP]
    //
    // RFLAGS: 0x202 = IF (interrupts enabled) + reserved bit 1 (always 1)

    core::arch::asm!(
        // Push IRET frame
        "push {user_ss}",      // SS (user data segment)
        "push {user_rsp}",     // RSP (user stack pointer)
        "pushfq",              // RFLAGS (current flags)
        "pop rax",
        "or rax, 0x200",       // Set IF (interrupts enabled)
        "push rax",            // RFLAGS with IF set
        "push {user_cs}",      // CS (user code segment)
        "push {user_rip}",     // RIP (entry point)

        // Clear registers for security
        "xor rax, rax",
        "xor rbx, rbx",
        "xor rcx, rcx",
        "xor rdx, rdx",
        "xor rsi, rsi",
        "xor rdi, rdi",
        "xor r8, r8",
        "xor r9, r9",
        "xor r10, r10",
        "xor r11, r11",
        "xor r12, r12",
        "xor r13, r13",
        "xor r14, r14",
        "xor r15, r15",

        // Jump to userspace
        "iretq",

        user_ss = in(reg) user_ss.0 as u64,
        user_rsp = in(reg) user_stack.as_u64(),
        user_cs = in(reg) user_cs.0 as u64,
        user_rip = in(reg) entry_point.as_u64(),
        options(noreturn)
    );
}

/// Allocate user stack
///
/// Allocates a page for user stack and returns the top address (stack grows down).
///
/// # Returns
/// Top of user stack (highest address)
pub fn allocate_user_stack() -> VirtAddr {
    use crate::memory;
    use crate::memory::paging::flags;

    // User stack address (high user memory, typical for stacks)
    // Must be below 0x7FFFFFFFF000 to avoid non-canonical addresses
    // when we add 4096 for stack top
    const USER_STACK_BASE: u64 = 0x7FFF_FFFE_F000;

    // Allocate one page for user stack
    let stack_page_addr = memory::physical::alloc_page()
        .expect("Failed to allocate user stack page");

    // Map at user-accessible address with USER_STACK flags
    memory::paging::map_page(
        USER_STACK_BASE as usize,
        stack_page_addr,
        flags::USER_STACK,
    ).expect("Failed to map user stack page");

    // Zero the stack page via HHDM (we can't write through user address from kernel)
    let hhdm_addr = crate::phys_to_virt(stack_page_addr);
    unsafe {
        core::ptr::write_bytes(hhdm_addr as *mut u8, 0, 4096);
    }

    crate::serial_println!("[USERMODE] Allocated user stack at {:#x} (physical {:#x})",
        USER_STACK_BASE, stack_page_addr);

    // Return top of stack (page base + 4096)
    VirtAddr::new(USER_STACK_BASE + 4096)
}

/// Map and load user code into address space
///
/// Maps the user program code at a fixed userspace address and loads the code bytes.
///
/// # Arguments
/// * `code` - Slice of code bytes to load
///
/// # Returns
/// Virtual address where code was mapped (entry point)
pub fn map_and_load_user_code(code: &[u8]) -> VirtAddr {
    use crate::memory;
    use crate::memory::paging::flags;

    // Fixed user code address (standard ELF user code location)
    const USER_CODE_ADDR: u64 = 0x400000; // 4 MB

    // Calculate number of pages needed
    let pages_needed = (code.len() + 4095) / 4096;

    crate::serial_println!("[USERMODE] Mapping {} pages for user code", pages_needed);

    // Allocate physical page for user code
    let code_page_addr = memory::physical::alloc_page()
        .expect("Failed to allocate user code page");

    // Map at user-accessible address with USER_CODE flags
    memory::paging::map_page(
        USER_CODE_ADDR as usize,
        code_page_addr,
        flags::USER_CODE,
    ).expect("Failed to map user code page");

    // Copy code to the page via HHDM (kernel can't write to user addresses directly)
    let hhdm_addr = crate::phys_to_virt(code_page_addr);
    unsafe {
        core::ptr::copy_nonoverlapping(
            code.as_ptr(),
            hhdm_addr as *mut u8,
            code.len(),
        );
    }

    crate::serial_println!("[USERMODE] User code physical {:#x}, virtual {:#x}, size {} bytes",
        code_page_addr, USER_CODE_ADDR, code.len());

    // Dump first 16 bytes to verify it was copied correctly
    crate::serial_print!("[USERMODE] First 16 bytes: ");
    for i in 0..16 {
        let byte = unsafe { *((hhdm_addr + i) as *const u8) };
        crate::serial_print!("{:02x} ", byte);
    }
    crate::serial_println!("");

    // Flush TLB for user code page to ensure mapping is active
    use x86_64::instructions::tlb;
    unsafe {
        tlb::flush(VirtAddr::new(USER_CODE_ADDR));
    }

    crate::serial_println!("[USERMODE] TLB flushed for user code page");

    VirtAddr::new(USER_CODE_ADDR)
}
