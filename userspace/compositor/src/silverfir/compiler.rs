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
//! Code emission happens in translate.rs which uses CodeBuffer.emit().

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

    /// Get the emitted code (immutable).
    pub fn code(&self) -> &[u8] {
        &self.code
    }

    /// Get the emitted code (mutable, for jump patching).
    pub fn code_mut(&mut self) -> &mut [u8] {
        &mut self.code
    }

    /// Consume the buffer and return the code bytes.
    pub fn into_code(self) -> Vec<u8> {
        self.code
    }

}
