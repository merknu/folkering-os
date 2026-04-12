//! Silverfir-nano — Lightweight WASM JIT for Folkering OS
//!
//! Compiles WebAssembly bytecode to x86_64 machine code via W^X memory.
//! Used for trusted internal modules (hardware drivers, Draug-verified code).
//!
//! Supported opcodes: i32 arithmetic, comparisons, locals, control flow
//! (block/loop/if/else/br/br_if), call, return. Enough for fib, gcd, etc.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

pub mod compiler;
pub mod jit_memory;
pub mod parser;
pub mod translate;

/// A compiled WASM module ready for native execution.
pub struct JitModule {
    /// Compiled function code blocks (one per function)
    functions: Vec<Vec<u8>>,
    /// Export name → function index
    exports: Vec<(String, usize)>,
    /// W^X memory block holding executable code
    code_block: Option<jit_memory::JitMemoryBlock>,
    /// Offset of each function within the code block
    func_offsets: Vec<usize>,
    /// Linear memory (64KB default)
    pub memory: Vec<u8>,
}

/// JIT compilation/execution error
pub enum JitError {
    ParseError(String),
    UnsupportedOpcode(u8),
    MemoryError,
    NotYetImplemented,
    ExportNotFound,
    ExecutionFault,
}

impl JitModule {
    /// Compile a WASM binary to native x86_64 code.
    pub fn compile(wasm_bytes: &[u8]) -> Result<Self, JitError> {
        // Parse WASM binary
        let module = parser::WasmModule::parse(wasm_bytes)
            .map_err(|_| JitError::ParseError(String::from("WASM parse failed")))?;

        // Translate each function body to x86_64
        let mut functions = Vec::new();
        for (i, body) in module.code_bodies.iter().enumerate() {
            let type_idx = *module.func_type_indices.get(i)
                .ok_or(JitError::ParseError(String::from("func/type mismatch")))? as usize;
            let func_type = module.types.get(type_idx)
                .ok_or(JitError::ParseError(String::from("type index OOB")))?;

            let code = translate::translate_function(
                body,
                func_type.params.len(),
                !func_type.results.is_empty(),
            )?;
            functions.push(code);
        }

        // Calculate total code size and lay out functions
        let total_size: usize = functions.iter().map(|f| f.len()).sum();
        if total_size == 0 {
            return Err(JitError::ParseError(String::from("no code to compile")));
        }

        // Allocate W^X memory
        let mut code_block = jit_memory::JitMemoryBlock::alloc(total_size)
            .map_err(|_| JitError::MemoryError)?;

        // Write all function code into the block
        let mut func_offsets = Vec::new();
        let mut offset = 0;
        {
            let mem = code_block.as_mut_slice();
            for func_code in &functions {
                func_offsets.push(offset);
                mem[offset..offset + func_code.len()].copy_from_slice(func_code);
                offset += func_code.len();
            }
        }

        // Flip to executable (W^X: no longer writable)
        code_block.make_executable()
            .map_err(|_| JitError::MemoryError)?;

        // Build export table
        let mut exports = Vec::new();
        for exp in &module.exports {
            if exp.kind == 0 { // function export
                let local_idx = exp.index.saturating_sub(module.num_imports) as usize;
                exports.push((exp.name.clone(), local_idx));
            }
        }

        libfolk::sys::io::write_str("[silverfir] JIT compiled ");
        libfolk::sys::io::write_str(&alloc::format!("{} functions, {} bytes\n",
            functions.len(), total_size));

        Ok(Self {
            functions,
            exports,
            code_block: Some(code_block),
            func_offsets,
            memory: alloc::vec![0u8; 65536], // 64KB linear memory
        })
    }

    /// Call an exported void→void function by name.
    pub fn call_void(&mut self, name: &str) -> Result<(), JitError> {
        // Find export
        let func_idx = self.exports.iter()
            .find(|(n, _)| n == name)
            .map(|(_, idx)| *idx)
            .ok_or(JitError::ExportNotFound)?;

        let offset = *self.func_offsets.get(func_idx)
            .ok_or(JitError::ExportNotFound)?;

        let block = self.code_block.as_ref()
            .ok_or(JitError::MemoryError)?;

        unsafe {
            block.call_void(offset)
                .map_err(|_| JitError::ExecutionFault)?;
        }

        Ok(())
    }
}
