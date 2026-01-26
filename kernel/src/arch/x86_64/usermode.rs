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
    use crate::task::task;

    let user_cs = super::gdt::user_code_selector();
    let user_ss = super::gdt::user_data_selector();

    crate::serial_println!("[USERMODE] Transitioning to Ring 3 at {:#x}", entry_point.as_u64());

    // Set up Context pointer for syscalls
    // Get current task ID (should be 1 for first user task)
    let current_id = task::get_current_task();
    crate::serial_println!("[USERMODE] Current task ID: {}", current_id);

    if current_id != 0 {
        // Get task and set Context pointer
        if let Some(task_arc) = task::get_task(current_id) {
            let task_locked = task_arc.lock();
            let ctx_ptr = &task_locked.context as *const _ as *mut _;
            crate::serial_println!("[USERMODE] Setting Context ptr to {:#x}", ctx_ptr as usize);
            super::syscall::set_current_context_ptr(ctx_ptr);
        } else {
            crate::serial_println!("[USERMODE] WARNING: Could not get task {}!", current_id);
        }
    } else {
        crate::serial_println!("[USERMODE] WARNING: Current task ID is 0!");
    }

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

/// Allocate user stack at specific address
///
/// Allocates a page for user stack at specified base and returns the top address.
///
/// # Arguments
/// * `stack_base` - Base address for stack page
///
/// # Returns
/// Top of user stack (highest address, stack_base + 4096)
pub fn allocate_user_stack_at(stack_base: u64) -> VirtAddr {
    use crate::memory;
    use crate::memory::paging::flags;

    crate::serial_println!("[USER_STACK] Allocating user stack at {:#x}", stack_base);

    // Allocate one page for user stack
    let stack_page_addr = memory::physical::alloc_page()
        .expect("Failed to allocate user stack page");
    crate::serial_println!("[USER_STACK] Physical page: {:#x}", stack_page_addr);

    // Map at user-accessible address with USER_STACK flags
    crate::serial_println!("[USER_STACK] Mapping with flags USER_STACK");
    memory::paging::map_page(
        stack_base as usize,
        stack_page_addr,
        flags::USER_STACK,
    ).expect("Failed to map user stack page");
    crate::serial_println!("[USER_STACK] Page mapped");

    // Zero the stack page via HHDM
    let hhdm_addr = crate::phys_to_virt(stack_page_addr);
    unsafe {
        core::ptr::write_bytes(hhdm_addr as *mut u8, 0, 4096);
    }

    // Flush TLB
    use x86_64::instructions::tlb;
    unsafe {
        tlb::flush(VirtAddr::new(stack_base));
    }

    let stack_top = stack_base + 4096 - 8;
    crate::serial_println!("[USER_STACK] Stack top: {:#x}", stack_top);

    // Verify we can read the stack
    crate::serial_println!("[USER_STACK] Checking translation...");
    let translate_result = memory::paging::translate(stack_base as usize);
    match translate_result {
        Some(phys) => crate::serial_println!("[USER_STACK] Translation OK: phys={:#x}", phys),
        None => crate::serial_println!("[USER_STACK] Translation FAILED!"),
    }

    // Try reading from the stack
    crate::serial_println!("[USER_STACK] Reading from stack...");
    unsafe {
        let test_byte = core::ptr::read_volatile(stack_base as *const u8);
        crate::serial_println!("[USER_STACK] Read OK: val={:#x}", test_byte);
    }

    crate::serial_println!("[USER_STACK] User stack setup complete");
    VirtAddr::new(stack_top)
}

/// Allocate user stack (legacy wrapper)
///
/// Allocates a page for user stack and returns the top address (stack grows down).
///
/// # Returns
/// Top of user stack (highest address)
pub fn allocate_user_stack() -> VirtAddr {
    // User stack address (high user memory, typical for stacks)
    // Must be below 0x7FFFFFFFF000 to avoid non-canonical addresses
    // when we add 4096 for stack top
    const USER_STACK_BASE: u64 = 0x7FFF_FFFE_F000;
    allocate_user_stack_at(USER_STACK_BASE)
}

/// Map and load user code into address space at specific address
///
/// Maps the user program code at a specified userspace address and loads the code bytes.
///
/// # Arguments
/// * `code` - Slice of code bytes to load
/// * `base_addr` - Virtual address where code should be mapped
///
/// # Returns
/// Virtual address where code was mapped (entry point)
#[inline(never)]
pub fn map_and_load_user_code_at(code: &[u8], base_addr: u64) -> VirtAddr {
    use crate::memory;
    use crate::memory::paging::flags;

    crate::serial_println!("[USER_CODE] Mapping user code at {:#x}, len={}", base_addr, code.len());

    // Calculate number of pages needed
    let code_len = code.len();
    let _pages_needed = (code_len + 4095) / 4096;

    // Allocate physical page for user code
    let code_page_addr = memory::physical::alloc_page()
        .expect("Failed to allocate user code page");
    crate::serial_println!("[USER_CODE] Physical page: {:#x}", code_page_addr);

    // Map at user-accessible address with USER_CODE flags
    crate::serial_println!("[USER_CODE] Mapping with flags USER_CODE (PRESENT | USER_ACCESSIBLE)");
    memory::paging::map_page(
        base_addr as usize,
        code_page_addr,
        flags::USER_CODE,
    ).expect("Failed to map user code page");
    crate::serial_println!("[USER_CODE] Page mapped successfully");

    // Copy code to the page via HHDM
    let hhdm_addr = crate::phys_to_virt(code_page_addr);
    crate::serial_println!("[USER_CODE] Copying {} bytes via HHDM at {:#x}", code_len, hhdm_addr);
    unsafe {
        core::ptr::copy_nonoverlapping(
            code.as_ptr(),
            hhdm_addr as *mut u8,
            code_len,
        );
    }
    crate::serial_println!("[USER_CODE] Code copied");

    // Flush TLB
    use x86_64::instructions::tlb;
    unsafe {
        tlb::flush(VirtAddr::new(base_addr));
    }

    // Verify the mapping
    crate::serial_println!("[USER_CODE] Checking translation...");
    let translate_result = memory::paging::translate(base_addr as usize);
    match translate_result {
        Some(phys) => crate::serial_println!("[USER_CODE] Translation OK: phys={:#x}", phys),
        None => crate::serial_println!("[USER_CODE] Translation FAILED!"),
    }

    // Try reading the first byte from the user address (via kernel)
    crate::serial_println!("[USER_CODE] Reading first byte from user code...");
    unsafe {
        let first_byte = core::ptr::read_volatile(base_addr as *const u8);
        crate::serial_println!("[USER_CODE] Read OK: first_byte={:#x}", first_byte);
    }

    crate::serial_println!("[USER_CODE] User code setup complete");
    VirtAddr::new(base_addr)
}

/// Map and load user code into address space (legacy wrapper)
///
/// Maps the user program code at a fixed userspace address and loads the code bytes.
///
/// # Arguments
/// * `code` - Slice of code bytes to load
///
/// # Returns
/// Virtual address where code was mapped (entry point)
pub fn map_and_load_user_code(code: &[u8]) -> VirtAddr {
    // Fixed user code address (standard ELF user code location)
    const USER_CODE_ADDR: u64 = 0x400000; // 4 MB
    map_and_load_user_code_at(code, USER_CODE_ADDR)
}
