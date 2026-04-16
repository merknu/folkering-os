//! AArch64 (A64) instruction encoder.
//!
//! Emits 32-bit little-endian A64 opcodes for use by a JIT. Scope is
//! deliberately narrow for Phase 1: just enough instructions to build
//! a "return 42" function and compose small arithmetic loops. Future
//! phases add branching, conditional ops, SIMD, and WASM lowering.
//!
//! # References
//!
//! - ARM Architecture Reference Manual for A-profile (DDI 0487), C6.2
//!   (A64 Base Instructions alphabetical listing)
//! - Each encoding includes a comment with the exact bit layout from
//!   the ARM ARM so reviewers can cross-check without re-reading the
//!   spec.
//!
//! # Design
//!
//! Instructions are emitted into a `Vec<u8>` in little-endian order,
//! matching how the ARMv8 memory system loads instruction words.
//! The builder API is a thin struct so callers can own their own
//! buffers (for JIT code caches, test harnesses, etc).

pub mod wasm_lower;
pub mod wasm_parse;
pub use wasm_lower::{LowerError, Lowerer, WasmOp};
pub use wasm_parse::{parse_function_body, ParseError};

/// A64 general-purpose register.
///
/// Registers 0-30 are the normal GPRs (X0..X30, or W0..W30 in 32-bit
/// form). Register 31 encodes either the zero register (XZR/WZR) or
/// the stack pointer (SP/WSP) depending on the instruction — we use
/// [`Reg::ZR`] for the zero-register semantics and [`Reg::SP`] for
/// the stack-pointer semantics as sibling constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reg(pub u8);

impl Reg {
    /// Construct a GPR index 0..=30. Returns None for out-of-range.
    pub fn new(idx: u8) -> Option<Self> {
        if idx <= 30 { Some(Reg(idx)) } else { None }
    }

    /// Raw 5-bit register number used in encoding.
    #[inline(always)]
    fn enc(self) -> u32 { (self.0 as u32) & 0x1F }

    pub const X0:  Reg = Reg(0);
    pub const X1:  Reg = Reg(1);
    pub const X2:  Reg = Reg(2);
    pub const X3:  Reg = Reg(3);
    pub const X4:  Reg = Reg(4);
    pub const X5:  Reg = Reg(5);
    pub const X6:  Reg = Reg(6);
    pub const X7:  Reg = Reg(7);
    pub const X8:  Reg = Reg(8);
    pub const X9:  Reg = Reg(9);
    pub const X10: Reg = Reg(10);
    pub const X16: Reg = Reg(16);
    pub const X17: Reg = Reg(17);
    pub const X19: Reg = Reg(19);
    pub const X29: Reg = Reg(29);
    pub const X30: Reg = Reg(30);
    /// Zero register (reads 0, ignores writes).
    pub const ZR:  Reg = Reg(31);
    /// Stack pointer. Same 5-bit encoding as ZR (31); the instruction
    /// opcode determines which interpretation the CPU uses.
    pub const SP:  Reg = Reg(31);
}

/// Logical shift for MOVZ/MOVN/MOVK "hw" field. Each value shifts the
/// 16-bit immediate left by a multiple of 16, so the four values
/// together cover the whole 64-bit range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovShift {
    Lsl0 = 0,
    Lsl16 = 1,
    Lsl32 = 2,
    Lsl48 = 3,
}

/// A64 condition codes (4-bit field used by CSET/B.cond/CSEL/etc).
/// Values match the architectural encoding from ARM ARM table C1-1
/// so they can be written straight into the instruction word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Condition {
    Eq = 0b0000,  // Equal (Z=1)
    Ne = 0b0001,  // Not equal
    Hs = 0b0010,  // Unsigned higher or same (aka CS)
    Lo = 0b0011,  // Unsigned lower (aka CC)
    Hi = 0b1000,  // Unsigned higher
    Ls = 0b1001,  // Unsigned lower or same
    Ge = 0b1010,  // Signed greater than or equal
    Lt = 0b1011,  // Signed less than
    Gt = 0b1100,  // Signed greater than
    Le = 0b1101,  // Signed less than or equal
}

impl Condition {
    /// Logical inverse of the condition. Each cond-pair in the ARM ARM
    /// differs only in bit 0, so the table is compact and symmetric.
    /// Used by CSET's underlying CSINC encoding, which takes the
    /// *inverted* condition compared to the assembly mnemonic.
    pub fn invert(self) -> Self {
        match self {
            Condition::Eq => Condition::Ne,
            Condition::Ne => Condition::Eq,
            Condition::Hs => Condition::Lo,
            Condition::Lo => Condition::Hs,
            Condition::Hi => Condition::Ls,
            Condition::Ls => Condition::Hi,
            Condition::Ge => Condition::Lt,
            Condition::Lt => Condition::Ge,
            Condition::Gt => Condition::Le,
            Condition::Le => Condition::Gt,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// Immediate doesn't fit in the instruction's field width.
    ImmediateOutOfRange,
    /// Offset isn't aligned to the required granule (4 for branches,
    /// 8 for 64-bit loads/stores, etc).
    OffsetMisaligned,
}

/// Builder for a sequence of A64 instructions. Wraps a byte buffer
/// and appends each opcode as four little-endian bytes.
#[derive(Debug, Default, Clone)]
pub struct Encoder {
    buf: alloc_placeholder::Vec<u8>,
}

/// Tiny module so the file reads without pulling alloc publicly — we
/// re-export std's Vec under the same path in tests.
mod alloc_placeholder {
    pub use std::vec::Vec;
}

impl Encoder {
    pub fn new() -> Self { Self { buf: Vec::new() } }

    /// Current cursor in bytes. Useful for computing branch offsets.
    pub fn pos(&self) -> usize { self.buf.len() }

    /// Consume the builder and return the encoded bytes.
    pub fn into_bytes(self) -> Vec<u8> { self.buf }

    /// Borrow the bytes without consuming.
    pub fn as_bytes(&self) -> &[u8] { &self.buf }

    /// Append a raw 32-bit opcode. Internal helper; all public
    /// emitters route through this so byte-order is consistent.
    fn emit(&mut self, word: u32) {
        self.buf.extend_from_slice(&word.to_le_bytes());
    }

    // ── Data-processing — immediate ──────────────────────────────────

    /// MOVZ Xd, #imm16 {, LSL #shift}
    ///
    /// Encoding (C6.2.191):
    ///   `1 10 100101 hw(2) imm16(16) Rd(5)`
    ///
    /// Writes `imm16 << (16 * hw)` into Xd, zeroing all other bits.
    /// The most common form of "load immediate" when the value fits.
    pub fn movz(&mut self, rd: Reg, imm16: u16, shift: MovShift) -> Result<(), EncodeError> {
        let word = 0xD280_0000u32
            | ((shift as u32) << 21)
            | ((imm16 as u32) << 5)
            | rd.enc();
        self.emit(word);
        Ok(())
    }

    /// MOVN Xd, #imm16 {, LSL #shift}
    ///
    /// Encoding (C6.2.189): `1 00 100101 hw(2) imm16(16) Rd(5)`.
    /// Writes `~(imm16 << shift)` — useful for negative constants.
    pub fn movn(&mut self, rd: Reg, imm16: u16, shift: MovShift) -> Result<(), EncodeError> {
        let word = 0x9280_0000u32
            | ((shift as u32) << 21)
            | ((imm16 as u32) << 5)
            | rd.enc();
        self.emit(word);
        Ok(())
    }

    /// MOVK Xd, #imm16, LSL #shift
    ///
    /// Encoding (C6.2.190): `1 11 100101 hw(2) imm16(16) Rd(5)`.
    /// Patches the 16-bit window without touching the rest of Xd.
    /// Used to compose 64-bit constants via a MOVZ/MOVK/MOVK/MOVK chain.
    pub fn movk(&mut self, rd: Reg, imm16: u16, shift: MovShift) -> Result<(), EncodeError> {
        let word = 0xF280_0000u32
            | ((shift as u32) << 21)
            | ((imm16 as u32) << 5)
            | rd.enc();
        self.emit(word);
        Ok(())
    }

    // ── Data-processing — shifted register ──────────────────────────

    /// ADD Xd, Xn, Xm (shift=LSL #0 implied).
    ///
    /// Encoding (C6.2.4): `1 0 0 01011 00 0 Rm(5) 000000 Rn(5) Rd(5)`.
    pub fn add(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        let word = 0x8B00_0000u32
            | (rm.enc() << 16)
            | (rn.enc() << 5)
            | rd.enc();
        self.emit(word);
        Ok(())
    }

    /// SUB Xd, Xn, Xm (shift=LSL #0).
    ///
    /// Encoding (C6.2.335): `1 1 0 01011 00 0 Rm(5) 000000 Rn(5) Rd(5)`.
    pub fn sub(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        let word = 0xCB00_0000u32
            | (rm.enc() << 16)
            | (rn.enc() << 5)
            | rd.enc();
        self.emit(word);
        Ok(())
    }

    /// MUL Xd, Xn, Xm — 64-bit multiply.
    ///
    /// A64 has no dedicated MUL; it's an alias for `MADD Xd, Xn, Xm, XZR`
    /// (multiply-add with the zero register as accumulator).
    ///
    /// Encoding (C6.2.173 — MADD): `1 0011011 000 Rm(5) 0 Ra(5) Rn(5) Rd(5)`.
    /// We hardcode `Ra = XZR = 31` and `o0 = 0` to get plain multiply.
    pub fn mul(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        let word = 0x9B00_0000u32
            | (rm.enc() << 16)
            | (31u32 << 10) // Ra = XZR
            | (rn.enc() << 5)
            | rd.enc();
        self.emit(word);
        Ok(())
    }

    /// SDIV Xd, Xn, Xm — signed 64-bit divide (rounds toward zero).
    ///
    /// Encoding (C6.2.294): `1 0011010110 Rm(5) 000011 Rn(5) Rd(5)`.
    /// Divide-by-zero returns 0 (no trap on A64); INT_MIN / -1 wraps
    /// to INT_MIN without trapping. WASM spec requires *trapping* on
    /// both — that's the lowerer's problem, not the encoder's.
    pub fn sdiv(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        let word = 0x9AC0_0C00u32
            | (rm.enc() << 16)
            | (rn.enc() << 5)
            | rd.enc();
        self.emit(word);
        Ok(())
    }

    /// ADD Xd, Xn, Wm, UXTW — 64-bit add where Wm is zero-extended
    /// from 32 to 64 bits before the add. The canonical form for
    /// computing an effective memory address from a WASM i32 index
    /// plus a 64-bit base: upper 32 of the index are zeroed so stray
    /// bits from prior arithmetic don't perturb the base.
    ///
    /// Encoding (C6.2.5, Extended register): `1 0 0 01011 00 1 Rm(5) 010 000 Rn(5) Rd(5)`.
    /// The `010` field is the `option` for UXTW; `000` is imm3 (no shift).
    pub fn add_ext_uxtw(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        let word = 0x8B20_4000u32
            | (rm.enc() << 16)
            | (rn.enc() << 5)
            | rd.enc();
        self.emit(word);
        Ok(())
    }

    /// CMP Wn, Wm — 32-bit compare (alias for `SUBS WZR, Wn, Wm`).
    ///
    /// Subtracts Wm from Wn on the low 32 bits and updates the NZCV
    /// flags; discards the numerical result.  32-bit form matches
    /// WASM i32 comparison semantics — upper 32 bits of the hosting
    /// X register are ignored, so stale sign-extension from prior
    /// ops doesn't affect the flag outcome.
    ///
    /// Encoding (C6.2.340 — SUBS shifted register, sf=0, Rd=WZR):
    /// `0 1 1 01011 00 0 Rm(5) 000000 Rn(5) 11111`.
    pub fn cmp_w(&mut self, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        let word = 0x6B00_0000u32
            | (rm.enc() << 16)
            | (rn.enc() << 5)
            | 31u32; // Rd = WZR
        self.emit(word);
        Ok(())
    }

    /// CSET Xd, cond — set Xd to 1 if `cond` holds, else 0.
    ///
    /// Canonical A64 idiom for converting an NZCV flag state into a
    /// boolean integer.  Assembler alias for `CSINC Xd, XZR, XZR,
    /// invert(cond)`: when the inverted condition is false (i.e. the
    /// original cond holds), CSINC picks `XZR + 1 = 1`; otherwise
    /// it picks `XZR = 0`.
    ///
    /// Encoding (C6.2.49 — CSINC, sf=1, Rm=Rn=XZR):
    /// `1 0 0 11010100 11111 cond(4) 01 11111 Rd(5)`.
    pub fn cset(&mut self, rd: Reg, cond: Condition) -> Result<(), EncodeError> {
        let inv = cond.invert() as u32;
        let word = 0x9A80_0400u32
            | (31u32 << 16)   // Rm = XZR
            | (inv << 12)
            | (31u32 << 5)    // Rn = XZR
            | rd.enc();
        self.emit(word);
        Ok(())
    }

    /// UDIV Xd, Xn, Xm — unsigned 64-bit divide.
    ///
    /// Encoding (C6.2.351): `1 0011010110 Rm(5) 000010 Rn(5) Rd(5)`.
    /// Differs from SDIV only in bit 10 (0 vs 1).  Same divide-by-zero
    /// behaviour (returns 0, no trap).
    pub fn udiv(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        let word = 0x9AC0_0800u32
            | (rm.enc() << 16)
            | (rn.enc() << 5)
            | rd.enc();
        self.emit(word);
        Ok(())
    }

    /// LDR Wt, [Xn, #imm] — 32-bit load, unsigned immediate offset.
    ///
    /// Encoding (C6.2.131, 32-bit variant): `10 111 0 01 01 imm12(12) Rn(5) Rt(5)`.
    /// imm is bytes, must be a multiple of 4 (scaled by 4), range 0..=16380.
    /// Zeroes the upper 32 bits of the X register — matches WASM
    /// `i32.load` semantics.
    pub fn ldr_w_imm(&mut self, rt: Reg, rn: Reg, offset: u32) -> Result<(), EncodeError> {
        if offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm12 = offset >> 2;
        if imm12 > 0xFFF { return Err(EncodeError::ImmediateOutOfRange); }
        let word = 0xB940_0000u32
            | (imm12 << 10)
            | (rn.enc() << 5)
            | rt.enc();
        self.emit(word);
        Ok(())
    }

    /// STR Wt, [Xn, #imm] — 32-bit store, unsigned immediate offset.
    ///
    /// Encoding (C6.2.340, 32-bit variant): `10 111 0 01 00 imm12(12) Rn(5) Rt(5)`.
    /// Same scaling and range as [`Encoder::ldr_w_imm`].
    pub fn str_w_imm(&mut self, rt: Reg, rn: Reg, offset: u32) -> Result<(), EncodeError> {
        if offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm12 = offset >> 2;
        if imm12 > 0xFFF { return Err(EncodeError::ImmediateOutOfRange); }
        let word = 0xB900_0000u32
            | (imm12 << 10)
            | (rn.enc() << 5)
            | rt.enc();
        self.emit(word);
        Ok(())
    }

    // ── Loads and stores — unsigned immediate offset ────────────────

    /// LDR Xt, [Xn, #imm]
    ///
    /// Encoding (C6.2.131): `11 111 0 01 01 imm12(12) Rn(5) Rt(5)`.
    /// The immediate is scaled by 8 (since this is a 64-bit load), so
    /// `offset` here is in bytes and must be a multiple of 8, in the
    /// range 0..=32760.
    pub fn ldr_imm(&mut self, rt: Reg, rn: Reg, offset: u32) -> Result<(), EncodeError> {
        if offset & 0x7 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm12 = offset >> 3;
        if imm12 > 0xFFF { return Err(EncodeError::ImmediateOutOfRange); }
        let word = 0xF940_0000u32
            | (imm12 << 10)
            | (rn.enc() << 5)
            | rt.enc();
        self.emit(word);
        Ok(())
    }

    /// STR Xt, [Xn, #imm]
    ///
    /// Encoding (C6.2.340): `11 111 0 01 00 imm12(12) Rn(5) Rt(5)`.
    /// Same scaling rules as [`Encoder::ldr_imm`].
    pub fn str_imm(&mut self, rt: Reg, rn: Reg, offset: u32) -> Result<(), EncodeError> {
        if offset & 0x7 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm12 = offset >> 3;
        if imm12 > 0xFFF { return Err(EncodeError::ImmediateOutOfRange); }
        let word = 0xF900_0000u32
            | (imm12 << 10)
            | (rn.enc() << 5)
            | rt.enc();
        self.emit(word);
        Ok(())
    }

    // ── Branches ────────────────────────────────────────────────────

    /// B (unconditional branch to PC-relative offset).
    ///
    /// Encoding (C6.2.30): `0 0 0101 imm26(26)`.
    /// `offset` is in *bytes*, must be 4-aligned, range ±128 MiB.
    /// Positive values branch forward; negative branch backward.
    pub fn b(&mut self, offset: i32) -> Result<(), EncodeError> {
        if offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm26 = offset >> 2;
        // Must fit in 26-bit signed: -33554432..=33554431
        if !(-(1 << 25)..(1 << 25)).contains(&imm26) {
            return Err(EncodeError::ImmediateOutOfRange);
        }
        let word = 0x1400_0000u32 | ((imm26 as u32) & 0x03FF_FFFF);
        self.emit(word);
        Ok(())
    }

    /// BR Xn (branch to register — no link, no return-prediction hint).
    ///
    /// Encoding (C6.2.33): `1101 0110 0 0 011111 000000 Rn(5) 00000`.
    pub fn br(&mut self, rn: Reg) -> Result<(), EncodeError> {
        let word = 0xD61F_0000u32 | (rn.enc() << 5);
        self.emit(word);
        Ok(())
    }

    /// RET {Xn} — branch to register with return-prediction hint. Default
    /// target is X30 (LR), which matches how `call/return` works under
    /// AAPCS64. Pass `Reg::X30` or a custom register.
    ///
    /// Encoding (C6.2.236): `1101 0110 0 1 011111 000000 Rn(5) 00000`.
    pub fn ret(&mut self, rn: Reg) -> Result<(), EncodeError> {
        let word = 0xD65F_0000u32 | (rn.enc() << 5);
        self.emit(word);
        Ok(())
    }

    /// NOP — no operation. Useful as a branch-target padding or a
    /// rewrite slot in a patchable JIT prologue.
    ///
    /// Encoding (C6.2.203): fixed `0xD503201F`.
    pub fn nop(&mut self) {
        self.emit(0xD503_201F);
    }

    /// STP Xt1, Xt2, [Xn, #imm]! (pre-indexed, 64-bit store pair).
    ///
    /// Encoding (C6.2.319 — Pre-index): `1010 1001 10 imm7(7) Rt2(5) Rn(5) Rt(5)`.
    /// `imm` is in bytes and must be a multiple of 8, range -512..=504.
    /// `SP` (register 31) is a legal `Rn` here — the CPU uses the
    /// stack-pointer reading for this instruction.
    pub fn stp_pre_indexed_64(
        &mut self,
        rt: Reg,
        rt2: Reg,
        rn: Reg,
        imm: i16,
    ) -> Result<(), EncodeError> {
        if imm & 0x7 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm7 = imm >> 3;
        if !(-64..64).contains(&imm7) {
            return Err(EncodeError::ImmediateOutOfRange);
        }
        let word = 0xA980_0000u32
            | (((imm7 as u32) & 0x7F) << 15)
            | (rt2.enc() << 10)
            | (rn.enc() << 5)
            | rt.enc();
        self.emit(word);
        Ok(())
    }

    /// LDP Xt1, Xt2, [Xn], #imm (post-indexed, 64-bit load pair).
    ///
    /// Encoding (C6.2.133 — Post-index): `1010 1000 11 imm7(7) Rt2(5) Rn(5) Rt(5)`.
    /// Symmetric to [`Encoder::stp_pre_indexed_64`] — typical epilogue.
    pub fn ldp_post_indexed_64(
        &mut self,
        rt: Reg,
        rt2: Reg,
        rn: Reg,
        imm: i16,
    ) -> Result<(), EncodeError> {
        if imm & 0x7 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm7 = imm >> 3;
        if !(-64..64).contains(&imm7) {
            return Err(EncodeError::ImmediateOutOfRange);
        }
        let word = 0xA8C0_0000u32
            | (((imm7 as u32) & 0x7F) << 15)
            | (rt2.enc() << 10)
            | (rn.enc() << 5)
            | rt.enc();
        self.emit(word);
        Ok(())
    }

    /// BL #offset — branch with link (unconditional call).
    /// Saves the return address (PC+4) in X30, then branches.
    ///
    /// Encoding (C6.2.32): `1 0 0 1 0 1 imm26(26)`.
    /// Same offset rules as [`Encoder::b`] — 4-aligned, ±128 MiB.
    pub fn bl(&mut self, offset: i32) -> Result<(), EncodeError> {
        if offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm26 = offset >> 2;
        if !(-(1 << 25)..(1 << 25)).contains(&imm26) {
            return Err(EncodeError::ImmediateOutOfRange);
        }
        let word = 0x9400_0000u32 | ((imm26 as u32) & 0x03FF_FFFF);
        self.emit(word);
        Ok(())
    }

    /// BLR Xn — branch with link to register. The CPU's standard
    /// register-indirect call; pairs naturally with a MOVZ/MOVK
    /// chain that loads an absolute address into X16.
    ///
    /// Encoding (C6.2.34): `1101 0110 0 0 111111 000000 Rn(5) 00000`.
    pub fn blr(&mut self, rn: Reg) -> Result<(), EncodeError> {
        let word = 0xD63F_0000u32 | (rn.enc() << 5);
        self.emit(word);
        Ok(())
    }

    /// CBNZ Wt, offset — compare-and-branch if register is non-zero.
    /// 32-bit variant (W register) so we compare the low 32 bits only
    /// — matches WASM `br_if` semantics on i32 values.
    ///
    /// Encoding (C6.2.39): `0 011010 1 imm19(19) Rt(5)`.
    /// `offset` is bytes, must be 4-aligned, range ±1 MiB (imm19 × 4).
    pub fn cbnz_w(&mut self, rt: Reg, offset: i32) -> Result<(), EncodeError> {
        self.emit(encode_cbnz_w(rt, offset)?);
        Ok(())
    }

    /// CBZ Wt, offset — compare-and-branch if register is zero.
    /// 32-bit variant; see [`Encoder::cbnz_w`] for range rules.
    ///
    /// Encoding (C6.2.38): `0 011010 0 imm19(19) Rt(5)`.
    pub fn cbz_w(&mut self, rt: Reg, offset: i32) -> Result<(), EncodeError> {
        self.emit(encode_cbz_w(rt, offset)?);
        Ok(())
    }

    /// Overwrite the 4 bytes at `pos` with the given opcode word,
    /// little-endian. Used to patch forward-branch placeholders once
    /// their target is known. Panics (via slice-len assertion) if
    /// the position is beyond the emitted range.
    pub fn patch_word(&mut self, pos: usize, word: u32) {
        let bytes = word.to_le_bytes();
        self.buf[pos..pos + 4].copy_from_slice(&bytes);
    }
}

// ── Public encoding helpers ─────────────────────────────────────────
//
// These return the raw opcode word without touching the buffer, so
// JIT code generators can pre-compute or re-compute instructions (for
// branch patching, code splicing, etc) independent of an Encoder.

/// Encode an unconditional B instruction. See [`Encoder::b`].
pub fn encode_b(offset: i32) -> Result<u32, EncodeError> {
    if offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
    let imm26 = offset >> 2;
    if !(-(1 << 25)..(1 << 25)).contains(&imm26) {
        return Err(EncodeError::ImmediateOutOfRange);
    }
    Ok(0x1400_0000u32 | ((imm26 as u32) & 0x03FF_FFFF))
}

/// Encode CBNZ Wt, offset. Signed 19-bit offset, scaled by 4.
pub fn encode_cbnz_w(rt: Reg, offset: i32) -> Result<u32, EncodeError> {
    if offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
    let imm19 = offset >> 2;
    if !(-(1 << 18)..(1 << 18)).contains(&imm19) {
        return Err(EncodeError::ImmediateOutOfRange);
    }
    Ok(0x3500_0000u32 | (((imm19 as u32) & 0x0007_FFFF) << 5) | rt.enc())
}

/// Encode CBZ Wt, offset. Same range rules as CBNZ.
pub fn encode_cbz_w(rt: Reg, offset: i32) -> Result<u32, EncodeError> {
    if offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
    let imm19 = offset >> 2;
    if !(-(1 << 18)..(1 << 18)).contains(&imm19) {
        return Err(EncodeError::ImmediateOutOfRange);
    }
    Ok(0x3400_0000u32 | (((imm19 as u32) & 0x0007_FFFF) << 5) | rt.enc())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper — encode one instruction and return it as a u32 (native
    /// endian). Reading 4 bytes little-endian matches how a CPU would
    /// fetch the instruction from memory.
    fn one(f: impl FnOnce(&mut Encoder) -> Result<(), EncodeError>) -> u32 {
        let mut enc = Encoder::new();
        f(&mut enc).expect("encode");
        let bytes: [u8; 4] = enc.as_bytes().try_into().unwrap();
        u32::from_le_bytes(bytes)
    }

    #[test]
    fn movz_x0_42() {
        // Reference (from `echo "mov x0, #42; ret" | aarch64-linux-gnu-as -o /tmp/a.o
        //                  && aarch64-linux-gnu-objdump -d /tmp/a.o`):
        //   d2800540   mov x0, #0x2a
        assert_eq!(one(|e| e.movz(Reg::X0, 42, MovShift::Lsl0)), 0xD2800540);
    }

    #[test]
    fn movz_x1_0xffff_shifted() {
        // mov x1, #0xffff, lsl #16  →  d2bfffe1
        assert_eq!(one(|e| e.movz(Reg::X1, 0xFFFF, MovShift::Lsl16)), 0xD2BFFFE1);
    }

    #[test]
    fn movn_x0_0() {
        // movn x0, #0  →  92800000  (writes -1 into X0)
        assert_eq!(one(|e| e.movn(Reg::X0, 0, MovShift::Lsl0)), 0x92800000);
    }

    #[test]
    fn movk_x0_beef_at_16() {
        // movk x0, #0xbeef, lsl #16  →  f2b7dde0
        assert_eq!(one(|e| e.movk(Reg::X0, 0xBEEF, MovShift::Lsl16)), 0xF2B7DDE0);
    }

    #[test]
    fn add_x0_x1_x2() {
        // add x0, x1, x2  →  8b020020
        assert_eq!(one(|e| e.add(Reg::X0, Reg::X1, Reg::X2)), 0x8B020020);
    }

    #[test]
    fn sub_x3_x4_x5() {
        // sub x3, x4, x5  →  cb050083
        assert_eq!(one(|e| e.sub(Reg::X3, Reg::X4, Reg::X5)), 0xCB050083);
    }

    #[test]
    fn ldr_x0_x1_0() {
        // ldr x0, [x1]  →  f9400020
        assert_eq!(one(|e| e.ldr_imm(Reg::X0, Reg::X1, 0)), 0xF9400020);
    }

    #[test]
    fn ldr_x0_x1_8() {
        // ldr x0, [x1, #8]  →  f9400420
        assert_eq!(one(|e| e.ldr_imm(Reg::X0, Reg::X1, 8)), 0xF9400420);
    }

    #[test]
    fn str_x2_x3_16() {
        // str x2, [x3, #16]  →  f9000862
        assert_eq!(one(|e| e.str_imm(Reg::X2, Reg::X3, 16)), 0xF9000862);
    }

    #[test]
    fn ldr_rejects_misaligned() {
        let mut e = Encoder::new();
        assert_eq!(e.ldr_imm(Reg::X0, Reg::X1, 4), Err(EncodeError::OffsetMisaligned));
    }

    #[test]
    fn ldr_rejects_out_of_range() {
        // Max legal offset is 0xFFF * 8 = 32760.
        let mut e = Encoder::new();
        assert!(e.ldr_imm(Reg::X0, Reg::X1, 32760).is_ok());
        assert_eq!(
            e.ldr_imm(Reg::X0, Reg::X1, 32768),
            Err(EncodeError::ImmediateOutOfRange)
        );
    }

    #[test]
    fn b_zero() {
        // b .  (branch to self, offset 0)  →  14000000
        assert_eq!(one(|e| e.b(0)), 0x14000000);
    }

    #[test]
    fn b_forward_4() {
        // b <next>  offset +4 bytes  →  14000001
        assert_eq!(one(|e| e.b(4)), 0x14000001);
    }

    #[test]
    fn b_backward_4() {
        // b <prev>  offset -4 bytes  →  17ffffff
        assert_eq!(one(|e| e.b(-4)), 0x17FFFFFF);
    }

    #[test]
    fn b_rejects_misaligned() {
        let mut e = Encoder::new();
        assert_eq!(e.b(2), Err(EncodeError::OffsetMisaligned));
    }

    #[test]
    fn br_x0() {
        // br x0  →  d61f0000
        assert_eq!(one(|e| e.br(Reg::X0)), 0xD61F0000);
    }

    #[test]
    fn ret_x30() {
        // ret  (ret x30 is the default)  →  d65f03c0
        assert_eq!(one(|e| e.ret(Reg::X30)), 0xD65F03C0);
    }

    #[test]
    fn nop_encoding() {
        // nop  →  d503201f
        let mut e = Encoder::new();
        e.nop();
        assert_eq!(u32::from_le_bytes(e.as_bytes().try_into().unwrap()), 0xD503201F);
    }

    /// Full program: MOVZ X0, #42; RET.
    /// This is the canonical "return 42" function for the AAPCS64
    /// calling convention (X0 holds the return value). If a host
    /// disassembler confirms these exact bytes, we can trust the
    /// encoder output.
    #[test]
    fn return_42_program() {
        let mut e = Encoder::new();
        e.movz(Reg::X0, 42, MovShift::Lsl0).unwrap();
        e.ret(Reg::X30).unwrap();
        assert_eq!(
            e.as_bytes(),
            &[
                0x40, 0x05, 0x80, 0xD2, // movz x0, #42
                0xC0, 0x03, 0x5F, 0xD6, // ret
            ]
        );
    }

    #[test]
    fn register_out_of_range() {
        assert_eq!(Reg::new(31), None);
        assert!(Reg::new(30).is_some());
    }

    // ── Phase 2.3 encoders ──────────────────────────────────────────

    #[test]
    fn stp_prologue() {
        // stp x29, x30, [sp, #-16]!   →  a9bf7bfd
        assert_eq!(
            one(|e| e.stp_pre_indexed_64(Reg::X29, Reg::X30, Reg::SP, -16)),
            0xA9BF7BFD
        );
    }

    #[test]
    fn ldp_epilogue() {
        // ldp x29, x30, [sp], #16   →  a8c17bfd
        assert_eq!(
            one(|e| e.ldp_post_indexed_64(Reg::X29, Reg::X30, Reg::SP, 16)),
            0xA8C17BFD
        );
    }

    #[test]
    fn stp_rejects_misaligned() {
        let mut e = Encoder::new();
        assert_eq!(
            e.stp_pre_indexed_64(Reg::X0, Reg::X1, Reg::SP, -4),
            Err(EncodeError::OffsetMisaligned)
        );
    }

    #[test]
    fn bl_forward() {
        // bl +32 (8 instructions forward) → 0x94000008
        assert_eq!(one(|e| e.bl(32)), 0x94000008);
    }

    #[test]
    fn bl_backward() {
        // bl -4 → 0x97ffffff (2's complement imm26 = -1)
        assert_eq!(one(|e| e.bl(-4)), 0x97FFFFFF);
    }

    #[test]
    fn blr_x16() {
        // blr x16 → d63f0200
        assert_eq!(one(|e| e.blr(Reg::X16)), 0xD63F0200);
    }

    // ── Phase 4B encoders ───────────────────────────────────────────

    #[test]
    fn mul_x0_x0_x1() {
        // mul x0, x0, x1  →  9b017c00  (MADD Rd=0, Rn=0, Rm=1, Ra=XZR)
        assert_eq!(one(|e| e.mul(Reg::X0, Reg::X0, Reg::X1)), 0x9B017C00);
    }

    #[test]
    fn mul_x3_x4_x5() {
        // mul x3, x4, x5  →  9b057c83
        assert_eq!(one(|e| e.mul(Reg::X3, Reg::X4, Reg::X5)), 0x9B057C83);
    }

    #[test]
    fn sdiv_x0_x0_x1() {
        // sdiv x0, x0, x1  →  9ac10c00
        assert_eq!(one(|e| e.sdiv(Reg::X0, Reg::X0, Reg::X1)), 0x9AC10C00);
    }

    #[test]
    fn udiv_x0_x0_x1() {
        // udiv x0, x0, x1  →  9ac10800
        assert_eq!(one(|e| e.udiv(Reg::X0, Reg::X0, Reg::X1)), 0x9AC10800);
    }

    #[test]
    fn ldr_w_x0_x1_0() {
        // ldr w0, [x1]  →  b9400020
        assert_eq!(one(|e| e.ldr_w_imm(Reg::X0, Reg::X1, 0)), 0xB9400020);
    }

    #[test]
    fn ldr_w_x0_x1_4() {
        // ldr w0, [x1, #4]  →  b9400420
        assert_eq!(one(|e| e.ldr_w_imm(Reg::X0, Reg::X1, 4)), 0xB9400420);
    }

    #[test]
    fn str_w_x2_x3_8() {
        // str w2, [x3, #8]  →  b9000862
        assert_eq!(one(|e| e.str_w_imm(Reg::X2, Reg::X3, 8)), 0xB9000862);
    }

    // ── Phase 5 comparisons ─────────────────────────────────────────

    #[test]
    fn cmp_w_x0_x1() {
        // cmp w0, w1  →  6b01001f
        assert_eq!(one(|e| e.cmp_w(Reg::X0, Reg::X1)), 0x6B01001F);
    }

    #[test]
    fn cset_x0_eq() {
        // cset x0, eq  →  9a9f17e0
        assert_eq!(one(|e| e.cset(Reg::X0, Condition::Eq)), 0x9A9F17E0);
    }

    #[test]
    fn cset_x0_ne() {
        // cset x0, ne  →  9a9f07e0
        assert_eq!(one(|e| e.cset(Reg::X0, Condition::Ne)), 0x9A9F07E0);
    }

    #[test]
    fn cset_x0_lt() {
        // cset x0, lt  →  9a9fa7e0
        assert_eq!(one(|e| e.cset(Reg::X0, Condition::Lt)), 0x9A9FA7E0);
    }

    #[test]
    fn cset_x0_gt() {
        // cset x0, gt  →  9a9fd7e0
        assert_eq!(one(|e| e.cset(Reg::X0, Condition::Gt)), 0x9A9FD7E0);
    }

    #[test]
    fn condition_invert_roundtrips() {
        for c in [
            Condition::Eq, Condition::Ne,
            Condition::Hs, Condition::Lo,
            Condition::Hi, Condition::Ls,
            Condition::Ge, Condition::Lt,
            Condition::Gt, Condition::Le,
        ] {
            assert_eq!(c.invert().invert(), c);
        }
    }

    #[test]
    fn add_ext_uxtw_x0_x28_w1() {
        // add x0, x28, w1, uxtw  →  8b214380
        assert_eq!(
            one(|e| e.add_ext_uxtw(Reg::X0, Reg(28), Reg::X1)),
            0x8B214380
        );
    }

    #[test]
    fn ldr_w_rejects_misaligned() {
        let mut e = Encoder::new();
        assert_eq!(
            e.ldr_w_imm(Reg::X0, Reg::X1, 2),
            Err(EncodeError::OffsetMisaligned)
        );
    }
}
