//! Minimal WASM function-body parser.
//!
//! Consumes the opcode bytes of a WebAssembly function body (the
//! inside of a Code section entry, *after* the local decl header)
//! and produces a `Vec<WasmOp>` that the lowerer understands.
//!
//! Scope is deliberately aligned with what the lowerer supports —
//! it's not a full WASM parser:
//!
//! | Opcode | Mnemonic      | Payload                  |
//! |--------|---------------|--------------------------|
//! | 0x02   | block         | 1-byte block type (ignored) |
//! | 0x03   | loop          | 1-byte block type (ignored) |
//! | 0x0B   | end           | —                        |
//! | 0x0C   | br            | uleb128 label depth      |
//! | 0x0D   | br_if         | uleb128 label depth      |
//! | 0x0F   | return        | —                        |
//! | 0x20   | local.get     | uleb128 local idx        |
//! | 0x21   | local.set     | uleb128 local idx        |
//! | 0x41   | i32.const     | sleb128 constant         |
//! | 0x6A   | i32.add       | —                        |
//! | 0x6B   | i32.sub       | —                        |
//!
//! Anything else returns `ParseError::UnknownOpcode`.
//!
//! Block-type bytes after `block`/`loop` are consumed but not
//! interpreted. Real WASM encodes a type index or the magic 0x40
//! "empty" sentinel; we ignore because our control-flow lowering
//! doesn't track block signatures yet.

use crate::wasm_lower::WasmOp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Ran off the end of the input mid-instruction.
    UnexpectedEof,
    /// Integer encoding exceeded 64 bits (malformed LEB128).
    IntegerTooLarge,
    /// Opcode isn't in our supported set.
    UnknownOpcode(u8),
    /// i32.const value didn't fit in i32 after sleb128 decode.
    ConstantOutOfRange,
}

/// Parse a full function body. Consumes all bytes; the sequence
/// returned includes the final `End`. Use [`parse_ops`] when you
/// want to consume part of a stream.
pub fn parse_function_body(bytes: &[u8]) -> Result<Vec<WasmOp>, ParseError> {
    let mut pos = 0;
    let ops = parse_ops(bytes, &mut pos)?;
    if pos != bytes.len() {
        // Trailing bytes — caller likely passed a too-large slice.
        // We treat this as malformed; strict WASM would also reject.
        return Err(ParseError::UnexpectedEof);
    }
    Ok(ops)
}

/// Parse until the outermost `End` (0x0B) is consumed. Advances
/// `pos` past it. Returns the ops including the terminating End.
pub fn parse_ops(bytes: &[u8], pos: &mut usize) -> Result<Vec<WasmOp>, ParseError> {
    let mut ops = Vec::new();
    let mut depth: u32 = 0; // nesting inside the body
    loop {
        let opcode = read_u8(bytes, pos)?;
        match opcode {
            0x02 => {
                // block — skip block type byte
                read_u8(bytes, pos)?;
                ops.push(WasmOp::Block);
                depth += 1;
            }
            0x03 => {
                // loop — skip block type byte
                read_u8(bytes, pos)?;
                ops.push(WasmOp::Loop);
                depth += 1;
            }
            0x04 => {
                // if — pops condition at run time, skip block type byte
                read_u8(bytes, pos)?;
                ops.push(WasmOp::If);
                depth += 1;
            }
            0x05 => {
                // else — delimits then/else; does NOT open a new block
                // (it's an inline marker within the current if block),
                // so don't touch `depth`.
                ops.push(WasmOp::Else);
            }
            0x10 => {
                // call funcidx
                let idx = read_uleb128(bytes, pos)?;
                if idx > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::Call(idx as u32));
            }
            0x0B => {
                ops.push(WasmOp::End);
                if depth == 0 {
                    // Outermost End — we're done with the body.
                    return Ok(ops);
                }
                depth -= 1;
            }
            0x0C => {
                let d = read_uleb128(bytes, pos)?;
                if d > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::Br(d as u32));
            }
            0x0D => {
                let d = read_uleb128(bytes, pos)?;
                if d > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::BrIf(d as u32));
            }
            0x0F => {
                ops.push(WasmOp::Return);
            }
            0x20 => {
                let idx = read_uleb128(bytes, pos)?;
                if idx > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::LocalGet(idx as u32));
            }
            0x21 => {
                let idx = read_uleb128(bytes, pos)?;
                if idx > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::LocalSet(idx as u32));
            }
            0x41 => {
                let v = read_sleb128(bytes, pos)?;
                if !(i32::MIN as i64..=i32::MAX as i64).contains(&v) {
                    return Err(ParseError::ConstantOutOfRange);
                }
                ops.push(WasmOp::I32Const(v as i32));
            }
            0x43 => {
                // f32.const — 4 bytes little-endian, NOT a varint.
                // WASM spec encodes floats directly by bit pattern.
                if *pos + 4 > bytes.len() { return Err(ParseError::UnexpectedEof); }
                let b = &bytes[*pos..*pos + 4];
                *pos += 4;
                let bits = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                ops.push(WasmOp::F32Const(f32::from_bits(bits)));
            }
            0x44 => {
                // f64.const — 8 bytes little-endian bit pattern.
                if *pos + 8 > bytes.len() { return Err(ParseError::UnexpectedEof); }
                let b = &bytes[*pos..*pos + 8];
                *pos += 8;
                let bits = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                ops.push(WasmOp::F64Const(f64::from_bits(bits)));
            }
            0x42 => {
                // i64.const — signed LEB128, full 64-bit range.
                let v = read_sleb128(bytes, pos)?;
                ops.push(WasmOp::I64Const(v));
            }
            0x50 => ops.push(WasmOp::I64Eqz),
            0x51 => ops.push(WasmOp::I64Eq),
            0x52 => ops.push(WasmOp::I64Ne),
            0x53 => ops.push(WasmOp::I64LtS),
            0x54 => ops.push(WasmOp::I64LtU),
            0x55 => ops.push(WasmOp::I64GtS),
            0x56 => ops.push(WasmOp::I64GtU),
            0x57 => ops.push(WasmOp::I64LeS),
            0x58 => ops.push(WasmOp::I64LeU),
            0x59 => ops.push(WasmOp::I64GeS),
            0x5A => ops.push(WasmOp::I64GeU),
            0x7C => ops.push(WasmOp::I64Add),
            0x7D => ops.push(WasmOp::I64Sub),
            0x7E => ops.push(WasmOp::I64Mul),
            0x7F => ops.push(WasmOp::I64DivS),
            0x80 => ops.push(WasmOp::I64DivU),
            0x83 => ops.push(WasmOp::I64And),
            0x84 => ops.push(WasmOp::I64Or),
            0x85 => ops.push(WasmOp::I64Xor),
            0x86 => ops.push(WasmOp::I64Shl),
            0x87 => ops.push(WasmOp::I64ShrS),
            0x88 => ops.push(WasmOp::I64ShrU),
            0x29 => {
                // i64.load memarg: align (uleb) then offset (uleb).
                let _align = read_uleb128(bytes, pos)?;
                let off = read_uleb128(bytes, pos)?;
                if off > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::I64Load(off as u32));
            }
            0x37 => {
                // i64.store memarg.
                let _align = read_uleb128(bytes, pos)?;
                let off = read_uleb128(bytes, pos)?;
                if off > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::I64Store(off as u32));
            }
            0xA7 => ops.push(WasmOp::I32WrapI64),
            0xAC => ops.push(WasmOp::I64ExtendI32S),
            0xAD => ops.push(WasmOp::I64ExtendI32U),
            0x28 => {
                // i32.load memarg: align (uleb) then offset (uleb).
                let _align = read_uleb128(bytes, pos)?;
                let off = read_uleb128(bytes, pos)?;
                if off > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::I32Load(off as u32));
            }
            0x2A => {
                // f32.load — same memarg shape as i32.load.
                let _align = read_uleb128(bytes, pos)?;
                let off = read_uleb128(bytes, pos)?;
                if off > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::F32Load(off as u32));
            }
            0x36 => {
                let _align = read_uleb128(bytes, pos)?;
                let off = read_uleb128(bytes, pos)?;
                if off > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::I32Store(off as u32));
            }
            0x38 => {
                // f32.store
                let _align = read_uleb128(bytes, pos)?;
                let off = read_uleb128(bytes, pos)?;
                if off > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::F32Store(off as u32));
            }
            0x2B => {
                // f64.load
                let _align = read_uleb128(bytes, pos)?;
                let off = read_uleb128(bytes, pos)?;
                if off > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::F64Load(off as u32));
            }
            0x39 => {
                // f64.store
                let _align = read_uleb128(bytes, pos)?;
                let off = read_uleb128(bytes, pos)?;
                if off > u32::MAX as u64 { return Err(ParseError::IntegerTooLarge); }
                ops.push(WasmOp::F64Store(off as u32));
            }
            0x45 => ops.push(WasmOp::I32Eqz),
            0x46 => ops.push(WasmOp::I32Eq),
            0x47 => ops.push(WasmOp::I32Ne),
            0x48 => ops.push(WasmOp::I32LtS),
            0x49 => ops.push(WasmOp::I32LtU),
            0x4A => ops.push(WasmOp::I32GtS),
            0x4B => ops.push(WasmOp::I32GtU),
            0x4C => ops.push(WasmOp::I32LeS),
            0x4D => ops.push(WasmOp::I32LeU),
            0x4E => ops.push(WasmOp::I32GeS),
            0x4F => ops.push(WasmOp::I32GeU),
            0x6A => ops.push(WasmOp::I32Add),
            0x6B => ops.push(WasmOp::I32Sub),
            0x6C => ops.push(WasmOp::I32Mul),
            0x6D => ops.push(WasmOp::I32DivS),
            0x6E => ops.push(WasmOp::I32DivU),
            0x71 => ops.push(WasmOp::I32And),
            0x72 => ops.push(WasmOp::I32Or),
            0x73 => ops.push(WasmOp::I32Xor),
            0x74 => ops.push(WasmOp::I32Shl),
            0x75 => ops.push(WasmOp::I32ShrS),
            0x76 => ops.push(WasmOp::I32ShrU),
            0x5B => ops.push(WasmOp::F32Eq),
            0x5C => ops.push(WasmOp::F32Ne),
            0x5D => ops.push(WasmOp::F32Lt),
            0x5E => ops.push(WasmOp::F32Gt),
            0x5F => ops.push(WasmOp::F32Le),
            0x60 => ops.push(WasmOp::F32Ge),
            0x92 => ops.push(WasmOp::F32Add),
            0x93 => ops.push(WasmOp::F32Sub),
            0x94 => ops.push(WasmOp::F32Mul),
            0x95 => ops.push(WasmOp::F32Div),
            0x61 => ops.push(WasmOp::F64Eq),
            0x62 => ops.push(WasmOp::F64Ne),
            0x63 => ops.push(WasmOp::F64Lt),
            0x64 => ops.push(WasmOp::F64Gt),
            0x65 => ops.push(WasmOp::F64Le),
            0x66 => ops.push(WasmOp::F64Ge),
            0xA0 => ops.push(WasmOp::F64Add),
            0xA1 => ops.push(WasmOp::F64Sub),
            0xA2 => ops.push(WasmOp::F64Mul),
            0xA3 => ops.push(WasmOp::F64Div),
            _ => return Err(ParseError::UnknownOpcode(opcode)),
        }
    }
}

// ── LEB128 helpers ──────────────────────────────────────────────────
//
// Each continuation byte stores 7 bits with the MSB as "more to
// come". The unsigned form just concatenates; the signed form
// sign-extends the last byte based on its 6th bit.

fn read_u8(bytes: &[u8], pos: &mut usize) -> Result<u8, ParseError> {
    let b = *bytes.get(*pos).ok_or(ParseError::UnexpectedEof)?;
    *pos += 1;
    Ok(b)
}

/// Decode an unsigned LEB128 value. Bounded to 64 bits.
pub fn read_uleb128(bytes: &[u8], pos: &mut usize) -> Result<u64, ParseError> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let b = read_u8(bytes, pos)?;
        result |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            return Err(ParseError::IntegerTooLarge);
        }
    }
}

/// Decode a signed LEB128 value. Sign-extends the final byte.
pub fn read_sleb128(bytes: &[u8], pos: &mut usize) -> Result<i64, ParseError> {
    let mut result: i64 = 0;
    let mut shift = 0;
    let last: u8;
    loop {
        let b = read_u8(bytes, pos)?;
        result |= ((b & 0x7F) as i64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            last = b;
            break;
        }
        if shift >= 64 {
            return Err(ParseError::IntegerTooLarge);
        }
    }
    // Sign-extend if the high bit of the last 7-bit chunk is set.
    if shift < 64 && (last & 0x40) != 0 {
        result |= -1i64 << shift;
    }
    Ok(result)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uleb_small() {
        let mut pos = 0;
        assert_eq!(read_uleb128(&[0x00], &mut pos).unwrap(), 0);
        assert_eq!(pos, 1);

        let mut pos = 0;
        assert_eq!(read_uleb128(&[0x2A], &mut pos).unwrap(), 42);
        assert_eq!(pos, 1);
    }

    #[test]
    fn uleb_multi_byte() {
        // 300 = 0b1_0010_1100 → 0xAC 0x02
        let mut pos = 0;
        assert_eq!(read_uleb128(&[0xAC, 0x02], &mut pos).unwrap(), 300);
        assert_eq!(pos, 2);
    }

    #[test]
    fn sleb_small_positive() {
        let mut pos = 0;
        assert_eq!(read_sleb128(&[0x2A], &mut pos).unwrap(), 42);
    }

    #[test]
    fn sleb_small_negative() {
        // -1 in sleb128 is 0x7F (7-bit all ones, high sign bit).
        let mut pos = 0;
        assert_eq!(read_sleb128(&[0x7F], &mut pos).unwrap(), -1);
    }

    #[test]
    fn sleb_multi_byte_negative() {
        // -42 in sleb128: encode -42 = 0xFFFFFFFFFFFFFFD6
        //   low 7 bits of -42: 0x56
        //   continuation bit set because more bits matter (sign ext)
        //   next 7 bits: all 1s = 0x7F, no continuation
        //   So bytes: 0xD6 0x7F
        let mut pos = 0;
        assert_eq!(read_sleb128(&[0xD6, 0x7F], &mut pos).unwrap(), -42);
    }

    #[test]
    fn parse_return_42() {
        // Body: i32.const 42 ; end
        //   0x41 0x2A 0x0B
        let ops = parse_function_body(&[0x41, 0x2A, 0x0B]).unwrap();
        assert_eq!(ops, vec![WasmOp::I32Const(42), WasmOp::End]);
    }

    #[test]
    fn parse_load_store() {
        // Body: i32.const 0 ; i32.const 42 ; i32.store align=2 offset=0
        //       ; i32.const 0 ; i32.load align=2 offset=4 ; end
        //   0x41 0x00 0x41 0x2A 0x36 0x02 0x00
        //   0x41 0x00 0x28 0x02 0x04 0x0B
        let ops = parse_function_body(&[
            0x41, 0x00, 0x41, 0x2A, 0x36, 0x02, 0x00,
            0x41, 0x00, 0x28, 0x02, 0x04, 0x0B,
        ])
        .unwrap();
        assert_eq!(
            ops,
            vec![
                WasmOp::I32Const(0),
                WasmOp::I32Const(42),
                WasmOp::I32Store(0),
                WasmOp::I32Const(0),
                WasmOp::I32Load(4),
                WasmOp::End,
            ]
        );
    }

    #[test]
    fn parse_comparisons() {
        // i32.const 3 ; i32.const 5 ; i32.lt_s ;
        // i32.const 5 ; i32.const 5 ; i32.eq ;
        // end
        //   0x41 0x03 0x41 0x05 0x48 0x41 0x05 0x41 0x05 0x46 0x0B
        let ops = parse_function_body(&[
            0x41, 0x03, 0x41, 0x05, 0x48,
            0x41, 0x05, 0x41, 0x05, 0x46,
            0x0B,
        ])
        .unwrap();
        assert_eq!(
            ops,
            vec![
                WasmOp::I32Const(3),
                WasmOp::I32Const(5),
                WasmOp::I32LtS,
                WasmOp::I32Const(5),
                WasmOp::I32Const(5),
                WasmOp::I32Eq,
                WasmOp::End,
            ]
        );
    }

    #[test]
    fn parse_mul_div() {
        // i32.const 6 ; i32.const 7 ; i32.mul ; i32.const 3 ; i32.div_s ; end
        //   0x41 0x06 0x41 0x07 0x6C 0x41 0x03 0x6D 0x0B
        let ops = parse_function_body(&[
            0x41, 0x06, 0x41, 0x07, 0x6C,
            0x41, 0x03, 0x6D, 0x0B,
        ])
        .unwrap();
        assert_eq!(
            ops,
            vec![
                WasmOp::I32Const(6),
                WasmOp::I32Const(7),
                WasmOp::I32Mul,
                WasmOp::I32Const(3),
                WasmOp::I32DivS,
                WasmOp::End,
            ]
        );
    }

    #[test]
    fn parse_arithmetic() {
        // Body: i32.const 10 ; i32.const 20 ; i32.add ; end
        //   0x41 0x0A 0x41 0x14 0x6A 0x0B
        let ops = parse_function_body(&[0x41, 0x0A, 0x41, 0x14, 0x6A, 0x0B]).unwrap();
        assert_eq!(
            ops,
            vec![
                WasmOp::I32Const(10),
                WasmOp::I32Const(20),
                WasmOp::I32Add,
                WasmOp::End,
            ]
        );
    }

    #[test]
    fn parse_locals_and_sub() {
        // Body: local.get 0 ; local.get 1 ; i32.sub ; end
        //   0x20 0x00 0x20 0x01 0x6B 0x0B
        let ops = parse_function_body(&[0x20, 0x00, 0x20, 0x01, 0x6B, 0x0B]).unwrap();
        assert_eq!(
            ops,
            vec![
                WasmOp::LocalGet(0),
                WasmOp::LocalGet(1),
                WasmOp::I32Sub,
                WasmOp::End,
            ]
        );
    }

    #[test]
    fn parse_block_br() {
        // Body: block (type 0x40) ; br 0 ; end ; i32.const 1 ; end
        //   0x02 0x40 0x0C 0x00 0x0B 0x41 0x01 0x0B
        let ops = parse_function_body(&[0x02, 0x40, 0x0C, 0x00, 0x0B, 0x41, 0x01, 0x0B]).unwrap();
        assert_eq!(
            ops,
            vec![
                WasmOp::Block,
                WasmOp::Br(0),
                WasmOp::End,
                WasmOp::I32Const(1),
                WasmOp::End,
            ]
        );
    }

    #[test]
    fn parse_negative_const() {
        // i32.const -1 ; end
        //   0x41 0x7F 0x0B
        let ops = parse_function_body(&[0x41, 0x7F, 0x0B]).unwrap();
        assert_eq!(ops, vec![WasmOp::I32Const(-1), WasmOp::End]);
    }

    #[test]
    fn parse_rejects_unknown_opcode() {
        let err = parse_function_body(&[0xFF, 0x0B]).unwrap_err();
        assert_eq!(err, ParseError::UnknownOpcode(0xFF));
    }

    #[test]
    fn parse_rejects_truncated() {
        // local.get with no index byte
        assert_eq!(
            parse_function_body(&[0x20]),
            Err(ParseError::UnexpectedEof)
        );
    }

    /// End-to-end: parse bytes → lower → expected machine code.
    /// This is the real proof that the parser and lowerer compose.
    #[test]
    fn end_to_end_return_42_from_bytes() {
        use crate::wasm_lower::Lowerer;
        let ops = parse_function_body(&[0x41, 0x2A, 0x0B]).unwrap();
        let mut lw = Lowerer::new();
        lw.lower_all(&ops).unwrap();
        assert_eq!(
            lw.finish(),
            vec![
                0x40, 0x05, 0x80, 0xD2, // movz x0, #42
                0xC0, 0x03, 0x5F, 0xD6, // ret
            ]
        );
    }
}
