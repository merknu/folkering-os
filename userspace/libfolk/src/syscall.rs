//! Raw syscall interface for Folkering OS
//!
//! Uses the x86-64 SYSCALL instruction with the following ABI:
//! - RAX: syscall number
//! - RDI: arg1
//! - RSI: arg2
//! - RDX: arg3
//! - R10: arg4 (RCX is clobbered by SYSCALL)
//! - R8:  arg5
//! - R9:  arg6
//! - Return value in RAX

// Syscall numbers (must match kernel/src/arch/x86_64/syscall.rs)
pub const SYS_IPC_SEND: u64 = 0;
pub const SYS_IPC_RECEIVE: u64 = 1;
pub const SYS_IPC_REPLY: u64 = 2;
pub const SYS_SHMEM_CREATE: u64 = 3;
pub const SYS_SHMEM_MAP: u64 = 4;
pub const SYS_SHMEM_GRANT: u64 = 15;
pub const SYS_SPAWN: u64 = 5;
pub const SYS_EXIT: u64 = 6;
pub const SYS_YIELD: u64 = 7;
pub const SYS_READ_KEY: u64 = 8;
pub const SYS_WRITE_CHAR: u64 = 9;
pub const SYS_GET_PID: u64 = 10;
pub const SYS_TASK_LIST: u64 = 11;
pub const SYS_UPTIME: u64 = 12;

/// Execute a syscall with no arguments
#[inline(always)]
pub unsafe fn syscall0(nr: u64) -> u64 {
    let ret: u64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        out("rcx") _,  // Clobbered by SYSCALL (saved RIP)
        out("r11") _,  // Clobbered by SYSCALL (saved RFLAGS)
        options(nostack, preserves_flags)
    );
    ret
}

/// Execute a syscall with 1 argument
#[inline(always)]
pub unsafe fn syscall1(nr: u64, a1: u64) -> u64 {
    let ret: u64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1,
        out("rcx") _,
        out("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Execute a syscall with 2 arguments
#[inline(always)]
pub unsafe fn syscall2(nr: u64, a1: u64, a2: u64) -> u64 {
    let ret: u64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1,
        in("rsi") a2,
        out("rcx") _,
        out("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Execute a syscall with 3 arguments
#[inline(always)]
pub unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        out("rcx") _,
        out("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Execute a syscall with 4 arguments
#[inline(always)]
pub unsafe fn syscall4(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,  // R10 instead of RCX for arg4
        out("rcx") _,
        out("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Execute a syscall with 5 arguments
#[inline(always)]
pub unsafe fn syscall5(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> u64 {
    let ret: u64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        in("r8") a5,
        out("rcx") _,
        out("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}

/// Execute a syscall with 6 arguments
#[inline(always)]
pub unsafe fn syscall6(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    let ret: u64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        in("r8") a5,
        in("r9") a6,
        out("rcx") _,
        out("r11") _,
        options(nostack, preserves_flags)
    );
    ret
}
