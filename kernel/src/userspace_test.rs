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

/// Static user program instance (simple yield test)
pub static USER_PROGRAM: UserProgram = UserProgram::new();

/// IPC Sender Program
///
/// This program sends IPC messages to task 3 (receiver) in a loop.
///
/// Assembly:
/// ```asm
/// sender_start:
///     mov rax, 0          ; syscall IpcSend
///     mov rdi, 3          ; target_task = 3 (receiver)
///     mov rsi, 0x1234     ; payload[0] = test data
///     syscall
///     mov rax, 7          ; syscall Yield
///     syscall
///     jmp sender_start    ; loop
/// ```
#[repr(align(4096))]
pub struct IpcSenderProgram {
    pub code: [u8; 4096],
}

impl IpcSenderProgram {
    pub const fn new() -> Self {
        let mut code = [0u8; 4096];
        let mut pos = 0;

        // mov rax, 0 (IpcSend)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc0; pos += 1;  // ModR/M: RAX
        code[pos] = 0x00; pos += 1;  // Immediate: 0
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;

        // mov rdi, 3 (target task ID)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc7; pos += 1;  // ModR/M: RDI
        code[pos] = 0x03; pos += 1;  // Immediate: 3
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;

        // mov rsi, 0x1234 (payload data)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc6; pos += 1;  // ModR/M: RSI
        code[pos] = 0x34; pos += 1;  // Immediate: 0x1234
        code[pos] = 0x12; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;

        // syscall
        code[pos] = 0x0f; pos += 1;
        code[pos] = 0x05; pos += 1;

        // mov rax, 7 (Yield)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc0; pos += 1;  // ModR/M: RAX
        code[pos] = 0x07; pos += 1;  // Immediate: 7
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;

        // syscall
        code[pos] = 0x0f; pos += 1;
        code[pos] = 0x05; pos += 1;

        // jmp to start (offset calculation: -(pos+2))
        // Current pos is 37, so offset is -(37+2) = -39 = 0xD9
        code[pos] = 0xeb; pos += 1;  // JMP rel8
        code[pos] = 0xd9; // -39 bytes

        IpcSenderProgram { code }
    }

    pub const fn code_size() -> usize {
        39 // Total bytes of actual code
    }
}

/// IPC Receiver Program
///
/// This program receives IPC messages and replies to them.
///
/// Assembly:
/// ```asm
/// receiver_start:
///     mov rax, 1          ; syscall IpcReceive
///     mov rdi, 0          ; from_task = 0 (any sender)
///     syscall
///     mov rax, 2          ; syscall IpcReply
///     syscall
///     mov rax, 7          ; syscall Yield
///     syscall
///     jmp receiver_start  ; loop
/// ```
#[repr(align(4096))]
pub struct IpcReceiverProgram {
    pub code: [u8; 4096],
}

impl IpcReceiverProgram {
    pub const fn new() -> Self {
        let mut code = [0u8; 4096];
        let mut pos = 0;

        // mov rax, 1 (IpcReceive)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc0; pos += 1;  // ModR/M: RAX
        code[pos] = 0x01; pos += 1;  // Immediate: 1
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;

        // mov rdi, 0 (any sender)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc7; pos += 1;  // ModR/M: RDI
        code[pos] = 0x00; pos += 1;  // Immediate: 0
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;

        // syscall (IpcReceive)
        code[pos] = 0x0f; pos += 1;
        code[pos] = 0x05; pos += 1;

        // mov rax, 2 (IpcReply)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc0; pos += 1;  // ModR/M: RAX
        code[pos] = 0x02; pos += 1;  // Immediate: 2
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;

        // syscall (IpcReply)
        code[pos] = 0x0f; pos += 1;
        code[pos] = 0x05; pos += 1;

        // mov rax, 7 (Yield)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc0; pos += 1;  // ModR/M: RAX
        code[pos] = 0x07; pos += 1;  // Immediate: 7
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;

        // syscall (Yield)
        code[pos] = 0x0f; pos += 1;
        code[pos] = 0x05; pos += 1;

        // jmp to start (offset: -(pos+2) = -(35+2) = -37 = 0xDB)
        code[pos] = 0xeb; pos += 1;  // JMP rel8
        code[pos] = 0xdb; // -37 bytes

        IpcReceiverProgram { code }
    }

    pub const fn code_size() -> usize {
        37 // Total bytes of actual code
    }
}

/// Static IPC program instances
pub static IPC_SENDER: IpcSenderProgram = IpcSenderProgram::new();
pub static IPC_RECEIVER: IpcReceiverProgram = IpcReceiverProgram::new();

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
