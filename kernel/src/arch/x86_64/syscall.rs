//! System Call Interface
//!
//! Fast syscall entry using SYSCALL/SYSRET instructions (AMD64).

use x86_64::registers::model_specific::{Efer, EferFlags, LStar, Star};
use x86_64::structures::gdt::SegmentSelector;
use x86_64::{VirtAddr, PrivilegeLevel};
use crate::task::task;

/// Syscall numbers
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syscall {
    /// Send IPC message
    IpcSend = 0,
    /// Receive IPC message
    IpcReceive = 1,
    /// Reply to IPC message
    IpcReply = 2,
    /// Create shared memory
    ShmemCreate = 3,
    /// Map shared memory
    ShmemMap = 4,
    /// Spawn new task
    Spawn = 5,
    /// Exit current task
    Exit = 6,
    /// Yield CPU
    Yield = 7,
}

/// Debug function called from int_syscall_entry
#[no_mangle]
extern "C" fn debug_int_entry() {
}

/// Debug function called after yield returns
#[no_mangle]
extern "C" fn debug_after_yield() {
}

/// INT 0x80 syscall entry
/// Solution from Phil Opp + Redox research:
/// Switch to dedicated kernel stack, build fresh IRETQ frame there
#[unsafe(naked)]
pub unsafe extern "C" fn int_syscall_entry() {
    core::arch::naked_asm!(
        // INT has pushed: SS, RSP, RFLAGS, CS, RIP (on INT's stack)

        // Call debug function FIRST to verify naked asm is executing
        "call {debug_entry_fn}",

        // Checkpoint 0xA0: INT entry
        "push rax",
        "mov rax, 0xA0",
        "mov qword ptr [rip + {debug_marker}], rax",
        "pop rax",

        // Increment syscall counter (using INT's stack briefly)
        "push rax",
        "mov rax, qword ptr [rip + {syscall_counter}]",
        "inc rax",
        "mov qword ptr [rip + {syscall_counter}], rax",
        "pop rax",

        // Checkpoint 0xA1: After counter increment
        "push rax",
        "mov rax, 0xA1",
        "mov qword ptr [rip + {debug_marker}], rax",
        "pop rax",

        // NOTE: INT frame was already popped by int_0x80_handler
        // We're now on the TSS stack but without the frame

        // Switch to dedicated kernel stack
        "lea rsp, [rip + {kernel_stack}]",
        "add rsp, 16384",  // Top of stack

        // Checkpoint 0xA2: After stack switch
        "mov qword ptr [rip + {debug_marker}], 0xA2",

        // Call yield unconditionally (simplifies logic for now)
        // Context was already saved by previous syscall or spawn
        "call {yield_fn}",

        // Checkpoint 0xA3: After yield returned
        "mov qword ptr [rip + {debug_marker}], 0xA3",

        // Debug: Call after-yield function
        "call {debug_after_yield_fn}",

        // After yield, get NEW context (task switched!)
        "call {get_ctx_fn}",
        "mov r11, rax",    // R11 = context pointer

        // Checkpoint 0xA4: After get_ctx_fn, save context pointer
        "mov qword ptr [rip + {debug_ctx_ptr}], r11",
        "mov qword ptr [rip + {debug_marker}], 0xA4",

        // Save IRETQ frame values for debugging BEFORE building frame
        "mov rax, [r11 + 128]",   // RIP
        "mov qword ptr [rip + {debug_rip}], rax",
        "mov rax, [r11 + 0]",     // RSP
        "mov qword ptr [rip + {debug_rsp}], rax",
        "mov rax, [r11 + 136]",   // RFLAGS
        "mov qword ptr [rip + {debug_rflags}], rax",

        // Checkpoint 0xA5: After saving debug values
        "mov qword ptr [rip + {debug_marker}], 0xA5",

        // Build IRETQ frame on kernel stack (like restore_context_only)
        "mov rax, [r11 + 152]",   // SS
        "push rax",
        "mov rax, [r11 + 0]",     // RSP
        "push rax",
        "mov rax, [r11 + 136]",   // RFLAGS
        "push rax",
        "mov rax, [r11 + 144]",   // CS
        "push rax",
        "mov rax, [r11 + 128]",   // RIP
        "push rax",

        // Checkpoint 0xA6: After building IRETQ frame
        "mov qword ptr [rip + {debug_marker}], 0xA6",

        // Restore all registers from context
        "mov rax, [r11 + 16]",
        "mov rbx, [r11 + 24]",
        "mov rcx, [r11 + 32]",
        "mov rdx, [r11 + 40]",
        "mov rsi, [r11 + 48]",
        "mov rdi, [r11 + 56]",
        "mov rbp, [r11 + 8]",
        "mov r8, [r11 + 64]",
        "mov r9, [r11 + 72]",
        "mov r10, [r11 + 80]",
        "mov r12, [r11 + 96]",
        "mov r13, [r11 + 104]",
        "mov r14, [r11 + 112]",
        "mov r15, [r11 + 120]",

        // Restore R11 last
        "mov r11, [r11 + 88]",

        // Checkpoint 0xA7: Before IRETQ (can't set - no free registers!)
        // The debug_marker will be 0xA6 when we crash

        // IRETQ from kernel stack!
        "iretq",

        get_ctx_fn = sym get_current_task_context_ptr,
        yield_fn = sym syscall_do_yield,
        syscall_counter = sym SYSCALL_COUNT,
        kernel_stack = sym SYSCALL_STACK,
        debug_marker = sym DEBUG_MARKER,
        debug_ctx_ptr = sym DEBUG_CONTEXT_PTR,
        debug_rip = sym DEBUG_RIP,
        debug_rsp = sym DEBUG_RSP,
        debug_rflags = sym DEBUG_RFLAGS,
        debug_entry_fn = sym debug_int_entry,
        debug_after_yield_fn = sym debug_after_yield,
    );
}

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
    let kernel_cs = super::gdt::kernel_code_selector();
    let kernel_data = super::gdt::kernel_data_selector();  // 0x10

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
}

/// Per-CPU context pointer (single-core for now)
/// This is set during context switch to avoid mutex locking in syscall path
static CURRENT_CONTEXT_PTR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Kernel syscall stack (16KB)
/// Used for handling syscalls - SYSCALL doesn't switch stacks automatically
#[no_mangle]
#[link_section = ".bss"]
static mut SYSCALL_STACK: [u8; 16384] = [0; 16384];

/// Syscall counter for debugging
#[no_mangle]
static SYSCALL_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug marker for tracking exact crash location
#[no_mangle]
pub static DEBUG_MARKER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: Last context pointer value (for crash analysis)
/// Also used to store RIP before IRETQ
#[no_mangle]
static DEBUG_CONTEXT_PTR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: RIP value before IRETQ
#[no_mangle]
static DEBUG_RIP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: RAX value at yield check
#[no_mangle]
static DEBUG_RAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: RSP value before IRETQ
#[no_mangle]
static DEBUG_RSP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: RFLAGS value before IRETQ
#[no_mangle]
static DEBUG_RFLAGS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: RCX value at syscall entry (saved RIP from SYSCALL instruction)
#[no_mangle]
static DEBUG_RCX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Temporary storage for user R15 during syscall (R15 is used as Context pointer)
#[no_mangle]
static USER_R15_SAVE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Temporary storage for user R12 during syscall (R12 is used for saved RIP)
#[no_mangle]
static USER_R12_SAVE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Temporary storage for user RSI during syscall (RSI clobbered by function calls)
#[no_mangle]
static USER_RSI_SAVE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Temporary storage for user RDX during syscall (RDX clobbered by function calls)
#[no_mangle]
static USER_RDX_SAVE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Temporary storage for user R13 during syscall (R13 is used for saved RFLAGS)
#[no_mangle]
static USER_R13_SAVE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Get the current syscall count
pub fn get_syscall_count() -> u64 {
    SYSCALL_COUNT.load(core::sync::atomic::Ordering::Relaxed)
}

/// Get the debug marker value
pub fn get_debug_marker() -> u64 {
    DEBUG_MARKER.load(core::sync::atomic::Ordering::Relaxed)
}

/// Get DEBUG_RAX value
pub fn get_debug_rax() -> u64 {
    DEBUG_RAX.load(core::sync::atomic::Ordering::Relaxed)
}

/// Set the debug marker value (for debugging from Rust code)
pub fn set_debug_marker(value: u64) {
    DEBUG_MARKER.store(value, core::sync::atomic::Ordering::Relaxed);
}

/// Get the debug context pointer value
pub fn get_debug_context_ptr() -> u64 {
    DEBUG_CONTEXT_PTR.load(core::sync::atomic::Ordering::Relaxed)
}

/// Debug function to save R15 value
#[no_mangle]
extern "C" fn debug_save_r15_value(value: u64) {
    DEBUG_CONTEXT_PTR.store(value, core::sync::atomic::Ordering::Relaxed);
}

/// Get the debug RIP value
pub fn get_debug_rip() -> u64 {
    DEBUG_RIP.load(core::sync::atomic::Ordering::Relaxed)
}

/// Get the debug RSP value
pub fn get_debug_rsp() -> u64 {
    DEBUG_RSP.load(core::sync::atomic::Ordering::Relaxed)
}

/// Get the debug RFLAGS value
pub fn get_debug_rflags() -> u64 {
    DEBUG_RFLAGS.load(core::sync::atomic::Ordering::Relaxed)
}

/// Get the debug RCX value (saved RIP from SYSCALL)
pub fn get_debug_rcx() -> u64 {
    DEBUG_RCX.load(core::sync::atomic::Ordering::Relaxed)
}

/// Debug: Check if page is mapped before IRETQ
#[no_mangle]
extern "C" fn debug_check_page_mapping(rip: u64) {
    use crate::memory::paging;

    crate::serial_println!("[DEBUG_IRETQ] About to IRETQ to RIP={:#x}", rip);

    match paging::translate(rip as usize) {
        Some(phys) => {
            crate::serial_println!("[DEBUG_IRETQ] RIP page IS mapped: virt={:#x} -> phys={:#x}", rip, phys);
        }
        None => {
            crate::serial_println!("[DEBUG_IRETQ] ERROR: RIP page NOT MAPPED! virt={:#x}", rip);
        }
    }
}

/// Debug: Check RSP value before building IRETQ frame
#[no_mangle]
extern "C" fn debug_check_rsp(rsp: u64) {
    crate::serial_println!("[DEBUG_IRETQ] Kernel RSP before building frame: {:#x}", rsp);

    // Check if RSP is in valid kernel range
    if rsp < 0xFFFF_8000_0000_0000 {
        crate::serial_println!("[DEBUG_IRETQ] ERROR: RSP is NOT in kernel space!");
    } else if rsp == 0 {
        crate::serial_println!("[DEBUG_IRETQ] ERROR: RSP is 0!");
    } else {
        crate::serial_println!("[DEBUG_IRETQ] RSP looks valid (kernel space)");
    }
}

/// Debug: Show all IRETQ frame values before pushing
#[no_mangle]
extern "C" fn debug_show_iretq_frame(ctx_ptr: u64) {
    use crate::task::task::Context;
    use crate::memory::paging;

    let ctx = unsafe { &*(ctx_ptr as *const Context) };

    crate::serial_println!("[DEBUG_IRETQ] IRETQ frame values:");
    crate::serial_println!("  RIP:    {:#x}", ctx.rip);
    crate::serial_println!("  CS:     {:#x}", ctx.cs);
    crate::serial_println!("  RFLAGS: {:#x}", ctx.rflags);
    crate::serial_println!("  RSP:    {:#x}", ctx.rsp);
    crate::serial_println!("  SS:     {:#x}", ctx.ss);

    // Check if user stack page is mapped
    match paging::translate(ctx.rsp as usize) {
        Some(phys) => {
            crate::serial_println!("[DEBUG_IRETQ] User stack IS mapped: virt={:#x} -> phys={:#x}", ctx.rsp, phys);
        }
        None => {
            crate::serial_println!("[DEBUG_IRETQ] ERROR: User stack NOT MAPPED! virt={:#x}", ctx.rsp);
        }
    }
}

/// Set the current context pointer (called during task switch)
pub fn set_current_context_ptr(ptr: *mut crate::task::task::Context) {
    CURRENT_CONTEXT_PTR.store(ptr as usize, core::sync::atomic::Ordering::Release);
}

/// Get current task's context pointer (lock-free, fast path for syscalls)
#[no_mangle]
extern "C" fn get_current_task_context_ptr() -> *mut crate::task::task::Context {
    CURRENT_CONTEXT_PTR.load(core::sync::atomic::Ordering::Acquire) as *mut _
}

/// Yield CPU from syscall (may not return if task switch)
#[no_mangle]
extern "C" fn syscall_do_yield() {
    crate::task::scheduler::yield_cpu();
}

/// Debug function to print syscall entry state
#[inline(never)]
#[no_mangle]
extern "C" fn debug_syscall_entry(rcx: u64, rax: u64) {
    static mut CALL_COUNT: u64 = 0;
    unsafe {
        CALL_COUNT += 1;
        crate::serial_println!("[SYSCALL_ENTRY] Call #{}: RAX={:#x} (syscall num), RCX={:#x} (saved RIP)",
                              CALL_COUNT, rax, rcx);
    }
}

/// Debug function to print saved context
#[inline(never)]
#[no_mangle]
extern "C" fn debug_context_saved(ctx_ptr: usize) {
    static mut CALL_COUNT: u64 = 0;
    unsafe {
        CALL_COUNT += 1;
        let ctx = &*(ctx_ptr as *const crate::task::task::Context);
        crate::serial_println!("[CONTEXT_SAVED] Call #{}: Context at {:#x}, RIP={:#x}, RSP={:#x}",
                              CALL_COUNT, ctx_ptr, ctx.rip, ctx.rsp);
    }
}

/// Debug: Increment entry counter (quiet)
#[no_mangle]
extern "C" fn debug_syscall_entry_hit(_rax: u64) {
}

#[no_mangle]
extern "C" fn debug_before_yield_check(rax: u64) {
    static mut COUNT: u64 = 0;
    unsafe {
        COUNT += 1;
        if COUNT <= 3 {
            crate::serial_println!("[YIELD_CHECK] Before comparison #{}: RAX={}", COUNT, rax);
        }
    }
}

#[no_mangle]
extern "C" fn debug_iretq_frame(rip: u64, cs: u64, rflags: u64, rsp: u64, ss: u64) {
    static mut COUNT: u64 = 0;
    unsafe {
        COUNT += 1;
        crate::serial_println!("[IRETQ_FRAME #{}] RIP={:#x}, CS={:#x}, RFLAGS={:#x}, RSP={:#x}, SS={:#x}",
            COUNT, rip, cs, rflags, rsp, ss);
    }
}

/// Debug: print context before loading for IRETQ
#[no_mangle]
extern "C" fn debug_context_values(ctx_ptr: usize) {
    unsafe {
        let ctx = &*(ctx_ptr as *const crate::task::task::Context);
        crate::serial_println!("[CTX_DEBUG] Context at {:#x}:", ctx_ptr);
        crate::serial_println!("  RIP={:#x}, CS={:#x}, RFLAGS={:#x}", ctx.rip, ctx.cs, ctx.rflags);
        crate::serial_println!("  RSP={:#x}, SS={:#x}", ctx.rsp, ctx.ss);
        crate::serial_println!("  RAX={:#x}, RCX={:#x}", ctx.rax, ctx.rcx);
    }
}

/// Syscall entry point
///
/// Saves ALL registers to current task's Context on entry.
/// For yield, switches tasks. For other syscalls, calls handler.
/// Restores from (possibly different) task's Context on exit.
///
/// # Register Convention (x86-64 SYSCALL)
/// - RAX: syscall number
/// - RDI: arg1
/// - RSI: arg2
/// - RDX: arg3
/// - R10: arg4 (RCX is used by SYSCALL for return address)
/// - R8:  arg5
/// - R9:  arg6
/// - RCX: saved RIP (by SYSCALL instruction)
/// - R11: saved RFLAGS (by SYSCALL instruction)
#[unsafe(naked)]
extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        // SYSCALL saved: RIP→RCX, RFLAGS→R11, switched to kernel CS/SS

        // CRITICAL FIRST STEP: Save user RSP before touching stack!
        // SYSCALL doesn't switch stacks, so RSP still points to user stack
        "mov r14, rsp",  // R14 = user RSP (BEFORE any pushes!)

        // Save user R12 and R13 to statics BEFORE overwriting them
        // (R12/R13 will be used to hold saved RIP/RFLAGS from SYSCALL)
        "push rax",
        "mov rax, r12",
        "mov qword ptr [rip + {user_r12_save}], rax",
        "mov rax, r13",
        "mov qword ptr [rip + {user_r13_save}], rax",
        "pop rax",

        // CRITICAL SECOND STEP: Save RCX and R11 to callee-saved registers
        // These are caller-saved and will be corrupted by function calls
        "mov r12, rcx",  // R12 = user RIP (from SYSCALL)
        "mov r13, r11",  // R13 = user RFLAGS (from SYSCALL)

        // Save RSI and RDX to statics BEFORE any function calls
        // (caller-saved registers, clobbered by debug_entry_hit and get_ctx_fn)
        "push rax",
        "mov rax, rsi",
        "mov qword ptr [rip + {user_rsi_save}], rax",
        "mov rax, rdx",
        "mov qword ptr [rip + {user_rdx_save}], rax",
        "pop rax",

        // DEBUG: Save RCX (user RIP) from R12
        "push rax",
        "mov rax, r12",
        "mov qword ptr [rip + {debug_rcx}], rax",
        "mov qword ptr [rip + {debug_rip}], rax",
        "pop rax",

        // DEBUG: Call function to verify we entered and check syscall number
        "push rax",       // Save RAX (function will overwrite it with return value!)
        "push rdi",
        "mov rdi, rax",   // Pass RAX (syscall number) as first arg
        "call {debug_entry_hit}",
        "pop rdi",
        "pop rax",        // Restore RAX

        // Increment syscall counter (for debugging)
        "push rax",
        "mov rax, qword ptr [rip + {syscall_counter}]",
        "inc rax",
        "mov qword ptr [rip + {syscall_counter}], rax",
        "pop rax",

        // Step 0: Switch to kernel stack immediately

        // Load kernel stack pointer (top of SYSCALL_STACK)
        "lea rsp, [rip + {syscall_stack}]",  // Load base address
        "add rsp, 16384",                     // Add stack size to get top

        // Step 1: Get current task's Context pointer
        // R12 and R13 are callee-saved, so they'll survive the call
        // We only need to save caller-saved registers that we care about

        // DEBUG: Save RAX before first push (should be 7)
        "push rbx",
        "mov rbx, rax",
        "mov qword ptr [rip + {debug_rax}], rbx",  // Save RAX before ANY pushes
        "pop rbx",

        // Save user R15 to static (we'll need R15 for Context pointer)
        "push rax",
        "mov rax, r15",
        "mov qword ptr [rip + {user_r15_save}], rax",
        "pop rax",

        "push rax",      // Syscall number
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r14",      // User RSP

        "call {get_ctx_fn}",
        // RAX now has Context* (or NULL if error)

        "mov r15, rax",  // R15 = Context pointer (overwrites user R15!)

        // DEBUG: Print R12 value BEFORE saving to Context.rip
        "push rax",
        "push rdi",
        "mov rdi, r12",   // Pass R12 as first argument
        "call {debug_r12_before_save_fn}",
        "pop rdi",
        "pop rax",

        // CRITICAL FIX: Save user RIP and RFLAGS IMMEDIATELY before anything can corrupt them!
        // We saved RCX→R12 and R11→R13 immediately after SYSCALL (lines 537-538)
        // R12/R13 are CALLEE-SAVED, so they survived the get_ctx_fn() call!
        // RCX/R11 are CALLER-SAVED, so get_ctx_fn() may have clobbered them!
        "mov [r15 + 128], r12",  // Save RIP from R12 (user RIP preserved across function call)
        "mov [r15 + 136], r13",  // Save RFLAGS from R13 (user RFLAGS preserved across function call)

        // DEBUG: Verify what we saved
        "push rax",
        "push rdi",
        "mov rdi, [r15 + 128]",   // Load what we just saved
        "call {debug_rip_after_save_fn}",
        "pop rdi",
        "pop rax",

        // Restore actual user R12/R13 values from statics into their Context slots
        "push rbx",
        "mov rbx, qword ptr [rip + {user_r12_save}]",
        "mov [r15 + 96], rbx",   // R12 slot: actual user R12
        "mov rbx, qword ptr [rip + {user_r13_save}]",
        "mov [r15 + 104], rbx",  // R13 slot: actual user R13
        "pop rbx",

        // Restore all saved registers
        "pop r14",       // User RSP
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rax",       // Syscall number

        // DEBUG: Verify R12 (user RIP) is correct
        "push rbx",
        "mov rbx, r12",
        "mov qword ptr [rip + {debug_rsp}], rbx",  // Abuse DEBUG_RSP for R12-after-call
        "pop rbx",

        // DEBUG: Save RAX immediately after pop to verify stack is correct
        "push rbx",
        "mov rbx, rax",
        "mov qword ptr [rip + {debug_rax}], rbx",  // Should be 7 if pop worked
        "pop rbx",

        // Note: Assuming R15 is valid (if NULL, we're in big trouble anyway)

        // Step 2: Save remaining registers to Context
        // Context layout: rsp, rbp, rax, rbx, rcx, rdx, rsi, rdi,
        //                 r8, r9, r10, r11, r12, r13, r14, r15,
        //                 rip, rflags, cs, ss
        // NOTE: RIP and RFLAGS already saved above! R12/R13 also already saved!

        // Restore original RSI/RDX from statics before saving to Context
        "mov rsi, qword ptr [rip + {user_rsi_save}]",
        "mov rdx, qword ptr [rip + {user_rdx_save}]",

        "mov [r15 + 0], r14",      // RSP (user RSP saved before stack switch)
        "mov [r15 + 8], rbp",      // RBP
        "mov [r15 + 16], rax",     // RAX (syscall number)
        "mov [r15 + 24], rbx",     // RBX
        "mov [r15 + 32], rcx",     // RCX (NOTE: Contains user RIP after SYSCALL, not user's RCX!)
        "mov [r15 + 40], rdx",     // RDX (restored from static)
        "mov [r15 + 48], rsi",     // RSI (restored from static)
        "mov [r15 + 56], rdi",     // RDI
        "mov [r15 + 64], r8",      // R8
        "mov [r15 + 72], r9",      // R9
        "mov [r15 + 80], r10",     // R10
        "mov [r15 + 88], r11",     // R11 (NOTE: Contains user RFLAGS after SYSCALL, not user's R11!)
        // R12/R13 already saved above (offset 96, 104)
        "mov [r15 + 112], r14",    // R14

        // Save user R15 from static
        "push rbx",
        "mov rbx, qword ptr [rip + {user_r15_save}]",
        "mov [r15 + 120], rbx",    // R15 (user R15 value from static)
        "pop rbx",

        // RIP and RFLAGS already saved above! (offsets 128, 136)

        // DEBUG: Save RAX value BEFORE push (should be syscall number = 7)
        "push rbx",
        "mov rbx, rax",
        "mov qword ptr [rip + {debug_rax}], rbx",  // Save RAX before push
        "pop rbx",

        // DEBUG: Save what we wrote to Context.rip
        "push rax",
        "mov rax, [r15 + 128]",  // Load back what we just saved
        "mov qword ptr [rip + {debug_rip}], rax",  // Save to DEBUG_RIP
        "pop rax",

        // DEBUG: Immediately save RAX after pop to verify it's still correct
        "push rbx",
        "mov rbx, rax",
        "mov qword ptr [rip + {debug_rax}], rbx",  // Save RAX value right after pop
        "pop rbx",

        // Save USER segment selectors (SYSCALL switches to kernel segments,
        // but we need user segments for IRETQ back to user mode)
        "mov qword ptr [r15 + 144], 0x23",    // CS = user code (0x20 | RPL=3)
        "mov qword ptr [r15 + 152], 0x1B",    // SS = user data (0x18 | RPL=3)

        // NOTE: MSR save removed - caused format! crashes at boot

        // DEBUG: Print what RIP we just saved to Context
        "push rax",
        "push rdi",
        "mov rdi, [r15 + 128]",  // Load RIP from Context
        "call {debug_context_rip_saved_fn}",
        "pop rdi",
        "pop rax",

        "cmp rax, 7",
        "je yield_path",   // Jump to yield path

        // DEBUG: If we reach here, we didn't jump to yield_path (0xAA)
        "push rbx",
        "mov rbx, 0xAA",
        "mov qword ptr [rip + {debug_marker}], rbx",
        "pop rbx",

        // 0xAB = before push rax
        "push rbx",
        "mov rbx, 0xAB",
        "mov qword ptr [rip + {debug_marker}], rbx",
        "pop rbx",

        "push rax",

        // 0xBB = after push rax (using rax which was just pushed)
        "mov rax, 0xBB",
        "mov qword ptr [rip + {debug_marker}], rax",

        // 0xB1 = immediate after setting marker (no push/pop, reuse rax)
        "mov rax, 0xB1",
        "mov qword ptr [rip + {debug_marker}], rax",

        // Restore RAX value from stack (peek, don't pop)
        "mov rax, [rsp]",

        // 0xB2 = after restoring rax from stack peek
        "push rbx",
        "mov rbx, 0xB2",
        "mov qword ptr [rip + {debug_marker}], rbx",
        "pop rbx",

        "pop rax",

        // DEBUG: 0xAE = After pop rax, before normal_path
        "push rbx",
        "mov rbx, 0xAE",
        "mov qword ptr [rip + {debug_marker}], rbx",
        "pop rbx",

        // Normal syscall path
        // Rearrange arguments for C ABI
        "normal_path:",

        // DEBUG: 0xBC = About to rearrange args (use push/pop pattern for safety)
        "push rbx",
        "mov rbx, 0xBC",
        "mov qword ptr [rip + {debug_marker}], rbx",
        "pop rbx",

        // Restore original RSI and RDX from statics (clobbered by earlier function calls)
        "mov rsi, qword ptr [rip + {user_rsi_save}]",
        "mov rdx, qword ptr [rip + {user_rdx_save}]",

        "push rax",
        "mov r9, r8",
        "mov r8, r10",
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "pop rdi",

        // DEBUG: 0xBD = About to call syscall_handler (use push/pop pattern)
        "push rbx",
        "mov rbx, 0xBD",
        "mov qword ptr [rip + {debug_marker}], rbx",
        "pop rbx",

        "call {handler}",

        // DEBUG: 0xBE = Handler returned
        "push rbx",
        "mov rbx, 0xBE",
        "mov qword ptr [rip + {debug_marker}], rbx",
        "pop rbx",

        // Handler returned with result in RAX
        // Check if result is 0xFFFF_FFFF_FFFF_FFFE (EWOULDBLOCK - should yield)
        "mov r14, rax",           // R14 = return value
        "mov r13, 0xFFFFFFFFFFFFFFFE", // R13 = EWOULDBLOCK marker
        "cmp rax, r13",
        "je yield_path",          // If EWOULDBLOCK, go to yield path

        // Normal return path - restore and return to user
        // Step 4: Restore from (possibly same) task's Context
        "call {get_ctx_fn}",
        "mov r15, rax",

        // Restore all registers from Context (EXCEPT RAX - use return value!)
        "mov r11, [r15 + 136]",   // RFLAGS
        "mov rcx, [r15 + 128]",   // RIP
        // Skip RAX - use handler return value (in R14)
        "mov rbx, [r15 + 24]",
        "mov rdx, [r15 + 40]",
        "mov rsi, [r15 + 48]",
        "mov rdi, [r15 + 56]",
        "mov rbp, [r15 + 8]",
        "mov r8, [r15 + 64]",
        "mov r9, [r15 + 72]",
        "mov r10, [r15 + 80]",
        "mov r12, [r15 + 96]",
        "mov r13, [r15 + 104]",
        // R14 has return value, restore later
        "mov rax, r14",           // RAX = return value from handler
        "mov r14, [r15 + 112]",   // Now restore R14

        // NOTE: MSR restore removed - caused format! crashes at boot

        // Build IRETQ frame: SS, RSP, RFLAGS, CS, RIP
        "push qword ptr [r15 + 152]",  // SS
        "push qword ptr [r15 + 0]",    // RSP
        "push r11",                     // RFLAGS (already in R11)
        "push qword ptr [r15 + 144]",  // CS
        "push rcx",                     // RIP (already in RCX)

        // Restore all registers
        "mov rbx, [r15 + 24]",
        "mov rdx, [r15 + 40]",
        "mov rsi, [r15 + 48]",
        "mov rdi, [r15 + 56]",
        "mov rbp, [r15 + 8]",
        "mov r8, [r15 + 64]",
        "mov r9, [r15 + 72]",
        "mov r10, [r15 + 80]",
        "mov r12, [r15 + 96]",
        "mov r13, [r15 + 104]",
        "mov r14, [r15 + 112]",
        "mov r15, [r15 + 120]",
        // RAX already has return value

        "iretq",

        // Yield path - switch to next task
        "yield_path:",

        // DEBUG: Mark that we entered yield path (checkpoint 0xCC first)
        "push rax",
        "mov rax, 0xCC",
        "mov qword ptr [rip + {debug_marker}], rax",
        "pop rax",

        // DEBUG: Second marker for yield path
        "push rax",
        "mov rax, 0xFF",
        "mov qword ptr [rip + {debug_marker}], rax",
        "pop rax",

        // Update RAX in Context to return value (0 for yield)
        "mov qword ptr [r15 + 16], 0",  // Set RAX=0 (yield return value)

        // CANARY DISABLED FOR DEBUGGING - just call yield directly
        "call {yield_fn}",

        // Debug: Increment counter after yield (if we returned)
        "push rax",
        "mov rax, qword ptr [rip + {syscall_counter}]",
        "inc rax",
        "mov qword ptr [rip + {syscall_counter}], rax",
        "pop rax",

        // Debug: Counter = 4 (after yield returned)
        "push rax",
        "mov rax, qword ptr [rip + {syscall_counter}]",
        "inc rax",
        "mov qword ptr [rip + {syscall_counter}], rax",
        "pop rax",

        // If we get here, no task switch - restore and return
        "call {get_ctx_fn}",
        "mov r15, rax",  // CRITICAL: Save Context* to R15 IMMEDIATELY (RAX will be clobbered by next call!)

        // DEBUG: Call function to save Context* value (uses R15, not RAX, since calls clobber RAX)
        "push rdi",
        "mov rdi, r15",  // Pass Context* from R15 (callee-saved)
        "call {debug_save_r15_fn}",
        "pop rdi",

        // CRITICAL DEBUG: Verify Context values before building IRETQ frame
        "push rdi",
        "mov rdi, r15",  // Pass Context* as first arg (R15 is callee-saved, still valid!)
        "call {debug_verify_ctx_fn}",
        "pop rdi",

        // Debug: Counter = 5 (after get_ctx_fn)
        "push rax",
        "mov rax, qword ptr [rip + {syscall_counter}]",
        "inc rax",
        "mov qword ptr [rip + {syscall_counter}], rax",
        "pop rax",

        // DEBUG: Verify R15 is valid before dereferencing
        "push rax",
        "mov rax, r15",
        "mov qword ptr [rip + {debug_rcx}], rax",  // Save R15 value (abuse debug_rcx temporarily)
        "pop rax",

        "mov r11, [r15 + 136]",   // RFLAGS
        "mov rcx, [r15 + 128]",   // RIP

        // NOTE: MSR restore removed - caused format! crashes at boot

        // DEBUG: Verify Context address and CS/SS values
        "push rax",
        "mov rax, qword ptr [r15 + 144]",             // Load CS value
        "mov qword ptr [rip + {debug_rsp}], rax",    // Save CS (abuse debug_rsp)
        "mov rax, qword ptr [r15 + 152]",             // Load SS value
        "mov qword ptr [rip + {debug_rflags}], rax", // Save SS (abuse debug_rflags)
        "mov qword ptr [rip + {debug_rcx}], rcx",    // RCX = RIP value
        "mov qword ptr [rip + {debug_rax}], r11",    // R11 = RFLAGS value
        "mov rax, 0xABCD1234",
        "mov qword ptr [rip + {debug_marker}], rax",
        "pop rax",

        // Build IRETQ frame on kernel stack (not user stack!)
        // IRETQ pops: RIP, CS, RFLAGS, RSP, SS (so push in REVERSE order!)
        "mov qword ptr [rip + {debug_marker}], 0xBB01",  // About to push SS
        "push qword ptr [r15 + 152]",  // SS (offset 152 = user data 0x1B)
        "mov qword ptr [rip + {debug_marker}], 0xBB02",  // About to push RSP
        "push qword ptr [r15 + 0]",    // RSP
        "mov qword ptr [rip + {debug_marker}], 0xBB03",  // About to push RFLAGS
        "push r11",                     // RFLAGS (R11)
        "mov qword ptr [rip + {debug_marker}], 0xBB04",  // About to push CS
        "push qword ptr [r15 + 144]",  // CS (offset 144 = user code 0x23)
        "mov qword ptr [rip + {debug_marker}], 0xBB05",  // About to push RIP
        "push rcx",                     // RIP (RCX)
        "mov qword ptr [rip + {debug_marker}], 0xBB06",  // Frame complete

        // DEBUG: Verify IRETQ frame on stack
        "mov qword ptr [rip + {debug_marker}], 0xCC01",  // About to push rax
        "push rax",
        "mov rax, [rsp + 8]",           // RIP value (skip pushed RAX)
        "mov qword ptr [rip + {debug_rip}], rax",
        "mov rax, [rsp + 16]",          // CS value
        "mov qword ptr [rip + {debug_rsp}], rax",
        "mov rax, [rsp + 24]",          // RFLAGS value
        "mov qword ptr [rip + {debug_rflags}], rax",
        "pop rax",
        "mov qword ptr [rip + {debug_marker}], 0xCC02",  // Debug section done

        // Restore all general-purpose registers (EXCEPT RCX/R11 which are in IRETQ frame!)
        "mov qword ptr [rip + {debug_marker}], 0xDD01",  // About to restore GPRs
        "xor rax, rax",           // RAX = 0 (yield return value)
        "mov rbx, [r15 + 24]",
        "mov rcx, [r15 + 32]",    // Restore user RCX
        "mov rdx, [r15 + 40]",
        "mov rsi, [r15 + 48]",
        "mov rdi, [r15 + 56]",
        "mov rbp, [r15 + 8]",
        "mov r8, [r15 + 64]",
        "mov r9, [r15 + 72]",
        "mov r10, [r15 + 80]",
        "mov r11, [r15 + 88]",    // Restore user R11
        "mov r12, [r15 + 96]",
        "mov r13, [r15 + 104]",
        "mov r14, [r15 + 112]",
        "mov qword ptr [rip + {debug_marker}], 0xDD02",  // About to clobber R15
        "mov r15, [r15 + 120]",   // R15 = user R15
        "mov qword ptr [rip + {debug_marker}], 0xEE01",  // About to IRETQ

        // Return to user mode via IRETQ
        "iretq",

        get_ctx_fn = sym get_current_task_context_ptr,
        handler = sym syscall_handler,
        yield_fn = sym syscall_do_yield,
        syscall_stack = sym SYSCALL_STACK,
        syscall_counter = sym SYSCALL_COUNT,
        user_r15_save = sym USER_R15_SAVE,
        debug_rcx = sym DEBUG_RCX,
        debug_rip = sym DEBUG_RIP,
        debug_rax = sym DEBUG_RAX,
        debug_rsp = sym DEBUG_RSP,
        debug_rflags = sym DEBUG_RFLAGS,
        debug_marker = sym DEBUG_MARKER,
        debug_entry_hit = sym debug_syscall_entry_hit,
        debug_save_r15_fn = sym debug_save_r15_value,
        debug_r12_before_save_fn = sym debug_r12_before_save,
        debug_rip_after_save_fn = sym debug_rip_after_save,
        debug_context_rip_saved_fn = sym debug_context_rip_saved,
        debug_verify_ctx_fn = sym debug_verify_context_before_iretq,
        user_r12_save = sym USER_R12_SAVE,
        user_r13_save = sym USER_R13_SAVE,
        user_rsi_save = sym USER_RSI_SAVE,
        user_rdx_save = sym USER_RDX_SAVE,
    );
}

/// Syscall handler (called from assembly)
#[no_mangle]
extern "C" fn syscall_handler(
    syscall_num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    arg6: u64,
) -> u64 {
    let current_task = crate::task::task::get_current_task();
    crate::task::statistics::record_syscall(current_task);

    match syscall_num {
        0 => syscall_ipc_send(arg1, arg2, arg3),
        1 => syscall_ipc_receive(arg1),
        2 => syscall_ipc_reply(arg1, arg2),
        3 => syscall_shmem_create(arg1),
        4 => syscall_shmem_map(arg1, arg2),
        5 => syscall_spawn(arg1, arg2),
        6 => syscall_exit(arg1),
        7 => syscall_yield(),
        8 => syscall_read_key(),
        9 => syscall_write_char(arg1),
        10 => syscall_get_pid(),
        11 => syscall_task_list(),
        12 => syscall_uptime(),
        13 => syscall_fs_read_dir(arg1, arg2),
        14 => syscall_fs_read_file(arg1, arg2, arg3),
        _ => {
            crate::drivers::serial::write_str("[HANDLER] Invalid syscall!\n");
            u64::MAX // Return error
        }
    }
}

// ===== Syscall Implementations =====

// Option B: Simplified register-based IPC for testing
// These syscalls work directly with register values, no memory pointers needed
// This allows the simple test programs to work without stack allocation

fn syscall_ipc_send(target: u64, payload0: u64, payload1: u64) -> u64 {
    use crate::ipc::{IpcMessage, ipc_send};
    use crate::task::task::get_current_task;

    let mut msg = IpcMessage::new_request([payload0, payload1, 0, 0]);
    msg.sender = get_current_task();

    let target_id = target as u32;
    match ipc_send(target_id, &msg) {
        Ok(reply) => {
            crate::task::statistics::record_ipc_sent(get_current_task());
            reply.payload[0]
        }
        Err(_err) => {
            u64::MAX
        }
    }
}

// Full version with memory pointers (Option A - for future use)
#[allow(dead_code)]
fn syscall_ipc_send_full(target: u64, msg_ptr: u64, _flags: u64) -> u64 {
    use crate::ipc::{IpcMessage, ipc_send};

    // 1. Validate pointer
    if msg_ptr == 0 {
        return u64::MAX; // EINVAL
    }

    // 2. Copy message from userspace
    let msg = unsafe {
        // TODO: Validate that msg_ptr is in userspace
        // For now, trust the pointer
        core::ptr::read(msg_ptr as *const IpcMessage)
    };

    // 3. Call kernel IPC send
    let target_id = target as u32;
    match ipc_send(target_id, &msg) {
        Ok(reply) => {
            // Copy reply back to userspace
            unsafe {
                core::ptr::write(msg_ptr as *mut IpcMessage, reply);
            }
            0 // Success
        }
        Err(err) => {
            // Convert errno to u64
            err as u64
        }
    }
}

fn syscall_ipc_receive(_from_filter: u64) -> u64 {
    use crate::ipc::{ipc_receive, send::Errno};

    // Non-blocking receive - userspace handles retries
    // This is necessary because yield_cpu() returns to userspace, not to the kernel loop
    // NOTE: Return value 0xFFFFFFFFFFFFFFFE triggers yield_path in syscall_entry,
    // so we use a different error code to avoid that.
    match ipc_receive() {
        Ok(msg) => {
            // Record IPC receive
            let current_task_id = crate::task::task::get_current_task();
            crate::task::statistics::record_ipc_received(current_task_id);

            // Save received message for later reply
            if let Some(task) = crate::task::task::get_task(current_task_id) {
                task.lock().ipc_reply = Some(msg);
            }

            // Return sender ID in lower 32 bits, first payload in upper 32 bits
            let result = ((msg.payload[0] & 0xFFFFFFFF) << 32) | (msg.sender as u64);
            result
        }
        Err(Errno::EWOULDBLOCK) => {
            // No messages available - return -3 as error code
            // IMPORTANT: NOT 0xFFFFFFFFFFFFFFFE which triggers yield_path!
            0xFFFF_FFFF_FFFF_FFFD
        }
        Err(_err) => {
            0xFFFF_FFFF_FFFF_FFFC
        }
    }
}

// Full version with memory pointer (Option A - for future use)
#[allow(dead_code)]
fn syscall_ipc_receive_full(msg_ptr: u64) -> u64 {
    use crate::ipc::{IpcMessage, ipc_receive};

    // 1. Validate pointer
    if msg_ptr == 0 {
        return u64::MAX; // EINVAL
    }

    // 2. Call kernel IPC receive (blocking)
    match ipc_receive() {
        Ok(msg) => {
            // Copy message to userspace
            unsafe {
                core::ptr::write(msg_ptr as *mut IpcMessage, msg);
            }
            0 // Success
        }
        Err(err) => err as u64,
    }
}

fn syscall_ipc_reply(payload0: u64, payload1: u64) -> u64 {
    use crate::ipc::{ipc_reply, IpcMessage};
    use crate::task::task;

    crate::serial_println!("[SYSCALL] ipc_reply_simple(payload0={:#x}, payload1={:#x})",
                          payload0, payload1);

    // Get current task to find the pending IPC reply context
    let current_task_id = task::get_current_task();

    // Get the task structure to access the pending reply
    let task_arc = match task::get_task(current_task_id) {
        Some(t) => t,
        None => {
            crate::serial_println!("[SYSCALL] ipc_reply FAILED - task not found");
            return u64::MAX;
        }
    };

    let request_msg: IpcMessage = {
        let task_guard = task_arc.lock();
        // Get the IPC reply context (the original request we received)
        match &task_guard.ipc_reply {
            Some(req) => *req, // Copy the message
            None => {
                drop(task_guard);
                crate::serial_println!("[SYSCALL] ipc_reply FAILED - no pending request");
                return u64::MAX; // No pending reply
            }
        }
    };

    // Create reply payload from register values
    let reply_payload = [payload0, payload1, 0, 0];

    // Send reply
    match ipc_reply(&request_msg, reply_payload) {
        Ok(()) => {
            crate::serial_println!("[SYSCALL] ipc_reply SUCCESS");
            // Record IPC reply
            crate::task::statistics::record_ipc_replied(current_task_id);
            0 // Success
        }
        Err(err) => {
            crate::serial_println!("[SYSCALL] ipc_reply FAILED - error: {:?}", err);
            u64::MAX
        }
    }
}

/// Debug function to print R12 value before saving to Context.rip
#[no_mangle]
extern "C" fn debug_r12_before_save(_r12_value: u64) {
}

/// Debug function to print what was saved to Context.rip
#[no_mangle]
extern "C" fn debug_rip_after_save(_rip_value: u64) {
}

/// Debug function to print RIP value saved during YIELD
#[no_mangle]
extern "C" fn debug_yield_saved_rip(rip_value: u64) {
    crate::serial_println!("[YIELD_SAVE] Context.rip = {:#x}", rip_value);
}

/// Debug function to print current task ID during YIELD
#[no_mangle]
extern "C" fn debug_yield_task_id(task_id: u64) {
    crate::serial_println!("[YIELD_SAVE] Current task ID = {}", task_id);
}

/// Get current task ID (for assembly debugging)
#[no_mangle]
extern "C" fn get_current_task_id() -> u64 {
    crate::task::task::get_current_task() as u64
}

/// Debug function to print RIP value immediately after saving to Context
#[no_mangle]
extern "C" fn debug_context_rip_saved(_rip_value: u64) {
}

/// Debug function to verify Context values before IRETQ frame build
#[no_mangle]
extern "C" fn debug_verify_context_before_iretq(_ctx_ptr: usize) {
}

/// CANARY: Verify Task Context integrity at critical points
#[no_mangle]
pub extern "C" fn verify_task_context(task_id: u32, checkpoint_name: &'static str) {
    use crate::task::task::get_task;

    if let Some(task_arc) = get_task(task_id) {
        let task = task_arc.lock();
        let ctx = &task.context;

        crate::serial_println!("[CANARY] Task {} at '{}' checkpoint:", task_id, checkpoint_name);
        crate::serial_println!("  - Context @ {:#x}", ctx as *const _ as usize);
        crate::serial_println!("  - RIP:      {:#x}", ctx.rip);
        crate::serial_println!("  - RSP:      {:#x}", ctx.rsp);
        crate::serial_println!("  - CS:       {:#x}", ctx.cs);
        crate::serial_println!("  - SS:       {:#x}", ctx.ss);
        crate::serial_println!("  - RFLAGS:   {:#x}", ctx.rflags);

        // Sanity checks
        let mut alarm = false;

        if ctx.rip == 0 {
            crate::serial_println!("  [ALARM] RIP is NULL!");
            alarm = true;
        }

        if ctx.rip >= 0xF000_0000_0000_0000 {
            crate::serial_println!("  [ALARM] RIP looks like kernel address but too high!");
            alarm = true;
        }

        // User code should be in lower half (< 0x8000_0000_0000_0000)
        if ctx.rip >= 0x8000_0000_0000_0000 && ctx.rip < 0xFFFF_0000_0000_0000 {
            crate::serial_println!("  [ALARM] RIP in canonical hole (invalid)!");
            alarm = true;
        }

        // Expected user RIP range: 0x400000 - 0x7FFF_FFFF_FFFF
        if ctx.rip >= 0xFFFF_8000_0000_0000 && ctx.rip < 0xFFFF_FFFF_8000_0000 {
            crate::serial_println!("  [ALARM] RIP is kernel address, should be user!");
            alarm = true;
        }

        if ctx.cs != 0x23 && ctx.cs != 0x1B {
            crate::serial_println!("  [ALARM] CS is not user segment!");
            alarm = true;
        }

        if alarm {
            crate::serial_println!("  [CANARY] *** CORRUPTION DETECTED AT {} ***", checkpoint_name);
        } else {
            crate::serial_println!("  [CANARY] OK - Context looks valid");
        }
    } else {
        crate::serial_println!("[CANARY] ERROR: Task {} not found!", task_id);
    }
}

/// CANARY: Simplified version for assembly (takes task_id as u64)
#[no_mangle]
pub extern "C" fn verify_context_canary(_task_id: u64, _checkpoint_id: u64) {
    // ULTRA-MINIMAL: Just write marker and return immediately
    // NO serial_println - it might crash in syscall context
    unsafe {
        core::arch::asm!(
            "push rax",
            "mov rax, 0x9999",
            "mov qword ptr [rip + {marker}], rax",
            "pop rax",
            marker = sym DEBUG_MARKER,
        );
    }
    // Return immediately - if this works, DEBUG_MARKER will be 0x9999
}

// Full version with memory pointers (Option A - for future use)
#[allow(dead_code)]
fn syscall_ipc_reply_full(request_ptr: u64, reply_payload_ptr: u64) -> u64 {
    use crate::ipc::{IpcMessage, ipc_reply};

    // 1. Validate pointers
    if request_ptr == 0 || reply_payload_ptr == 0 {
        return u64::MAX; // EINVAL
    }

    // 2. Copy request and reply payload from userspace
    let request = unsafe {
        core::ptr::read(request_ptr as *const IpcMessage)
    };

    let reply_payload = unsafe {
        core::ptr::read(reply_payload_ptr as *const [u64; 4])
    };

    // 3. Call kernel IPC reply
    match ipc_reply(&request, reply_payload) {
        Ok(()) => 0, // Success
        Err(err) => err as u64,
    }
}

fn syscall_shmem_create(size: u64) -> u64 {
    use crate::ipc::shared_memory::{shmem_create, ShmemPerms};

    // 1. Validate size (must be page-aligned and reasonable)
    if size == 0 || size > 1024 * 1024 * 1024 {
        // Max 1GB
        return u64::MAX; // EINVAL
    }

    // 2. Create shared memory region
    match shmem_create(size as usize, ShmemPerms::ReadWrite) {
        Ok(shmem_id) => shmem_id.get() as u64,
        Err(_) => u64::MAX, // Error
    }
}

fn syscall_shmem_map(shmem_id: u64, virt_addr: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_map;
    use core::num::NonZeroU32;

    // 1. Validate shmem_id and address
    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX, // EINVAL
    };

    if virt_addr == 0 {
        return u64::MAX; // EINVAL
    }

    // 2. Map shared memory into task's address space
    match shmem_map(id, virt_addr as usize) {
        Ok(()) => 0, // Success
        Err(_) => u64::MAX, // Error
    }
}

fn syscall_spawn(binary_ptr: u64, binary_len: u64) -> u64 {
    use crate::task::spawn;

    // 1. Validate parameters
    if binary_ptr == 0 || binary_len == 0 || binary_len > 100 * 1024 * 1024 {
        // Max 100MB binary
        return u64::MAX; // EINVAL
    }

    // 2. Create slice from userspace pointer
    let binary = unsafe {
        core::slice::from_raw_parts(binary_ptr as *const u8, binary_len as usize)
    };

    // 3. Spawn new task
    match spawn(binary, &[]) {
        Ok(task_id) => task_id as u64,
        Err(_) => u64::MAX, // Error
    }
}

fn syscall_exit(exit_code: u64) -> u64 {
    // TODO: Implement task exit
    crate::serial_println!("syscall: exit(code={})", exit_code);
    // Mark task as exited and never return
    loop {
        x86_64::instructions::hlt();
    }
}

fn syscall_yield() -> u64 {
    // This should never be called - yield is handled directly in syscall_entry
    crate::serial_println!("[SYSCALL] ERROR: yield handler called (should be handled in assembly!)");
    0
}

/// Read a key from the keyboard buffer (non-blocking)
/// Returns: key code if available, 0 if no key, u64::MAX on error
fn syscall_read_key() -> u64 {
    match crate::drivers::keyboard::read_key() {
        Some(key) => key as u64,
        None => 0, // No key available
    }
}

/// Write a character to the console (serial for now)
/// arg1: character to write (low byte)
/// Returns: 0 on success
fn syscall_write_char(char_code: u64) -> u64 {
    let ch = (char_code & 0xFF) as u8;
    crate::drivers::serial::write_byte(ch);
    0 // Success
}

/// Get current task's PID
/// Returns: current task ID
fn syscall_get_pid() -> u64 {
    crate::task::task::get_current_task() as u64
}

/// List all tasks (prints to serial)
/// Returns: number of tasks
fn syscall_task_list() -> u64 {
    use crate::task::task::{TASK_TABLE, TaskState};
    use crate::drivers::serial;

    serial::write_str("\n=== TASK LIST ===\n");
    serial::write_str("ID   STATE         \n");
    serial::write_str("-----------------\n");

    let table = TASK_TABLE.lock();
    let count = table.len();

    for (&id, task_arc) in table.iter() {
        let task = task_arc.lock();

        // Print ID
        serial::write_dec(id);
        serial::write_str("    ");

        // Print state
        match task.state {
            TaskState::Runnable => serial::write_str("Runnable"),
            TaskState::Running => serial::write_str("Running"),
            TaskState::BlockedOnReceive => serial::write_str("Blocked(Recv)"),
            TaskState::BlockedOnSend(t) => {
                serial::write_str("Blocked(Send:");
                serial::write_dec(t);
                serial::write_str(")");
            }
            TaskState::Exited => serial::write_str("Exited"),
        }
        serial::write_str("\n");
    }

    serial::write_str("-----------------\n");
    serial::write_str("Total: ");
    serial::write_dec(count as u32);
    serial::write_str(" tasks\n\n");

    count as u64
}

/// Get system uptime in milliseconds
/// Returns: number of milliseconds since boot
fn syscall_uptime() -> u64 {
    crate::timer::uptime_ms()
}

/// Read directory entries from the ramdisk into a userspace buffer.
///
/// Arguments:
/// - buf_ptr: pointer to userspace buffer for DirEntry structs
/// - buf_size: size of the buffer in bytes
///
/// Returns: number of entries written, 0 if no ramdisk, u64::MAX on error
fn syscall_fs_read_dir(buf_ptr: u64, buf_size: u64) -> u64 {
    use crate::fs::format::DirEntry;

    if buf_ptr == 0 || buf_size == 0 {
        return u64::MAX;
    }

    let rd = match crate::fs::ramdisk() {
        Some(rd) => rd,
        None => return 0,
    };

    let entry_size = core::mem::size_of::<DirEntry>(); // 48 bytes
    let max_entries = buf_size as usize / entry_size;
    let entries = rd.entries();
    let count = entries.len().min(max_entries);

    for i in 0..count {
        let fpk = &entries[i];

        // CRITICAL: Use volatile reads to prevent LLVM from generating SSE instructions
        // that may cause GPF due to alignment assumptions in syscall context.
        // See: https://github.com/rust-lang/rust/issues/XXXXX for background
        let fpk_ptr = fpk as *const _ as *const u8;

        // Read fields using volatile reads (offsets based on FpkEntry #[repr(C)] layout)
        let id = unsafe { core::ptr::read_volatile(fpk_ptr as *const u16) };
        let entry_type = unsafe { core::ptr::read_volatile(fpk_ptr.add(2) as *const u16) };

        let mut name = [0u8; 32];
        for j in 0..32 {
            name[j] = unsafe { core::ptr::read_volatile(fpk_ptr.add(4 + j)) };
        }

        // FpkEntry layout: id(2) + type(2) + name(32) + pad(4) + offset(8) + size(8) + hash(8)
        // size is at offset 48
        let size = unsafe { core::ptr::read_volatile(fpk_ptr.add(48) as *const u64) };

        let dir_entry = DirEntry {
            id,
            entry_type,
            name,
            size,
        };

        let dst = (buf_ptr as *mut u8).wrapping_add(i * entry_size);

        unsafe {
            let src = &dir_entry as *const DirEntry as *const u8;
            core::ptr::copy_nonoverlapping(src, dst, entry_size);
        }
    }

    count as u64
}

/// Read a file's contents from the ramdisk into a userspace buffer.
///
/// Arguments:
/// - name_ptr: pointer to null-terminated filename (max 32 bytes)
/// - buf_ptr: userspace destination buffer
/// - buf_size: max bytes to write
///
/// Returns: number of bytes written, u64::MAX on error
fn syscall_fs_read_file(name_ptr: u64, buf_ptr: u64, buf_size: u64) -> u64 {
    if name_ptr == 0 || buf_ptr == 0 || buf_size == 0 {
        return u64::MAX;
    }

    // Read filename from userspace (byte-by-byte, max 32 bytes)
    let mut name_buf = [0u8; 32];
    let name_src = name_ptr as *const u8;
    let mut name_len = 0;
    for i in 0..32 {
        let b = unsafe { core::ptr::read(name_src.add(i)) };
        if b == 0 { break; }
        name_buf[i] = b;
        name_len = i + 1;
    }

    let name = match core::str::from_utf8(&name_buf[..name_len]) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    // Look up file in ramdisk
    let rd = match crate::fs::ramdisk() {
        Some(rd) => rd,
        None => return u64::MAX,
    };

    let entry = match rd.find(name) {
        Some(e) => e,
        None => return u64::MAX,
    };

    let data = rd.read(entry);
    let copy_len = data.len().min(buf_size as usize);

    unsafe {
        core::ptr::copy_nonoverlapping(
            data.as_ptr(),
            buf_ptr as *mut u8,
            copy_len,
        );
    }

    copy_len as u64
}

/// Debug: Print RAX value at syscall entry
#[no_mangle]
extern "C" fn debug_syscall_rax(rax_value: u64) {
    crate::serial_println!("[SYSCALL_ENTRY] RAX={:#x} ({})", rax_value, rax_value);
}

/// Debug: Print RAX value before cmp
#[no_mangle]
extern "C" fn debug_syscall_rax_before_cmp(rax_value: u64) {
    crate::serial_println!("[BEFORE_CMP] RAX={:#x} ({})", rax_value, rax_value);
}

// Debug: Print saved RIP (RCX from SYSCALL)
#[no_mangle]
extern "C" fn debug_saved_rip(rip_value: u64) {
    crate::serial_println!("[SYSCALL_SAVE] Saved RIP={:#x}", rip_value);
}

/// Get current task's context pointer (simple version for assembly)
#[no_mangle]
extern "C" fn get_current_task_context_ptr_simple() -> *mut task::Context {
    use crate::task::task;

    let current_id = task::get_current_task();
    if current_id == 0 {
        return core::ptr::null_mut();
    }

    let task_arc = match task::get_task(current_id) {
        Some(t) => t,
        None => return core::ptr::null_mut(),
    };

    let mut task_locked = task_arc.lock();
    &mut task_locked.context as *mut task::Context
}

/// Special yield path from syscall (called from assembly)
///
/// This function does NOT return if a task switch happens.
/// Assumes current task's context has already been saved.
/// Switches to next task and directly restores its context + IRETQ.
#[no_mangle]
extern "C" fn yield_cpu_from_syscall_asm() {
    use crate::task::{scheduler, task};

    // Disable interrupts
    x86_64::instructions::interrupts::disable();

    // Get next task
    let next_id = match scheduler::schedule_next() {
        Some(id) => id,
        None => {
            // No task to switch to, just return
            x86_64::instructions::interrupts::enable();
            return;
        }
    };

    // Get current task ID
    let current_id = task::get_current_task();

    if current_id == next_id {
        // Same task, just return
        x86_64::instructions::interrupts::enable();
        return;
    }

    crate::serial_println!("[YIELD] Switching from task {} to task {}", current_id, next_id);

    // Current task's context was already saved by syscall_entry

    // Get target task's context pointer
    let target = task::get_task(next_id).expect("Target task not found");
    let target_ctx_ptr = {
        let target_locked = target.lock();
        &target_locked.context as *const task::Context as usize
    };

    // Update current task
    task::set_current_task(next_id);

    // DEBUG: Print context before restoring
    crate::task::switch::debug_context_before_restore(target_ctx_ptr);

    // Jump to new task using IRETQ (does not return)
    unsafe {
        crate::task::switch::restore_context_only(target_ctx_ptr);
    }
}
