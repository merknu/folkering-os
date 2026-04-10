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

    // Initialise per-CPU local storage for the SWAPGS-based syscall entry.
    // CpuLocal.kernel_rsp is loaded by `mov rsp, gs:[0]` on every SYSCALL.
    let stack_top = unsafe {
        (&SYSCALL_STACK as *const AlignedStack as usize + core::mem::size_of::<AlignedStack>()) as u64
    };
    super::cpu_local::init(stack_top);

    // Stack Guard Page: map a non-present guard page one page below SYSCALL_STACK.
    // Any kernel stack overflow hitting this page will immediately raise #PF.
    let stack_base = unsafe { &SYSCALL_STACK as *const AlignedStack as u64 };
    let guard_vaddr = VirtAddr::new(stack_base.saturating_sub(4096));
    crate::memory::paging::map_guard_page(guard_vaddr);
}

/// Per-CPU context pointer (single-core for now)
/// This is set during context switch to avoid mutex locking in syscall path
static CURRENT_CONTEXT_PTR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Kernel syscall stack (16KB) - Must be 16-byte aligned for x86-64 ABI!
/// Used for handling syscalls - SYSCALL doesn't switch stacks automatically
#[repr(C, align(16))]
struct AlignedStack([u8; 16384]);

#[no_mangle]
#[link_section = ".bss"]
static mut SYSCALL_STACK: AlignedStack = AlignedStack([0; 16384]);

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

/// Debug: Return value before IRETQ (for syscall 13)
#[no_mangle]
static DEBUG_RETURN_VAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: Handler result for syscall 13 (to compare with assembly)
/// Using a raw mutable static for direct assembly access
#[no_mangle]
pub static mut SYSCALL_RESULT: u64 = 0;

/// Old atomic version for comparison
#[no_mangle]
static DEBUG_HANDLER_RESULT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Helper function to get SYSCALL_RESULT value
/// This avoids RIP-relative addressing issues in naked functions
/// NOTE: Must NOT print anything - that would trigger syscalls that overwrite R14!
#[no_mangle]
#[inline(never)]
extern "C" fn get_syscall_result() -> u64 {
    // Use volatile read to prevent optimization
    unsafe { core::ptr::read_volatile(&SYSCALL_RESULT) }
}

/// Debug: RFLAGS value before IRETQ
#[no_mangle]
static DEBUG_RFLAGS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: RCX value at syscall entry (saved RIP from SYSCALL instruction)
#[no_mangle]
static DEBUG_RCX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: Context.r14 value read from task Context JUST BEFORE restoring R14.
/// If this equals user_RSP at crash time, Context.r14 was already corrupt in the kernel.
/// If this equals 800 (fb.height), the corruption happens after restore.
#[no_mangle]
pub static DEBUG_CONTEXT_R14: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: Context.rsp value read alongside Context.r14 (user RSP for comparison)
#[no_mangle]
pub static DEBUG_CONTEXT_RSP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: Context pointer returned by timer_preempt_handler (captured right before return)
#[no_mangle]
pub static DEBUG_NEXT_CTX_PTR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: context.cs value at the pointer returned by timer_preempt_handler
#[no_mangle]
pub static DEBUG_NEXT_CTX_CS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Debug: context.rip value at the pointer returned by timer_preempt_handler
#[no_mangle]
pub static DEBUG_NEXT_CTX_RIP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// ─── User register scratch statics REMOVED ───────────────────────────────────
// USER_R15_SAVE, USER_R12_SAVE, USER_RSI_SAVE, USER_RDX_SAVE,
// USER_R13_SAVE, USER_R14_SAVE have been eliminated.
//
// The SWAPGS + kernel-stack approach (see syscall_entry below) saves every
// user GPR directly onto the kernel stack, so no global scratch storage is
// needed. This is SMP-correct from day one since each CPU uses its own
// CpuLocal block (via IA32_KERNEL_GS_BASE) and its own kernel stack slot.

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

/// Get the debug return value (RAX before IRETQ)
pub fn get_debug_return_val() -> u64 {
    DEBUG_RETURN_VAL.load(core::sync::atomic::Ordering::Relaxed)
}

/// Get Context.r14 value read just before restoring R14 (key diagnostic)
pub fn get_debug_context_r14() -> u64 {
    DEBUG_CONTEXT_R14.load(core::sync::atomic::Ordering::Relaxed)
}

/// Get Context.rsp value read alongside Context.r14
pub fn get_debug_context_rsp() -> u64 {
    DEBUG_CONTEXT_RSP.load(core::sync::atomic::Ordering::Relaxed)
}

/// Get Context pointer returned by timer_preempt_handler
pub fn get_debug_next_ctx_ptr() -> u64 {
    DEBUG_NEXT_CTX_PTR.load(core::sync::atomic::Ordering::Relaxed)
}

/// Get context.cs at the pointer returned by timer_preempt_handler
pub fn get_debug_next_ctx_cs() -> u64 {
    DEBUG_NEXT_CTX_CS.load(core::sync::atomic::Ordering::Relaxed)
}

/// Get context.rip at the pointer returned by timer_preempt_handler
pub fn get_debug_next_ctx_rip() -> u64 {
    DEBUG_NEXT_CTX_RIP.load(core::sync::atomic::Ordering::Relaxed)
}

/// Wrapper for get_debug_context_r14 callable from naked asm
#[no_mangle]
extern "C" fn get_debug_context_r14_wrapper() -> u64 {
    get_debug_context_r14()
}

/// Wrapper for get_debug_context_rsp callable from naked asm
#[no_mangle]
extern "C" fn get_debug_context_rsp_wrapper() -> u64 {
    get_debug_context_rsp()
}

/// Get the handler result (what the Rust handler returned)
pub fn get_debug_handler_result() -> u64 {
    DEBUG_HANDLER_RESULT.load(core::sync::atomic::Ordering::Relaxed)
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
        // ═══════════════════════════════════════════════════════════════════
        // SYSCALL ENTRY — SWAPGS + kernel-stack approach
        //
        // On entry (set by SYSCALL instruction):
        //   RSP   = user stack pointer (unchanged)
        //   RCX   = user RIP (return address)
        //   R11   = user RFLAGS
        //   RAX   = syscall number
        //   IF    = 0  (cleared by IA32_FMASK = 0x600)
        //   CS/SS = kernel selectors
        //
        // No GPRs are free. We use SWAPGS + GS-relative addressing to
        // access the per-CPU CpuLocal block (kernel_rsp, scratch) without
        // touching any GPR before switching stacks.
        // ═══════════════════════════════════════════════════════════════════

        // DIAGNOSTIC: Read IA32_KERNEL_GS_BASE before SWAPGS.
        // Save/restore rax/rcx/rdx using RIP-relative writes (no stack needed).
        // Store IA32_KERNEL_GS_BASE to DEBUG_MARKER so #GP handler prints it.
        "mov qword ptr [rip + {debug_rax}], rax",
        "mov qword ptr [rip + {debug_rcx}], rcx",
        "mov qword ptr [rip + {debug_rdx_scratch}], rdx",
        "mov ecx, 0xC0000102",       // IA32_KERNEL_GS_BASE MSR
        "rdmsr",                     // edx:eax = IA32_KERNEL_GS_BASE
        "shl rdx, 32",
        "or rax, rdx",               // rax = full 64-bit IA32_KERNEL_GS_BASE
        "mov qword ptr [rip + {debug_marker}], rax",  // save full value to DEBUG_MARKER
        "mov rax, qword ptr [rip + {debug_rax}]",
        "mov rcx, qword ptr [rip + {debug_rcx}]",
        "mov rdx, qword ptr [rip + {debug_rdx_scratch}]",

        // Step 1 — SWAPGS
        // GS.base ↔ IA32_KERNEL_GS_BASE
        // After this: GS.base = &CpuLocal; gs:[0] = kernel_rsp; gs:[8] = scratch
        "swapgs",

        // Step 2 — Switch to kernel stack (zero GPRs clobbered)
        "mov qword ptr gs:[8], rsp",    // save user RSP into CpuLocal.user_rsp_scratch
        "mov rsp, qword ptr gs:[0]",    // rsp = CpuLocal.kernel_rsp (kernel stack top)

        // Step 3 — Push all user GPRs onto kernel stack
        // r15 first (highest stack slot), user_rsp last (lowest, [rsp+0])
        "push r15",          // [rsp+120] after all pushes
        "push r14",          // [rsp+112]
        "push r13",          // [rsp+104]
        "push r12",          // [rsp+96]
        "push r11",          // [rsp+88]  user RFLAGS  (set by SYSCALL)
        "push r10",          // [rsp+80]
        "push r9",           // [rsp+72]
        "push r8",           // [rsp+64]
        "push rbp",          // [rsp+56]
        "push rdi",          // [rsp+48]
        "push rsi",          // [rsp+40]
        "push rdx",          // [rsp+32]
        "push rcx",          // [rsp+24]  user RIP     (set by SYSCALL)
        "push rbx",          // [rsp+16]
        "push rax",          // [rsp+8]   syscall number
        // Load user RSP from the scratch slot and push it as the bottom entry.
        // (rax is sacrificed here — syscall# was already saved at [rsp+8])
        "mov rax, qword ptr gs:[8]",
        "push rax",          // [rsp+0]   user RSP

        // Kernel stack frame summary:
        //  [rsp+0]   = user_rsp   [rsp+8]   = rax(syscall#)  [rsp+16]  = rbx
        //  [rsp+24]  = rcx(rip)   [rsp+32]  = rdx            [rsp+40]  = rsi
        //  [rsp+48]  = rdi        [rsp+56]  = rbp            [rsp+64]  = r8
        //  [rsp+72]  = r9         [rsp+80]  = r10            [rsp+88]  = r11(rflags)
        //  [rsp+96]  = r12        [rsp+104] = r13            [rsp+112] = r14
        //  [rsp+120] = r15

        // Step 4 — SWAPGS again: restore user GS base
        // (Rust code and libfolk use GS for nothing, but keep it clean)
        "swapgs",

        // Step 5 — Retrieve current task's Context pointer
        // get_current_task_context_ptr() is callee-saved-register-safe.
        // r12–r15 are callee-saved → user register values on the stack are safe.
        // RAX is clobbered (return value = Context*); save to r15.
        "call {get_ctx_fn}",
        "mov r15, rax",      // r15 = *mut Context (r15 user value is safely at [rsp+120])

        // Step 6 — Copy kernel stack frame → task Context struct
        // We use rax as a scratch register.  Context layout (repr(C)):
        //   rsp+0    → Context.rsp    (offset   0)
        //   rsp+8    → Context.rax    (offset  16)  (syscall number)
        //   rsp+16   → Context.rbx    (offset  24)
        //   rsp+24   → Context.rcx    (offset  32)  (= user RIP after SYSCALL)
        //   rsp+24   → Context.rip    (offset 128)  (same value — SYSCALL return addr)
        //   rsp+32   → Context.rdx    (offset  40)
        //   rsp+40   → Context.rsi    (offset  48)
        //   rsp+48   → Context.rdi    (offset  56)
        //   rsp+56   → Context.rbp    (offset   8)
        //   rsp+64   → Context.r8     (offset  64)
        //   rsp+72   → Context.r9     (offset  72)
        //   rsp+80   → Context.r10    (offset  80)
        //   rsp+88   → Context.r11    (offset  88)  (= user RFLAGS after SYSCALL)
        //   rsp+88   → Context.rflags (offset 136)  (same value)
        //   rsp+96   → Context.r12    (offset  96)
        //   rsp+104  → Context.r13    (offset 104)
        //   rsp+112  → Context.r14    (offset 112)
        //   rsp+120  → Context.r15    (offset 120)
        "mov rax, [rsp + 0]",   "mov [r15 + 0],   rax",  // rsp
        "mov rax, [rsp + 8]",   "mov [r15 + 16],  rax",  // rax  (syscall#)
        "mov rax, [rsp + 16]",  "mov [r15 + 24],  rax",  // rbx
        "mov rax, [rsp + 24]",  "mov [r15 + 32],  rax",  // rcx  (user RIP — SYSCALL clobbers rcx)
        "mov rax, [rsp + 24]",  "mov [r15 + 128], rax",  // rip
        "mov rax, [rsp + 32]",  "mov [r15 + 40],  rax",  // rdx
        "mov rax, [rsp + 40]",  "mov [r15 + 48],  rax",  // rsi
        "mov rax, [rsp + 48]",  "mov [r15 + 56],  rax",  // rdi
        "mov rax, [rsp + 56]",  "mov [r15 + 8],   rax",  // rbp
        "mov rax, [rsp + 64]",  "mov [r15 + 64],  rax",  // r8
        "mov rax, [rsp + 72]",  "mov [r15 + 72],  rax",  // r9
        "mov rax, [rsp + 80]",  "mov [r15 + 80],  rax",  // r10
        "mov rax, [rsp + 88]",  "mov [r15 + 88],  rax",  // r11  (user RFLAGS — SYSCALL clobbers r11)
        "mov rax, [rsp + 88]",  "mov [r15 + 136], rax",  // rflags
        "mov rax, [rsp + 96]",  "mov [r15 + 96],  rax",  // r12
        "mov rax, [rsp + 104]", "mov [r15 + 104], rax",  // r13
        "mov rax, [rsp + 112]", "mov [r15 + 112], rax",  // r14
        "mov rax, [rsp + 120]", "mov [r15 + 120], rax",  // r15
        "mov qword ptr [r15 + 144], 0x23",               // cs  = user_code | RPL3
        "mov qword ptr [r15 + 152], 0x1B",               // ss  = user_data | RPL3

        // Step 7 — Reload registers for yield check + normal_path arg rearrangement
        "mov rax, [rsp + 8]",    // rax = syscall number  (for "cmp rax, 7")
        "mov rdi, [rsp + 48]",   // rdi = syscall arg0
        "mov rsi, [rsp + 40]",   // rsi = syscall arg1
        "mov rdx, [rsp + 32]",   // rdx = syscall arg2
        "mov r10, [rsp + 80]",   // r10 = syscall arg3
        "mov r8,  [rsp + 64]",   // r8  = syscall arg4
        "mov r9,  [rsp + 72]",   // r9  = syscall arg5

        "cmp rax, 7",
        "je yield_path",   // Jump to yield path

        // Normal syscall path — rearrange arguments for C ABI
        "normal_path:",
        "push rax",
        "mov r9, r8",
        "mov r8, r10",
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "pop rdi",

        // FXSAVE: save current task's XMM/FPU state before kernel Rust code runs.
        "mov rax, qword ptr [rip + {fxsave_ptr}]",
        "test rax, rax",
        "jz 5f",
        "fxsave64 [rax]",
        "5:",

        // CRITICAL: Align stack to 16 bytes before call (x86-64 ABI requirement)
        // This prevents #GP from SSE instructions that require 16-byte alignment
        "and rsp, 0xFFFFFFFFFFFFFFF0",

        "call {handler}",

        // CRITICAL: Handler RAX return is unreliable (0), so use helper function
        // that reads SYSCALL_RESULT which was stored by the handler
        "call {get_result_fn}",
        // RAX now has SYSCALL_RESULT value from helper

        // Handler returned with result in RAX
        // Check if result is 0xFFFF_FFFF_FFFF_FFFE (EWOULDBLOCK - should yield)
        "mov r14, rax",           // R14 = return value

        "mov r13, 0xFFFFFFFFFFFFFFFE", // R13 = EWOULDBLOCK marker
        "cmp rax, r13",
        "je yield_path",          // If EWOULDBLOCK, go to yield path

        // Normal return path - restore and return to user
        // BUGFIX: Explicitly save R14 (return value) before call, since
        // the compiler might not preserve it correctly in all cases
        "push r14",
        "call {get_ctx_fn}",
        "mov r15, rax",
        "pop r14",        // Restore return value

        // FXRSTOR: restore current task's XMM/FPU state before returning to userspace.
        "mov rax, qword ptr [rip + {fxsave_ptr}]",
        "test rax, rax",
        "jz 6f",
        "fxrstor64 [rax]",
        "6:",

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
        "mov rax, r14",           // RAX = return value from handler
        "mov r14, [r15 + 112]",   // Restore R14

        // CRITICAL: Disable interrupts during the final restore sequence
        // to prevent any interrupt from corrupting RAX before IRETQ
        "cli",

        // Build IRETQ frame: SS, RSP, RFLAGS, CS, RIP
        "push qword ptr [r15 + 152]",  // SS
        "push qword ptr [r15 + 0]",    // RSP
        "push r11",                     // RFLAGS (already in R11)
        "push qword ptr [r15 + 144]",  // CS
        "push rcx",                     // RIP (already in RCX)

        // Restore remaining registers
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

        // FXSAVE: save user's XMM/FPU state NOW, before any Rust code runs.
        // yield_cpu() is Rust and may auto-vectorize, clobbering XMM registers.
        // We must capture the user's XMM HERE, while they're still intact.
        "mov rax, qword ptr [rip + {fxsave_ptr}]",
        "test rax, rax",
        "jz 7f",
        "fxsave64 [rax]",
        "7:",

        // Update RAX in Context to return value (0 for yield)
        "mov qword ptr [r15 + 16], 0",  // Set RAX=0 (yield return value)

        "call {yield_fn}",

        // FXRSTOR: restore user's XMM/FPU state (same-task no-switch case).
        // yield_fn returned without switching tasks; Rust may have clobbered XMM.
        // FXSAVE_CURRENT_PTR still points to THIS task's fxsave_area (saved above).
        "mov rax, qword ptr [rip + {fxsave_ptr}]",
        "test rax, rax",
        "jz 8f",
        "fxrstor64 [rax]",
        "8:",

        // If we get here, no task switch - restore and return
        "call {get_ctx_fn}",
        "mov r15, rax",  // r15 = Context*

        "mov r11, [r15 + 136]",   // RFLAGS
        "mov rcx, [r15 + 128]",   // RIP

        // Build IRETQ frame on kernel stack
        // IRETQ pops: RIP, CS, RFLAGS, RSP, SS (so push in REVERSE order!)
        "push qword ptr [r15 + 152]",  // SS
        "push qword ptr [r15 + 0]",    // RSP
        "push r11",                     // RFLAGS (R11)
        "push qword ptr [r15 + 144]",  // CS
        "push rcx",                     // RIP (RCX)

        // Restore all general-purpose registers
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
        "mov r15, [r15 + 120]",   // R15 = user R15

        // Return to user mode via IRETQ
        "iretq",

        fxsave_ptr = sym crate::task::task::FXSAVE_CURRENT_PTR,
        get_ctx_fn = sym get_current_task_context_ptr,
        handler = sym syscall_handler,
        yield_fn = sym syscall_do_yield,
        get_result_fn = sym get_syscall_result,
        debug_marker = sym DEBUG_MARKER,
        debug_rax = sym DEBUG_RAX,
        debug_rcx = sym DEBUG_RCX,
        debug_rdx_scratch = sym DEBUG_RIP,
    );
}

/// Syscall handler (called from assembly)
#[no_mangle]
#[inline(never)]
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

    let result = match syscall_num {
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
        15 => syscall_shmem_grant(arg1, arg2),
        16 => syscall_poweroff(),
        17 => syscall_check_interrupt(),
        18 => syscall_clear_interrupt(),
        19 => syscall_shmem_unmap(arg1, arg2),
        20 => syscall_shmem_destroy(arg1),
        // Phase 6: Reply-Later IPC
        0x20 => syscall_ipc_recv_async(),
        0x21 => syscall_ipc_reply_token(arg1, arg2, arg3),
        0x22 => syscall_ipc_get_recv_payload(),
        0x23 => syscall_ipc_get_recv_sender(),
        // Phase 6.2: Physical memory mapping
        0x24 => syscall_map_physical(arg1, arg2, arg3, arg4, arg5),
        // Phase 7: Input
        0x25 => syscall_read_mouse(),
        // Phase 8: Detailed task list via userspace buffer
        0x26 => syscall_task_list_detailed(arg1, arg2),
        // Phase 9: Anonymous memory mapping
        0x30 => syscall_mmap(arg1, arg2, arg3),
        0x31 => syscall_munmap(arg1, arg2),
        // Milestone 5: Block device I/O
        0x40 => syscall_block_read(arg1, arg2, arg3),
        0x41 => syscall_block_write(arg1, arg2, arg3),
        // Milestone 26-27: Network
        0x50 => syscall_ping(arg1),
        0x51 => syscall_dns_lookup(arg1, arg2),
        // Milestone 28: Entropy & RTC
        0x52 => syscall_get_time(),
        0x53 => syscall_get_random(arg1, arg2),
        // Milestone 30-32: HTTPS, GitHub & Clone
        0x54 => syscall_https_test(arg1),
        0x55 => syscall_github_fetch(arg1, arg2, arg3, arg4),
        0x56 => syscall_github_clone(arg1, arg2, arg3, arg4),
        // Direct HTTP fetch (URL → DNS → TLS → body)
        0x57 => syscall_http_fetch(arg1, arg2, arg3, arg4),
        // UDP send (target_ip:port, data)
        0x58 => syscall_udp_send(arg1, arg2, arg3, arg4),
        // UDP send + recv (target_ip:port, data, response_buf, timeout_ms)
        0x59 => syscall_udp_send_recv(arg1, arg2, arg3, arg4, arg5, arg6),
        // Audio: play raw PCM samples (16-bit signed stereo @ 44100Hz)
        0x5A => syscall_audio_play(arg1, arg2),
        // Audio: beep (440Hz sine wave for duration_ms)
        0x5B => syscall_audio_beep(arg1),
        // SMP: Parallel GEMM
        0x60 => syscall_parallel_gemm(arg1, arg2, arg3, arg4, arg5, arg6),
        // Hybrid AI: Ask Gemini cloud API
        0x70 => syscall_ask_gemini(arg1, arg2, arg3),
        // VirtIO GPU
        0x80 => syscall_gpu_flush(arg1, arg2, arg3, arg4),
        0x81 => syscall_gpu_info(arg1),
        // VSync: flush + wait for GPU fence completion (CPU sleeps via HLT)
        0x82 => {
            crate::drivers::virtio_gpu::flush_and_vsync(
                arg1 as u32, arg2 as u32, arg3 as u32, arg4 as u32
            );
            0
        },
        // Real-Time Clock (CMOS RTC)
        0x83 => super::rtc::read_rtc_packed(),
        // System stats: (total_pages << 32 | free_pages)
        0x84 => {
            let (total, free) = crate::memory::physical::memory_stats();
            ((total as u64) << 32) | (free as u64 & 0xFFFFFFFF)
        },
        // God Mode Pipe: read byte from COM3
        0x90 => {
            match crate::drivers::serial::com3_read_byte() {
                Some(b) => b as u64,
                None => u64::MAX,
            }
        },
        // IQE: Interaction Quality Engine telemetry
        0x91 => crate::drivers::iqe::read_to_user(arg1 as usize, arg2 as usize) as u64,
        0x92 => crate::drivers::iqe::tsc_ticks_per_us(),
        // Batched GPU flush: transfer N rects with 1 doorbell (1 VM-exit)
        // arg1 = ptr to [(x,y,w,h); N] as [u32; N*4], arg2 = N (max 4)
        0x95 => {
            let n = (arg2 as usize).min(4);
            let ptr = arg1 as *const u32;
            if !ptr.is_null() && arg1 >= 0x200000 && arg1 < 0xFFFF_8000_0000_0000 && n > 0 {
                let mut rects = [(0u32, 0u32, 0u32, 0u32); 4];
                for i in 0..n {
                    unsafe {
                        rects[i] = (
                            core::ptr::read_volatile(ptr.add(i * 4)),
                            core::ptr::read_volatile(ptr.add(i * 4 + 1)),
                            core::ptr::read_volatile(ptr.add(i * 4 + 2)),
                            core::ptr::read_volatile(ptr.add(i * 4 + 3)),
                        );
                    }
                }
                crate::drivers::virtio_gpu::flush_rects_batched(&rects[..n]);
            }
            0
        },
        // Async COM2: send + activate RX polling. len=0 activates polling without sending.
        0x96 => {
            let len = (arg2 as usize).min(8192);
            if len == 0 {
                // Activate RX polling only (no TX)
                crate::drivers::serial::com2_async_send(&[]);
                0
            } else {
                let ptr = arg1 as *const u8;
                if !ptr.is_null() && arg1 >= 0x200000 && arg1 < 0xFFFF_8000_0000_0000 {
                    let data = unsafe { core::slice::from_raw_parts(ptr, len) };
                    crate::drivers::serial::com2_async_send(data);
                    0
                } else {
                    u64::MAX
                }
            }
        },
        // Async COM2: poll for RX bytes + check for 0x00 COBS sentinel
        // arg1: 0 = COBS sentinel (0x00), 1 = legacy @@END@@ delimiter
        // Returns: 0 = still waiting, >0 = frame length before delimiter
        0x97 => {
            crate::drivers::serial::com2_async_poll();
            let use_legacy = arg1 == 1;
            let result = if use_legacy {
                crate::drivers::serial::com2_async_check_legacy()
            } else {
                crate::drivers::serial::com2_async_check_sentinel()
            };
            match result {
                Some(len) => len as u64,
                None => 0,
            }
        },
        // Async COM2: read response into userspace buffer, arg1=buf_ptr, arg2=max_len
        // Returns bytes copied
        0x98 => {
            let max_len = arg2 as usize;
            let ptr = arg1 as *mut u8;
            if !ptr.is_null() && arg1 >= 0x200000 && arg1 < 0xFFFF_8000_0000_0000 && max_len > 0 {
                let buf = unsafe { core::slice::from_raw_parts_mut(ptr, max_len.min(131072)) };
                crate::drivers::serial::com2_async_read(buf, max_len) as u64
            } else {
                u64::MAX
            }
        },
        // Wait for interrupt (HLT). Enables interrupts, halts CPU, wakes on ANY IRQ.
        // This is the correct idle primitive under WHPX: causes VM-exit so hypervisor
        // can inject pending interrupts (mouse, keyboard, timer).
        0x99 => {
            // Poll network stack before halting (replaces timer-ISR polling
            // which caused #GP from misaligned SSE in smoltcp)
            crate::net::poll();
            unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)); }
            0
        },
        // COM2 raw TX write (does NOT reset async RX state).
        // Used for MCP frames (send without disrupting RX polling).
        // arg1=buf_ptr, arg2=len (max 8KB)
        0x9A => {
            let len = (arg2 as usize).min(8192);
            let ptr = arg1 as *const u8;
            if !ptr.is_null() && arg1 >= 0x200000 && arg1 < 0xFFFF_8000_0000_0000 && len > 0 {
                let data = unsafe { core::slice::from_raw_parts(ptr, len) };
                crate::drivers::serial::com2_write(data);
                len as u64
            } else {
                u64::MAX
            }
        },
        // COM3 write: export telemetry to host (arg1=buf_ptr, arg2=len)
        0x94 => {
            let len = (arg2 as usize).min(64); // cap at 64 bytes for safety
            let ptr = arg1 as *const u8;
            if !ptr.is_null() && arg1 >= 0x200000 && arg1 < 0xFFFF_8000_0000_0000 {
                // Read bytes one at a time from userspace (safe, no bulk copy)
                for i in 0..len {
                    let byte = unsafe { core::ptr::read_volatile(ptr.add(i)) };
                    crate::drivers::serial::com3_write_byte(byte);
                }
            }
            len as u64
        },
        // Phase 10: Hardware Discovery — PCI device enumeration for WASM drivers
        // arg1 = userspace buffer ptr, arg2 = buffer size in bytes
        // Returns: number of devices written, or u64::MAX on error
        // Each device is 64 bytes (PciDeviceUserInfo struct)
        0xA0 => syscall_pci_enumerate(arg1, arg2),
        // Phase 10: Capability-gated Port I/O for WASM drivers
        // arg1 = port number, arg2 = value (for OUT), returns value (for IN)
        0xA1 => syscall_port_inb(arg1),     // IN byte
        0xA2 => syscall_port_inw(arg1),     // IN word
        0xA3 => syscall_port_inl(arg1),     // IN dword
        0xA4 => syscall_port_outb(arg1, arg2), // OUT byte
        0xA5 => syscall_port_outw(arg1, arg2), // OUT word
        0xA6 => syscall_port_outl(arg1, arg2), // OUT dword
        // Phase 10: IRQ routing for WASM drivers
        0xA7 => syscall_bind_irq(arg1, arg2),  // Bind IRQ vector to task
        0xA8 => syscall_ack_irq(arg1),          // Acknowledge IRQ (unmask)
        0xA9 => syscall_check_irq(arg1),         // Check if IRQ fired (non-blocking)
        // Phase 10: DMA + IOMMU
        0xAA => syscall_dma_alloc(arg1, arg2),   // Allocate DMA buffer (size, vaddr)
        0xAB => syscall_iommu_status(),            // Query IOMMU availability
        // Phase 11: WASM Network Driver Bridge
        0xAC => syscall_net_register(arg1, arg2),    // Register WASM net driver (mac_hi, mac_lo)
        0xAD => syscall_net_submit_rx(arg1, arg2),   // Submit received packet (vaddr, len)
        0xAE => syscall_net_poll_tx(arg1, arg2),     // Poll for TX packet (vaddr, max_len)
        0xAF => syscall_dma_sync_read(arg1, arg2),  // Read physical memory via HHDM
        0xB0 => syscall_net_dma_rx(arg1, arg2),     // Kernel-assisted RX: read DMA + deliver to smoltcp
        0xB1 => syscall_dma_sync_write(arg1, arg2), // Write to physical memory via HHDM
        0xB2 => syscall_net_metrics(arg1, arg2),    // OS metrics for AI introspection
        // WebSocket: connect to server
        // arg1 = packed IP (a | b<<8 | c<<16 | d<<24), arg2 = port | (path_len << 16)
        // arg3 = ptr to "host\0path" string
        // Returns: slot_id (0-3) or u64::MAX on error
        0xA0 => {
            let ip = [arg1 as u8, (arg1 >> 8) as u8, (arg1 >> 16) as u8, (arg1 >> 24) as u8];
            let port = (arg2 & 0xFFFF) as u16;
            let path_len = ((arg2 >> 16) & 0xFFFF) as usize;
            let ptr = arg3 as *const u8;
            if ptr.is_null() || arg3 < 0x200000 { u64::MAX } else {
                let data = unsafe { core::slice::from_raw_parts(ptr, path_len.min(256)) };
                // Split at first null byte: host\0path
                let split = data.iter().position(|&b| b == 0).unwrap_or(data.len());
                let host = core::str::from_utf8(&data[..split]).unwrap_or("localhost");
                let path = if split + 1 < data.len() {
                    core::str::from_utf8(&data[split+1..]).unwrap_or("/")
                } else { "/" };
                match crate::net::websocket::ws_connect(ip, port, host, path) {
                    Ok(id) => id as u64,
                    Err(_) => u64::MAX,
                }
            }
        },
        // WebSocket: send text data
        // arg1 = slot_id, arg2 = data_ptr, arg3 = data_len
        // Returns: 0 on success, u64::MAX on error
        0xA1 => {
            let ptr = arg2 as *const u8;
            let len = (arg3 as usize).min(8192);
            if ptr.is_null() || arg2 < 0x200000 || len == 0 { u64::MAX } else {
                let data = unsafe { core::slice::from_raw_parts(ptr, len) };
                match crate::net::websocket::ws_send(arg1 as u8, data) {
                    Ok(()) => 0,
                    Err(_) => u64::MAX,
                }
            }
        },
        // WebSocket: non-blocking receive poll
        // arg1 = slot_id, arg2 = buf_ptr, arg3 = max_len
        // Returns: bytes read (0 = nothing yet, u64::MAX-1 = closed/error)
        0xA2 => {
            let ptr = arg2 as *mut u8;
            let max = (arg3 as usize).min(8192);
            if ptr.is_null() || arg2 < 0x200000 { u64::MAX } else {
                let buf = unsafe { core::slice::from_raw_parts_mut(ptr, max) };
                let result = crate::net::websocket::ws_poll_recv(arg1 as u8, buf);
                if result < 0 { u64::MAX } else { result as u64 }
            }
        },
        // WebSocket: close connection
        // arg1 = slot_id
        0xA3 => {
            crate::net::websocket::ws_close(arg1 as u8);
            0
        },

        // Telemetry Ring: record app-level event for AutoDream pattern mining
        // arg1 = action_type (u8), arg2 = target_id (u32), arg3 = duration_ms (u32)
        0x9B => {
            crate::drivers::telemetry::record(
                crate::drivers::telemetry::ActionType::from_u8(arg1 as u8),
                arg2 as u32,
                arg3 as u32,
            );
            0
        },
        // Telemetry Ring: drain all events to userspace buffer (AutoDream)
        // arg1 = buf_ptr, arg2 = max_events
        // Returns: number of events drained
        0x9C => {
            crate::drivers::telemetry::drain_to_user(arg1 as usize, arg2 as usize) as u64
        },
        // Telemetry Ring: get stats (pending, total, overflow)
        // Returns: pending in bits 0-15, total in bits 16-31, overflow in bits 32-47
        0x9D => {
            let (pending, total, overflow) = crate::drivers::telemetry::stats();
            (pending as u64) | ((total as u64) << 16) | ((overflow as u64) << 32)
        },

        _ => {
            crate::drivers::serial::write_str("[HANDLER] Invalid syscall!\n");
            u64::MAX // Return error
        }
    };

    // WORKAROUND: Save result to static because RAX is being clobbered
    // somewhere between function return and assembly reading it
    // Store for ALL syscalls so get_result_fn always returns the right value
    unsafe { SYSCALL_RESULT = result; }

    result
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

/// Grant another task access to a shared memory region
/// This allows zero-copy data transfer between tasks
fn syscall_shmem_grant(shmem_id: u64, target_task: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_grant;
    use core::num::NonZeroU32;

    // 1. Validate shmem_id
    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX, // EINVAL
    };

    // 2. Validate target task ID
    if target_task == 0 || target_task > u32::MAX as u64 {
        return u64::MAX; // EINVAL
    }

    // 3. Grant access to the target task
    match shmem_grant(id, target_task as u32) {
        Ok(()) => 0, // Success
        Err(_) => u64::MAX, // Error
    }
}

/// Unmap a shared memory region from current task's address space
/// This unmaps the pages but does NOT free the physical memory
fn syscall_shmem_unmap(shmem_id: u64, virt_addr: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_unmap;
    use core::num::NonZeroU32;

    // 1. Validate shmem_id
    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX, // EINVAL
    };

    // 2. Validate virtual address
    if virt_addr == 0 {
        return u64::MAX; // EINVAL
    }

    // 3. Unmap the shared memory region
    match shmem_unmap(id, virt_addr as usize) {
        Ok(()) => 0, // Success
        Err(_) => u64::MAX, // Error
    }
}

/// Destroy a shared memory region and free physical pages
/// Only the creator (owner) can destroy the region
fn syscall_shmem_destroy(shmem_id: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_destroy;
    use core::num::NonZeroU32;

    // 1. Validate shmem_id
    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX, // EINVAL
    };

    // 2. Destroy the shared memory region
    match shmem_destroy(id) {
        Ok(()) => 0, // Success
        Err(_) => u64::MAX, // Error
    }
}

/// Anonymous memory mapping (mmap)
///
/// Allocates physical pages and maps them into the calling task's address space.
///
/// # Arguments
/// - `hint_addr`: desired virtual address (0 = kernel chooses)
/// - `size`: requested size in bytes (rounded up to page boundary)
/// - `flags`: protection flags (bit 0=read, bit 1=write, bit 2=exec)
///
/// # Returns
/// - Virtual address of the mapped region on success
/// - u64::MAX on failure
fn syscall_mmap(hint_addr: u64, size: u64, flags: u64) -> u64 {
    use crate::memory::physical::alloc_page;
    use crate::memory::paging::map_page_in_table;
    use x86_64::structures::paging::PageTableFlags;

    const PAGE_SIZE: u64 = 4096;
    // Limits
    const MAX_MMAP_SIZE: u64 = 16 * 1024 * 1024; // 16MB max per call
    // User mmap region: 0x4000_0000 .. 0x7FFF_0000_0000
    const MMAP_BASE: u64 = 0x4000_0000;

    if size == 0 || size > MAX_MMAP_SIZE {
        return u64::MAX;
    }

    let num_pages = ((size + PAGE_SIZE - 1) / PAGE_SIZE) as usize;

    // Get current task's page table
    let task_pml4 = crate::task::task::current_task().lock().page_table_phys;
    if task_pml4 == 0 {
        return u64::MAX;
    }

    // Choose virtual address
    let virt_base = if hint_addr != 0 {
        // Use hint if page-aligned and in user range
        if hint_addr % PAGE_SIZE != 0 || hint_addr < MMAP_BASE {
            return u64::MAX;
        }
        hint_addr
    } else {
        // Auto-assign: use a per-task bump allocator
        // Store next_mmap_addr in task struct (simple approach: use atomic counter)
        use core::sync::atomic::{AtomicU64, Ordering};
        static NEXT_MMAP_ADDR: AtomicU64 = AtomicU64::new(MMAP_BASE);
        let addr = NEXT_MMAP_ADDR.fetch_add(num_pages as u64 * PAGE_SIZE, Ordering::Relaxed);
        if addr + (num_pages as u64 * PAGE_SIZE) > 0x7FFF_0000_0000 {
            return u64::MAX; // Address space exhausted
        }
        addr
    };

    // Build page flags
    let mut pt_flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if flags & 0x2 != 0 { // PROT_WRITE
        pt_flags |= PageTableFlags::WRITABLE;
    }
    if flags & 0x4 == 0 { // No PROT_EXEC → set NX
        pt_flags |= PageTableFlags::NO_EXECUTE;
    }

    // Allocate and map pages
    for i in 0..num_pages {
        let phys = match alloc_page() {
            Some(p) => p,
            None => {
                // TODO: unmap already-mapped pages on failure
                return u64::MAX;
            }
        };

        let virt = virt_base + (i as u64 * PAGE_SIZE);
        if map_page_in_table(task_pml4, virt as usize, phys, pt_flags).is_err() {
            return u64::MAX;
        }

        // Zero the page (security: don't leak kernel data)
        // We need to write through the HHDM since the page is mapped in user space
        let hhdm_ptr = crate::phys_to_virt(phys) as *mut u8;
        unsafe {
            core::ptr::write_bytes(hhdm_ptr, 0, PAGE_SIZE as usize);
        }
    }

    virt_base
}

/// Unmap anonymous memory previously allocated with SYS_MMAP
///
/// # Arguments
/// - `virt_addr`: virtual address (must be page-aligned, in user mmap range)
/// - `size`: number of bytes to unmap (rounded up to page boundary)
///
/// # Returns
/// - 0 on success, u64::MAX on failure
fn syscall_munmap(virt_addr: u64, size: u64) -> u64 {
    use crate::memory::paging::unmap_page_in_table;
    use crate::memory::physical::free_pages;

    const PAGE_SIZE: u64 = 4096;
    const MMAP_BASE: u64 = 0x4000_0000;

    if size == 0 || virt_addr % PAGE_SIZE != 0 || virt_addr < MMAP_BASE {
        return u64::MAX;
    }

    let num_pages = ((size + PAGE_SIZE - 1) / PAGE_SIZE) as usize;

    let task_pml4 = crate::task::task::current_task().lock().page_table_phys;
    if task_pml4 == 0 {
        return u64::MAX;
    }

    let mut freed = 0usize;
    for i in 0..num_pages {
        let virt = virt_addr + (i as u64 * PAGE_SIZE);
        match unmap_page_in_table(task_pml4, virt as usize) {
            Ok(phys_addr) => {
                // Return physical page to PMM
                free_pages(phys_addr, 0);
                freed += 1;
            }
            Err(_) => {
                // Page wasn't mapped — skip silently (like Linux munmap)
            }
        }
    }

    if freed > 0 {
        crate::serial_println!("[MUNMAP] Freed {} pages at {:#x}", freed, virt_addr);
    }

    0 // success
}

// ── Block Device Syscalls (Milestone 5) ──────────────────────────────────────

fn syscall_block_read(sector: u64, buf_ptr: u64, count: u64) -> u64 {
    use crate::drivers::virtio_blk;

    if !virtio_blk::is_initialized() {
        return u64::MAX; // No block device
    }

    if buf_ptr == 0 || count == 0 || count > 128 {
        return u64::MAX; // EINVAL (max 64KB per call)
    }

    // Validate userspace pointer (must be in lower half)
    if buf_ptr >= 0x0000_8000_0000_0000 {
        return u64::MAX; // EFAULT
    }

    let count = count as usize;
    let mut offset = 0usize;
    let mut sec = sector;
    let mut remaining = count;

    // ULTRA 36: Use multi-sector DMA bursts for large reads.
    // block_read_multi writes to its internal DMA buffer then copies to the
    // output buffer. We pass the userspace pointer directly — the driver copies
    // from its kernel DMA buffer to the destination after the transfer completes.
    while remaining > 0 {
        let burst = remaining.min(virtio_blk::MAX_BURST_SECTORS);
        let data_len = burst * 512;

        if burst > 1 {
            // Multi-sector DMA burst: one VirtIO request
            // block_read_multi copies from its internal DMA buf to user buf
            let dst = unsafe {
                core::slice::from_raw_parts_mut(
                    (buf_ptr as usize + offset) as *mut u8,
                    data_len,
                )
            };

            match virtio_blk::block_read_multi(sec, dst, burst) {
                Ok(()) => {
                    offset += data_len;
                    sec += burst as u64;
                    remaining -= burst;
                }
                Err(_) => return u64::MAX,
            }
        } else {
            // Single sector fallback
            let mut sector_buf = [0u8; 512];
            match virtio_blk::block_read(sec, &mut sector_buf) {
                Ok(()) => {
                    let dst = (buf_ptr as usize + offset) as *mut u8;
                    unsafe {
                        core::ptr::copy_nonoverlapping(sector_buf.as_ptr(), dst, 512);
                    }
                    offset += 512;
                    sec += 1;
                    remaining -= 1;
                }
                Err(_) => return u64::MAX,
            }
        }
    }
    0
}

fn syscall_block_write(sector: u64, buf_ptr: u64, count: u64) -> u64 {
    use crate::drivers::virtio_blk;

    if !virtio_blk::is_initialized() {
        return u64::MAX;
    }

    if buf_ptr == 0 || count == 0 || count > 128 {
        return u64::MAX;
    }

    let buf_len = (count as usize) * virtio_blk::SECTOR_SIZE;

    if buf_ptr >= 0x0000_8000_0000_0000 {
        return u64::MAX;
    }

    let current_task = crate::task::task::get_current_task();
    let _ = virtio_blk::write_journal_entry(current_task, 1, sector, count);

    let buf = unsafe {
        core::slice::from_raw_parts(buf_ptr as *const u8, buf_len)
    };

    match virtio_blk::write_sectors(sector, buf, count as usize) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

// ── Network Ping Syscall (Milestone 26) ──────────────────────────────────────

/// Send an ICMP echo request to an IPv4 address
/// arg1: packed IPv4 address (a | b<<8 | c<<16 | d<<24)
/// Returns: 0 on success
fn syscall_ping(ip_packed: u64) -> u64 {
    let a = (ip_packed & 0xFF) as u8;
    let b = ((ip_packed >> 8) & 0xFF) as u8;
    let c = ((ip_packed >> 16) & 0xFF) as u8;
    let d = ((ip_packed >> 24) & 0xFF) as u8;
    crate::net::send_ping(a, b, c, d);
    0
}

/// Resolve a domain name to an IPv4 address (blocking).
/// arg1: pointer to domain name string (null-terminated or with length)
/// arg2: length of domain name
/// Returns: packed IPv4 (a | b<<8 | c<<16 | d<<24) on success, 0 on failure
fn syscall_dns_lookup(name_ptr: u64, name_len: u64) -> u64 {
    if name_ptr == 0 || name_len == 0 || name_len > 255 {
        return 0;
    }

    // Read domain name from userspace
    let name_bytes = unsafe {
        core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize)
    };

    let name = match core::str::from_utf8(name_bytes) {
        Ok(s) => s,
        Err(_) => return 0,
    };

    crate::net::dns_lookup(name)
}

// ── Milestone 28: Entropy & RTC Syscalls ─────────────────────────────────────

/// Get current Unix timestamp (seconds since 1970-01-01 UTC)
fn syscall_get_time() -> u64 {
    crate::drivers::cmos::unix_timestamp()
}

/// Fill a userspace buffer with random bytes
/// arg1: buffer pointer, arg2: buffer length
/// Returns: 0 on success, u64::MAX on error
fn syscall_get_random(buf_ptr: u64, buf_len: u64) -> u64 {
    if buf_ptr == 0 || buf_len == 0 || buf_len > 4096 {
        return u64::MAX;
    }
    let buf = unsafe {
        core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_len as usize)
    };
    crate::drivers::rng::fill_bytes(buf);
    0
}

/// Fetch GitHub repo info: arg1=user_ptr, arg2=user_len, arg3=repo_ptr, arg4=repo_len
/// Prints results to serial. Returns 0 on success.
fn syscall_github_fetch(user_ptr: u64, user_len: u64, repo_ptr: u64, repo_len: u64) -> u64 {
    if user_ptr == 0 || user_len == 0 || user_len > 64 || repo_ptr == 0 || repo_len == 0 || repo_len > 64 {
        return u64::MAX;
    }
    let user = unsafe { core::slice::from_raw_parts(user_ptr as *const u8, user_len as usize) };
    let repo = unsafe { core::slice::from_raw_parts(repo_ptr as *const u8, repo_len as usize) };
    let user_str = match core::str::from_utf8(user) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };
    let repo_str = match core::str::from_utf8(repo) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    match crate::net::github::fetch_repo_info(user_str, repo_str) {
        Ok(info) => {
            crate::net::github::print_repo_info(&info);
            0
        }
        Err(e) => {
            crate::drivers::serial::write_str("[GITHUB] Error: ");
            crate::drivers::serial::write_str(e);
            crate::drivers::serial::write_newline();
            u64::MAX
        }
    }
}

/// Clone a GitHub repo: download JSON, store in shmem for shell to write to VFS.
/// arg1=user_ptr, arg2=user_len, arg3=repo_ptr, arg4=repo_len
/// Returns: (size << 32) | shmem_handle on success, u64::MAX on error.
fn syscall_github_clone(user_ptr: u64, user_len: u64, repo_ptr: u64, repo_len: u64) -> u64 {
    if user_ptr == 0 || user_len == 0 || user_len > 64 || repo_ptr == 0 || repo_len == 0 || repo_len > 64 {
        return u64::MAX;
    }
    let user = unsafe { core::slice::from_raw_parts(user_ptr as *const u8, user_len as usize) };
    let repo = unsafe { core::slice::from_raw_parts(repo_ptr as *const u8, repo_len as usize) };
    let user_str = match core::str::from_utf8(user) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };
    let repo_str = match core::str::from_utf8(repo) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    // Download repo JSON from GitHub
    let data = match crate::net::github::clone_repo(user_str, repo_str) {
        Ok(d) => d,
        Err(e) => {
            crate::drivers::serial::write_str("[CLONE] Error: ");
            crate::drivers::serial::write_str(e);
            crate::drivers::serial::write_newline();
            return u64::MAX;
        }
    };

    if data.is_empty() {
        return u64::MAX;
    }

    let size = data.len();
    let shmem_size = ((size + 4095) / 4096) * 4096;
    let shmem_size = if shmem_size == 0 { 4096 } else { shmem_size };

    // Create shmem region
    use crate::ipc::shared_memory::{shmem_create, shmem_grant, ShmemPerms, SHMEM_TABLE};
    let id = match shmem_create(shmem_size, ShmemPerms::ReadWrite) {
        Ok(id) => id,
        Err(_) => return u64::MAX,
    };

    // Grant to userspace tasks
    for tid in 2..=8u32 {
        let _ = shmem_grant(id, tid);
    }

    // Write data directly to shmem physical pages via HHDM
    {
        let table = SHMEM_TABLE.lock();
        if let Some(shmem) = table.get(&id.get()) {
            let mut offset = 0;
            for &phys_page in &shmem.phys_pages {
                let virt = crate::phys_to_virt(phys_page);
                let chunk = (size - offset).min(4096);
                if chunk == 0 { break; }
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        data.as_ptr().add(offset),
                        virt as *mut u8,
                        chunk,
                    );
                }
                offset += chunk;
            }
        }
    }

    let handle = id.get();

    crate::serial_str!("[CLONE] Data in shmem handle=");
    crate::drivers::serial::write_dec(handle);
    crate::serial_str!(", size=");
    crate::drivers::serial::write_dec(size as u32);
    crate::serial_strln!(" bytes");

    ((size as u64) << 32) | (handle as u64)
}

/// HTTPS test — TLS handshake only (DNS done by caller).
/// Uses the already-resolved IP from the DNS lookup syscall.
fn syscall_https_test(ip_packed: u64) -> u64 {
    // Use DNS-resolved IP if provided, otherwise fallback
    let ip = if ip_packed != 0 {
        [
            (ip_packed >> 24) as u8,
            (ip_packed >> 16) as u8,
            (ip_packed >> 8) as u8,
            ip_packed as u8,
        ]
    } else {
        [93, 184, 215, 14] // example.com fallback
    };
    crate::serial_str!("[TLS] HTTPS GET to example.com...");
    match crate::net::tls::https_get(ip, "example.com", "/") {
        Ok(()) => {
            crate::serial_strln!("[TLS] HTTPS SUCCESS!");
            0
        }
        Err(e) => {
            crate::serial_str!("[TLS] HTTPS failed: ");
            crate::serial_strln!(e);
            u64::MAX
        }
    }
}

/// Direct HTTP(S) fetch: takes URL from userspace, resolves DNS, does TLS GET,
/// returns response body. Eliminates proxy dependency for simple page fetches.
///
/// Args: url_ptr, url_len, buf_ptr, buf_len
/// Returns: bytes written to buf, or u64::MAX on error
fn syscall_http_fetch(url_ptr: u64, url_len: u64, buf_ptr: u64, buf_len: u64) -> u64 {
    if url_len == 0 || url_len > 512 || buf_len == 0 || buf_len > 65536 {
        return u64::MAX;
    }

    let url = unsafe {
        let slice = core::slice::from_raw_parts(url_ptr as *const u8, url_len as usize);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return u64::MAX,
        }
    };

    // Parse URL: strip https:// prefix, split host/path
    let stripped = url.strip_prefix("https://").unwrap_or(
        url.strip_prefix("http://").unwrap_or(url));
    let (host, path) = match stripped.find('/') {
        Some(i) => (&stripped[..i], &stripped[i..]),
        None => (stripped, "/"),
    };

    crate::serial_str!("[HTTP_FETCH] ");
    crate::serial_str!(host);
    crate::serial_str!(path);
    crate::serial_str!("\n");

    // DNS resolve
    let ip_packed = crate::net::dns_lookup(host);
    if ip_packed == 0 || ip_packed == u64::MAX {
        crate::serial_strln!("[HTTP_FETCH] DNS failed");
        return u64::MAX;
    }
    let ip = [
        (ip_packed >> 24) as u8,
        (ip_packed >> 16) as u8,
        (ip_packed >> 8) as u8,
        ip_packed as u8,
    ];

    // Build HTTP/1.1 request
    let mut request = alloc::vec::Vec::with_capacity(256 + host.len() + path.len());
    request.extend_from_slice(b"GET ");
    request.extend_from_slice(path.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(b"\r\nUser-Agent: FolkeringOS/1.0\r\nAccept: text/html,*/*\r\nConnection: close\r\n\r\n");

    // Do HTTPS GET
    let response = match crate::net::tls::https_get_raw(ip, host, &request) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[HTTP_FETCH] TLS failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    // Find HTTP body (after \r\n\r\n)
    let body_start = response.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(0);

    let body = &response[body_start..];
    let copy_len = body.len().min(buf_len as usize);

    // Copy to userspace buffer
    unsafe {
        let dst = core::slice::from_raw_parts_mut(buf_ptr as *mut u8, copy_len);
        dst.copy_from_slice(&body[..copy_len]);
    }

    crate::serial_str!("[HTTP_FETCH] OK, ");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_str!(" bytes body\n");

    copy_len as u64
}

/// Audio: play raw PCM samples (16-bit signed stereo @ 44100Hz)
fn syscall_audio_play(samples_ptr: u64, samples_count: u64) -> u64 {
    if samples_count == 0 || samples_count > 1_000_000 { return u64::MAX; }
    let samples = unsafe {
        core::slice::from_raw_parts(samples_ptr as *const i16, samples_count as usize)
    };
    if crate::drivers::ac97::play_pcm(samples) { 0 } else { u64::MAX }
}

/// Audio: beep — generate 440Hz tone for duration_ms milliseconds
fn syscall_audio_beep(duration_ms: u64) -> u64 {
    if crate::drivers::ac97::beep(duration_ms as u32) { 0 } else { u64::MAX }
}

/// UDP send: target packed as ip|port (32+16 bits), data_ptr, data_len
/// arg1 = (a<<24)|(b<<16)|(c<<8)|d, arg2 = port, arg3 = data_ptr, arg4 = data_len
fn syscall_udp_send(target_packed: u64, port: u64, data_ptr: u64, data_len: u64) -> u64 {
    if data_len == 0 || data_len > 1472 { return u64::MAX; }
    let ip = [
        ((target_packed >> 24) & 0xFF) as u8,
        ((target_packed >> 16) & 0xFF) as u8,
        ((target_packed >> 8) & 0xFF) as u8,
        (target_packed & 0xFF) as u8,
    ];
    let data = unsafe {
        core::slice::from_raw_parts(data_ptr as *const u8, data_len as usize)
    };
    if crate::net::udp_send(ip, port as u16, data) { 0 } else { u64::MAX }
}

/// UDP send + receive: returns bytes received
fn syscall_udp_send_recv(
    target_packed: u64, port: u64,
    data_ptr: u64, data_len: u64,
    resp_ptr: u64, resp_len_and_timeout: u64,
) -> u64 {
    let resp_len = (resp_len_and_timeout & 0xFFFF_FFFF) as usize;
    let timeout_ms = (resp_len_and_timeout >> 32) as u32;
    if data_len == 0 || data_len > 1472 || resp_len == 0 || resp_len > 4096 {
        return u64::MAX;
    }
    let ip = [
        ((target_packed >> 24) & 0xFF) as u8,
        ((target_packed >> 16) & 0xFF) as u8,
        ((target_packed >> 8) & 0xFF) as u8,
        (target_packed & 0xFF) as u8,
    ];
    let data = unsafe {
        core::slice::from_raw_parts(data_ptr as *const u8, data_len as usize)
    };
    let response = unsafe {
        core::slice::from_raw_parts_mut(resp_ptr as *mut u8, resp_len)
    };
    crate::net::udp_send_recv(ip, port as u16, data, response, timeout_ms) as u64
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
    use crate::task::task::{self, TaskState};

    let current_id = task::get_current_task();
    crate::serial_println!("syscall: exit(code={}) task={}", exit_code, current_id);

    // Mark task as Exited so scheduler skips it and IPC senders get errors
    if let Some(task_arc) = task::get_task(current_id) {
        let mut t = task_arc.lock();
        t.state = TaskState::Exited;
    }

    // Remove from task table (Arc refcount will drop when all refs gone)
    let _ = task::remove_task(current_id);

    crate::serial_println!("[EXIT] Task {} removed from scheduler", current_id);

    // Yield to let scheduler pick another task — we'll never return
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
///
/// NOTE: This function checks BOTH the PS/2 keyboard buffer AND the serial port.
/// When running QEMU with `-serial stdio`, keyboard input comes via serial.
fn syscall_read_key() -> u64 {
    // First, check the PS/2 keyboard buffer
    if let Some(key) = crate::drivers::keyboard::read_key() {
        // IQE: record the moment userspace pulls a key from the buffer
        crate::drivers::iqe::record(
            crate::drivers::iqe::IqeEventType::KeyboardRead,
            crate::drivers::iqe::rdtsc(),
            key as u64,
        );
        // Check for Ctrl+C (ASCII 0x03)
        if key == 0x03 {
            set_current_task_interrupt();
            return 0x03;
        }
        return key as u64;
    }

    // Then, check the serial port for input (QEMU -serial stdio mode)
    if let Some(byte) = crate::drivers::serial::read_byte() {
        // Check for Ctrl+C (ASCII 0x03)
        if byte == 0x03 {
            set_current_task_interrupt();
            return 0x03; // Return Ctrl+C so shell can also handle it
        }
        // Handle carriage return as newline
        if byte == b'\r' {
            return b'\n' as u64;
        }
        return byte as u64;
    }

    0 // No key available
}

/// Read a mouse event from the input buffer
/// Returns: packed u64 with buttons, dx, dy; or 0 if no event
/// Format: bits 0-7: buttons, bits 8-23: dx (signed), bits 24-39: dy (signed)
/// High bit (63) set indicates valid event
fn syscall_read_mouse() -> u64 {
    if let Some(event) = crate::drivers::mouse::read_event() {
        // IQE: record the moment userspace pulls a mouse event
        crate::drivers::iqe::record(
            crate::drivers::iqe::IqeEventType::MouseRead,
            crate::drivers::iqe::rdtsc(),
            0,
        );
        let buttons = event.buttons as u64;
        let dx = (event.dx as u16) as u64;
        let dy = (event.dy as u16) as u64;

        (1u64 << 63) | (dy << 24) | (dx << 8) | buttons
    } else {
        0 // No event available
    }
}

/// Set interrupt flag on current task
fn set_current_task_interrupt() {
    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        task_arc.lock().interrupt_pending = true;
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

/// List all tasks
///
/// If arg1 (shmem_handle) is 0: just returns count (backward compatible).
/// If arg1 != 0: fills shmem with task info and returns count.
///
/// Shmem format per task (32 bytes):
///   [task_id: u32][state: u32][name: [u8; 16]][cpu_time_ms: u64]
fn syscall_task_list() -> u64 {
    use crate::task::task::{TASK_TABLE, TaskState};

    let table = TASK_TABLE.lock();
    let count = table.len();
    count as u64
}

/// Extended task list that fills a userspace buffer with task details
/// arg1: userspace buffer pointer
/// arg2: buffer size in bytes
/// Returns: count of tasks written
///
/// Buffer format per task (32 bytes):
///   [task_id: u32][state: u32][name: [u8; 16]][cpu_time_ms: u64]
fn syscall_task_list_detailed(buf_ptr: u64, buf_size: u64) -> u64 {
    use crate::task::task::{TASK_TABLE, TaskState};

    if buf_ptr == 0 || buf_size == 0 {
        let table = TASK_TABLE.lock();
        return table.len() as u64;
    }

    let buf = unsafe {
        core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_size as usize)
    };

    let table = TASK_TABLE.lock();
    let mut offset = 0usize;
    let mut written = 0u64;

    for (&id, task_arc) in table.iter() {
        if offset + 32 > buf.len() {
            break;
        }
        let task = task_arc.lock();

        // task_id: u32
        buf[offset..offset+4].copy_from_slice(&id.to_le_bytes());

        // state: u32
        let state_val: u32 = match task.state {
            TaskState::Runnable => 0,
            TaskState::Running => 1,
            TaskState::BlockedOnReceive => 2,
            TaskState::BlockedOnSend(_) => 3,
            TaskState::WaitingForReply(_) => 4,
            TaskState::Exited => 5,
        };
        buf[offset+4..offset+8].copy_from_slice(&state_val.to_le_bytes());

        // name: [u8; 16]
        buf[offset+8..offset+24].copy_from_slice(&task.name);

        // cpu_time_ms: u64
        buf[offset+24..offset+32].copy_from_slice(&task.cpu_time_used_ms.to_le_bytes());

        offset += 32;
        written += 1;
    }

    written
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

/// Power off the system (exits QEMU)
/// Uses QEMU's debug exit port to terminate the emulator.
/// Parallel GEMM: distribute output projection across AP compute workers
/// Args: input_ptr, weight_ptr, output_ptr, k, n, quant_type
fn syscall_parallel_gemm(
    input_ptr: u64,
    weight_ptr: u64,
    output_ptr: u64,
    k: u64,
    n: u64,
    quant_type: u64,
) -> u64 {
    crate::serial_str!("[PGEMM] syscall entry k=");
    crate::drivers::serial::write_dec(k as u32);
    crate::serial_str!(" n=");
    crate::drivers::serial::write_dec(n as u32);
    crate::drivers::serial::write_newline();

    // Get current task's page table for AP workers
    let task_id = crate::task::task::get_current_task();
    let cr3 = match crate::task::task::get_task(task_id) {
        Some(t) => t.lock().page_table_phys,
        None => return u64::MAX,
    };

    crate::serial_str!("[PGEMM] task CR3=");
    crate::drivers::serial::write_hex(cr3);
    crate::serial_str!(" APs=");
    crate::drivers::serial::write_dec(super::smp::ap_count() as u32);
    crate::drivers::serial::write_newline();

    let result = super::smp::dispatch_parallel_gemm(
        input_ptr,
        weight_ptr,
        output_ptr,
        k as u32,
        n as u32,
        quant_type as u8,
        cr3,
    );

    if result == 0 { 0 } else { u64::MAX }
}

/// Ask Gemini cloud API via HTTPS POST.
/// Args: prompt_ptr (userspace), prompt_len, response_buf_ptr (userspace buffer)
/// Returns: response_len on success (bytes written to buf), u64::MAX on error
///
/// The response buffer must be pre-allocated by userspace (recommended 128KB).
/// Gracefully handles DNS/TLS/HTTP failures — writes error message to buffer.
fn syscall_ask_gemini(prompt_ptr: u64, prompt_len: u64, response_buf_ptr: u64) -> u64 {
    let prompt_len = prompt_len as usize;

    if prompt_len == 0 || prompt_len > 8192 {
        return u64::MAX;
    }

    // Read prompt from userspace memory
    let prompt_bytes = unsafe {
        core::slice::from_raw_parts(prompt_ptr as *const u8, prompt_len)
    };
    let prompt = match core::str::from_utf8(prompt_bytes) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    crate::serial_str!("[SYS_GEMINI] Prompt: ");
    let preview = &prompt[..prompt.len().min(80)];
    crate::drivers::serial::write_str(preview);
    crate::drivers::serial::write_newline();

    // Call Gemini API (DNS + TLS + POST — may take 5-10 seconds)
    let result = crate::net::gemini::ask_gemini(prompt);

    let response_bytes = match result {
        Ok(bytes) => bytes,
        Err(e) => {
            crate::serial_str!("[SYS_GEMINI] Error: ");
            crate::drivers::serial::write_str(e);
            crate::drivers::serial::write_newline();
            let msg = alloc::format!("Error: {}", e);
            msg.into_bytes()
        }
    };

    // Write response to userspace buffer (max 128KB)
    let max_write = response_bytes.len().min(131072);
    unsafe {
        core::ptr::copy_nonoverlapping(
            response_bytes.as_ptr(),
            response_buf_ptr as *mut u8,
            max_write,
        );
    }

    max_write as u64
}

/// Flush GPU framebuffer region to display.
/// Args: x, y, width, height (dirty rectangle)
fn syscall_gpu_flush(x: u64, y: u64, w: u64, h: u64) -> u64 {
    crate::drivers::virtio_gpu::flush_rect(x as u32, y as u32, w as u32, h as u32);
    0
}

/// Get GPU info and map framebuffer pages into task address space.
/// arg1 = userspace virtual address to map framebuffer at.
/// Returns: packed (width << 32 | height) on success, u64::MAX on error.
fn syscall_gpu_info(virt_addr: u64) -> u64 {
    use crate::drivers::virtio_gpu;

    if !virtio_gpu::GPU_ACTIVE.load(core::sync::atomic::Ordering::Relaxed) {
        return u64::MAX;
    }

    let (width, height) = match virtio_gpu::display_size() {
        Some(wh) => wh,
        None => return u64::MAX,
    };

    let pages = match virtio_gpu::framebuffer_pages() {
        Some(p) => p,
        None => return u64::MAX,
    };

    // Map each physical page into the calling task's address space
    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        let pml4_phys = task_arc.lock().page_table_phys;
        let flags = x86_64::structures::paging::PageTableFlags::PRESENT
            | x86_64::structures::paging::PageTableFlags::WRITABLE
            | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE
            | x86_64::structures::paging::PageTableFlags::NO_EXECUTE
            | x86_64::structures::paging::PageTableFlags::WRITE_THROUGH;

        for (i, &phys_page) in pages.iter().enumerate() {
            let virt = virt_addr as usize + i * 4096;
            let _ = crate::memory::paging::map_page_in_table(
                pml4_phys, virt, phys_page, flags
            );
        }
    }

    ((width as u64) << 32) | (height as u64)
}

fn syscall_poweroff() -> u64 {
    crate::serial_println!("\n[KERNEL] System poweroff requested");
    crate::serial_println!("[KERNEL] Goodbye!");

    // QEMU debug exit: writing to port 0xf4 exits QEMU
    // Exit code will be (value << 1) | 1, so 0x10 gives exit code 33
    unsafe {
        x86_64::instructions::port::Port::<u32>::new(0xf4).write(0x10);
    }

    // If debug exit isn't available, try ACPI shutdown
    // ACPI PM1a control block shutdown (common location)
    unsafe {
        x86_64::instructions::port::Port::<u16>::new(0x604).write(0x2000);
    }

    // Should never reach here
    loop {
        x86_64::instructions::hlt();
    }
}

/// Check if interrupt is pending for current task
/// Returns: 1 if interrupt pending, 0 otherwise
fn syscall_check_interrupt() -> u64 {
    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        if task_arc.lock().interrupt_pending {
            return 1;
        }
    }
    0
}

/// Clear interrupt flag for current task
/// Returns: 0 on success
fn syscall_clear_interrupt() -> u64 {
    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        task_arc.lock().interrupt_pending = false;
    }
    0
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

    // Jump to new task using IRETQ (does not return)
    unsafe {
        crate::task::switch::restore_context_only(target_ctx_ptr);
    }
}

// ============================================================================
// Phase 6: Reply-Later IPC Syscalls
// ============================================================================

/// Async IPC receive - returns CallerToken for deferred reply (syscall 0x20)
///
/// Returns:
/// - On success: the raw CallerToken value (64 bits) that must be used for reply_with_token.
///               The sender ID can be decoded from token: ((token ^ KEY) >> 32) as u32
///               The payload is stored in task's ipc_reply for separate retrieval.
/// - On no messages: 0xFFFF_FFFF_FFFF_FFFD (EWOULDBLOCK)
/// - On error: 0xFFFF_FFFF_FFFF_FFFC
fn syscall_ipc_recv_async() -> u64 {
    use crate::ipc::{ipc_recv_async, send::Errno};

    match ipc_recv_async() {
        Ok((token, msg)) => {
            let current_task_id = crate::task::task::get_current_task();
            crate::task::statistics::record_ipc_received(current_task_id);

            // Store the original message for payload retrieval
            if let Some(task) = crate::task::task::get_task(current_task_id) {
                task.lock().ipc_reply = Some(msg);
            }

            // Return the raw token value - userspace MUST use this for reply_with_token
            token.as_raw()
        }
        Err(Errno::EWOULDBLOCK) => {
            0xFFFF_FFFF_FFFF_FFFD
        }
        Err(_) => {
            0xFFFF_FFFF_FFFF_FFFC
        }
    }
}

/// Reply using CallerToken (syscall 0x21)
///
/// Arguments:
/// - arg1: CallerToken raw value (u64)
/// - arg2: payload0
/// - arg3: payload1
///
/// Returns:
/// - 0 on success
/// - u64::MAX on error
fn syscall_ipc_reply_token(token_raw: u64, payload0: u64, payload1: u64) -> u64 {
    use crate::ipc::{ipc_reply_with_token, CallerToken};

    let token = CallerToken::from_raw(token_raw);
    let reply_payload = [payload0, payload1, 0, 0];

    match ipc_reply_with_token(token, reply_payload) {
        Ok(()) => {
            let current_task_id = crate::task::task::get_current_task();
            crate::task::statistics::record_ipc_replied(current_task_id);
            0
        }
        Err(_) => {
            u64::MAX
        }
    }
}

/// Get payload from last recv_async (syscall 0x22)
///
/// Returns the full 64-bit payload[0] from the last received message.
/// Sender can be retrieved separately via syscall 0x23.
///
/// Returns:
/// - On success: full 64-bit payload[0]
/// - On error (no stored message): u64::MAX
fn syscall_ipc_get_recv_payload() -> u64 {
    let current_task_id = crate::task::task::get_current_task();

    if let Some(task) = crate::task::task::get_task(current_task_id) {
        let task_guard = task.lock();
        if let Some(ref msg) = task_guard.ipc_reply {
            // Return full 64-bit payload[0]
            return msg.payload[0];
        }
    }

    u64::MAX
}

/// Get sender from last recv_async (syscall 0x23)
///
/// Returns:
/// - On success: sender task ID
/// - On error (no stored message): u64::MAX
fn syscall_ipc_get_recv_sender() -> u64 {
    let current_task_id = crate::task::task::get_current_task();

    if let Some(task) = crate::task::task::get_task(current_task_id) {
        let task_guard = task.lock();
        if let Some(ref msg) = task_guard.ipc_reply {
            return msg.sender as u64;
        }
    }

    u64::MAX
}

// ============================================================================
// Phase 6.2: Physical Memory Mapping Syscall
// ============================================================================

/// Map physical memory flags
pub mod map_flags {
    /// Allow reading from mapped memory
    pub const MAP_READ: u64 = 0x01;
    /// Allow writing to mapped memory
    pub const MAP_WRITE: u64 = 0x02;
    /// Allow executing from mapped memory (usually not used for MMIO)
    pub const MAP_EXEC: u64 = 0x04;
    /// Use Write-Combining caching (PAT index 4) - for framebuffer
    pub const MAP_CACHE_WC: u64 = 0x10;
    /// Use Uncached mode - for MMIO devices
    pub const MAP_CACHE_UC: u64 = 0x20;
}

/// Map physical memory into current task's address space (syscall 0x24)
///
/// This syscall allows userspace drivers (like the compositor) to map
/// physical device memory (like the framebuffer) into their address space.
///
/// # Arguments
/// * `phys_addr` - Physical address to map (must be page-aligned)
/// * `virt_addr` - Virtual address to map to (must be page-aligned, in userspace)
/// * `size` - Size in bytes to map (rounded up to page boundary)
/// * `flags` - Mapping flags (MAP_READ, MAP_WRITE, MAP_CACHE_WC, etc.)
/// * `_reserved` - Reserved for future use
///
/// # Returns
/// * 0 on success
/// * u64::MAX on error (permission denied, invalid address, etc.)
///
/// # Security
/// This syscall requires a Framebuffer capability that covers the requested
/// physical address range. Without this capability, the call fails.
fn syscall_map_physical(phys_addr: u64, virt_addr: u64, size: u64, flags: u64, _reserved: u64) -> u64 {
    use crate::capability;
    use crate::memory::paging;
    use crate::task::task::{get_current_task, get_task};
    use x86_64::structures::paging::PageTableFlags as PTF;

    let task_id = get_current_task();

    // Validate alignment
    if phys_addr & 0xFFF != 0 || virt_addr & 0xFFF != 0 {
        crate::serial_println!("[MAP_PHYSICAL] Error: Address not page-aligned");
        return u64::MAX;
    }

    // Validate virtual address is in userspace (< 0x8000_0000_0000_0000)
    if virt_addr >= 0x8000_0000_0000_0000 {
        crate::serial_println!("[MAP_PHYSICAL] Error: Virtual address in kernel space");
        return u64::MAX;
    }

    // Validate size
    if size == 0 || size > 256 * 1024 * 1024 {
        // Max 256MB
        crate::serial_println!("[MAP_PHYSICAL] Error: Invalid size");
        return u64::MAX;
    }

    // Check capability — allow framebuffer AND PCI MMIO BAR regions
    // PCI MMIO BARs are typically above 0xF0000000 (MMIO hole)
    let is_pci_mmio = phys_addr >= 0xF000_0000 && size <= 1024 * 1024; // Max 1MB BAR
    if !is_pci_mmio && !capability::has_framebuffer_access(task_id, phys_addr, size) {
        crate::serial_str!("[MAP_PHYSICAL] Error: No capability for task ");
        crate::drivers::serial::write_dec(task_id);
        crate::serial_str!(" phys=");
        crate::drivers::serial::write_hex(phys_addr);
        crate::drivers::serial::write_newline();
        return u64::MAX;
    }

    // Get task's page table physical address
    let pml4_phys = match get_task(task_id) {
        Some(task) => task.lock().page_table_phys,
        None => {
            crate::serial_println!("[MAP_PHYSICAL] Error: Task not found");
            return u64::MAX;
        }
    };

    // Build page table flags based on request
    let mut ptf = PTF::PRESENT.union(PTF::USER_ACCESSIBLE).union(PTF::NO_EXECUTE);

    if flags & map_flags::MAP_WRITE != 0 {
        ptf = ptf.union(PTF::WRITABLE);
    }

    // Note: MAP_EXEC would remove NO_EXECUTE, but we don't allow it for MMIO

    if flags & map_flags::MAP_CACHE_WC != 0 {
        // Write-Combining: PAT index 4 would need PAT bit (bit 7)
        // But x86_64 crate rejects bit 7 as HUGE_PAGE in intermediate entries
        // For now, use uncached instead of WC (slower but works)
        // TODO: Implement proper PAT bit setting after map_to()
        ptf = ptf.union(PTF::NO_CACHE);
        crate::serial_println!("[MAP_PHYSICAL] Note: WC requested but using UC (PAT not supported by crate)");
    } else if flags & map_flags::MAP_CACHE_UC != 0 {
        // Uncached: PAT index 3 (set PCD and PWT)
        ptf = ptf.union(PTF::NO_CACHE).union(PTF::WRITE_THROUGH);
    }

    // Calculate number of pages
    let num_pages = ((size + 0xFFF) / 0x1000) as usize;

    crate::serial_println!("[MAP_PHYSICAL] Mapping {} pages from phys {:#x} to virt {:#x}",
                          num_pages, phys_addr, virt_addr);

    // Map each page
    for i in 0..num_pages {
        let phys = phys_addr as usize + i * 0x1000;
        let virt = virt_addr as usize + i * 0x1000;

        if let Err(_) = paging::map_page_in_table(pml4_phys, virt, phys, ptf) {
            crate::serial_println!("[MAP_PHYSICAL] Error: Failed to map page at {:#x}", virt);
            // TODO: Unmap already mapped pages on failure
            return u64::MAX;
        }
    }

    crate::serial_println!("[MAP_PHYSICAL] Successfully mapped {} pages", num_pages);
    0
}

// ===== Phase 10: PCI Enumeration for WASM Drivers =====

/// Compact PCI device info for userspace (64 bytes, C-repr)
/// This is the bridge between kernel PCI discovery and WASM driver generation.
#[repr(C)]
#[derive(Clone, Copy)]
struct PciDeviceUserInfo {
    vendor_id: u16,       // 0
    device_id: u16,       // 2
    class_code: u8,       // 4
    subclass: u8,         // 5
    prog_if: u8,          // 6
    revision: u8,         // 7
    header_type: u8,      // 8
    interrupt_line: u8,   // 9
    interrupt_pin: u8,    // 10
    bus: u8,              // 11
    device: u8,           // 12
    function: u8,         // 13
    capabilities_ptr: u8, // 14
    _pad: u8,             // 15
    bar_addrs: [u64; 3],  // 16-39: BAR physical addresses (MMIO base, decoded)
    bar_sizes: [u32; 6],  // 40-63: BAR sizes in bytes
}

/// Syscall 0xA0: Enumerate PCI devices into userspace buffer.
/// arg1 = userspace buffer ptr, arg2 = buffer size
/// Returns number of devices written.
fn syscall_pci_enumerate(buf_ptr: u64, buf_size: u64) -> u64 {
    let entry_size = core::mem::size_of::<PciDeviceUserInfo>();
    let max_entries = (buf_size as usize) / entry_size;

    if buf_ptr < 0x200000 || buf_ptr >= 0xFFFF_8000_0000_0000 || max_entries == 0 {
        return u64::MAX;
    }

    let list = crate::drivers::pci::PCI_DEVICES.lock();
    let mut written = 0usize;

    for i in 0..list.count.min(max_entries) {
        if let Some(ref dev) = list.devices[i] {
            // Decode BARs into physical addresses
            let mut bar_addrs = [0u64; 3];
            let mut bar_sizes = [0u32; 6];

            for b in 0..6 {
                bar_sizes[b] = crate::drivers::pci::bar_size(dev.bus, dev.device, dev.function, b as u8);
                match crate::drivers::pci::decode_bar(dev, b) {
                    crate::drivers::pci::BarType::Mmio32 { base, .. } => {
                        if b < 3 { bar_addrs[b] = base as u64; }
                    }
                    crate::drivers::pci::BarType::Mmio64 { base, .. } => {
                        if b < 3 { bar_addrs[b] = base; }
                    }
                    crate::drivers::pci::BarType::Io { base } => {
                        if b < 3 { bar_addrs[b] = base as u64 | 0x1_0000_0000; } // Flag: bit 32 = I/O
                    }
                    crate::drivers::pci::BarType::None => {}
                }
            }

            let info = PciDeviceUserInfo {
                vendor_id: dev.vendor_id,
                device_id: dev.device_id,
                class_code: dev.class_code,
                subclass: dev.subclass,
                prog_if: dev.prog_if,
                revision: dev.revision,
                header_type: dev.header_type,
                interrupt_line: dev.interrupt_line,
                interrupt_pin: dev.interrupt_pin,
                bus: dev.bus,
                device: dev.device,
                function: dev.function,
                capabilities_ptr: dev.capabilities_ptr,
                _pad: 0,
                bar_addrs,
                bar_sizes,
            };

            // Write to userspace buffer
            let dest = (buf_ptr as usize) + written * entry_size;
            unsafe {
                let src = &info as *const PciDeviceUserInfo as *const u8;
                let dst = dest as *mut u8;
                core::ptr::copy_nonoverlapping(src, dst, entry_size);
            }
            written += 1;
        }
    }

    crate::serial_str!("[PCI] Enumerated ");
    crate::drivers::serial::write_dec(written as u32);
    crate::serial_strln!(" devices to userspace");

    written as u64
}

// ===== Phase 10: Capability-Gated Port I/O =====
//
// These syscalls validate that the requested port falls within a known
// PCI device's I/O BAR range. This implements the seL4-style capability
// model: userspace WASM drivers can only touch ports they're authorized for.
//
// BLOCKED ports (kernel-reserved):
//   0x0020-0x0021: PIC1
//   0x00A0-0x00A1: PIC2
//   0x0040-0x0043: PIT timer
//   0x0060, 0x0064: PS/2 keyboard/mouse controller
//   0x0070-0x0071: CMOS/RTC
//   0x03F8-0x03FF: COM1 (kernel serial log)
//   0x02F8-0x02FF: COM2 (MCP proxy)
//   0x03E8-0x03EF: COM3 (God Mode pipe)
//   0x0CF8-0x0CFF: PCI configuration space

/// Check if a port is within a known PCI device's I/O BAR range.
/// Returns true if the port is permitted for userspace access.
fn port_io_allowed(port: u16) -> bool {
    // Blocklist: kernel-critical ports
    match port {
        0x0020..=0x0021 => return false, // PIC1
        0x00A0..=0x00A1 => return false, // PIC2
        0x0040..=0x0043 => return false, // PIT
        0x0060 | 0x0064 => return false, // PS/2
        0x0070..=0x0071 => return false, // CMOS
        0x03F8..=0x03FF => return false, // COM1
        0x02F8..=0x02FF => return false, // COM2
        0x03E8..=0x03EF => return false, // COM3
        0x0CF8..=0x0CFF => return false, // PCI config
        _ => {}
    }

    // Allowlist: check PCI device I/O BARs
    let list = crate::drivers::pci::PCI_DEVICES.lock();
    for i in 0..list.count {
        if let Some(ref dev) = list.devices[i] {
            for bar_idx in 0..6u8 {
                let bar_val = dev.bars[bar_idx as usize];
                if bar_val & 1 != 0 {
                    // I/O BAR: base is bits 2-15, size from bar_size()
                    let base = (bar_val & 0xFFFC) as u16;
                    let size = crate::drivers::pci::bar_size(
                        dev.bus, dev.device, dev.function, bar_idx
                    ) as u16;
                    if size > 0 && port >= base && port < base.saturating_add(size) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

// ===== Phase 10: IRQ Routing for WASM Drivers =====
//
// Binding table: maps IDT vector → task_id + pending flag.
// When an interrupt fires, the IDT handler sets the pending flag.
// Userspace polls via SYS_CHECK_IRQ (non-blocking) or uses HLT + poll.
// This is the MINIX-3 pattern adapted for wasmi call_resumable.

/// Maximum bindable IRQ vectors (vectors 46-63 for WASM drivers)
const MAX_IRQ_BINDINGS: usize = 24;
/// First vector available for WASM driver binding
const WASM_IRQ_BASE_VECTOR: u8 = 46;

/// IRQ binding entry
struct IrqBinding {
    vector: u8,       // IDT vector number
    task_id: u32,     // Bound userspace task
    pending: bool,    // Set by IDT handler, cleared by ACK
    active: bool,     // Binding is live
}

/// Global IRQ binding table (accessed from IDT handlers and syscalls)
static IRQ_BINDINGS: spin::Mutex<[IrqBinding; MAX_IRQ_BINDINGS]> = spin::Mutex::new({
    const EMPTY: IrqBinding = IrqBinding { vector: 0, task_id: 0, pending: false, active: false };
    [EMPTY; MAX_IRQ_BINDINGS]
});

/// Called from IDT handlers to signal a bound IRQ.
/// Sets the pending flag so userspace can detect it via poll.
pub fn signal_irq(vector: u8) {
    // Fast path: direct array index
    let idx = vector.wrapping_sub(WASM_IRQ_BASE_VECTOR) as usize;
    if idx < MAX_IRQ_BINDINGS {
        if let Some(mut bindings) = IRQ_BINDINGS.try_lock() {
            if bindings[idx].active && bindings[idx].vector == vector {
                bindings[idx].pending = true;
            }
        }
        // If lock fails (contention from nested IRQ), the signal is lost.
        // Acceptable: hardware will re-assert level-triggered interrupts.
    }
}

/// Syscall 0xA7: Bind an IRQ vector to the calling task.
/// arg1 = PCI interrupt_line (will be mapped to a vector)
/// arg2 = 0 (reserved)
/// Returns: the IDT vector number assigned, or u64::MAX on error.
fn syscall_bind_irq(irq_line: u64, _reserved: u64) -> u64 {
    let irq = irq_line as u8;
    let task_id = crate::task::task::get_current_task();

    // Map IRQ line to an IDT vector (base + offset)
    // IRQ lines 0-23 map to vectors WASM_IRQ_BASE_VECTOR + irq
    if irq >= MAX_IRQ_BINDINGS as u8 {
        crate::serial_strln!("[IRQ] Bind failed: IRQ line out of range");
        return u64::MAX;
    }

    let vector = WASM_IRQ_BASE_VECTOR + irq;
    let idx = irq as usize;

    {
        let mut bindings = IRQ_BINDINGS.lock();
        bindings[idx] = IrqBinding {
            vector,
            task_id,
            pending: false,
            active: true,
        };
    }

    // Enable the IRQ at the IOAPIC (level-triggered for PCI)
    super::ioapic::enable_irq_level(irq, vector);

    crate::serial_str!("[IRQ] Bound IRQ");
    crate::drivers::serial::write_dec(irq as u32);
    crate::serial_str!(" -> vector ");
    crate::drivers::serial::write_dec(vector as u32);
    crate::serial_str!(" for task ");
    crate::drivers::serial::write_dec(task_id);
    crate::serial_strln!("");

    vector as u64
}

/// Syscall 0xA8: Acknowledge an IRQ (clear pending + unmask at IOAPIC).
/// arg1 = IRQ line number
fn syscall_ack_irq(irq_line: u64) -> u64 {
    let irq = irq_line as u8;
    let idx = irq as usize;

    if idx >= MAX_IRQ_BINDINGS { return u64::MAX; }

    {
        let mut bindings = IRQ_BINDINGS.lock();
        if bindings[idx].active {
            bindings[idx].pending = false;
        }
    }

    // Re-enable at IOAPIC (was masked by handler)
    let vector = WASM_IRQ_BASE_VECTOR + irq;
    super::ioapic::enable_irq_level(irq, vector);

    0
}

/// Syscall 0xA9: Check if a bound IRQ has fired (non-blocking poll).
/// arg1 = IRQ line number
/// Returns: 1 if pending, 0 if not, u64::MAX if not bound.
fn syscall_check_irq(irq_line: u64) -> u64 {
    let idx = irq_line as usize;
    if idx >= MAX_IRQ_BINDINGS { return u64::MAX; }

    let bindings = IRQ_BINDINGS.lock();
    if !bindings[idx].active { return u64::MAX; }
    if bindings[idx].pending { 1 } else { 0 }
}

/// Syscall 0xA1: Read byte from I/O port (capability-gated)
fn syscall_port_inb(port: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u8>::new(port);
        p.read() as u64
    }
}

/// Syscall 0xA2: Read word from I/O port (capability-gated)
fn syscall_port_inw(port: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u16>::new(port);
        p.read() as u64
    }
}

/// Syscall 0xA3: Read dword from I/O port (capability-gated)
fn syscall_port_inl(port: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u32>::new(port);
        p.read() as u64
    }
}

/// Syscall 0xA4: Write byte to I/O port (capability-gated)
fn syscall_port_outb(port: u64, value: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u8>::new(port);
        p.write(value as u8);
    }
    0
}

/// Syscall 0xA5: Write word to I/O port (capability-gated)
fn syscall_port_outw(port: u64, value: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u16>::new(port);
        p.write(value as u16);
    }
    0
}

/// Syscall 0xA6: Write dword to I/O port (capability-gated)
fn syscall_port_outl(port: u64, value: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u32>::new(port);
        p.write(value as u32);
    }
    0
}

// ===== Phase 10: DMA Buffer Allocation + IOMMU Status =====

/// Syscall 0xAA: Allocate a contiguous physical DMA buffer.
/// arg1 = size (bytes, rounded up to page boundary)
/// arg2 = virtual address to map at in caller's address space
/// Returns: physical address of buffer, or u64::MAX on error.
///
/// The physical memory is allocated contiguously (required for DMA).
/// When IOMMU is available, it would also set up IOMMU page tables.
fn syscall_dma_alloc(size: u64, vaddr: u64) -> u64 {
    let num_pages = ((size as usize) + 4095) / 4096;
    if num_pages == 0 || num_pages > 256 { // Max 1MB DMA buffer
        return u64::MAX;
    }
    if vaddr < 0x200000 || vaddr >= 0xFFFF_8000_0000_0000 {
        return u64::MAX;
    }

    // Allocate contiguous physical pages
    // Use the physical allocator to get a contiguous block
    let phys_addr = match crate::memory::physical::alloc_contiguous(num_pages) {
        Some(addr) => addr,
        None => {
            crate::serial_strln!("[DMA] Failed to allocate contiguous memory");
            return u64::MAX;
        }
    };

    // Map into caller's address space with Uncacheable attributes (for DMA)
    use crate::memory::paging;
    use crate::task::task::{get_current_task, get_task};
    use x86_64::structures::paging::PageTableFlags as Ptf;
    let task_id = get_current_task();
    let pml4_phys = match get_task(task_id) {
        Some(task) => task.lock().page_table_phys,
        None => return u64::MAX,
    };

    let ptf = Ptf::PRESENT | Ptf::WRITABLE | Ptf::USER_ACCESSIBLE | Ptf::NO_EXECUTE
        | Ptf::WRITE_THROUGH | Ptf::NO_CACHE;

    for i in 0..num_pages {
        let virt = vaddr as usize + i * 4096;
        let phys = phys_addr + i * 4096;
        if paging::map_page_in_table(pml4_phys, virt, phys, ptf).is_err() {
            crate::serial_strln!("[DMA] Page mapping failed");
            return u64::MAX;
        }
    }

    // TODO: When IOMMU is initialized, create IOMMU page table entries here
    // to restrict which PCI device can access this physical memory.
    let iommu = super::acpi::iommu_available();

    crate::serial_str!("[DMA] Allocated ");
    crate::drivers::serial::write_dec(num_pages as u32);
    crate::serial_str!(" pages at phys=");
    crate::drivers::serial::write_hex(phys_addr as u64);
    crate::serial_str!(" vaddr=");
    crate::drivers::serial::write_hex(vaddr);
    if iommu {
        crate::serial_str!(" (IOMMU available)");
    }
    crate::drivers::serial::write_newline();

    phys_addr as u64
}

/// Syscall 0xAB: Query IOMMU status.
/// Returns: (iommu_base << 32) | available_flag
fn syscall_iommu_status() -> u64 {
    let available = super::acpi::iommu_available();
    let base = super::acpi::iommu_base();
    if available {
        (base & 0xFFFFFFFF_00000000) | 1
    } else {
        0
    }
}

// ── Phase 11: WASM Network Driver Bridge ──────────────────────────────────

/// Syscall 0xAC: Register a WASM network driver.
/// arg1 = MAC bytes 0-3 (little-endian), arg2 = MAC bytes 4-5 (little-endian)
/// Initializes the smoltcp stack with this MAC address.
fn syscall_net_register(mac_lo: u64, mac_hi: u64) -> u64 {
    let mac = [
        (mac_lo & 0xFF) as u8,
        ((mac_lo >> 8) & 0xFF) as u8,
        ((mac_lo >> 16) & 0xFF) as u8,
        ((mac_lo >> 24) & 0xFF) as u8,
        (mac_hi & 0xFF) as u8,
        ((mac_hi >> 8) & 0xFF) as u8,
    ];
    crate::net::init_wasm_net(mac);
    0
}

/// Syscall 0xAD: Submit a received Ethernet frame to the kernel network stack.
/// arg1 = virtual address of frame data (in caller's address space)
/// arg2 = length in bytes
/// Returns: 0 on success, u64::MAX on error
fn syscall_net_submit_rx(vaddr: u64, length: u64) -> u64 {
    let len = length as usize;
    if len == 0 || len > 1514 || vaddr < 0x200000 {
        return u64::MAX;
    }
    let data = unsafe {
        core::slice::from_raw_parts(vaddr as *const u8, len)
    };
    if crate::net::wasm_net_submit_rx(data) {
        0
    } else {
        u64::MAX
    }
}

/// Syscall 0xAE: Poll for a packet to transmit.
/// arg1 = virtual address of buffer (caller provides)
/// arg2 = max buffer length
/// Returns: packet length if available, 0 if no packet, u64::MAX on error
fn syscall_net_poll_tx(vaddr: u64, max_len: u64) -> u64 {
    let max = max_len as usize;
    if max == 0 || max > 2048 || vaddr < 0x200000 {
        return u64::MAX;
    }
    let buf = unsafe {
        core::slice::from_raw_parts_mut(vaddr as *mut u8, max)
    };
    match crate::net::wasm_net_poll_tx(buf) {
        Some(len) => {
            static TX_POP_LOG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            let c = TX_POP_LOG.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if c < 5 {
                crate::serial_str!("[NET-POP] ");
                crate::drivers::serial::write_dec(len as u32);
                crate::serial_strln!("B popped from TX ring");
            }
            len as u64
        }
        None => 0,
    }
}

/// Syscall 0xAF: Read physical memory via HHDM (DMA coherency fallback).
/// Used when userspace NO_CACHE mapping doesn't reflect DMA writeback (WHPX bug).
/// arg1 = physical address to read from
/// arg2 = destination virtual address in caller's space + (len << 32)
/// Returns: number of bytes read, or u64::MAX on error.
/// Syscall 0xAF: Read from physical memory via HHDM.
/// Mode 1 (len > 0): Copy len bytes from phys to dest (bulk copy)
/// Mode 2 (len == 0): Read u64 from phys_addr, return directly (no buffer needed)
fn syscall_dma_sync_read(phys_addr: u64, dest_and_len: u64) -> u64 {
    if phys_addr == 0 { return u64::MAX; }

    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let src_virt = hhdm + phys_addr as usize;

    let len = ((dest_and_len >> 32) & 0xFFFF) as usize;

    if len == 0 {
        // Mode 2: read u64 directly — flush cache line first to see DMA writes
        unsafe {
            // CLFLUSH invalidates the cache line containing this address
            core::arch::asm!("clflush [{}]", in(reg) src_virt, options(nostack));
            // Memory fence to ensure the flush completes
            core::arch::asm!("mfence", options(nostack));
        }
        let val = unsafe { core::ptr::read_volatile(src_virt as *const u64) };
        return val;
    }

    // Mode 1: bulk copy
    let dest_vaddr = (dest_and_len & 0xFFFFFFFF) as usize;
    if len > 4096 || dest_vaddr < 0x200000 {
        return u64::MAX;
    }

    let src = src_virt as *const u8;
    let dst = dest_vaddr as *mut u8;
    unsafe {
        // Flush cache lines for the source range to see DMA writes
        let mut addr = src_virt;
        while addr < src_virt + len {
            core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack));
            addr += 64; // cache line size
        }
        core::arch::asm!("mfence", options(nostack));

        for i in 0..len {
            let byte = core::ptr::read_volatile(src.add(i));
            core::ptr::write_volatile(dst.add(i), byte);
        }
    }

    len as u64
}

/// Syscall 0xB0: Kernel-assisted DMA RX — read packet from physical DMA buffers
/// and deliver directly to smoltcp. Bypasses ALL userspace cache coherency issues.
///
/// arg1 = ring_phys | (desc_idx << 48) — physical address of descriptor ring + index
/// arg2 = buf_phys | (buf_size << 48) — physical address of packet buffer pool + per-buffer size
///
/// The kernel reads the E1000 RX descriptor via HHDM, extracts packet length,
/// reads the packet data from the buffer, and submits it to smoltcp.
/// Returns: packet length on success, 0 if no packet, u64::MAX on error.
fn syscall_net_dma_rx(ring_and_idx: u64, buf_and_size: u64) -> u64 {
    let ring_phys = ring_and_idx & 0x0000_FFFF_FFFF_FFFF;
    let desc_idx = ((ring_and_idx >> 48) & 0xFFFF) as usize;
    let buf_phys = buf_and_size & 0x0000_FFFF_FFFF_FFFF;
    let buf_size = ((buf_and_size >> 48) & 0xFFFF) as usize;

    if ring_phys == 0 || buf_phys == 0 || buf_size == 0 || desc_idx > 7 {
        return u64::MAX;
    }

    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);

    // Read the descriptor (16 bytes) from physical memory via HHDM
    let desc_phys = ring_phys + (desc_idx as u64 * 16);
    let desc_virt = hhdm + desc_phys as usize;

    // Flush cache line to see DMA writes
    unsafe {
        core::arch::asm!("clflush [{}]", in(reg) desc_virt, options(nostack));
        core::arch::asm!("mfence", options(nostack));
    }

    // Read length (bytes 8-9 of descriptor) and status (byte 12)
    let len_status = unsafe { core::ptr::read_volatile((desc_virt + 8) as *const u64) };
    let pkt_len = (len_status & 0xFFFF) as usize;

    if pkt_len == 0 || pkt_len > 2048 {
        return 0; // No packet or invalid length
    }

    // Read packet data from the buffer pool
    let pkt_phys = buf_phys + (desc_idx as u64 * buf_size as u64);
    let pkt_virt = hhdm + pkt_phys as usize;

    // Flush cache lines for the packet data
    unsafe {
        let mut addr = pkt_virt;
        let end = pkt_virt + pkt_len;
        while addr < end {
            core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack));
            addr += 64;
        }
        core::arch::asm!("mfence", options(nostack));
    }

    // Read packet data into a temporary buffer
    let mut pkt_buf = [0u8; 2048];
    unsafe {
        let src = pkt_virt as *const u8;
        for i in 0..pkt_len {
            pkt_buf[i] = core::ptr::read_volatile(src.add(i));
        }
    }

    // Submit to smoltcp via the WASM_NET ring
    if crate::net::wasm_net_submit_rx(&pkt_buf[..pkt_len]) {
        pkt_len as u64
    } else {
        0 // Ring full
    }
}

/// Syscall 0xB1: Write to physical memory via HHDM (DMA coherency for writes).
/// arg1 = physical address, arg2 = source vaddr | (len << 32)
fn syscall_dma_sync_write(phys_addr: u64, src_and_len: u64) -> u64 {
    let src_vaddr = (src_and_len & 0xFFFFFFFF) as usize;
    let len = ((src_and_len >> 32) & 0xFFFF) as usize;

    if len == 0 || len > 4096 || phys_addr == 0 || src_vaddr < 0x200000 {
        return u64::MAX;
    }

    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let dst_virt = hhdm + phys_addr as usize;
    let src = src_vaddr as *const u8;
    let dst = dst_virt as *mut u8;

    unsafe {
        for i in 0..len {
            let byte = core::ptr::read_volatile(src.add(i));
            core::ptr::write_volatile(dst.add(i), byte);
        }
        // Flush written cache lines so DMA device sees them
        let mut addr = dst_virt;
        while addr < dst_virt + len {
            core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack));
            addr += 64;
        }
        core::arch::asm!("mfence", options(nostack));
    }

    len as u64
}

/// Syscall 0xB2: OS metrics for AI introspection.
/// The kernel's own AI (Draug, WASM apps) can query live system state.
///
/// arg1 = metric_id:
///   0 = network summary (packed: has_ip(1) | ip_bytes(32))
///   1 = firewall stats (packed: allows(32) | drops(32))
///   2 = uptime_ms
///   3 = suspicious packet count
///
/// Returns: packed u64 with the requested metric.
fn syscall_net_metrics(metric_id: u64, _reserved: u64) -> u64 {
    match metric_id {
        0 => {
            // Network: has_ip(1) | ip_a(8) | ip_b(8) | ip_c(8) | ip_d(8)
            let has_ip = if crate::net::has_ip() { 1u64 } else { 0u64 };
            let guard = crate::net::NET_STATE.lock();
            if let Some(ref state) = *guard {
                let addrs = state.iface.ip_addrs();
                if let Some(cidr) = addrs.first() {
                    if let smoltcp::wire::IpAddress::Ipv4(v4) = cidr.address() {
                        let o = v4.octets();
                        drop(guard);
                        return has_ip
                            | ((o[0] as u64) << 8)
                            | ((o[1] as u64) << 16)
                            | ((o[2] as u64) << 24)
                            | ((o[3] as u64) << 32);
                    }
                }
            }
            drop(guard);
            has_ip
        }
        1 => {
            // Firewall: allows(32) | drops(32)
            let allows = crate::net::firewall::ALLOWS.load(core::sync::atomic::Ordering::Relaxed) as u64;
            let drops = crate::net::firewall::DROPS.load(core::sync::atomic::Ordering::Relaxed) as u64;
            allows | (drops << 32)
        }
        2 => crate::timer::uptime_ms(),
        3 => crate::net::firewall::SUSPICIOUS.count.load(core::sync::atomic::Ordering::Relaxed) as u64,
        4 => {
            // Anomaly detection stats: blocked_ips(16) | total_syn_attempts(16)
            let (blocked, attempts) = crate::net::firewall::anomaly_stats();
            (blocked as u64) | ((attempts as u64) << 16)
        }
        _ => u64::MAX,
    }
}
