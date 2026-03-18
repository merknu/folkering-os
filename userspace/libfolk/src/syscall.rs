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
pub const SYS_POWEROFF: u64 = 16;
pub const SYS_CHECK_INTERRUPT: u64 = 17;
pub const SYS_CLEAR_INTERRUPT: u64 = 18;
pub const SYS_SHMEM_UNMAP: u64 = 19;
pub const SYS_SHMEM_DESTROY: u64 = 20;

// Phase 6: Reply-Later IPC
pub const SYS_IPC_RECV_ASYNC: u64 = 0x20;  // 32 - Async receive with CallerToken
pub const SYS_IPC_REPLY_TOKEN: u64 = 0x21; // 33 - Reply using CallerToken
pub const SYS_IPC_GET_RECV_PAYLOAD: u64 = 0x22; // 34 - Get payload from last recv_async
pub const SYS_IPC_GET_RECV_SENDER: u64 = 0x23;  // 35 - Get sender from last recv_async

// Phase 6.2: Physical memory mapping
pub const SYS_MAP_PHYSICAL: u64 = 0x24;  // 36 - Map physical memory with capability check

// Phase 7: Input
pub const SYS_READ_MOUSE: u64 = 0x25;    // 37 - Read mouse event (packed buttons/dx/dy)

// Phase 8: Detailed task list
pub const SYS_TASK_LIST_DETAILED: u64 = 0x26; // 38 - Fill shmem with task details

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
