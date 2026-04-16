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
//!
//! # `no_std` mode
//!
//! The crate is `no_std`-compatible when built with
//! `--no-default-features`. This is used by Folkering OS userspace /
//! kernel integrations that don't link the Rust standard library.
//! All allocations go through `alloc` (which requires a global
//! allocator in the consuming crate). Under the default `std`
//! feature (tests, examples, host-side JIT tools) nothing changes.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;

pub mod wasm_lower;
pub mod wasm_module;
pub mod wasm_parse;
pub use wasm_lower::{FnSig, LowerError, Lowerer, ValType, WasmOp};
pub use wasm_module::{parse_module, FunctionBody};
pub use wasm_parse::{parse_function_body, ParseError};

/// A64 SIMD/FP register (V-bank, also known as S/D/Q depending on
/// access width). We use it in the S0..S31 32-bit form for f32
/// scalar arithmetic; wider accesses are Phase 10+ SIMD work.
///
/// Separate type from [`Reg`] because the V-bank is architecturally
/// distinct from the X/W bank — the 5-bit encoding field is the
/// same, but mixing them at the API level would silently produce
/// meaningless instructions (e.g. `ADD X0, S0, S1` isn't a real
/// instruction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vreg(pub u8);

impl Vreg {
    /// Construct a V-bank index 0..=31.
    pub fn new(idx: u8) -> Option<Self> {
        if idx <= 31 { Some(Vreg(idx)) } else { None }
    }

    #[inline(always)]
    pub(crate) fn enc(self) -> u32 { (self.0 as u32) & 0x1F }

    pub const S0:  Vreg = Vreg(0);
    pub const S1:  Vreg = Vreg(1);
    pub const S2:  Vreg = Vreg(2);
    pub const S3:  Vreg = Vreg(3);
    pub const S4:  Vreg = Vreg(4);
    pub const S5:  Vreg = Vreg(5);
    pub const S6:  Vreg = Vreg(6);
    pub const S7:  Vreg = Vreg(7);
    pub const S15: Vreg = Vreg(15);
}

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
    buf: Vec<u8>,
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

    /// Embed a raw 32-bit word in the code buffer as data (not an
    /// instruction). Used for literal pools — e.g. a v128.const
    /// materializes as 16 bytes of constant sandwiched between a
    /// forward B and a PC-relative LDR Q. Callers are responsible
    /// for branching over the data so it's never executed as code.
    pub fn emit_raw_word(&mut self, word: u32) {
        self.emit(word);
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

    /// ADD Xd, Xn, Wm, UXTW #shift — same as [`Encoder::add_ext_uxtw`]
    /// but with an optional left shift on the extended value
    /// (shift ∈ 0..=4). Used by `call_indirect` to compute a table
    /// entry address as `table_base + idx*16` in a single instruction
    /// (with `shift=4`).
    pub fn add_ext_uxtw_shifted(
        &mut self,
        rd: Reg,
        rn: Reg,
        rm: Reg,
        shift: u32,
    ) -> Result<(), EncodeError> {
        if shift > 4 { return Err(EncodeError::ImmediateOutOfRange); }
        let word = 0x8B20_4000u32
            | (rm.enc() << 16)
            | ((shift & 0x7) << 10)
            | (rn.enc() << 5)
            | rd.enc();
        self.emit(word);
        Ok(())
    }

    // ── Floating-point (single precision, Phase 9) ──────────────────
    //
    // All ops operate on the low 32 bits of a V-register (aka Sn).
    // Encodings follow ARM ARM C6.2 — "Floating-point Data-processing"
    // (1-source, 2-source) and "Floating-point<->Integer conversion".

    /// FMOV Sd, Wn — move a 32-bit integer bit-pattern from a W
    /// register into the low 32 bits of an S register. Bit-cast,
    /// not numeric conversion — matches WASM `f32.reinterpret_i32`.
    ///
    /// Encoding (C6.2.108, "General ↔ FP"):
    /// `0 0 011110 00 1 00 111 000000 Rn(5) Rd(5)`, base 0x1E270000.
    pub fn fmov_s_from_w(&mut self, sd: Vreg, wn: Reg) -> Result<(), EncodeError> {
        self.emit(0x1E27_0000u32 | (wn.enc() << 5) | sd.enc());
        Ok(())
    }

    /// FMOV Wd, Sn — inverse of [`Encoder::fmov_s_from_w`]. Used at
    /// function end to propagate an f32 result into X0 for AAPCS64
    /// callers that expect integer return conventions (like our
    /// Pi-side harness).
    ///
    /// Encoding (C6.2.108): base 0x1E260000.
    pub fn fmov_w_from_s(&mut self, wd: Reg, sn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E26_0000u32 | (sn.enc() << 5) | wd.enc());
        Ok(())
    }

    /// FADD Sd, Sn, Sm — single-precision add.
    /// Encoding (C6.2.95, 2-source, ftype=00): base 0x1E202800.
    pub fn fadd_s(&mut self, sd: Vreg, sn: Vreg, sm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E20_2800u32 | (sm.enc() << 16) | (sn.enc() << 5) | sd.enc());
        Ok(())
    }

    /// FSUB Sd, Sn, Sm — single-precision subtract.
    /// Encoding (C6.2.128): base 0x1E203800.
    pub fn fsub_s(&mut self, sd: Vreg, sn: Vreg, sm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E20_3800u32 | (sm.enc() << 16) | (sn.enc() << 5) | sd.enc());
        Ok(())
    }

    /// FMUL Sd, Sn, Sm — single-precision multiply.
    /// Encoding (C6.2.112): base 0x1E200800.
    pub fn fmul_s(&mut self, sd: Vreg, sn: Vreg, sm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E20_0800u32 | (sm.enc() << 16) | (sn.enc() << 5) | sd.enc());
        Ok(())
    }

    /// FDIV Sd, Sn, Sm — single-precision divide.
    /// Encoding (C6.2.102): base 0x1E201800.
    pub fn fdiv_s(&mut self, sd: Vreg, sn: Vreg, sm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E20_1800u32 | (sm.enc() << 16) | (sn.enc() << 5) | sd.enc());
        Ok(())
    }

    /// FMOV Sd, Sn — SIMD/FP register-to-register move (single-precision).
    /// Used for `local.get` / `local.set` on f32 locals.
    ///
    /// Encoding (C6.2.106, FP register, ftype=00, opcode=0000):
    /// `0 0 011110 00 1 00000 010000 Rn Rd`, base 0x1E204000.
    pub fn fmov_s_s(&mut self, sd: Vreg, sn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E20_4000u32 | (sn.enc() << 5) | sd.enc());
        Ok(())
    }

    /// LDR Si, [Xn, #imm] — 32-bit SIMD/FP load (single-precision).
    /// `imm` is bytes, must be a multiple of 4; range 0..=16380.
    ///
    /// Encoding (C6.2.132, 32-bit FP variant, size=10, V=1, opc=01):
    /// `10 111 1 01 01 imm12(12) Rn(5) Rt(5)`, base 0xBD400000.
    pub fn ldr_s_imm(&mut self, sd: Vreg, xn: Reg, offset: u32) -> Result<(), EncodeError> {
        if offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm12 = offset >> 2;
        if imm12 > 0xFFF { return Err(EncodeError::ImmediateOutOfRange); }
        self.emit(0xBD40_0000u32 | (imm12 << 10) | (xn.enc() << 5) | sd.enc());
        Ok(())
    }

    /// STR Si, [Xn, #imm] — 32-bit SIMD/FP store.
    /// Encoding (C6.2.341): base 0xBD000000.
    pub fn str_s_imm(&mut self, sd: Vreg, xn: Reg, offset: u32) -> Result<(), EncodeError> {
        if offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm12 = offset >> 2;
        if imm12 > 0xFFF { return Err(EncodeError::ImmediateOutOfRange); }
        self.emit(0xBD00_0000u32 | (imm12 << 10) | (xn.enc() << 5) | sd.enc());
        Ok(())
    }

    /// FCMP Sn, Sm — compare two f32 values, set FPSR-propagated NZCV.
    /// Sets Z=1 for equal; different flag combinations encode GT/LT/
    /// unordered (NaN). CSET after FCMP yields the comparison result
    /// as a 0/1 integer.
    ///
    /// Encoding (C6.2.96): `0 0 011110 00 1 Rm(5) 00 1000 Rn(5) 00000`,
    /// base 0x1E202000.
    pub fn fcmp_s(&mut self, sn: Vreg, sm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E20_2000u32 | (sm.enc() << 16) | (sn.enc() << 5));
        Ok(())
    }

    // ── Floating-point (double precision, Phase 14) ─────────────────
    //
    // Uses the SAME V-register file as f32, but the full 64 bits (aka
    // Dn). Encoding identical to the f32 ops except `ftype = 01`
    // (bit 22 set), which selects double-precision math. INT↔FP moves
    // use `sf = 1` (64-bit int) + `ftype = 01`.

    /// FMOV Dd, Xn — move a 64-bit integer bit-pattern from an X
    /// register into a D register. Bit-cast, not numeric conversion —
    /// matches WASM `f64.reinterpret_i64`.
    ///
    /// Encoding (C6.2.108, sf=1, type=01, rmode=00, opcode=111):
    /// `1 0 011110 01 1 00 111 000000 Rn Rd`, base 0x9E670000.
    pub fn fmov_d_from_x(&mut self, dd: Vreg, xn: Reg) -> Result<(), EncodeError> {
        self.emit(0x9E67_0000u32 | (xn.enc() << 5) | dd.enc());
        Ok(())
    }

    /// FMOV Xd, Dn — inverse of [`Encoder::fmov_d_from_x`]. Used at
    /// function end to propagate an f64 result into X0 for AAPCS64
    /// callers that expect integer return conventions.
    ///
    /// Encoding (C6.2.108, sf=1, type=01, rmode=00, opcode=110):
    /// base 0x9E660000.
    pub fn fmov_x_from_d(&mut self, xd: Reg, dn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x9E66_0000u32 | (dn.enc() << 5) | xd.enc());
        Ok(())
    }

    /// FADD Dd, Dn, Dm — double-precision add.
    /// Encoding (C6.2.95, ftype=01): base 0x1E602800.
    pub fn fadd_d(&mut self, dd: Vreg, dn: Vreg, dm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E60_2800u32 | (dm.enc() << 16) | (dn.enc() << 5) | dd.enc());
        Ok(())
    }

    /// FSUB Dd, Dn, Dm — double-precision subtract.
    /// Encoding (C6.2.128, ftype=01): base 0x1E603800.
    pub fn fsub_d(&mut self, dd: Vreg, dn: Vreg, dm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E60_3800u32 | (dm.enc() << 16) | (dn.enc() << 5) | dd.enc());
        Ok(())
    }

    /// FMUL Dd, Dn, Dm — double-precision multiply.
    /// Encoding (C6.2.112, ftype=01): base 0x1E600800.
    pub fn fmul_d(&mut self, dd: Vreg, dn: Vreg, dm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E60_0800u32 | (dm.enc() << 16) | (dn.enc() << 5) | dd.enc());
        Ok(())
    }

    /// FDIV Dd, Dn, Dm — double-precision divide.
    /// Encoding (C6.2.102, ftype=01): base 0x1E601800.
    pub fn fdiv_d(&mut self, dd: Vreg, dn: Vreg, dm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E60_1800u32 | (dm.enc() << 16) | (dn.enc() << 5) | dd.enc());
        Ok(())
    }

    /// FMOV Dd, Dn — SIMD/FP register-to-register move (double).
    /// Used for `local.get` / `local.set` on f64 locals.
    ///
    /// Encoding (C6.2.106, ftype=01): base 0x1E604000.
    pub fn fmov_d_d(&mut self, dd: Vreg, dn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E60_4000u32 | (dn.enc() << 5) | dd.enc());
        Ok(())
    }

    /// LDR Di, [Xn, #imm] — 64-bit SIMD/FP load.
    /// `imm` is bytes, must be a multiple of 8; range 0..=32760.
    ///
    /// Encoding (C6.2.132, 64-bit FP variant, size=11, V=1, opc=01):
    /// `11 111 1 01 01 imm12(12) Rn(5) Rt(5)`, base 0xFD400000.
    pub fn ldr_d_imm(&mut self, dd: Vreg, xn: Reg, offset: u32) -> Result<(), EncodeError> {
        if offset & 0x7 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm12 = offset >> 3;
        if imm12 > 0xFFF { return Err(EncodeError::ImmediateOutOfRange); }
        self.emit(0xFD40_0000u32 | (imm12 << 10) | (xn.enc() << 5) | dd.enc());
        Ok(())
    }

    /// STR Di, [Xn, #imm] — 64-bit SIMD/FP store.
    /// Encoding (C6.2.341, size=11, V=1, opc=00): base 0xFD000000.
    pub fn str_d_imm(&mut self, dd: Vreg, xn: Reg, offset: u32) -> Result<(), EncodeError> {
        if offset & 0x7 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm12 = offset >> 3;
        if imm12 > 0xFFF { return Err(EncodeError::ImmediateOutOfRange); }
        self.emit(0xFD00_0000u32 | (imm12 << 10) | (xn.enc() << 5) | dd.enc());
        Ok(())
    }

    /// FCMP Dn, Dm — compare two f64 values, set NZCV.
    /// Encoding (C6.2.96, ftype=01): base 0x1E602000.
    pub fn fcmp_d(&mut self, dn: Vreg, dm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E60_2000u32 | (dm.enc() << 16) | (dn.enc() << 5));
        Ok(())
    }

    // ── SIMD / v128 (NEON) — Phase SIMD/1 ───────────────────────────
    //
    // These touch the 128-bit Q view of the V register file (Q0-Q31).
    // The S-width (low 32) and D-width (low 64) encoders above
    // address the SAME physical registers; a Q write overwrites both
    // halves. That matches WASM's v128 semantics, where v128.load to
    // Vn clobbers any scalar f32/f64 you thought was there.

    /// LDR Qt, [Xn, #imm] — 128-bit SIMD/FP load.
    /// `imm` is bytes, must be a multiple of 16; range 0..=65520.
    ///
    /// Encoding (C6.2.132, 128-bit FP variant, size=00, V=1, opc=11):
    /// `00 111 1 01 11 imm12(12) Rn(5) Rt(5)`, base 0x3DC00000.
    pub fn ldr_q_imm(&mut self, qt: Vreg, xn: Reg, offset: u32) -> Result<(), EncodeError> {
        if offset & 0xF != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm12 = offset >> 4;
        if imm12 > 0xFFF { return Err(EncodeError::ImmediateOutOfRange); }
        self.emit(0x3DC0_0000u32 | (imm12 << 10) | (xn.enc() << 5) | qt.enc());
        Ok(())
    }

    /// STR Qt, [Xn, #imm] — 128-bit SIMD/FP store.
    /// Same alignment rules as [`Encoder::ldr_q_imm`].
    ///
    /// Encoding (C6.2.341, size=00, V=1, opc=10): base 0x3D800000.
    pub fn str_q_imm(&mut self, qt: Vreg, xn: Reg, offset: u32) -> Result<(), EncodeError> {
        if offset & 0xF != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm12 = offset >> 4;
        if imm12 > 0xFFF { return Err(EncodeError::ImmediateOutOfRange); }
        self.emit(0x3D80_0000u32 | (imm12 << 10) | (xn.enc() << 5) | qt.enc());
        Ok(())
    }

    /// LDR Qt, [PC, #byte_offset] — 128-bit SIMD/FP PC-relative
    /// literal load. `byte_offset` is from the LDR instruction's
    /// own address to the 16-byte literal. Must be 4-aligned and
    /// fit in the signed 19-bit word-offset field (±1 MiB range).
    ///
    /// Used by the v128.const lowering to pull an arbitrary
    /// 16-byte constant out of a local literal pool emitted in the
    /// function body (jumped over at runtime via a preceding `B`).
    ///
    /// Encoding (C6.2.133 LDR literal, SIMD&FP, 128-bit):
    /// `10 011 1 0 0 imm19(19) Rt(5)`, base 0x9C000000.
    /// `imm19` is the byte_offset scaled by 4 (word-relative).
    pub fn ldr_q_literal(&mut self, qt: Vreg, byte_offset: i32) -> Result<(), EncodeError> {
        if byte_offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm19 = byte_offset >> 2;
        if !(-(1 << 18)..(1 << 18)).contains(&imm19) {
            return Err(EncodeError::ImmediateOutOfRange);
        }
        let imm19_enc = (imm19 as u32) & 0x7_FFFF;
        self.emit(0x9C00_0000u32 | (imm19_enc << 5) | qt.enc());
        Ok(())
    }

    /// FADD Vd.4S, Vn.4S, Vm.4S — 4-lane f32 add.
    ///
    /// Encoding (C6.2.95 AdvSIMD vector, U=0, Q=1, sz=0):
    /// `0 Q 0 0 1 1 1 0 0 sz 1 Rm 1 1 0 1 0 1 Rn Rd`, base 0x4E20D400.
    /// Matches WASM `f32x4.add` exactly — element-wise, IEEE-754 per
    /// lane, no cross-lane carry.
    pub fn fadd_4s(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x4E20_D400u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// FMUL Vd.4S, Vn.4S, Vm.4S — 4-lane f32 multiply.
    ///
    /// Encoding (C6.2.112 AdvSIMD vector, U=1, Q=1, sz=0):
    /// `0 Q 1 0 1 1 1 0 0 sz 1 Rm 1 1 0 1 1 1 Rn Rd`, base 0x6E20DC00.
    /// Matches WASM `f32x4.mul` — the building block for GEMM
    /// inner loops once we stitch in enough of the SIMD ISA.
    pub fn fmul_4s(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x6E20_DC00u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// DUP Sd, Vn.S[lane] — pull one 32-bit lane of Vn into a scalar
    /// Sd. Upper bits of Vd are zeroed. This is the lowering for
    /// WASM `f32x4.extract_lane N` — consumes a V128 slot, produces
    /// an F32 slot that the existing f32 arith / cmp / store paths
    /// consume unchanged.
    ///
    /// Encoding (C6.2.73 DUP (element, scalar)):
    /// `01 0111110 imm5(5) 00000 1 Rn(5) Rd(5)`, base 0x5E000400.
    /// For 32-bit lanes, imm5 = `lane:2 100` (low 3 bits fixed to
    /// 100, bits 4:3 hold the lane index). `lane` is 0..=3.
    pub fn dup_s_from_v_s_lane(
        &mut self,
        sd: Vreg,
        vn: Vreg,
        lane: u8,
    ) -> Result<(), EncodeError> {
        if lane >= 4 { return Err(EncodeError::ImmediateOutOfRange); }
        // imm5 = (lane << 3) | 0b100
        let imm5 = ((lane as u32) << 3) | 0b100;
        self.emit(0x5E00_0400u32 | (imm5 << 16) | (vn.enc() << 5) | sd.enc());
        Ok(())
    }

    /// DUP Vd.4S, Vn.S[0] — broadcast lane 0 of Vn to all 4 lanes of
    /// Vd. Lowering for `f32x4.splat`: the source f32 lives in Sn
    /// (low 32 bits of Vn lane 0), and this instruction replicates
    /// it across the full 128-bit register.
    ///
    /// Encoding (AdvSIMD DUP element, vector):
    /// `0 Q 0 0 1 1 1 0 0 0 0 imm5(5) 0 0 0 0 0 1 Rn(5) Rd(5)`
    /// For Q=1 (4S), imm5=00100 (32-bit lane 0), base 0x4E040400.
    pub fn dup_4s_from_vs_lane0(&mut self, vd: Vreg, vn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x4E04_0400u32 | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// DUP Vd.4S, Wn — replicate the low 32 bits of a W register
    /// across all 4 lanes of Vd. Lowering for `i32x4.splat`: the
    /// integer source is in the X-bank, this crosses the bank.
    ///
    /// Encoding (AdvSIMD DUP general):
    /// `0 Q 0 0 1 1 1 0 0 0 0 imm5(5) 0 0 0 0 1 1 Rn(5) Rd(5)`
    /// For Q=1 (4S), imm5=00100 (32-bit element), base 0x4E040C00.
    pub fn dup_4s_from_w(&mut self, vd: Vreg, wn: Reg) -> Result<(), EncodeError> {
        self.emit(0x4E04_0C00u32 | (wn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// ADD Vd.4S, Vn.4S, Vm.4S — lane-wise i32 add (no float).
    ///
    /// Encoding (C6.2.1 AdvSIMD 3-same, U=0, size=10, opcode=10000):
    /// `0 Q 0 0 1 1 1 0 size 1 Rm 1 0 0 0 0 1 Rn Rd`, base 0x4EA08400.
    pub fn add_4s_vector(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x4EA0_8400u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// SUB Vd.4S, Vn.4S, Vm.4S — lane-wise i32 subtract.
    /// Encoding (same as ADD but U=1): base 0x6EA08400.
    pub fn sub_4s_vector(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x6EA0_8400u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// MUL Vd.4S, Vn.4S, Vm.4S — lane-wise i32 multiply (low 32 bits
    /// of product). Unlike FMUL this is plain integer mul.
    ///
    /// Encoding (C6.2.180 AdvSIMD 3-same, U=0, size=10, opcode=10011):
    /// bits 15:10 = 100111, base 0x4EA09C00.
    pub fn mul_4s_vector(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x4EA0_9C00u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// UMOV Wd, Vn.S[lane] — extract one 32-bit lane of Vn into
    /// general register Wd. Lowering for `i32x4.extract_lane N`.
    /// Zero-extends into the full X register.
    ///
    /// Encoding (C6.2.312 AdvSIMD copy UMOV general):
    /// `0 Q 0 0 1 1 1 0 0 0 0 imm5(5) 0 0 1 1 1 1 Rn(5) Rd(5)`
    /// For 32-bit S lanes, Q=0 (destination is W), imm5 = lane<<3 | 0b100.
    /// Base 0x0E003C00.
    pub fn umov_w_from_vs_lane(
        &mut self,
        wd: Reg,
        vn: Vreg,
        lane: u8,
    ) -> Result<(), EncodeError> {
        if lane >= 4 { return Err(EncodeError::ImmediateOutOfRange); }
        let imm5 = ((lane as u32) << 3) | 0b100;
        self.emit(0x0E00_3C00u32 | (imm5 << 16) | (vn.enc() << 5) | wd.enc());
        Ok(())
    }

    /// FMLA Vd.4S, Vn.4S, Vm.4S — fused multiply-add:
    /// `Vd = Vd + Vn * Vm`, per lane, single rounding.
    /// This is the matmul primitive — one instruction per inner-
    /// loop element, ~2× faster than separate FMUL + FADD and also
    /// more accurate (no intermediate rounding).
    ///
    /// Encoding (C6.2.104 AdvSIMD 3-same, U=0, sz=0, opcode=11001):
    /// `0 Q 0 0 1 1 1 0 0 sz 1 Rm 1 1 0 0 1 1 Rn Rd`, base 0x4E20CC00.
    pub fn fmla_4s(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x4E20_CC00u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// FSUB Vd.4S, Vn.4S, Vm.4S — lane-wise f32 subtract.
    ///
    /// Encoding (C6.2.128 AdvSIMD vector, U=0, bit 23 set for sub):
    /// base 0x4EA0D400.
    pub fn fsub_4s(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x4EA0_D400u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// FDIV Vd.4S, Vn.4S, Vm.4S — lane-wise f32 divide.
    ///
    /// Encoding (C6.2.102 AdvSIMD vector, U=1, opcode=11111):
    /// `0 Q 1 0 1 1 1 0 0 sz 1 Rm 1 1 1 1 1 1 Rn Rd`, base 0x6E20FC00.
    pub fn fdiv_4s(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x6E20_FC00u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// FADDP Vd.4S, Vn.4S, Vm.4S — pairwise f32 add across 4S
    /// vectors. Result layout: [Vn[0]+Vn[1], Vn[2]+Vn[3],
    /// Vm[0]+Vm[1], Vm[2]+Vm[3]]. Half of a horizontal reduction
    /// (see `faddp_s_from_2s_scalar` for the second half).
    ///
    /// Encoding (AdvSIMD 3-same, U=1, sz=0, opcode=11010):
    /// `0 Q 1 0 1 1 1 0 0 sz 1 Rm 1 1 0 1 0 1 Rn Rd`, base 0x6E20D400.
    pub fn faddp_4s(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x6E20_D400u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// FADDP Sd, Vn.2S — scalar pair-wise add of the low two f32
    /// lanes of Vn. Second stage of a horizontal reduction:
    /// applied to a vector shaped [a+b, c+d, _, _] gives scalar
    /// `(a+b) + (c+d)`.
    ///
    /// Encoding (AdvSIMD scalar pairwise, sz=0):
    /// `01 1 11110 0 0 11000 0 110110 Rn Rd`, base 0x7E30D800.
    pub fn faddp_s_from_2s_scalar(&mut self, sd: Vreg, vn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x7E30_D800u32 | (vn.enc() << 5) | sd.enc());
        Ok(())
    }

    /// FMAX Vd.4S, Vn.4S, Vm.4S — lane-wise maximum of two f32x4.
    /// Single-instruction ReLU when paired with a zero splat.
    ///
    /// Encoding (C6.2.111 AdvSIMD 3-same, U=0, sz=0, opcode=11110):
    /// `0 Q 0 0 1 1 1 0 0 sz 1 Rm 1 1 1 1 0 1 Rn Rd`, base 0x4E20F400.
    pub fn fmax_4s(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x4E20_F400u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// FMIN Vd.4S, Vn.4S, Vm.4S — lane-wise minimum of two f32x4.
    /// Pairs with FMAX to build `clamp(x, lo, hi) = min(max(x, lo), hi)`.
    ///
    /// Encoding (same family, bit 23 set): base 0x4EA0F400.
    pub fn fmin_4s(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x4EA0_F400u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// FABS Vd.4S, Vn.4S — lane-wise absolute value.
    ///
    /// Encoding (C6.2.90 AdvSIMD, U=0, sz=0, opcode=11111):
    /// `0 Q 0 0 1 1 1 0 1 sz 1 0 0 0 0 0 1 1 1 1 1 0 Rn Rd`,
    /// base 0x4EA0F800.
    pub fn fabs_4s(&mut self, vd: Vreg, vn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x4EA0_F800u32 | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// FNEG Vd.4S, Vn.4S — lane-wise arithmetic negation.
    ///
    /// Encoding (same family as FABS, U=1): base 0x6EA0F800.
    pub fn fneg_4s(&mut self, vd: Vreg, vn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x6EA0_F800u32 | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// FSQRT Vd.4S, Vn.4S — lane-wise square root.
    /// Essential for L2-style normalization layers.
    ///
    /// Encoding (C6.2.127 AdvSIMD, U=1, sz=0): base 0x6EA1F800.
    pub fn fsqrt_4s(&mut self, vd: Vreg, vn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x6EA1_F800u32 | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// FCMEQ Vd.4S, Vn.4S, Vm.4S — lane-wise FP equality compare.
    /// Writes all-one-bits to each lane where `Vn[i] == Vm[i]`, and
    /// all-zero-bits elsewhere. The result is a proper bitmask
    /// usable with BSL / bitselect.
    ///
    /// Encoding (AdvSIMD 3-same, U=0, sz=0, opcode=11100):
    /// `0 Q 0 0 1 1 1 0 0 sz 1 Rm 1 1 1 0 0 1 Rn Rd`, base 0x4E20E400.
    pub fn fcmeq_4s(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x4E20_E400u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// FCMGT Vd.4S, Vn.4S, Vm.4S — lane-wise FP greater-than.
    /// All-one-bits in each lane where `Vn[i] > Vm[i]`, else zero.
    /// (For `<`, swap the operands at the lowerer level.)
    ///
    /// Encoding (U=1, bit 23 set): base 0x6EA0E400.
    pub fn fcmgt_4s(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x6EA0_E400u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// BSL Vd.16B, Vn.16B, Vm.16B — bitwise select:
    /// `Vd[i] = (Vd[i] AND Vn[i]) OR (NOT Vd[i] AND Vm[i])`.
    /// Vd is BOTH an input (the mask) and the result. When Vd's
    /// bit is 1, the output takes Vn's bit; when Vd's bit is 0,
    /// the output takes Vm's bit. Building block for every masked
    /// conditional computation.
    ///
    /// Encoding (AdvSIMD 3-same, U=1, size=01, opcode=00011):
    /// `0 Q 1 0 1 1 1 0 0 1 1 Rm 0 0 0 1 1 1 Rn Rd`, base 0x6E601C00.
    pub fn bsl_16b(&mut self, vd: Vreg, vn: Vreg, vm: Vreg) -> Result<(), EncodeError> {
        self.emit(0x6E60_1C00u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    /// ORR Vd.16B, Vn.16B, Vn.16B — bitwise OR of a register with
    /// itself = register copy. The canonical AdvSIMD `MOV Vd, Vn`
    /// instruction. Used by the bitselect lowering to move the
    /// BSL result from the mask's slot into the push slot where
    /// the operand stack expects it.
    ///
    /// Encoding (AdvSIMD 3-same, U=0, size=10, opcode=00011):
    /// `0 Q 0 0 1 1 1 0 1 0 1 Rm 0 0 0 1 1 1 Rn Rd`, base 0x4EA01C00.
    /// For MOV semantics, set Rm = Rn.
    pub fn orr_16b_vec(
        &mut self,
        vd: Vreg,
        vn: Vreg,
        vm: Vreg,
    ) -> Result<(), EncodeError> {
        self.emit(0x4EA0_1C00u32 | (vm.enc() << 16) | (vn.enc() << 5) | vd.enc());
        Ok(())
    }

    // ── Phase 15: conversions ───────────────────────────────────────
    //
    // Covers sign-extensions (WASM's extend8_s / extend16_s / extend32_s),
    // FP↔FP conversions (f32.demote_f64, f64.promote_f32), FP↔INT
    // conversions (FCVTZS/FCVTZU: round-toward-zero FP→INT; SCVTF/UCVTF:
    // INT→FP), and bit-cast reinterprets (reuse existing fmov_* pairs).
    //
    // Sign-extensions are SBFM aliases; FP-conversion encodings follow
    // ARM ARM C6.2.77–82 (sf + ftype selects width, rmode=11 = toward
    // zero, opcode picks signed/unsigned + direction).

    /// SXTB Wd, Wn — sign-extend low 8 bits of a W source into Wd.
    /// Alias for SBFM Wd, Wn, #0, #7. Encoding (C6.2.276): 0x13001C00.
    pub fn sxtb_w(&mut self, rd: Reg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x1300_1C00u32 | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// SXTH Wd, Wn — sign-extend low 16 bits. Alias for SBFM Wd, Wn, #0, #15.
    /// Encoding (C6.2.277): 0x13003C00.
    pub fn sxth_w(&mut self, rd: Reg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x1300_3C00u32 | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// SXTB Xd, Wn — sign-extend low 8 bits into full 64-bit Xd.
    /// Alias for SBFM Xd, Wn, #0, #7. Encoding: 0x93401C00.
    pub fn sxtb_x(&mut self, rd: Reg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x9340_1C00u32 | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// SXTH Xd, Wn — sign-extend low 16 bits into Xd.
    /// Alias for SBFM Xd, Wn, #0, #15. Encoding: 0x93403C00.
    pub fn sxth_x(&mut self, rd: Reg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x9340_3C00u32 | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    // Note: `sxtw` (sign-extend low 32 bits → Xd) already exists for
    // the Phase 12 i64.extend_i32_s lowering.

    /// FCVT Sd, Dn — f64 → f32 (WASM `f32.demote_f64`).
    /// Encoding (C6.2.77, ftype=01, opc=00): 0x1E624000.
    pub fn fcvt_s_d(&mut self, sd: Vreg, dn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E62_4000u32 | (dn.enc() << 5) | sd.enc());
        Ok(())
    }

    /// FCVT Dd, Sn — f32 → f64 (WASM `f64.promote_f32`).
    /// Encoding (C6.2.77, ftype=00, opc=01): 0x1E22C000.
    pub fn fcvt_d_s(&mut self, dd: Vreg, sn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E22_C000u32 | (sn.enc() << 5) | dd.enc());
        Ok(())
    }

    /// FCVTZS Wd, Sn — f32 → i32, round toward zero, signed.
    /// Encoding (C6.2.79, sf=0, ftype=00, rmode=11, opcode=000): 0x1E380000.
    pub fn fcvtzs_w_s(&mut self, rd: Reg, sn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E38_0000u32 | (sn.enc() << 5) | rd.enc());
        Ok(())
    }
    /// FCVTZS Wd, Dn — f64 → i32, toward zero. ftype=01: 0x1E780000.
    pub fn fcvtzs_w_d(&mut self, rd: Reg, dn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E78_0000u32 | (dn.enc() << 5) | rd.enc());
        Ok(())
    }
    /// FCVTZS Xd, Sn — f32 → i64, toward zero. sf=1, ftype=00: 0x9E380000.
    pub fn fcvtzs_x_s(&mut self, rd: Reg, sn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x9E38_0000u32 | (sn.enc() << 5) | rd.enc());
        Ok(())
    }
    /// FCVTZS Xd, Dn — f64 → i64, toward zero. sf=1, ftype=01: 0x9E780000.
    pub fn fcvtzs_x_d(&mut self, rd: Reg, dn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x9E78_0000u32 | (dn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// FCVTZU Wd, Sn — f32 → u32 (unsigned). Opcode=001: 0x1E390000.
    pub fn fcvtzu_w_s(&mut self, rd: Reg, sn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E39_0000u32 | (sn.enc() << 5) | rd.enc());
        Ok(())
    }
    /// FCVTZU Wd, Dn — f64 → u32: 0x1E790000.
    pub fn fcvtzu_w_d(&mut self, rd: Reg, dn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x1E79_0000u32 | (dn.enc() << 5) | rd.enc());
        Ok(())
    }
    /// FCVTZU Xd, Sn — f32 → u64: 0x9E390000.
    pub fn fcvtzu_x_s(&mut self, rd: Reg, sn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x9E39_0000u32 | (sn.enc() << 5) | rd.enc());
        Ok(())
    }
    /// FCVTZU Xd, Dn — f64 → u64: 0x9E790000.
    pub fn fcvtzu_x_d(&mut self, rd: Reg, dn: Vreg) -> Result<(), EncodeError> {
        self.emit(0x9E79_0000u32 | (dn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// SCVTF Sd, Wn — signed i32 → f32.
    /// Encoding (C6.2.259, sf=0, ftype=00, rmode=00, opcode=010): 0x1E220000.
    pub fn scvtf_s_w(&mut self, sd: Vreg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x1E22_0000u32 | (rn.enc() << 5) | sd.enc());
        Ok(())
    }
    /// SCVTF Dd, Wn — signed i32 → f64. ftype=01: 0x1E620000.
    pub fn scvtf_d_w(&mut self, dd: Vreg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x1E62_0000u32 | (rn.enc() << 5) | dd.enc());
        Ok(())
    }
    /// SCVTF Sd, Xn — signed i64 → f32. sf=1: 0x9E220000.
    pub fn scvtf_s_x(&mut self, sd: Vreg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x9E22_0000u32 | (rn.enc() << 5) | sd.enc());
        Ok(())
    }
    /// SCVTF Dd, Xn — signed i64 → f64: 0x9E620000.
    pub fn scvtf_d_x(&mut self, dd: Vreg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x9E62_0000u32 | (rn.enc() << 5) | dd.enc());
        Ok(())
    }

    /// UCVTF Sd, Wn — unsigned u32 → f32. Opcode=011: 0x1E230000.
    pub fn ucvtf_s_w(&mut self, sd: Vreg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x1E23_0000u32 | (rn.enc() << 5) | sd.enc());
        Ok(())
    }
    /// UCVTF Dd, Wn — unsigned u32 → f64: 0x1E630000.
    pub fn ucvtf_d_w(&mut self, dd: Vreg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x1E63_0000u32 | (rn.enc() << 5) | dd.enc());
        Ok(())
    }
    /// UCVTF Sd, Xn — unsigned u64 → f32: 0x9E230000.
    pub fn ucvtf_s_x(&mut self, sd: Vreg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x9E23_0000u32 | (rn.enc() << 5) | sd.enc());
        Ok(())
    }
    /// UCVTF Dd, Xn — unsigned u64 → f64: 0x9E630000.
    pub fn ucvtf_d_x(&mut self, dd: Vreg, rn: Reg) -> Result<(), EncodeError> {
        self.emit(0x9E63_0000u32 | (rn.enc() << 5) | dd.enc());
        Ok(())
    }

    /// AND Wd, Wn, Wm — 32-bit bitwise AND.
    ///
    /// Encoding (C6.2.13, sf=0): `0 00 01010 00 0 Rm(5) 000000 Rn(5) Rd(5)`.
    /// Base 0x0A000000.  Matches WASM i32.and semantics exactly (upper
    /// 32 bits of the hosting X register are written as zero).
    pub fn and_w(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0x0A00_0000u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// ORR Wd, Wn, Wm — 32-bit bitwise OR. Encoding C6.2.222, sf=0,
    /// base 0x2A000000.
    pub fn orr_w(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0x2A00_0000u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// EOR Wd, Wn, Wm — 32-bit bitwise XOR. Encoding C6.2.90, sf=0,
    /// base 0x4A000000.  WASM's `i32.xor`.
    pub fn eor_w(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0x4A00_0000u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// LSL Wd, Wn, Wm — 32-bit logical shift left by register.
    ///
    /// This is the LSLV form (variable-register shift), not the
    /// shift-by-immediate form. 32-bit variant uses the low 5 bits
    /// of Wm as the shift amount, matching WASM `i32.shl`'s
    /// "shift count mod 32" semantics.
    ///
    /// Encoding (C6.2.155, sf=0): base 0x1AC02000.
    pub fn lsl_w(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0x1AC0_2000u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// LSR Wd, Wn, Wm — 32-bit logical shift right (zero-fill).
    /// WASM `i32.shr_u`. Encoding C6.2.161, sf=0, base 0x1AC02400.
    pub fn lsr_w(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0x1AC0_2400u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// ASR Wd, Wn, Wm — 32-bit arithmetic shift right (sign-fill).
    /// WASM `i32.shr_s`. Encoding C6.2.17, sf=0, base 0x1AC02800.
    pub fn asr_w(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0x1AC0_2800u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// CMP Xn, Xm — 64-bit compare (alias for `SUBS XZR, Xn, Xm`).
    /// Used by i64 comparisons. Flags set from the full 64-bit
    /// subtraction so signed/unsigned comparisons work correctly
    /// across the entire i64 range.
    ///
    /// Encoding (C6.2.340, sf=1, Rd=XZR): base 0xEB000000.
    pub fn cmp_x(&mut self, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        let word = 0xEB00_0000u32
            | (rm.enc() << 16)
            | (rn.enc() << 5)
            | 31u32; // Rd = XZR
        self.emit(word);
        Ok(())
    }

    /// SXTW Xd, Wn — sign-extend 32-bit to 64-bit.
    /// Alias for `SBFM Xd, Xn, #0, #31`. Used by WASM's
    /// `i64.extend_i32_s`.
    ///
    /// Encoding (C6.2.264 — SBFM, sf=1, opc=00, N=1, immr=0, imms=31):
    /// `1 00 100110 1 000000 011111 Rn Rd`, base 0x93407C00.
    pub fn sxtw(&mut self, xd: Reg, wn: Reg) -> Result<(), EncodeError> {
        self.emit(0x9340_7C00u32 | (wn.enc() << 5) | xd.enc());
        Ok(())
    }

    // ── Integer bitops (64-bit X variants) ──────────────────────────
    //
    // Mirror the 32-bit W encoders but with sf=1 so shifts respect
    // mod 64 and the full X register is written. Used by i64 bit ops.

    /// AND Xd, Xn, Xm. Encoding base 0x8A000000.
    pub fn and_x(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0x8A00_0000u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// ORR Xd, Xn, Xm. Encoding base 0xAA000000.
    pub fn orr_x(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0xAA00_0000u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// EOR Xd, Xn, Xm. Encoding base 0xCA000000.
    pub fn eor_x(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0xCA00_0000u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// LSL Xd, Xn, Xm — 64-bit LSLV. Encoding base 0x9AC02000.
    pub fn lsl_x(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0x9AC0_2000u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// LSR Xd, Xn, Xm. Encoding base 0x9AC02400.
    pub fn lsr_x(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0x9AC0_2400u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
        Ok(())
    }

    /// ASR Xd, Xn, Xm. Encoding base 0x9AC02800.
    pub fn asr_x(&mut self, rd: Reg, rn: Reg, rm: Reg) -> Result<(), EncodeError> {
        self.emit(0x9AC0_2800u32 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc());
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

    /// B.cond — conditional branch to PC-relative offset.
    ///
    /// Encoding (C6.2.26): `0 1 0 1 0 1 0 0 imm19(19) 0 cond(4)`,
    /// base 0x5400_0000. `offset` is in *bytes*, must be 4-aligned,
    /// range ±1 MiB (signed 19-bit word offset). The condition is
    /// the same enum used by `CSET` so callers can reuse the
    /// signed/unsigned vocabulary (e.g. `Condition::Ls` for the
    /// bounds-check skip branch).
    pub fn b_cond(&mut self, cond: Condition, offset: i32) -> Result<(), EncodeError> {
        if offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm19 = offset >> 2;
        if !(-(1 << 18)..(1 << 18)).contains(&imm19) {
            return Err(EncodeError::ImmediateOutOfRange);
        }
        let word = 0x5400_0000u32
            | (((imm19 as u32) & 0x0007_FFFF) << 5)
            | (cond as u32);
        self.emit(word);
        Ok(())
    }

    /// Encode a B.cond instruction word directly (for patching).
    /// Same encoding as [`Encoder::b_cond`] but returns the u32 word
    /// instead of emitting it. Used by the lowerer to back-patch
    /// forward branches once their target becomes known.
    pub fn encode_b_cond(cond: Condition, offset: i32) -> Result<u32, EncodeError> {
        if offset & 0x3 != 0 { return Err(EncodeError::OffsetMisaligned); }
        let imm19 = offset >> 2;
        if !(-(1 << 18)..(1 << 18)).contains(&imm19) {
            return Err(EncodeError::ImmediateOutOfRange);
        }
        Ok(0x5400_0000u32
            | (((imm19 as u32) & 0x0007_FFFF) << 5)
            | (cond as u32))
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

    /// STP Xt1, Xt2, [Xn, #imm] (signed offset, no writeback).
    /// For saving callee-saved registers at fixed slots within
    /// a pre-allocated frame. Same alignment/range as pre-indexed.
    ///
    /// Encoding (C6.2.340 signed offset): base 0xA9000000.
    pub fn stp_offset_64(
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
        let word = 0xA900_0000u32
            | (((imm7 as u32) & 0x7F) << 15)
            | (rt2.enc() << 10)
            | (rn.enc() << 5)
            | rt.enc();
        self.emit(word);
        Ok(())
    }

    /// LDP Xt1, Xt2, [Xn, #imm] (signed offset, no writeback).
    /// For restoring callee-saved registers from fixed slots.
    ///
    /// Encoding (C6.2.133 signed offset): base 0xA9400000.
    pub fn ldp_offset_64(
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
        let word = 0xA940_0000u32
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
    fn b_cond_ls_forward_4() {
        // b.ls +4  →  imm19 = 1, cond = Ls (0b1001 = 9)
        // 0x54000000 | (1 << 5) | 9 = 0x54000029
        assert_eq!(one(|e| e.b_cond(Condition::Ls, 4)), 0x54000029);
    }

    #[test]
    fn b_cond_eq_zero() {
        // b.eq 0 — imm19 = 0, cond = Eq (0)
        assert_eq!(one(|e| e.b_cond(Condition::Eq, 0)), 0x54000000);
    }

    #[test]
    fn b_cond_ne_backward_8() {
        // b.ne -8 — imm19 = -2 (0x7FFFE in 19-bit), cond = Ne (1)
        // 0x54000000 | ((0x7FFFE & 0x7FFFF) << 5) | 1 = 0x54FFFFC1
        assert_eq!(one(|e| e.b_cond(Condition::Ne, -8)), 0x54FFFFC1);
    }

    #[test]
    fn b_cond_rejects_misaligned() {
        let mut e = Encoder::new();
        assert_eq!(e.b_cond(Condition::Eq, 3), Err(EncodeError::OffsetMisaligned));
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

    // ── Phase 9 FP encoders ─────────────────────────────────────────

    #[test]
    fn fmov_s_from_w_basic() {
        // fmov s0, w1  →  1e270020
        assert_eq!(
            one(|e| e.fmov_s_from_w(Vreg::S0, Reg::X1)),
            0x1E270020
        );
    }

    #[test]
    fn fmov_w_from_s_basic() {
        // fmov w0, s1  →  1e260020
        assert_eq!(
            one(|e| e.fmov_w_from_s(Reg::X0, Vreg::S1)),
            0x1E260020
        );
    }

    #[test]
    fn fadd_s_basic() {
        // fadd s0, s0, s1  →  1e212800
        assert_eq!(
            one(|e| e.fadd_s(Vreg::S0, Vreg::S0, Vreg::S1)),
            0x1E212800
        );
    }

    #[test]
    fn fsub_s_basic() {
        // fsub s0, s0, s1  →  1e213800
        assert_eq!(
            one(|e| e.fsub_s(Vreg::S0, Vreg::S0, Vreg::S1)),
            0x1E213800
        );
    }

    #[test]
    fn fmul_s_basic() {
        // fmul s0, s0, s1  →  1e210800
        assert_eq!(
            one(|e| e.fmul_s(Vreg::S0, Vreg::S0, Vreg::S1)),
            0x1E210800
        );
    }

    #[test]
    fn fdiv_s_basic() {
        // fdiv s0, s0, s1  →  1e211800
        assert_eq!(
            one(|e| e.fdiv_s(Vreg::S0, Vreg::S0, Vreg::S1)),
            0x1E211800
        );
    }

    #[test]
    fn fmov_s_s_basic() {
        // fmov s0, s1  →  1e204020
        assert_eq!(one(|e| e.fmov_s_s(Vreg::S0, Vreg::S1)), 0x1E204020);
    }

    #[test]
    fn ldr_s_basic() {
        // ldr s0, [x1]  →  bd400020
        assert_eq!(one(|e| e.ldr_s_imm(Vreg::S0, Reg::X1, 0)), 0xBD400020);
    }

    #[test]
    fn ldr_s_with_offset() {
        // ldr s2, [x3, #4]  →  bd400462
        assert_eq!(one(|e| e.ldr_s_imm(Vreg::S2, Reg::X3, 4)), 0xBD400462);
    }

    #[test]
    fn str_s_basic() {
        // str s0, [x1]  →  bd000020
        assert_eq!(one(|e| e.str_s_imm(Vreg::S0, Reg::X1, 0)), 0xBD000020);
    }

    #[test]
    fn fcmp_s_basic() {
        // fcmp s0, s1  →  1e212000
        assert_eq!(one(|e| e.fcmp_s(Vreg::S0, Vreg::S1)), 0x1E212000);
    }

    // ── Phase 14 f64 encoders ───────────────────────────────────────

    #[test]
    fn fmov_d_from_x_basic() {
        // fmov d0, x1  →  9e670020
        assert_eq!(
            one(|e| e.fmov_d_from_x(Vreg::S0, Reg::X1)),
            0x9E670020
        );
    }

    #[test]
    fn fmov_x_from_d_basic() {
        // fmov x0, d1  →  9e660020
        assert_eq!(
            one(|e| e.fmov_x_from_d(Reg::X0, Vreg::S1)),
            0x9E660020
        );
    }

    #[test]
    fn fadd_d_basic() {
        // fadd d0, d0, d1  →  1e612800
        assert_eq!(
            one(|e| e.fadd_d(Vreg::S0, Vreg::S0, Vreg::S1)),
            0x1E612800
        );
    }

    #[test]
    fn fsub_d_basic() {
        // fsub d0, d0, d1  →  1e613800
        assert_eq!(
            one(|e| e.fsub_d(Vreg::S0, Vreg::S0, Vreg::S1)),
            0x1E613800
        );
    }

    #[test]
    fn fmul_d_basic() {
        // fmul d0, d0, d1  →  1e610800
        assert_eq!(
            one(|e| e.fmul_d(Vreg::S0, Vreg::S0, Vreg::S1)),
            0x1E610800
        );
    }

    #[test]
    fn fdiv_d_basic() {
        // fdiv d0, d0, d1  →  1e611800
        assert_eq!(
            one(|e| e.fdiv_d(Vreg::S0, Vreg::S0, Vreg::S1)),
            0x1E611800
        );
    }

    #[test]
    fn fmov_d_d_basic() {
        // fmov d0, d1  →  1e604020
        assert_eq!(one(|e| e.fmov_d_d(Vreg::S0, Vreg::S1)), 0x1E604020);
    }

    #[test]
    fn ldr_d_basic() {
        // ldr d0, [x1]  →  fd400020
        assert_eq!(one(|e| e.ldr_d_imm(Vreg::S0, Reg::X1, 0)), 0xFD400020);
    }

    #[test]
    fn ldr_d_with_offset() {
        // ldr d2, [x3, #8]  →  fd400462
        assert_eq!(one(|e| e.ldr_d_imm(Vreg::S2, Reg::X3, 8)), 0xFD400462);
    }

    #[test]
    fn str_d_basic() {
        // str d0, [x1]  →  fd000020
        assert_eq!(one(|e| e.str_d_imm(Vreg::S0, Reg::X1, 0)), 0xFD000020);
    }

    #[test]
    fn fcmp_d_basic() {
        // fcmp d0, d1  →  1e612000
        assert_eq!(one(|e| e.fcmp_d(Vreg::S0, Vreg::S1)), 0x1E612000);
    }

    #[test]
    fn ldr_d_misaligned_offset_errors() {
        assert!(Encoder::new().ldr_d_imm(Vreg::S0, Reg::X1, 4).is_err());
    }

    // ── SIMD / v128 encoders ────────────────────────────────────────

    #[test]
    fn ldr_q_basic() {
        // ldr q0, [x1]  →  3dc00020
        assert_eq!(one(|e| e.ldr_q_imm(Vreg::S0, Reg::X1, 0)), 0x3DC00020);
    }

    #[test]
    fn ldr_q_offset_16() {
        // ldr q2, [x3, #16]  →  imm12 = 1, so base | (1<<10) | (3<<5) | 2
        // 0x3DC00000 | 0x400 | 0x60 | 2 = 0x3DC00462
        assert_eq!(one(|e| e.ldr_q_imm(Vreg::S2, Reg::X3, 16)), 0x3DC00462);
    }

    #[test]
    fn str_q_basic() {
        // str q0, [x1]  →  3d800020
        assert_eq!(one(|e| e.str_q_imm(Vreg::S0, Reg::X1, 0)), 0x3D800020);
    }

    #[test]
    fn ldr_q_misaligned_offset_errors() {
        assert!(Encoder::new().ldr_q_imm(Vreg::S0, Reg::X1, 8).is_err());
    }

    #[test]
    fn ldr_q_literal_back_16() {
        // ldr q0, [pc, #-16]  →  imm19 = -4 (two's complement 19-bit 0x7FFFC)
        // base 0x9C000000 | (0x7FFFC << 5) | 0 = 0x9CFFFF80
        assert_eq!(one(|e| e.ldr_q_literal(Vreg::S0, -16)), 0x9CFFFF80);
    }

    #[test]
    fn ldr_q_literal_forward_20() {
        // ldr q1, [pc, #20]  →  imm19 = 5
        // base 0x9C000000 | (5 << 5) | 1 = 0x9C0000A1
        assert_eq!(one(|e| e.ldr_q_literal(Vreg::S1, 20)), 0x9C0000A1);
    }

    #[test]
    fn ldr_q_literal_misaligned_rejected() {
        assert!(Encoder::new().ldr_q_literal(Vreg::S0, 3).is_err());
    }

    #[test]
    fn fadd_4s_basic() {
        // fadd v0.4s, v1.4s, v2.4s
        // base 0x4E20D400 | (2<<16) | (1<<5) | 0 = 0x4E22D420
        assert_eq!(one(|e| e.fadd_4s(Vreg::S0, Vreg::S1, Vreg::S2)), 0x4E22D420);
    }

    #[test]
    fn fmul_4s_basic() {
        // fmul v0.4s, v1.4s, v2.4s
        // base 0x6E20DC00 | (2<<16) | (1<<5) | 0 = 0x6E22DC20
        assert_eq!(one(|e| e.fmul_4s(Vreg::S0, Vreg::S1, Vreg::S2)), 0x6E22DC20);
    }

    #[test]
    fn dup_s_from_v_s_lane_0() {
        // dup s0, v1.s[0]  →  imm5 = 0b00100 = 4
        // 0x5E000400 | (4<<16) | (1<<5) | 0 = 0x5E040420
        assert_eq!(
            one(|e| e.dup_s_from_v_s_lane(Vreg::S0, Vreg::S1, 0)),
            0x5E040420
        );
    }

    #[test]
    fn dup_s_from_v_s_lane_3() {
        // dup s0, v1.s[3]  →  imm5 = 0b11100 = 28
        // 0x5E000400 | (28<<16) | (1<<5) | 0 = 0x5E1C0420
        assert_eq!(
            one(|e| e.dup_s_from_v_s_lane(Vreg::S0, Vreg::S1, 3)),
            0x5E1C0420
        );
    }

    #[test]
    fn dup_s_lane_out_of_range_rejected() {
        assert!(Encoder::new().dup_s_from_v_s_lane(Vreg::S0, Vreg::S0, 4).is_err());
    }

    #[test]
    fn dup_4s_from_vs_lane0_basic() {
        // dup v0.4s, v1.s[0]
        // base 0x4E040400 | (1<<5) | 0 = 0x4E040420
        assert_eq!(
            one(|e| e.dup_4s_from_vs_lane0(Vreg::S0, Vreg::S1)),
            0x4E040420
        );
    }

    #[test]
    fn dup_4s_from_w_basic() {
        // dup v0.4s, w1
        // base 0x4E040C00 | (1<<5) | 0 = 0x4E040C20
        assert_eq!(one(|e| e.dup_4s_from_w(Vreg::S0, Reg::X1)), 0x4E040C20);
    }

    #[test]
    fn add_4s_vector_basic() {
        // add v0.4s, v1.4s, v2.4s
        // base 0x4EA08400 | (2<<16) | (1<<5) | 0 = 0x4EA28420
        assert_eq!(
            one(|e| e.add_4s_vector(Vreg::S0, Vreg::S1, Vreg::S2)),
            0x4EA28420
        );
    }

    #[test]
    fn sub_4s_vector_basic() {
        // sub v0.4s, v1.4s, v2.4s
        // base 0x6EA08400 | (2<<16) | (1<<5) | 0 = 0x6EA28420
        assert_eq!(
            one(|e| e.sub_4s_vector(Vreg::S0, Vreg::S1, Vreg::S2)),
            0x6EA28420
        );
    }

    #[test]
    fn mul_4s_vector_basic() {
        // mul v0.4s, v1.4s, v2.4s
        // base 0x4EA09C00 | (2<<16) | (1<<5) | 0 = 0x4EA29C20
        assert_eq!(
            one(|e| e.mul_4s_vector(Vreg::S0, Vreg::S1, Vreg::S2)),
            0x4EA29C20
        );
    }

    #[test]
    fn umov_w_from_vs_lane_0() {
        // umov w0, v1.s[0]
        // imm5 = 0b00100 = 4
        // base 0x0E003C00 | (4<<16) | (1<<5) | 0 = 0x0E043C20
        assert_eq!(
            one(|e| e.umov_w_from_vs_lane(Reg::X0, Vreg::S1, 0)),
            0x0E043C20
        );
    }

    #[test]
    fn umov_w_from_vs_lane_3() {
        // umov w0, v1.s[3]
        // imm5 = 0b11100 = 28
        // base 0x0E003C00 | (28<<16) | (1<<5) | 0 = 0x0E1C3C20
        assert_eq!(
            one(|e| e.umov_w_from_vs_lane(Reg::X0, Vreg::S1, 3)),
            0x0E1C3C20
        );
    }

    #[test]
    fn fmla_4s_basic() {
        // fmla v0.4s, v1.4s, v2.4s
        // base 0x4E20CC00 | (2<<16) | (1<<5) | 0 = 0x4E22CC20
        assert_eq!(one(|e| e.fmla_4s(Vreg::S0, Vreg::S1, Vreg::S2)), 0x4E22CC20);
    }

    #[test]
    fn fsub_4s_basic() {
        // fsub v0.4s, v1.4s, v2.4s
        // base 0x4EA0D400 | (2<<16) | (1<<5) | 0 = 0x4EA2D420
        assert_eq!(one(|e| e.fsub_4s(Vreg::S0, Vreg::S1, Vreg::S2)), 0x4EA2D420);
    }

    #[test]
    fn fdiv_4s_basic() {
        // fdiv v0.4s, v1.4s, v2.4s
        // base 0x6E20FC00 | (2<<16) | (1<<5) | 0 = 0x6E22FC20
        assert_eq!(one(|e| e.fdiv_4s(Vreg::S0, Vreg::S1, Vreg::S2)), 0x6E22FC20);
    }

    #[test]
    fn faddp_4s_basic() {
        // faddp v0.4s, v1.4s, v2.4s
        // base 0x6E20D400 | (2<<16) | (1<<5) | 0 = 0x6E22D420
        assert_eq!(one(|e| e.faddp_4s(Vreg::S0, Vreg::S1, Vreg::S2)), 0x6E22D420);
    }

    #[test]
    fn faddp_s_from_2s_scalar_basic() {
        // faddp s0, v1.2s
        // base 0x7E30D800 | (1<<5) | 0 = 0x7E30D820
        assert_eq!(
            one(|e| e.faddp_s_from_2s_scalar(Vreg::S0, Vreg::S1)),
            0x7E30D820
        );
    }

    #[test]
    fn fmax_4s_basic() {
        // fmax v0.4s, v1.4s, v2.4s
        // base 0x4E20F400 | (2<<16) | (1<<5) | 0 = 0x4E22F420
        assert_eq!(one(|e| e.fmax_4s(Vreg::S0, Vreg::S1, Vreg::S2)), 0x4E22F420);
    }

    #[test]
    fn fmin_4s_basic() {
        // fmin v0.4s, v1.4s, v2.4s
        // base 0x4EA0F400 | (2<<16) | (1<<5) | 0 = 0x4EA2F420
        assert_eq!(one(|e| e.fmin_4s(Vreg::S0, Vreg::S1, Vreg::S2)), 0x4EA2F420);
    }

    #[test]
    fn fabs_4s_basic() {
        // fabs v0.4s, v1.4s
        // base 0x4EA0F800 | (1<<5) | 0 = 0x4EA0F820
        assert_eq!(one(|e| e.fabs_4s(Vreg::S0, Vreg::S1)), 0x4EA0F820);
    }

    #[test]
    fn fneg_4s_basic() {
        // fneg v0.4s, v1.4s
        // base 0x6EA0F800 | (1<<5) | 0 = 0x6EA0F820
        assert_eq!(one(|e| e.fneg_4s(Vreg::S0, Vreg::S1)), 0x6EA0F820);
    }

    #[test]
    fn fsqrt_4s_basic() {
        // fsqrt v0.4s, v1.4s
        // base 0x6EA1F800 | (1<<5) | 0 = 0x6EA1F820
        assert_eq!(one(|e| e.fsqrt_4s(Vreg::S0, Vreg::S1)), 0x6EA1F820);
    }

    #[test]
    fn fcmeq_4s_basic() {
        // fcmeq v0.4s, v1.4s, v2.4s
        // base 0x4E20E400 | (2<<16) | (1<<5) | 0 = 0x4E22E420
        assert_eq!(one(|e| e.fcmeq_4s(Vreg::S0, Vreg::S1, Vreg::S2)), 0x4E22E420);
    }

    #[test]
    fn fcmgt_4s_basic() {
        // fcmgt v0.4s, v1.4s, v2.4s
        // base 0x6EA0E400 | (2<<16) | (1<<5) | 0 = 0x6EA2E420
        assert_eq!(one(|e| e.fcmgt_4s(Vreg::S0, Vreg::S1, Vreg::S2)), 0x6EA2E420);
    }

    #[test]
    fn bsl_16b_basic() {
        // bsl v0.16b, v1.16b, v2.16b
        // base 0x6E601C00 | (2<<16) | (1<<5) | 0 = 0x6E621C20
        assert_eq!(one(|e| e.bsl_16b(Vreg::S0, Vreg::S1, Vreg::S2)), 0x6E621C20);
    }

    #[test]
    fn orr_16b_vec_mov() {
        // mov v0.16b, v1.16b  (disassembles as orr v0, v1, v1)
        // base 0x4EA01C00 | (1<<16) | (1<<5) | 0 = 0x4EA11C20
        assert_eq!(
            one(|e| e.orr_16b_vec(Vreg::S0, Vreg::S1, Vreg::S1)),
            0x4EA11C20
        );
    }

    // ── Phase 15 conversion encoders ────────────────────────────────

    #[test]
    fn sxtb_w_basic() {
        // sxtb w0, w1  →  13001c20
        assert_eq!(one(|e| e.sxtb_w(Reg::X0, Reg::X1)), 0x13001C20);
    }
    #[test]
    fn sxth_w_basic() {
        // sxth w0, w1  →  13003c20
        assert_eq!(one(|e| e.sxth_w(Reg::X0, Reg::X1)), 0x13003C20);
    }
    #[test]
    fn sxtb_x_basic() {
        // sxtb x0, w1  →  93401c20
        assert_eq!(one(|e| e.sxtb_x(Reg::X0, Reg::X1)), 0x93401C20);
    }
    #[test]
    fn sxth_x_basic() {
        // sxth x0, w1  →  93403c20
        assert_eq!(one(|e| e.sxth_x(Reg::X0, Reg::X1)), 0x93403C20);
    }

    #[test]
    fn fcvt_s_d_basic() {
        // fcvt s0, d1  →  1e624020
        assert_eq!(one(|e| e.fcvt_s_d(Vreg::S0, Vreg::S1)), 0x1E624020);
    }
    #[test]
    fn fcvt_d_s_basic() {
        // fcvt d0, s1  →  1e22c020
        assert_eq!(one(|e| e.fcvt_d_s(Vreg::S0, Vreg::S1)), 0x1E22C020);
    }

    #[test]
    fn fcvtzs_w_s_basic() {
        // fcvtzs w0, s1  →  1e380020
        assert_eq!(one(|e| e.fcvtzs_w_s(Reg::X0, Vreg::S1)), 0x1E380020);
    }
    #[test]
    fn fcvtzs_w_d_basic() {
        // fcvtzs w0, d1  →  1e780020
        assert_eq!(one(|e| e.fcvtzs_w_d(Reg::X0, Vreg::S1)), 0x1E780020);
    }
    #[test]
    fn fcvtzs_x_s_basic() {
        // fcvtzs x0, s1  →  9e380020
        assert_eq!(one(|e| e.fcvtzs_x_s(Reg::X0, Vreg::S1)), 0x9E380020);
    }
    #[test]
    fn fcvtzs_x_d_basic() {
        // fcvtzs x0, d1  →  9e780020
        assert_eq!(one(|e| e.fcvtzs_x_d(Reg::X0, Vreg::S1)), 0x9E780020);
    }
    #[test]
    fn fcvtzu_w_s_basic() {
        // fcvtzu w0, s1  →  1e390020
        assert_eq!(one(|e| e.fcvtzu_w_s(Reg::X0, Vreg::S1)), 0x1E390020);
    }
    #[test]
    fn fcvtzu_x_d_basic() {
        // fcvtzu x0, d1  →  9e790020
        assert_eq!(one(|e| e.fcvtzu_x_d(Reg::X0, Vreg::S1)), 0x9E790020);
    }

    #[test]
    fn scvtf_s_w_basic() {
        // scvtf s0, w1  →  1e220020
        assert_eq!(one(|e| e.scvtf_s_w(Vreg::S0, Reg::X1)), 0x1E220020);
    }
    #[test]
    fn scvtf_d_w_basic() {
        // scvtf d0, w1  →  1e620020
        assert_eq!(one(|e| e.scvtf_d_w(Vreg::S0, Reg::X1)), 0x1E620020);
    }
    #[test]
    fn scvtf_d_x_basic() {
        // scvtf d0, x1  →  9e620020
        assert_eq!(one(|e| e.scvtf_d_x(Vreg::S0, Reg::X1)), 0x9E620020);
    }
    #[test]
    fn ucvtf_s_x_basic() {
        // ucvtf s0, x1  →  9e230020
        assert_eq!(one(|e| e.ucvtf_s_x(Vreg::S0, Reg::X1)), 0x9E230020);
    }
    #[test]
    fn ucvtf_d_x_basic() {
        // ucvtf d0, x1  →  9e630020
        assert_eq!(one(|e| e.ucvtf_d_x(Vreg::S0, Reg::X1)), 0x9E630020);
    }

    // ── Phase 12 i64 encoders ───────────────────────────────────────

    #[test]
    fn cmp_x_basic() {
        // cmp x0, x1  →  eb01001f
        assert_eq!(one(|e| e.cmp_x(Reg::X0, Reg::X1)), 0xEB01001F);
    }

    #[test]
    fn sxtw_basic() {
        // sxtw x0, w1  →  93407c20
        assert_eq!(one(|e| e.sxtw(Reg::X0, Reg::X1)), 0x93407C20);
    }

    #[test]
    fn and_x_basic() {
        // and x0, x0, x1  →  8a010000
        assert_eq!(one(|e| e.and_x(Reg::X0, Reg::X0, Reg::X1)), 0x8A010000);
    }

    #[test]
    fn orr_x_basic() {
        // orr x0, x0, x1  →  aa010000
        assert_eq!(one(|e| e.orr_x(Reg::X0, Reg::X0, Reg::X1)), 0xAA010000);
    }

    #[test]
    fn eor_x_basic() {
        // eor x0, x0, x1  →  ca010000
        assert_eq!(one(|e| e.eor_x(Reg::X0, Reg::X0, Reg::X1)), 0xCA010000);
    }

    #[test]
    fn lsl_x_basic() {
        // lsl x0, x0, x1  →  9ac12000
        assert_eq!(one(|e| e.lsl_x(Reg::X0, Reg::X0, Reg::X1)), 0x9AC12000);
    }

    // ── Phase 8 bitops ──────────────────────────────────────────────

    #[test]
    fn and_w_basic() {
        // and w0, w0, w1  →  0a010000
        assert_eq!(one(|e| e.and_w(Reg::X0, Reg::X0, Reg::X1)), 0x0A010000);
    }

    #[test]
    fn orr_w_basic() {
        // orr w0, w0, w1  →  2a010000
        assert_eq!(one(|e| e.orr_w(Reg::X0, Reg::X0, Reg::X1)), 0x2A010000);
    }

    #[test]
    fn eor_w_basic() {
        // eor w0, w0, w1  →  4a010000
        assert_eq!(one(|e| e.eor_w(Reg::X0, Reg::X0, Reg::X1)), 0x4A010000);
    }

    #[test]
    fn lsl_w_basic() {
        // lsl w0, w0, w1  →  1ac12000
        assert_eq!(one(|e| e.lsl_w(Reg::X0, Reg::X0, Reg::X1)), 0x1AC12000);
    }

    #[test]
    fn lsr_w_basic() {
        // lsr w0, w0, w1  →  1ac12400
        assert_eq!(one(|e| e.lsr_w(Reg::X0, Reg::X0, Reg::X1)), 0x1AC12400);
    }

    #[test]
    fn asr_w_basic() {
        // asr w0, w0, w1  →  1ac12800
        assert_eq!(one(|e| e.asr_w(Reg::X0, Reg::X0, Reg::X1)), 0x1AC12800);
    }

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
    fn add_ext_uxtw_shifted_shift4() {
        // add x0, x28, w1, uxtw #4  →  8b215380
        assert_eq!(
            one(|e| e.add_ext_uxtw_shifted(Reg::X0, Reg(28), Reg::X1, 4)),
            0x8B215380
        );
    }

    #[test]
    fn add_ext_uxtw_shifted_shift0_matches_plain() {
        // shift=0 should produce identical bytes to add_ext_uxtw.
        assert_eq!(
            one(|e| e.add_ext_uxtw_shifted(Reg::X0, Reg(28), Reg::X1, 0)),
            one(|e| e.add_ext_uxtw(Reg::X0, Reg(28), Reg::X1)),
        );
    }

    #[test]
    fn add_ext_uxtw_shifted_rejects_overlarge() {
        assert!(Encoder::new().add_ext_uxtw_shifted(Reg::X0, Reg::X0, Reg::X0, 5).is_err());
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
