//! x86_64 code emitter for silverfir-nano.
//!
//! Translates WASM opcodes to x86_64 machine instructions.
//! Uses a single-pass compilation strategy (no IR, no optimization passes).
//!
//! Register allocation:
//!   RAX — accumulator / return value
//!   RBX — WASM stack pointer (points into operand stack)
//!   RCX — scratch / argument 1 (Windows x64 ABI)
//!   RDX — scratch / argument 2
//!   RSI — WASM linear memory base pointer
//!   RDI — host function table pointer
//!   RSP — native stack pointer (preserved)
//!   RBP — native frame pointer (preserved)
//!
//! Current status: SCAFFOLD — types defined, no code emission yet.

extern crate alloc;

use alloc::vec::Vec;

/// Raw x86_64 machine code buffer.
pub struct CodeBuffer {
    code: Vec<u8>,
}

impl CodeBuffer {
    pub fn new() -> Self {
        Self { code: Vec::with_capacity(4096) }
    }

    /// Emit raw bytes.
    pub fn emit(&mut self, bytes: &[u8]) {
        self.code.extend_from_slice(bytes);
    }

    /// Current code offset (for jump target calculation).
    pub fn offset(&self) -> usize {
        self.code.len()
    }

    /// Get the emitted code.
    pub fn code(&self) -> &[u8] {
        &self.code
    }

    // ── x86_64 instruction helpers ──────────────────────────────────

    /// push reg (1 byte: 0x50 + reg)
    pub fn push_reg(&mut self, reg: u8) { self.emit(&[0x50 + reg]); }

    /// pop reg (1 byte: 0x58 + reg)
    pub fn pop_reg(&mut self, reg: u8) { self.emit(&[0x58 + reg]); }

    /// mov reg, imm32 (5 bytes: 0xB8 + reg, imm32 LE)
    pub fn mov_reg_imm32(&mut self, reg: u8, val: u32) {
        self.emit(&[0xB8 + reg]);
        self.emit(&val.to_le_bytes());
    }

    /// add eax, reg (2 bytes: 0x01, 0xC0 + reg*8)
    pub fn add_eax_reg(&mut self, reg: u8) {
        self.emit(&[0x01, 0xC0 + reg * 8]);
    }

    /// sub eax, reg
    pub fn sub_eax_reg(&mut self, reg: u8) {
        self.emit(&[0x29, 0xC0 + reg * 8]);
    }

    /// ret (1 byte: 0xC3)
    pub fn ret(&mut self) { self.emit(&[0xC3]); }

    /// nop (1 byte: 0x90)
    pub fn nop(&mut self) { self.emit(&[0x90]); }
}

/// x86_64 register indices (lower 3 bits of ModR/M)
pub mod regs {
    pub const RAX: u8 = 0;
    pub const RCX: u8 = 1;
    pub const RDX: u8 = 2;
    pub const RBX: u8 = 3;
    pub const RSP: u8 = 4;
    pub const RBP: u8 = 5;
    pub const RSI: u8 = 6;
    pub const RDI: u8 = 7;
}
