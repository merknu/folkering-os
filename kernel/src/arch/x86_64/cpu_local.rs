//! Per-CPU Local Storage via IA32_KERNEL_GS_BASE MSR + SWAPGS
//!
//! This module provides GS-relative per-CPU storage for the syscall fast path.
//! On SYSCALL entry, SWAPGS switches GS.base to point at `CpuLocal`, allowing
//! the kernel to load the kernel RSP and save the user RSP without using any GPR.
//!
//! Layout (accessed via GS-relative addressing after SWAPGS):
//!   [gs:0]  = kernel_rsp       — top of this CPU's syscall stack
//!   [gs:8]  = user_rsp_scratch — temporary storage for user RSP during entry
//!   [gs:16] = context_ptr      — *mut Context for the current task
//!   [gs:24] = cpu_id           — logical CPU index (SMP-ready)

/// Per-CPU local storage block.
///
/// Must be `repr(C)` so that field offsets are stable and match
/// the hard-coded GS-relative offsets in syscall assembly.
#[repr(C)]
pub struct CpuLocal {
    /// [gs:0] Top of this CPU's syscall kernel stack (loaded into RSP on SYSCALL entry).
    pub kernel_rsp: u64,
    /// [gs:8] Scratch slot to save user RSP during the entry sequence
    ///        (before we have a kernel stack to push onto).
    pub user_rsp_scratch: u64,
    /// [gs:16] Raw pointer to the current task's Context struct.
    pub context_ptr: u64,
    /// [gs:24] Logical CPU index (always 0 on single-core).
    pub cpu_id: u64,
}

/// BSP (boot-strap processor) CPU-local block.
///
/// A pointer to this structure is written to IA32_KERNEL_GS_BASE so that
/// SWAPGS in the syscall entry path makes it accessible via GS-relative ops.
static mut CPU_LOCAL_BSP: CpuLocal = CpuLocal {
    kernel_rsp: 0,
    user_rsp_scratch: 0,
    context_ptr: 0,
    cpu_id: 0,
};

/// Initialize the BSP CPU-local block and write its address to IA32_KERNEL_GS_BASE.
///
/// Call once during kernel initialisation — after GDT/TSS are loaded so that
/// GS is already set to the kernel data segment — but before the first syscall
/// can be issued.
///
/// # Arguments
/// * `kernel_stack_top` — virtual address of the top (highest address) of the
///   dedicated syscall kernel stack. This is the value loaded into RSP on every
///   SYSCALL entry via `mov rsp, gs:[0]`.
pub fn init(kernel_stack_top: u64) {
    unsafe {
        CPU_LOCAL_BSP.kernel_rsp = kernel_stack_top;
        CPU_LOCAL_BSP.cpu_id = 0;

        let ptr = &CPU_LOCAL_BSP as *const CpuLocal as u64;

        // Write ptr to IA32_KERNEL_GS_BASE (MSR 0xC0000102).
        // This is the GS base that becomes active *after* SWAPGS, i.e. when
        // executing in kernel mode on the SYSCALL path.
        //
        // IA32_GS_BASE (0xC0000101) keeps whatever the user had (typically 0
        // since we don't use GS for kernel TLS outside the syscall window).
        // Use explicit register constraints to avoid Rust allocating `hi` to ecx,
        // which would be clobbered by the `in("ecx")` MSR-number input.
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC0000102u32,                    // IA32_KERNEL_GS_BASE
            in("eax") (ptr & 0xFFFF_FFFF) as u32,       // low 32 bits of ptr
            in("edx") (ptr >> 32) as u32,               // high 32 bits of ptr
            options(nostack, preserves_flags)
        );
    }

    // Readback: verify the MSR was written correctly
    let readback: u64;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            "shl rdx, 32",
            "or rax, rdx",
            in("ecx") 0xC0000102u32,
            out("rax") readback,
            out("rdx") _,
            options(nostack, preserves_flags)
        );
    }
    crate::serial_str!("[CPU_LOCAL] IA32_KERNEL_GS_BASE set to CpuLocal @ ");
    crate::drivers::serial::write_hex(unsafe { &CPU_LOCAL_BSP as *const _ as u64 });
    crate::serial_str!(" (MSR readback=");
    crate::drivers::serial::write_hex(readback);
    crate::serial_str!(")");
    crate::drivers::serial::write_newline();
    crate::serial_str!("[CPU_LOCAL] kernel_rsp = ");
    crate::drivers::serial::write_hex(kernel_stack_top);
    crate::drivers::serial::write_newline();
}
