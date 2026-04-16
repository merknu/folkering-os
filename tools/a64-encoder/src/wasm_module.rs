//! Minimal WebAssembly module parser.
//!
//! Reads the top-level binary format (magic + version + sections)
//! and extracts enough to feed `wasm_lower::Lowerer` a function
//! body — specifically, the Code section's function bodies with
//! their local declarations.
//!
//! Scope is deliberately aligned with what the JIT can lower:
//!
//! - Magic `\0asm` + version `01 00 00 00`
//! - Section walking (we *skip* unknown/unsupported sections rather
//!   than erroring, so real `.wasm` output from `wat2wasm` or
//!   `rustc --target wasm32*` works when it only exercises the ops
//!   the lowerer supports)
//! - Type and Function sections are inspected just enough to count
//!   signatures; we don't type-check.
//! - Code section: parses `vec<CodeEntry>`, where each entry has a
//!   size-prefixed body containing local-group declarations plus
//!   the raw op byte stream. The op stream is re-parsed with
//!   [`crate::wasm_parse::parse_ops`] into a `Vec<WasmOp>`.
//!
//! Out of scope for Phase 7:
//!
//! - Imports, tables, memory size, globals, exports (we ignore the
//!   sections but skip past them).
//! - Data and Element sections.
//! - Multi-byte SIMD / reference-type opcodes.

use crate::wasm_parse::{parse_ops, read_uleb128, ParseError};
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
    /// Ops in order, terminated by `WasmOp::End`.
    pub ops: Vec<WasmOp>,
}

/// Parse a full WebAssembly binary. Returns the bodies from the
/// Code section in the order they appeared. Other sections are
/// validated enough to skip past but not otherwise interpreted.
pub fn parse_module(bytes: &[u8]) -> Result<Vec<FunctionBody>, ParseError> {
    // ── Magic + version ──
    // \0 a s m — four bytes. Anything else is "not a WASM binary".
    if bytes.len() < 8 { return Err(ParseError::UnexpectedEof); }
    if &bytes[0..4] != b"\0asm" { return Err(ParseError::UnknownOpcode(bytes[0])); }
    // Version must be 1. We treat anything else as unsupported.
    if &bytes[4..8] != [0x01, 0x00, 0x00, 0x00] {
        return Err(ParseError::UnknownOpcode(bytes[4]));
    }
    let mut pos = 8;

    let mut code_bodies: Option<Vec<FunctionBody>> = None;

    while pos < bytes.len() {
        let section_id = bytes[pos];
        pos += 1;
        let size = read_uleb128(bytes, &mut pos)? as usize;
        let end = pos.checked_add(size).ok_or(ParseError::UnexpectedEof)?;
        if end > bytes.len() { return Err(ParseError::UnexpectedEof); }

        if section_id == 10 {
            // Code section.
            code_bodies = Some(parse_code_section(&bytes[pos..end])?);
        }
        // Other sections: skip. A stricter parser would at least
        // walk the Type + Function sections to cross-reference, but
        // for our JIT the Code section is the source of truth.

        pos = end;
    }

    code_bodies.ok_or(ParseError::UnexpectedEof)
}

/// Parse just the Code section's contents (the bytes after the
/// section id + size prefix).
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

/// A single Code entry: `vec<local_group>` + op bytes ending in 0x0B.
///
/// Each local group is `(count: uleb128, valtype: u8)`. We count
/// total locals by summing all `count`s — the valtype byte is
/// ignored because our lowerer only handles i32 for now.
fn parse_code_entry(bytes: &[u8]) -> Result<FunctionBody, ParseError> {
    let mut pos = 0;
    let group_count = read_uleb128(bytes, &mut pos)? as usize;
    let mut total_locals: u32 = 0;
    for _ in 0..group_count {
        let n = read_uleb128(bytes, &mut pos)?;
        if n > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
        // Skip valtype byte. Real WASM has 0x7F = i32, 0x7E = i64, etc.
        if pos >= bytes.len() { return Err(ParseError::UnexpectedEof); }
        pos += 1;
        total_locals = total_locals.saturating_add(n as u32);
    }
    let ops = parse_ops(bytes, &mut pos)?;
    Ok(FunctionBody { num_locals: total_locals, ops })
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal "return 42" WASM module, hand-assembled.
    ///
    /// Structure:
    ///   header: 00 61 73 6D 01 00 00 00
    ///   type section: id=1 size=5 | 1 type: func () -> i32 (60 00 01 7F)
    ///   function section: id=3 size=2 | 1 function, type idx 0 (01 00)
    ///   code section: id=0A size=6 | 1 entry, size=4, 0 local groups, body (00 41 2A 0B)
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
    fn rejects_non_wasm_magic() {
        let bytes = [0x7F, 0x45, 0x4C, 0x46, 0, 0, 0, 0]; // ELF magic
        assert!(parse_module(&bytes).is_err());
    }

    #[test]
    fn rejects_bad_version() {
        let mut bytes = RETURN_42_WASM.to_vec();
        bytes[4] = 2;
        assert!(parse_module(&bytes).is_err());
    }

    /// Module with one local group of 2 i32s, body `local.get 0 ; end`.
    /// Proves local-group count is aggregated correctly.
    #[test]
    fn parses_locals_declaration() {
        // Code entry layout:
        //   size       0x07  (7 bytes below follow)
        //   groups=1   0x01
        //   count=2    0x02
        //   valtype    0x7F  (i32)
        //   local.get 0: 0x20 0x00
        //   end        0x0B
        //
        // Total body including the leading group-count: 6 bytes, but
        // the size prefix is the SIZE of the entry, which includes
        // the group-count byte and everything up to (including) end.
        // That's "01 02 7F 20 00 0B" = 6 bytes. So entry_size=6.
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
        // A known WASM with a custom section (id=0) in the middle.
        // Custom sections carry a name+payload; we just want to
        // prove the parser walks past by section-size and still
        // finds the Code section.
        //   header
        //   custom id=0 size=4 name_len=2 "xy" pad=0
        //   code section same as before
        let module: &[u8] = &[
            0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00,
            // custom section: id=0, size=4, contents "02 78 79 00" (name_len=2, "xy", one byte)
            0x00, 0x04, 0x02, 0x78, 0x79, 0x00,
            // code section
            0x0A, 0x06, 0x01, 0x04, 0x00, 0x41, 0x2A, 0x0B,
        ];
        let bodies = parse_module(module).unwrap();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0].ops, vec![WasmOp::I32Const(42), WasmOp::End]);
    }
}
