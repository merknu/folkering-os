//! User Mode Test Program
//!
//! Simple user-space program to test syscall infrastructure.
//! Calls syscall_yield in a loop to verify user↔kernel transitions.

/// User-mode test program (x86-64 assembly)
///
/// This program:
/// 1. Calls syscall_yield (syscall #7)
/// 2. Increments a counter
/// 3. Loops forever
///
/// Embedded as raw bytes in kernel to avoid needing a separate user program binary.
#[repr(align(4096))]
pub struct UserProgram {
    pub code: [u8; 4096],
}

impl UserProgram {
    /// Get user program code
    ///
    /// Assembly:
    /// ```asm
    /// user_start:
    ///     mov rax, 7          ; syscall_yield
    ///     syscall
    ///     inc rbx             ; increment counter
    ///     jmp user_start      ; loop
    /// ```
    pub const fn new() -> Self {
        let mut code = [0u8; 4096];

        // mov rax, 7
        code[0] = 0x48;  // REX.W prefix
        code[1] = 0xc7;  // MOV r/m64, imm32
        code[2] = 0xc0;  // ModR/M: RAX
        code[3] = 0x07;  // Immediate: 7 (syscall_yield)
        code[4] = 0x00;
        code[5] = 0x00;
        code[6] = 0x00;

        // syscall
        code[7] = 0x0f;  // SYSCALL opcode
        code[8] = 0x05;

        // inc rbx
        code[9] = 0x48;   // REX.W prefix
        code[10] = 0xff;  // INC r/m64
        code[11] = 0xc3;  // ModR/M: RBX

        // jmp -14 (back to start)
        code[12] = 0xeb;  // JMP rel8
        code[13] = 0xf2;  // Offset: -14 bytes

        UserProgram { code }
    }

    /// Get entry point offset
    pub const fn entry_offset() -> usize {
        0
    }

    /// Get code size
    pub const fn code_size() -> usize {
        14 // 14 bytes of actual code
    }
}

/// Static user program instance
pub static USER_PROGRAM: UserProgram = UserProgram::new();

/// Load user program into memory at specified address
///
/// # Arguments
/// * `target_addr` - Virtual address to load program (must be page-aligned)
///
/// # Returns
/// Entry point address for the program
pub unsafe fn load_user_program(target_addr: u64) -> u64 {
    use core::ptr;

    // Copy code to target address
    ptr::copy_nonoverlapping(
        USER_PROGRAM.code.as_ptr(),
        target_addr as *mut u8,
        UserProgram::code_size(),
    );

    // Return entry point
    target_addr + UserProgram::entry_offset() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_program_size() {
        assert_eq!(UserProgram::code_size(), 14);
    }

    #[test]
    fn test_user_program_opcodes() {
        let prog = UserProgram::new();

        // Check mov rax, 7
        assert_eq!(prog.code[0], 0x48); // REX.W
        assert_eq!(prog.code[1], 0xc7); // MOV
        assert_eq!(prog.code[3], 0x07); // imm32 = 7

        // Check syscall
        assert_eq!(prog.code[7], 0x0f); // SYSCALL
        assert_eq!(prog.code[8], 0x05);

        // Check inc rbx
        assert_eq!(prog.code[9], 0x48);  // REX.W
        assert_eq!(prog.code[10], 0xff); // INC

        // Check jmp
        assert_eq!(prog.code[12], 0xeb); // JMP rel8
    }
}
