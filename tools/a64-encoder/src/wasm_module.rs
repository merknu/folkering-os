//! WebAssembly module parser.
//!
//! Reads the top-level binary format (magic + version + sections)
//! and extracts everything the JIT needs to compile a real module:
//!
//!   * Type section       — function signatures (params + results)
//!   * Import section     — **fail fast**: we reject modules that
//!     import anything, because Folkering's ABI is deliberately
//!     closed. Pure no_std math crates don't import anything.
//!   * Function section   — maps fn index → type index
//!   * Memory section     — noted but ignored; we use our own
//!     fixed 64 KiB linear memory
//!   * Global section     — variable-valued globals with init
//!     expressions; the lowerer places these in the top 256 B of
//!     linear memory (see `wasm_lower::types::GLOBAL_AREA_SIZE`)
//!   * Export section     — names for the entrypoints we may want
//!     to call; for now the daemon always runs fn 0
//!   * Data section       — initial linear-memory contents; the
//!     host prepends these to any DATA frame it sends
//!   * Code section       — function bodies
//!
//! Two entry points:
//!   * `parse_module` — legacy, returns `Vec<FunctionBody>`. Used
//!     by all existing examples, unchanged.
//!   * `parse_module_full` — new, returns a richer `Module` struct
//!     with all of the above. Used by Phase 1+ consumers.

use alloc::vec::Vec;
use alloc::string::String;

use crate::wasm_parse::{parse_ops, read_uleb128, read_sleb128, ParseError};
use crate::wasm_lower::WasmOp;

/// Parsed body of a single function — all of the information the
/// lowerer needs to emit machine code for it.
// Not `Eq` because `WasmOp::F32Const(f32)` only has `PartialEq`
// (f32 NaN semantics). Not meaningful for current test usage.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionBody {
    /// Total local count, counting each individual slot (groups are
    /// already expanded). Does NOT include function parameters —
    /// WASM stores those separately in the type signature.
    pub num_locals: u32,
    /// Per-local valtype bytes (one entry per slot, expanded from
    /// the local groups). 0x7F=i32, 0x7E=i64, 0x7D=f32, 0x7C=f64.
    /// Length matches `num_locals`. Useful when the lowerer needs
    /// to know whether a local is i32/f32/etc. — the legacy
    /// `num_locals`-only consumers can ignore this field.
    pub local_types: Vec<u8>,
    /// Ops in order, terminated by `WasmOp::End`.
    pub ops: Vec<WasmOp>,
}

/// A function signature from the Type section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncSig {
    pub params: Vec<u8>,   // valtype bytes: 0x7F=i32, 0x7E=i64, 0x7D=f32, 0x7C=f64
    pub results: Vec<u8>,
}

/// A global variable declared in the module's Global section.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalDef {
    /// Value type: 0x7F=i32, 0x7E=i64, 0x7D=f32, 0x7C=f64.
    pub valtype: u8,
    /// True if the global is mutable (`global.set` allowed).
    pub mutable: bool,
    /// Constant-evaluated init value. WASM allows imported-global
    /// references in init expressions but we fail-fast on imports,
    /// so every init is a simple const — store it as 8 little-endian
    /// bytes for i32/i64/f32/f64.
    pub init_bytes: [u8; 8],
}

/// A named export. `kind` is 0=func, 1=table, 2=mem, 3=global.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    pub kind: u8,
    pub index: u32,
}

/// A data segment from the Data section. Written into linear memory
/// at `offset` before any user code runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSegment {
    pub offset: u32,
    pub bytes: Vec<u8>,
}

/// Full module — everything the JIT needs to compile a multi-function
/// WASM binary. See [`parse_module_full`].
#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub types: Vec<FuncSig>,
    /// For each function, the index into `types` of its signature.
    /// Length = number of functions declared in Function section.
    /// This matches 1:1 with `bodies` for non-imported functions
    /// (we reject modules with imports, so it always does).
    pub func_types: Vec<u32>,
    pub globals: Vec<GlobalDef>,
    pub exports: Vec<Export>,
    pub data: Vec<DataSegment>,
    pub bodies: Vec<FunctionBody>,
}

/// Parse a WASM binary and return just the function bodies.
/// Preserved for backward compatibility with existing examples.
pub fn parse_module(bytes: &[u8]) -> Result<Vec<FunctionBody>, ParseError> {
    let m = parse_module_full(bytes)?;
    Ok(m.bodies)
}

/// Parse a WASM binary and return the full module structure.
pub fn parse_module_full(bytes: &[u8]) -> Result<Module, ParseError> {
    // ── Magic + version ──
    if bytes.len() < 8 { return Err(ParseError::UnexpectedEof); }
    if &bytes[0..4] != b"\0asm" { return Err(ParseError::UnknownOpcode(bytes[0])); }
    if &bytes[4..8] != [0x01, 0x00, 0x00, 0x00] {
        return Err(ParseError::UnknownOpcode(bytes[4]));
    }
    let mut pos = 8;

    let mut types = Vec::new();
    let mut func_types = Vec::new();
    let mut globals = Vec::new();
    let mut exports = Vec::new();
    let mut data = Vec::new();
    let mut bodies = Vec::new();

    while pos < bytes.len() {
        let section_id = bytes[pos];
        pos += 1;
        let size = read_uleb128(bytes, &mut pos)? as usize;
        let end = pos.checked_add(size).ok_or(ParseError::UnexpectedEof)?;
        if end > bytes.len() { return Err(ParseError::UnexpectedEof); }

        match section_id {
            0 => {
                // Custom section (name, producers, etc.) — skip.
            }
            1 => { types = parse_type_section(&bytes[pos..end])?; }
            2 => {
                // Import section — fail fast if non-empty.
                let n = {
                    let mut p = 0usize;
                    read_uleb128(&bytes[pos..end], &mut p)? as usize
                };
                if n > 0 {
                    return Err(ParseError::ImportsUnsupported);
                }
            }
            3 => { func_types = parse_function_section(&bytes[pos..end])?; }
            5 => {
                // Memory section — we use our own 64 KiB buffer, ignore.
            }
            6 => { globals = parse_global_section(&bytes[pos..end])?; }
            7 => { exports = parse_export_section(&bytes[pos..end])?; }
            10 => { bodies = parse_code_section(&bytes[pos..end])?; }
            11 => { data = parse_data_section(&bytes[pos..end])?; }
            _ => {
                // Unknown sections: skip. Real .wasm can have section
                // ids we don't recognise (e.g. "tag" section in
                // exception-handling proposal); walking past by size
                // is robust.
            }
        }

        pos = end;
    }

    if bodies.is_empty() {
        return Err(ParseError::UnexpectedEof);
    }
    Ok(Module { types, func_types, globals, exports, data, bodies })
}

// ── Section parsers ────────────────────────────────────────────────

fn parse_type_section(bytes: &[u8]) -> Result<Vec<FuncSig>, ParseError> {
    let mut pos = 0;
    let count = read_uleb128(bytes, &mut pos)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if pos >= bytes.len() { return Err(ParseError::UnexpectedEof); }
        // Every type must be a func type starting with 0x60.
        if bytes[pos] != 0x60 {
            return Err(ParseError::UnknownOpcode(bytes[pos]));
        }
        pos += 1;
        let params = read_valtype_vec(bytes, &mut pos)?;
        let results = read_valtype_vec(bytes, &mut pos)?;
        out.push(FuncSig { params, results });
    }
    Ok(out)
}

fn parse_function_section(bytes: &[u8]) -> Result<Vec<u32>, ParseError> {
    let mut pos = 0;
    let count = read_uleb128(bytes, &mut pos)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let ty = read_uleb128(bytes, &mut pos)?;
        if ty > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
        out.push(ty as u32);
    }
    Ok(out)
}

fn parse_global_section(bytes: &[u8]) -> Result<Vec<GlobalDef>, ParseError> {
    let mut pos = 0;
    let count = read_uleb128(bytes, &mut pos)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        // globaltype = valtype + mut byte (0 = const, 1 = mut)
        if pos + 2 > bytes.len() { return Err(ParseError::UnexpectedEof); }
        let valtype = bytes[pos];
        let mut_byte = bytes[pos + 1];
        pos += 2;
        let mutable = mut_byte == 0x01;
        // init expr: a sequence of ops ending in 0x0B (end).
        // For constant initializers we accept only the matching
        // const op for the valtype. Imports are rejected earlier so
        // `global.get $imported` can't appear.
        let init_bytes = read_const_init_expr(bytes, &mut pos, valtype)?;
        out.push(GlobalDef { valtype, mutable, init_bytes });
    }
    Ok(out)
}

fn parse_export_section(bytes: &[u8]) -> Result<Vec<Export>, ParseError> {
    let mut pos = 0;
    let count = read_uleb128(bytes, &mut pos)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let name_len = read_uleb128(bytes, &mut pos)? as usize;
        if pos + name_len > bytes.len() { return Err(ParseError::UnexpectedEof); }
        let name = core::str::from_utf8(&bytes[pos..pos + name_len])
            .map_err(|_| ParseError::InvalidUtf8)?
            .into();
        pos += name_len;
        if pos + 1 > bytes.len() { return Err(ParseError::UnexpectedEof); }
        let kind = bytes[pos];
        pos += 1;
        let index = read_uleb128(bytes, &mut pos)?;
        if index > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
        out.push(Export { name, kind, index: index as u32 });
    }
    Ok(out)
}

fn parse_data_section(bytes: &[u8]) -> Result<Vec<DataSegment>, ParseError> {
    let mut pos = 0;
    let count = read_uleb128(bytes, &mut pos)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        // WASM 1.0 active data-segment layout:
        //   flags (uleb128) — 0 for "active, memory 0"
        //   offset (init_expr)
        //   bytes (vec<u8>)
        let flags = read_uleb128(bytes, &mut pos)?;
        if flags != 0 {
            // We don't handle passive or multi-memory data yet.
            return Err(ParseError::UnknownOpcode(flags.min(0xFF) as u8));
        }
        // Offset is an init expr that evaluates to an i32.
        let init = read_const_init_expr(bytes, &mut pos, 0x7F)?;
        let offset = u32::from_le_bytes([init[0], init[1], init[2], init[3]]);
        let len = read_uleb128(bytes, &mut pos)? as usize;
        if pos + len > bytes.len() { return Err(ParseError::UnexpectedEof); }
        let seg_bytes = bytes[pos..pos + len].to_vec();
        pos += len;
        out.push(DataSegment { offset, bytes: seg_bytes });
    }
    Ok(out)
}

fn parse_code_section(bytes: &[u8]) -> Result<Vec<FunctionBody>, ParseError> {
    let mut pos = 0;
    let count = read_uleb128(bytes, &mut pos)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let entry_size = read_uleb128(bytes, &mut pos)? as usize;
        let entry_end = pos
            .checked_add(entry_size)
            .ok_or(ParseError::UnexpectedEof)?;
        if entry_end > bytes.len() { return Err(ParseError::UnexpectedEof); }
        out.push(parse_code_entry(&bytes[pos..entry_end])?);
        pos = entry_end;
    }
    Ok(out)
}

fn parse_code_entry(bytes: &[u8]) -> Result<FunctionBody, ParseError> {
    let mut pos = 0;
    let group_count = read_uleb128(bytes, &mut pos)? as usize;
    let mut total_locals: u32 = 0;
    let mut local_types: Vec<u8> = Vec::new();
    for _ in 0..group_count {
        let n = read_uleb128(bytes, &mut pos)?;
        if n > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
        if pos >= bytes.len() { return Err(ParseError::UnexpectedEof); }
        let valtype = bytes[pos];
        pos += 1;
        for _ in 0..n {
            local_types.push(valtype);
        }
        total_locals = total_locals.saturating_add(n as u32);
    }
    let ops = parse_ops(bytes, &mut pos)?;
    Ok(FunctionBody { num_locals: total_locals, local_types, ops })
}

// ── Helpers ────────────────────────────────────────────────────────

fn read_valtype_vec(bytes: &[u8], pos: &mut usize) -> Result<Vec<u8>, ParseError> {
    let n = read_uleb128(bytes, pos)? as usize;
    if *pos + n > bytes.len() { return Err(ParseError::UnexpectedEof); }
    let out = bytes[*pos..*pos + n].to_vec();
    *pos += n;
    Ok(out)
}

/// Read a constant init expression from `bytes` starting at `pos`.
/// Accepts only the single matching `<type>.const <val>` op followed
/// by `end` (0x0B). Returns the value as 8 little-endian bytes
/// (i32/f32 zero-padded in the upper 4 bytes).
///
/// `expected_valtype` — 0x7F i32, 0x7E i64, 0x7D f32, 0x7C f64.
fn read_const_init_expr(
    bytes: &[u8],
    pos: &mut usize,
    expected_valtype: u8,
) -> Result<[u8; 8], ParseError> {
    if *pos >= bytes.len() { return Err(ParseError::UnexpectedEof); }
    let opcode = bytes[*pos];
    *pos += 1;
    let mut out = [0u8; 8];
    match (opcode, expected_valtype) {
        (0x41, 0x7F) => {
            // i32.const
            let v = read_sleb128(bytes, pos)?;
            if !(i32::MIN as i64..=i32::MAX as i64).contains(&v) {
                return Err(ParseError::ConstantOutOfRange);
            }
            out[..4].copy_from_slice(&(v as i32).to_le_bytes());
        }
        (0x42, 0x7E) => {
            // i64.const
            let v = read_sleb128(bytes, pos)?;
            out.copy_from_slice(&v.to_le_bytes());
        }
        (0x43, 0x7D) => {
            // f32.const — 4 raw bytes LE
            if *pos + 4 > bytes.len() { return Err(ParseError::UnexpectedEof); }
            out[..4].copy_from_slice(&bytes[*pos..*pos + 4]);
            *pos += 4;
        }
        (0x44, 0x7C) => {
            // f64.const — 8 raw bytes LE
            if *pos + 8 > bytes.len() { return Err(ParseError::UnexpectedEof); }
            out.copy_from_slice(&bytes[*pos..*pos + 8]);
            *pos += 8;
        }
        _ => return Err(ParseError::UnknownOpcode(opcode)),
    }
    // Expect `end` (0x0B) to terminate the init expression.
    if *pos >= bytes.len() || bytes[*pos] != 0x0B {
        return Err(ParseError::UnknownOpcode(
            bytes.get(*pos).copied().unwrap_or(0),
        ));
    }
    *pos += 1;
    Ok(out)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Minimal "return 42" WASM module, hand-assembled.
    const RETURN_42_WASM: &[u8] = &[
        0x00, 0x61, 0x73, 0x6D,
        0x01, 0x00, 0x00, 0x00,
        0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F,
        0x03, 0x02, 0x01, 0x00,
        0x0A, 0x06, 0x01, 0x04, 0x00, 0x41, 0x2A, 0x0B,
    ];

    #[test]
    fn parse_return_42_module() {
        let bodies = parse_module(RETURN_42_WASM).expect("parse");
        assert_eq!(bodies.len(), 1);
        let f = &bodies[0];
        assert_eq!(f.num_locals, 0);
        assert_eq!(f.ops, vec![WasmOp::I32Const(42), WasmOp::End]);
    }

    #[test]
    fn parse_return_42_full() {
        let m = parse_module_full(RETURN_42_WASM).expect("parse");
        assert_eq!(m.bodies.len(), 1);
        assert_eq!(m.types.len(), 1);
        assert_eq!(m.types[0].params, vec![]);
        assert_eq!(m.types[0].results, vec![0x7F]); // i32
        assert_eq!(m.func_types, vec![0]);
        assert!(m.globals.is_empty());
        assert!(m.exports.is_empty());
        assert!(m.data.is_empty());
    }

    #[test]
    fn rejects_non_wasm_magic() {
        let bytes = [0x7F, 0x45, 0x4C, 0x46, 0, 0, 0, 0];
        assert!(parse_module(&bytes).is_err());
    }

    #[test]
    fn rejects_bad_version() {
        let mut bytes = RETURN_42_WASM.to_vec();
        bytes[4] = 2;
        assert!(parse_module(&bytes).is_err());
    }

    #[test]
    fn rejects_imports() {
        // Minimal WASM with a single import: env.foo (func, type 0).
        //   type section: 1 type (func () -> i32)
        //   import section: 1 entry: "env" "foo" kind=0 type=0
        //   code section: dummy (needed so parse doesn't error for other reasons,
        //                  but actually it'll fail at Import before reaching it).
        let module: &[u8] = &[
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
            // type: 1 func () -> i32
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7F,
            // import: 1 entry: env.foo, kind=func, type=0
            0x02, 0x0B, 0x01,
                0x03, b'e', b'n', b'v',
                0x03, b'f', b'o', b'o',
                0x00, 0x00,
        ];
        let err = parse_module_full(module).unwrap_err();
        assert!(matches!(err, ParseError::ImportsUnsupported));
    }

    #[test]
    fn parses_globals() {
        // 1 global: mut i32 = 0xFF00
        //   valtype=0x7F, mut=0x01, init: i32.const 0xFF00 (sleb128: 0x80 0xFE 0x03), end
        //   sleb128(0xFF00) = 65280 = 0b1111_1111_0000_0000
        //     7-bit groups: 0000 0000, 1111 111, 1 (with continuation bits)
        //     = 0x80, 0xFE, 0x03  — let's compute: 0xFF00 = 65280
        //       first byte: 0x00 | 0x80 (cont) = 0x80
        //       second:     0xFE | 0x80 = 0xFE — wait, 0xFF00 >> 7 = 0x01FE, low 7 bits = 0x7E, or ...
        //   Easier: use explicit test-only sleb128 encoder below.
        let mut init_bytes = alloc::vec![0x41]; // i32.const
        encode_sleb128(&mut init_bytes, 0xFF00);
        init_bytes.push(0x0B); // end
        let global_section_body = alloc::vec![
            0x01, // 1 global
            0x7F, 0x01, // mut i32
        ].iter().copied()
            .chain(init_bytes.iter().copied())
            .collect::<Vec<u8>>();

        let mut module = alloc::vec![
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
            // Minimal function: type section (no types), function section (no fns),
            // global section, code section (no bodies) — but we need ≥1 body for parse_module_full.
            // Add a dummy fn.
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type: () -> ()
            0x03, 0x02, 0x01, 0x00,              // function: 1 fn of type 0
            // global section
            0x06,
        ];
        module.push(global_section_body.len() as u8);
        module.extend_from_slice(&global_section_body);
        // dummy code: 1 entry size 2: 0 locals, `end`
        module.extend_from_slice(&[0x0A, 0x04, 0x01, 0x02, 0x00, 0x0B]);

        let m = parse_module_full(&module).expect("parse");
        assert_eq!(m.globals.len(), 1);
        assert_eq!(m.globals[0].valtype, 0x7F);
        assert!(m.globals[0].mutable);
        let init_val = i32::from_le_bytes(
            m.globals[0].init_bytes[..4].try_into().unwrap()
        );
        assert_eq!(init_val, 0xFF00);
    }

    /// Test helper — sleb128 encoder. Not used in production.
    fn encode_sleb128(out: &mut Vec<u8>, mut v: i64) {
        loop {
            let byte = (v & 0x7F) as u8;
            v >>= 7;
            let sign_bit = byte & 0x40;
            let done = (v == 0 && sign_bit == 0) || (v == -1 && sign_bit != 0);
            if done {
                out.push(byte);
                return;
            } else {
                out.push(byte | 0x80);
            }
        }
    }

    /// Module with one local group of 2 i32s, body `local.get 0 ; end`.
    #[test]
    fn parses_locals_declaration() {
        let module: &[u8] = &[
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
            0x0A, 0x08, 0x01, 0x06, 0x01, 0x02, 0x7F, 0x20, 0x00, 0x0B,
        ];
        let bodies = parse_module(module).unwrap();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0].num_locals, 2);
        assert_eq!(bodies[0].ops, vec![WasmOp::LocalGet(0), WasmOp::End]);
    }

    #[test]
    fn skips_unknown_sections() {
        let module: &[u8] = &[
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x04, 0x02, 0x78, 0x79, 0x00,
            0x0A, 0x06, 0x01, 0x04, 0x00, 0x41, 0x2A, 0x0B,
        ];
        let bodies = parse_module(module).unwrap();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0].ops, vec![WasmOp::I32Const(42), WasmOp::End]);
    }
}
