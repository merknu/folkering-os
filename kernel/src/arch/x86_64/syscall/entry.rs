//! Naked-asm syscall entry points: legacy `int 0x80` and modern SYSCALL/SYSRET.
//!
//! These are the lowest-level glue that runs on the user→kernel boundary
//! before any Rust code can execute. The entry asm references several
//! statics and helper extern "C" functions defined in sibling modules
//! (`debug`, `state`, `dispatch`).

use crate::task::task;

use super::debug::{
    DEBUG_MARKER, DEBUG_CONTEXT_PTR, DEBUG_RIP, DEBUG_RSP, DEBUG_RFLAGS,
    DEBUG_RAX, DEBUG_RCX,
    debug_int_entry, debug_after_yield, get_syscall_result,
};
use super::dispatch::syscall_handler;
use super::state::{CURRENT_CONTEXT_PTR, SYSCALL_COUNT, SYSCALL_STACK};

/// Get current task's context pointer (lock-free, fast path for syscalls)
#[no_mangle]
pub(super) extern "C" fn get_current_task_context_ptr() -> *mut crate::task::task::Context {
    CURRENT_CONTEXT_PTR.load(core::sync::atomic::Ordering::Acquire) as *mut _
}

/// Yield CPU from syscall (may not return if task switch)
#[no_mangle]
pub(super) extern "C" fn syscall_do_yield() {
    crate::task::scheduler::yield_cpu();
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
pub(super) extern "C" fn syscall_entry() {
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
