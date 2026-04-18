//! Shared type definitions for the WASM lowerer: ValType, WasmOp,
//! LowerError, FnSig, and internal helper enums/structs.

use alloc::{vec::Vec};
use crate::{Condition, EncodeError, Reg, Vreg};

// ── Public types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
    V128,
}

impl ValType {
    pub(super) fn is_int(self) -> bool {
        matches!(self, ValType::I32 | ValType::I64)
    }
    pub(super) fn is_fp(self) -> bool {
        matches!(self, ValType::F32 | ValType::F64 | ValType::V128)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WasmOp {
    I32Const(i32),
    I32Add,
    I32Sub,
    I32Mul,
    I32DivS,
    I32DivU,
    I32Eq,
    I32Ne,
    I32LtS,
    I32GtS,
    I32LeS,
    I32GeS,
    I32LtU,
    I32GtU,
    I32LeU,
    I32GeU,
    I32Eqz,
    I64Const(i64),
    I64Add,
    I64Sub,
    I64Mul,
    I64Eqz,
    I64Eq,
    I64Ne,
    I64LtS,
    I64GtS,
    I64LtU,
    I64GtU,
    I64LeS,
    I64LeU,
    I64GeS,
    I64GeU,
    I64DivS,
    I64DivU,
    I64And,
    I64Or,
    I64Xor,
    I64Shl,
    I64ShrS,
    I64ShrU,
    I64Load(u32),
    I64Store(u32),
    I32WrapI64,
    I64ExtendI32S,
    I64ExtendI32U,
    I32And,
    I32Or,
    I32Xor,
    I32Shl,
    I32ShrS,
    I32ShrU,
    F32Const(f32),
    F32Add,
    F32Sub,
    F32Mul,
    F32Div,
    F32Eq,
    F32Ne,
    F32Lt,
    F32Gt,
    F32Le,
    F32Ge,
    F32Load(u32),
    F32Store(u32),
    F64Const(f64),
    F64Add,
    F64Sub,
    F64Mul,
    F64Div,
    F64Eq,
    F64Ne,
    F64Lt,
    F64Gt,
    F64Le,
    F64Ge,
    F64Load(u32),
    F64Store(u32),
    I32Extend8S,
    I32Extend16S,
    I64Extend8S,
    I64Extend16S,
    I64Extend32S,
    I32TruncF32S, I32TruncF32U,
    I32TruncF64S, I32TruncF64U,
    I64TruncF32S, I64TruncF32U,
    I64TruncF64S, I64TruncF64U,
    F32ConvertI32S, F32ConvertI32U,
    F32ConvertI64S, F32ConvertI64U,
    F64ConvertI32S, F64ConvertI32U,
    F64ConvertI64S, F64ConvertI64U,
    F32DemoteF64,
    F64PromoteF32,
    I32ReinterpretF32,
    I64ReinterpretF64,
    F32ReinterpretI32,
    F64ReinterpretI64,
    V128Const(u128),
    V128Load(u32),
    V128Store(u32),
    F32x4Add,
    F32x4Mul,
    F32x4ExtractLane(u8),
    F32x4Splat,
    I32x4Splat,
    I32x4Add,
    I32x4Sub,
    I32x4Mul,
    I32x4ExtractLane(u8),
    F64x2Add, F64x2Sub, F64x2Mul, F64x2Div,
    F64x2Min, F64x2Max,
    F64x2Sqrt, F64x2Abs, F64x2Neg,
    F64x2Splat,
    F64x2ExtractLane(u8),
    I8x16Add, I8x16Sub, I8x16Splat, I8x16ExtractLaneU(u8),
    /// **Folkering-extension**: signed i8 dot product. Maps to
    /// AArch64 SDOT (ARMv8.4-A FEAT_DotProd).
    ///
    /// Pops three v128: an i32x4 accumulator, then two i8x16 source
    /// vectors. Splits each source into four 4-byte chunks; for each
    /// of the four output lanes, computes the i8 dot product of the
    /// matching chunks and adds it to the accumulator lane.
    /// Pushes the resulting i32x4 v128.
    ///
    /// One SDOT = 16 i8 multiplies + 12 i32 adds in a single
    /// Cortex-A76 cycle. The fastest path to int8 quantised matmul
    /// on Pi 5.
    I32x4DotI8x16Signed,
    /// **Folkering-extension**: unsigned variant — UDOT. Same shape
    /// as `I32x4DotI8x16Signed` but treats sources as u8.
    I32x4DotI8x16Unsigned,
    I16x8Add, I16x8Sub, I16x8Mul, I16x8Splat, I16x8ExtractLaneU(u8),
    F32x4Sub,
    F32x4Div,
    F32x4Fma,
    F32x4HorizontalSum,
    F32x4Max,
    F32x4Min,
    F32x4Abs,
    F32x4Neg,
    F32x4Sqrt,
    F32x4Eq,
    F32x4Gt,
    V128Bitselect,
    /// Pop and discard the top stack value.
    Drop,
    /// `select(val_true, val_false, i32_cond)` — conditional pick.
    /// Pops cond (i32), val_false, val_true; pushes val_true if
    /// cond != 0, else val_false. Both vals must be the same type.
    Select,
    LocalGet(u32),
    LocalSet(u32),
    /// Copy top of stack into local without popping.
    LocalTee(u32),
    /// `global.get idx` — push the current value of module-global `idx`
    /// onto the operand stack. Lowered as a load from the globals area
    /// at the top of linear memory (see `wasm_lower::globals`).
    GlobalGet(u32),
    /// `global.set idx` — pop operand stack, store into module-global
    /// `idx`. The lowerer enforces the global is mutable at parse time.
    GlobalSet(u32),
    Block,
    Loop,
    Br(u32),
    BrIf(u32),
    If,
    Else,
    Call(u32),
    CallIndirect(u32),
    Return,
    End,
    I32Load(u32),
    I32Store(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowerError {
    StackOverflow,
    StackUnderflow,
    StackNotSingleton,
    LocalOutOfRange,
    TooManyLocals,
    LabelOutOfRange,
    UnbalancedEnd,
    BranchOutOfRange,
    ElseWithoutIf,
    CallTargetMissing,
    /// `global.get` / `global.set` referenced an index beyond the
    /// module's declared global count.
    GlobalOutOfRange,
    /// `global.set` attempted on a const global.
    GlobalNotMutable,
    /// Module declares more globals than the reserved area can hold
    /// (GLOBAL_AREA_SIZE / 8 slots).
    TooManyGlobals,
    MemoryNotConfigured,
    TableNotConfigured,
    IndirectTypeMissing,
    IndirectArityUnsupported,
    IndirectTypeUnsupported,
    CallArityUnsupported,
    CallTypeUnsupported,
    V128LocalsUnsupported,
    V128ReturnUnsupported,
    TypeMismatch {
        expected: ValType,
        got: ValType,
    },
    TypedStackOverflow(ValType),
    Encode(EncodeError),
}

impl From<EncodeError> for LowerError {
    fn from(e: EncodeError) -> Self { LowerError::Encode(e) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FnSig {
    pub params: Vec<ValType>,
    pub result: Option<ValType>,
}

// ── Internal helper enums ───────────────────────────────────────────

#[derive(Clone, Copy)]
pub(super) enum BinOp {
    Add, Sub, Mul, DivS, DivU,
    And, Or, Xor, Shl, ShrS, ShrU,
    Cmp(Condition),
}

#[derive(Clone, Copy)]
pub(super) enum FBinOp { Add, Sub, Mul, Div }

#[derive(Clone, Copy)]
pub(super) enum I64Op {
    Add, Sub, Mul, DivS, DivU,
    And, Or, Xor, Shl, ShrS, ShrU,
}

#[derive(Clone, Copy)]
pub(super) enum ExtendWidth { B8, B16, B32 }

#[derive(Debug, Clone, Copy)]
pub(crate) enum LocalLoc {
    I32(Reg),
    I64(Reg),
    F32(Vreg),
    F64(Vreg),
    V128(Vreg),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum LabelKind {
    Block,
    Loop,
    If { cond_branch_pos: usize },
    IfElse { else_skip_pos: usize },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PendingPatch {
    B { pos: usize },
    CbnzW { pos: usize, rt: Reg },
}

#[derive(Debug)]
pub(crate) struct Label {
    pub(super) kind: LabelKind,
    pub(super) loop_start: Option<usize>,
    pub(super) pending: Vec<PendingPatch>,
    #[allow(dead_code)]
    pub(super) entry_depth: usize,
}

// ── Constants ───────────────────────────────────────────────────────

pub(super) const MAX_PRIMARY_INT: usize = 16;
pub(super) const MAX_CALLEE_SAVED_INT: u8 = 27;
pub(super) const MAX_I32_STACK: usize = MAX_PRIMARY_INT;
pub(super) const SPILL_SCRATCH_A: Reg = Reg(14);
pub(super) const SPILL_SCRATCH_B: Reg = Reg(15);
pub(super) const MAX_FRAME_SPILL: usize = 16;
pub(super) const SPILL_SLOT_BYTES: u32 = 8;
pub(super) const SPILL_AREA_BYTES: u32 = (MAX_FRAME_SPILL as u32) * SPILL_SLOT_BYTES;
pub(super) const MAX_F32_STACK: usize = 16;
pub(super) const FP_SPILL_SCRATCH_A: Vreg = Vreg(30);
pub(super) const FP_SPILL_SCRATCH_B: Vreg = Vreg(31);
pub(super) const MAX_FP_SPILL: usize = 8;
pub(super) const FP_SPILL_SLOT_BYTES: u32 = 16;
pub(super) const FP_SPILL_AREA_BYTES: u32 = (MAX_FP_SPILL as u32) * FP_SPILL_SLOT_BYTES;
// ── Globals area at top of linear memory ───────────────────────────
//
// WASM modules have a Global section containing `i32` / `i64` / `f32`
// / `f64` slots with optional mutability. Rust-compiled WASM almost
// always declares at least `__stack_pointer` (mut i32) so it can
// spill locals beyond the register file.
//
// We reserve the last `GLOBAL_AREA_SIZE` bytes of linear memory for
// globals. Each global occupies an 8-byte slot regardless of its
// actual width — we accept the waste to get 8-byte alignment for
// i64/f64 stores without branching on type.
//
// For our 64 KiB linear memory this gives us 32 global slots, which
// is well above what a well-shaped no_std Rust crate ever declares
// (typically 1-3).
pub const GLOBAL_AREA_SIZE: u32 = 256;
pub const GLOBAL_SLOT_BYTES: u32 = 8;
pub const MAX_GLOBALS: usize = (GLOBAL_AREA_SIZE / GLOBAL_SLOT_BYTES) as usize;

/// Conventional stack-pointer value for `__stack_pointer` if the
/// module's declared init value is larger than our memory. Grows
/// downward from this; `GLOBAL_AREA_SIZE` bytes above us are the
/// globals themselves (not the stack).
pub const STACK_POINTER_INIT_FALLBACK: u32 = 0xFF00; // for 64 KiB mem

pub const MAX_I32_LOCALS: usize = 9;
// F32 locals occupy V16..V(16+MAX-1). V0..V15 are operand-stack
// slots, V30/V31 are FP_SPILL_SCRATCH_{A,B}, so the largest safe
// upper bound is V29 ⇒ 14 local slots.
pub const MAX_F32_LOCALS: usize = 14;
pub const MAX_LOCALS: usize = MAX_I32_LOCALS;
pub(super) const LOCAL_I32_BASE_REG: u8 = 19;
pub(super) const LOCAL_F32_BASE_REG: u8 = 16;
pub(super) const MEM_BASE_REG: Reg = Reg(28);
