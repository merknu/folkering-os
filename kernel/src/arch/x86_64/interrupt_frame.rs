//! Interrupt Frame Structure
//!
//! Represents the CPU state pushed to the kernel stack for context switching.

/// Full interrupt/context frame for task switching
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InterruptFrame {
    // Callee-saved registers (pushed by software)
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    // Hardware-pushed by IRETQ
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl InterruptFrame {
    /// Create a new frame for a user-mode task
    pub fn new_user(entry_point: u64, user_stack_top: u64) -> Self {
        Self {
            r15: 0, r14: 0, r13: 0, r12: 0,
            r11: 0, r10: 0, r9: 0, r8: 0,
            rbp: 0, rdi: 0, rsi: 0, rdx: 0,
            rcx: 0, rbx: 0, rax: 0,
            rip: entry_point,
            cs: 0x1B,               // User code segment (GDT index 3, RPL=3)
            rflags: 0x202,          // IF=1 (interrupts enabled)
            rsp: user_stack_top,
            ss: 0x23,               // User data segment (GDT index 4, RPL=3)
        }
    }
}
