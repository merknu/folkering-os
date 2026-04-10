//! Debug statics, getters, and helper extern "C" functions used by the
//! naked syscall entry asm and the #GP/PF handlers for crash analysis.

use core::sync::atomic::AtomicU64;

// ── Debug statics referenced from naked asm ────────────────────────────

/// Debug marker for tracking exact crash location
#[no_mangle]
pub static DEBUG_MARKER: AtomicU64 = AtomicU64::new(0);

/// Debug: Last context pointer value (for crash analysis)
/// Also used to store RIP before IRETQ
#[no_mangle]
pub(super) static DEBUG_CONTEXT_PTR: AtomicU64 = AtomicU64::new(0);

/// Debug: RIP value before IRETQ
#[no_mangle]
pub(super) static DEBUG_RIP: AtomicU64 = AtomicU64::new(0);

/// Debug: RAX value at yield check
#[no_mangle]
pub(super) static DEBUG_RAX: AtomicU64 = AtomicU64::new(0);

/// Debug: RSP value before IRETQ
#[no_mangle]
pub(super) static DEBUG_RSP: AtomicU64 = AtomicU64::new(0);

/// Debug: Return value before IRETQ (for syscall 13)
#[no_mangle]
pub(super) static DEBUG_RETURN_VAL: AtomicU64 = AtomicU64::new(0);

/// Debug: Handler result for syscall 13 (to compare with assembly)
/// Using a raw mutable static for direct assembly access
#[no_mangle]
pub static mut SYSCALL_RESULT: u64 = 0;

/// Old atomic version for comparison
#[no_mangle]
pub(super) static DEBUG_HANDLER_RESULT: AtomicU64 = AtomicU64::new(0);

/// Helper function to get SYSCALL_RESULT value
/// This avoids RIP-relative addressing issues in naked functions
/// NOTE: Must NOT print anything - that would trigger syscalls that overwrite R14!
#[no_mangle]
#[inline(never)]
pub(super) extern "C" fn get_syscall_result() -> u64 {
    // Use volatile read to prevent optimization
    unsafe { core::ptr::read_volatile(&SYSCALL_RESULT) }
}

/// Debug: RFLAGS value before IRETQ
#[no_mangle]
pub(super) static DEBUG_RFLAGS: AtomicU64 = AtomicU64::new(0);

/// Debug: RCX value at syscall entry (saved RIP from SYSCALL instruction)
#[no_mangle]
pub(super) static DEBUG_RCX: AtomicU64 = AtomicU64::new(0);

/// Debug: Context.r14 value read from task Context JUST BEFORE restoring R14.
/// If this equals user_RSP at crash time, Context.r14 was already corrupt in the kernel.
/// If this equals 800 (fb.height), the corruption happens after restore.
#[no_mangle]
pub static DEBUG_CONTEXT_R14: AtomicU64 = AtomicU64::new(0);

/// Debug: Context.rsp value read alongside Context.r14 (user RSP for comparison)
#[no_mangle]
pub static DEBUG_CONTEXT_RSP: AtomicU64 = AtomicU64::new(0);

/// Debug: Context pointer returned by timer_preempt_handler (captured right before return)
#[no_mangle]
pub static DEBUG_NEXT_CTX_PTR: AtomicU64 = AtomicU64::new(0);

/// Debug: context.cs value at the pointer returned by timer_preempt_handler
#[no_mangle]
pub static DEBUG_NEXT_CTX_CS: AtomicU64 = AtomicU64::new(0);

/// Debug: context.rip value at the pointer returned by timer_preempt_handler
#[no_mangle]
pub static DEBUG_NEXT_CTX_RIP: AtomicU64 = AtomicU64::new(0);

// ─── User register scratch statics REMOVED ───────────────────────────────────
// USER_R15_SAVE, USER_R12_SAVE, USER_RSI_SAVE, USER_RDX_SAVE,
// USER_R13_SAVE, USER_R14_SAVE have been eliminated.
//
// The SWAPGS + kernel-stack approach (see syscall_entry below) saves every
// user GPR directly onto the kernel stack, so no global scratch storage is
// needed. This is SMP-correct from day one since each CPU uses its own
// CpuLocal block (via IA32_KERNEL_GS_BASE) and its own kernel stack slot.

// ── Public getters ─────────────────────────────────────────────────────

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

// ── Verbose debug print helpers ────────────────────────────────────────

/// Debug function called from int_syscall_entry
#[no_mangle]
pub(super) extern "C" fn debug_int_entry() {
}

/// Debug function called after yield returns
#[no_mangle]
pub(super) extern "C" fn debug_after_yield() {
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
