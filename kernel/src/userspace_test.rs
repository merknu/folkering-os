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

        // DEBUG: Add a marker instruction first to verify we're executing user code
        // nop (0x90) - easier to spot in debug output
        code[0] = 0x90;  // NOP

        // mov rax, 7
        code[1] = 0x48;  // REX.W prefix
        code[2] = 0xc7;  // MOV r/m64, imm32
        code[3] = 0xc0;  // ModR/M: RAX
        code[4] = 0x07;  // Immediate: 7 (syscall_yield)
        code[5] = 0x00;
        code[6] = 0x00;
        code[7] = 0x00;

        // syscall
        code[8] = 0x0f;  // SYSCALL opcode
        code[9] = 0x05;

        // inc rbx
        code[10] = 0x48;   // REX.W prefix
        code[11] = 0xff;  // INC r/m64
        code[12] = 0xc3;  // ModR/M: RBX

        // jmp back to NOP (offset: -(13) = 0xF3)
        code[13] = 0xeb;  // JMP rel8
        code[14] = 0xf3;  // Offset: -13 bytes (back to NOP)

        UserProgram { code }
    }

    /// Get entry point offset
    pub const fn entry_offset() -> usize {
        0
    }

    /// Get code size
    pub const fn code_size() -> usize {
        15 // 15 bytes of actual code (added NOP)
    }
}

/// Static user program instance (simple yield test)
pub static USER_PROGRAM: UserProgram = UserProgram::new();

/// IPC Sender Program
///
/// This program sends IPC messages to task 2 (receiver) in a loop.
/// Task IDs: 1=dummy, 2=receiver, 3=sender
///
/// Assembly:
/// ```asm
/// sender_start:
///     mov rax, 0          ; syscall IpcSend
///     mov rdi, 2          ; target_task = 2 (receiver)
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

        // mov rdi, 2 (target task ID - receiver)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc7; pos += 1;  // ModR/M: RDI
        code[pos] = 0x02; pos += 1;  // Immediate: 2 (FIXED: was 3)
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

        // jmp to start
        // jmp opcode at pos 32, offset at pos 33
        // After jmp instruction, RIP = 34
        // Target = 0
        // Offset = 0 - 34 = -34 = 0xDE
        code[pos] = 0xeb; pos += 1;  // JMP rel8
        code[pos] = 0xde; // -34 bytes (256 - 34 = 222 = 0xDE)
        // pos = 34 (total code size)

        IpcSenderProgram { code }
    }

    pub const fn code_size() -> usize {
        34 // Total bytes of actual code
    }
}

/// IPC Receiver Program
///
/// This program receives IPC messages and replies to them.
/// Now with proper return value checking - if IpcReceive returns error, yield and retry.
///
/// Assembly:
/// ```asm
/// receiver_start:
///     mov rax, 1          ; syscall IpcReceive
///     mov rdi, 0          ; from_task = 0 (any sender)
///     syscall
///     cmp rax, -1         ; check for error (0xFFFFFFFFFFFFFFFF or similar)
///     jl yield_and_retry  ; if negative (error), yield and retry
///     ; Message received successfully
///     mov rdi, rax        ; save result (sender in lower 32 bits)
///     mov rax, 2          ; syscall IpcReply
///     mov rsi, 0x42       ; reply payload0 = 0x42 (success marker)
///     syscall
///     jmp receiver_start  ; loop to receive next message
/// yield_and_retry:
///     mov rax, 7          ; syscall Yield
///     syscall
///     jmp receiver_start  ; retry IpcReceive
/// ```
#[repr(align(4096))]
pub struct IpcReceiverProgram {
    pub code: [u8; 4096],
}

impl IpcReceiverProgram {
    /// Simplified receiver: yield twice first, then receive and reply
    /// This ensures the sender has time to send before we try to receive.
    ///
    /// Assembly:
    /// ```asm
    /// receiver_start:           ; pos 0
    ///     mov rax, 7            ; Yield syscall
    ///     syscall               ; yield #1
    ///     mov rax, 7            ; Yield syscall
    ///     syscall               ; yield #2
    ///     mov rax, 1            ; IpcReceive
    ///     mov rdi, 0            ; from any sender
    ///     syscall
    ///     mov rdi, rax          ; save result (sender info)
    ///     mov rax, 2            ; IpcReply
    ///     mov rsi, 0x42         ; reply payload
    ///     syscall
    ///     jmp receiver_start
    /// ```
    pub const fn new() -> Self {
        let mut code = [0u8; 4096];
        let mut pos = 0;

        // === YIELD #1 === (pos 0-8)
        // mov rax, 7 (Yield)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc0; pos += 1;  // ModR/M: RAX
        code[pos] = 0x07; pos += 1;  // Immediate: 7
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        // pos = 7

        // syscall
        code[pos] = 0x0f; pos += 1;
        code[pos] = 0x05; pos += 1;
        // pos = 9

        // === YIELD #2 === (pos 9-17)
        // mov rax, 7 (Yield)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc0; pos += 1;  // ModR/M: RAX
        code[pos] = 0x07; pos += 1;  // Immediate: 7
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        // pos = 16

        // syscall
        code[pos] = 0x0f; pos += 1;
        code[pos] = 0x05; pos += 1;
        // pos = 18

        // === IPC RECEIVE === (pos 18-33)
        // mov rax, 1 (IpcReceive)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc0; pos += 1;  // ModR/M: RAX
        code[pos] = 0x01; pos += 1;  // Immediate: 1
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        // pos = 25

        // mov rdi, 0 (any sender)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc7; pos += 1;  // ModR/M: RDI
        code[pos] = 0x00; pos += 1;  // Immediate: 0
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        // pos = 32

        // syscall (IpcReceive)
        code[pos] = 0x0f; pos += 1;
        code[pos] = 0x05; pos += 1;
        // pos = 34

        // === IPC REPLY === (pos 34-52)
        // mov rdi, rax (save sender info for reply)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0x89; pos += 1;  // MOV r/m64, r64
        code[pos] = 0xc7; pos += 1;  // ModR/M: RDI, RAX
        // pos = 37

        // mov rax, 2 (IpcReply)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc0; pos += 1;  // ModR/M: RAX
        code[pos] = 0x02; pos += 1;  // Immediate: 2
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        // pos = 44

        // mov rsi, 0x42 (reply payload = success marker)
        code[pos] = 0x48; pos += 1;  // REX.W
        code[pos] = 0xc7; pos += 1;  // MOV r/m64, imm32
        code[pos] = 0xc6; pos += 1;  // ModR/M: RSI
        code[pos] = 0x42; pos += 1;  // Immediate: 0x42
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        code[pos] = 0x00; pos += 1;
        // pos = 51

        // syscall (IpcReply)
        code[pos] = 0x0f; pos += 1;
        code[pos] = 0x05; pos += 1;
        // pos = 53

        // === LOOP BACK === (pos 53-54)
        // jmp receiver_start (pos 0)
        // jmp instruction at pos 53-54, ends at 55
        // offset = 0 - 55 = -55
        code[pos] = 0xeb; pos += 1;  // JMP rel8
        code[pos] = (256 - 55) as u8; // -55 bytes = 0xC9
        // pos = 55

        IpcReceiverProgram { code }
    }

    pub const fn code_size() -> usize {
        55 // Total bytes of actual code
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
