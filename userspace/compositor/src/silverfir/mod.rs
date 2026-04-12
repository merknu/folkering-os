//! Silverfir-nano — Lightweight WASM JIT for Folkering OS
//!
//! Compiles WebAssembly bytecode directly to x86_64 machine code for
//! near-native execution speed. Used for trusted internal modules
//! (hardware drivers, Draug-verified code, inference kernels).
//!
//! Architecture:
//!   1. Parse WASM binary (function signatures, types, code sections)
//!   2. For each WASM function: translate opcodes → x86_64 instructions
//!   3. Emit machine code into an executable memory region (mmap RWX)
//!   4. Resolve host function imports via a trampoline table
//!   5. Call the compiled `run` export as a native function pointer
//!
//! Current status: SCAFFOLD. Core types defined, JIT not yet emitting code.
//! Falls back to wasmi when called. The API is stable for integration.
//!
//! ## Safety
//!
//! The JIT generates and executes native code. Bugs here can cause
//! segfaults, not just WASM traps. Only use for TRUSTED code that has
//! passed the 3-layer validation pipeline (ground-truth + tautology +
//! mutation testing).

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

pub mod compiler;

/// A compiled WASM module ready for native execution.
pub struct JitModule {
    /// Exported function names → native function pointers
    exports: Vec<(String, usize)>,
    /// Raw machine code (will be in RWX memory when JIT is active)
    _code: Vec<u8>,
    /// Linear memory (4MB default, growable)
    pub memory: Vec<u8>,
}

/// Result of JIT compilation
pub enum JitError {
    /// WASM binary is malformed
    ParseError(String),
    /// Unsupported WASM opcode encountered
    UnsupportedOpcode(u8),
    /// Memory allocation failed
    MemoryError,
    /// JIT not yet implemented — use wasmi fallback
    NotYetImplemented,
}

impl JitModule {
    /// Compile a WASM binary to native x86_64 code.
    ///
    /// Returns `Err(JitError::NotYetImplemented)` until the JIT
    /// compiler is functional. The WasmBackend::Trusted path catches
    /// this and falls back to wasmi.
    pub fn compile(_wasm_bytes: &[u8]) -> Result<Self, JitError> {
        // Phase 1: scaffold — return NotYetImplemented
        // Phase 2: parse WASM sections (type, import, function, code)
        // Phase 3: translate basic opcodes (i32.add, i32.sub, local.get/set, call)
        // Phase 4: emit x86_64 machine code (MOV, ADD, SUB, CALL, RET)
        // Phase 5: handle memory loads/stores with bounds checking
        // Phase 6: host function trampolines
        Err(JitError::NotYetImplemented)
    }

    /// Call an exported function by name.
    pub fn call_void(&mut self, _name: &str) -> Result<(), JitError> {
        Err(JitError::NotYetImplemented)
    }

    /// Get a mutable reference to linear memory.
    pub fn memory_mut(&mut self) -> &mut [u8] {
        &mut self.memory
    }
}
