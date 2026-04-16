//! WASM → AArch64 lowering.
//!
//! Phases 2.0–2.2 in one module:
//!   * 2.0 — stack machine with `i32.const` + `i32.add/sub` + `end`
//!   * 2.1 — negative constants, locals (`local.get` / `local.set`)
//!   * 2.2 — structured control flow: `block` / `loop` / `br` / `br_if`
//!
//! Host-testable throughout — emits bytes, no execution yet.
//!
//! # Register strategy
//!
//! - **Operand stack** → X0..X15 by depth.  Stack[0] = X0, so a
//!   function that leaves one value on the stack at `end` has its
//!   return value naturally in the AAPCS64 result register.
//! - **Locals** → X19..X28 (up to 10 locals).  These are
//!   callee-saved under AAPCS64; a production-grade prologue would
//!   save them to the stack on entry.  Phase 2 emits raw code
//!   without a prologue, which is fine for isolated unit tests but
//!   needs addressing before we call into JITted code from Rust.
//! - **Scratch** → none yet; we don't need any beyond the operand
//!   stack for Phase 2.
//!
//! # Control flow
//!
//! WASM's `block` ... `end` and `loop` ... `end` are structured —
//! every branch target is lexically known.  We track an in-flight
//! **label stack** where each label remembers the code-generation
//! state we need to resolve branches:
//!
//! - `loop` labels record their *start* offset so backward branches
//!   can resolve immediately at `br`/`br_if` time.
//! - `block` labels record a **pending-patch list** of branch
//!   instructions emitted with placeholder offsets.  At `end` we
//!   know the block-end offset and rewrite every placeholder.

use alloc::{vec, vec::Vec};

use crate::{
    encode_b, encode_cbnz_w, encode_cbz_w, Condition, Encoder, EncodeError, MovShift, Reg, Vreg,
};

/// WASM value type carried on the operand stack. Each slot is tagged
/// so the lowerer can pick the right register bank (X vs V) and the
/// right instruction family (integer vs FP) for each op.
///
/// I32 and I64 share the X register bank — same 5-bit register field
/// addresses a 32-bit W-view or a 64-bit X-view of the same file.
/// F32/F64 similarly share V-bank. The lowerer uses the type tag to
/// pick the right instruction width (e.g. `add` vs `and_w`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
    /// WASM SIMD 128-bit vector. Lives in the V-register bank (as
    /// Qn, the 128-bit view of Vn). Sharing the same physical file
    /// as F32 and F64: all three claim a slot via `fp_depth`. A
    /// V128 write to Vn clobbers any lower-width scalar that might
    /// have been there — which matches WASM semantics (v128 ops
    /// redefine the full register anyway).
    V128,
}

impl ValType {
    /// True for integer-bank types (X/W register views).
    fn is_int(self) -> bool {
        matches!(self, ValType::I32 | ValType::I64)
    }
    /// True for SIMD/FP-bank types (V/S/D/Q register views).
    fn is_fp(self) -> bool {
        matches!(self, ValType::F32 | ValType::F64 | ValType::V128)
    }
}

/// Simplified WASM operator set.  Hand-constructed by callers, or
/// produced by [`crate::wasm_parse::parse_function_body`].
// Note: `Eq` dropped because F32Const carries an f32, and f32 only
// implements `PartialEq` due to NaN. Tests that compare `WasmOp`s
// use `assert_eq!` via PartialEq — which handles Eq-less enums fine
// as long as the test values aren't NaN.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WasmOp {
    /// Push a 32-bit constant onto the stack.  Negative values are
    /// fine — we lower them as u32 bit-patterns, which is consistent
    /// with WASM i32 semantics (upper bits undefined).
    I32Const(i32),
    /// Pop two, push their sum.
    I32Add,
    /// Pop two, push (left − right).
    I32Sub,
    /// Pop two, push (left × right).
    I32Mul,
    /// Pop two, push (left ÷ right) — signed. Divide-by-zero and
    /// INT_MIN / -1 both return 0 on A64; WASM spec says trap, but
    /// we defer trap semantics to Phase 4C+ when we have a runtime.
    I32DivS,
    /// Pop two, push (left ÷ right) — unsigned. Divide-by-zero returns 0.
    I32DivU,
    /// Pop two, push 1 if equal else 0.
    I32Eq,
    /// Pop two, push 1 if not equal else 0.
    I32Ne,
    /// Pop two, push 1 if `left < right` (signed) else 0.
    I32LtS,
    /// Pop two, push 1 if `left > right` (signed) else 0.
    I32GtS,
    /// Pop two, push 1 if `left ≤ right` (signed) else 0.
    I32LeS,
    /// Pop two, push 1 if `left ≥ right` (signed) else 0.
    I32GeS,
    /// Pop two, push 1 if `left < right` (unsigned) else 0.
    I32LtU,
    /// Pop two, push 1 if `left > right` (unsigned) else 0.
    I32GtU,
    /// Pop two, push 1 if `left ≤ right` (unsigned) else 0.
    I32LeU,
    /// Pop two, push 1 if `left ≥ right` (unsigned) else 0.
    I32GeU,
    /// Pop one, push 1 if value is zero else 0. Unary version of
    /// `I32Eq` with implicit zero right-hand side.
    I32Eqz,
    /// Push a 64-bit constant onto the stack.
    I64Const(i64),
    /// Pop two i64s, push their sum.
    I64Add,
    /// Pop two i64s, push (left − right).
    I64Sub,
    /// Pop two i64s, push (left × right).
    I64Mul,
    /// Pop one i64, push 1 (i32) if zero else 0.
    I64Eqz,
    /// Pop two i64s, push i32 1 if equal else 0.
    I64Eq,
    /// Pop two i64s, push i32 1 if not equal else 0.
    I64Ne,
    /// Pop two i64s, push i32 1 if left < right (signed) else 0.
    I64LtS,
    /// Pop two i64s, push i32 1 if left > right (signed) else 0.
    I64GtS,
    /// Pop two i64s, push i32 1 if left < right (unsigned) else 0.
    I64LtU,
    /// Pop two i64s, push i32 1 if left > right (unsigned) else 0.
    I64GtU,
    /// Pop two i64s, push i32 1 if left ≤ right (signed) else 0.
    I64LeS,
    /// Pop two i64s, push i32 1 if left ≤ right (unsigned) else 0.
    I64LeU,
    /// Pop two i64s, push i32 1 if left ≥ right (signed) else 0.
    I64GeS,
    /// Pop two i64s, push i32 1 if left ≥ right (unsigned) else 0.
    I64GeU,
    /// Pop two i64s, push left / right (signed, trunc toward zero).
    I64DivS,
    /// Pop two i64s, push left / right (unsigned).
    I64DivU,
    /// Pop two i64s, push bitwise AND.
    I64And,
    /// Pop two i64s, push bitwise OR.
    I64Or,
    /// Pop two i64s, push bitwise XOR.
    I64Xor,
    /// Pop two i64s, push `left << (right mod 64)`.
    I64Shl,
    /// Pop two i64s, push `left >> (right mod 64)` — signed.
    I64ShrS,
    /// Pop two i64s, push `left >> (right mod 64)` — unsigned.
    I64ShrU,
    /// Load i64 from linear memory — offset must be 8-aligned.
    I64Load(u32),
    /// Store i64 to linear memory — offset must be 8-aligned.
    I64Store(u32),
    /// Pop i64, push its low 32 bits as i32 (wrap semantics).
    I32WrapI64,
    /// Pop i32, push as i64 with signed extension.
    I64ExtendI32S,
    /// Pop i32, push as i64 with zero extension.
    I64ExtendI32U,
    /// Pop two, push bitwise AND.
    I32And,
    /// Pop two, push bitwise OR.
    I32Or,
    /// Pop two, push bitwise XOR.
    I32Xor,
    /// Pop two, push `left << (right mod 32)`.
    I32Shl,
    /// Pop two, push `left >> (right mod 32)` — signed (sign-fill).
    I32ShrS,
    /// Pop two, push `left >> (right mod 32)` — unsigned (zero-fill).
    I32ShrU,
    /// Push a 32-bit IEEE-754 float constant onto the stack.
    F32Const(f32),
    /// Pop two F32s, push sum.
    F32Add,
    /// Pop two F32s, push (left - right).
    F32Sub,
    /// Pop two F32s, push (left × right).
    F32Mul,
    /// Pop two F32s, push (left / right).
    F32Div,
    /// Pop two F32s, push 1 (as i32) if equal else 0. Returns i32;
    /// this is the WASM comparison idiom, with the boolean result
    /// living on the stack as an integer.
    F32Eq,
    /// Pop two F32s, push i32 1 if not equal (or unordered) else 0.
    F32Ne,
    /// Pop two F32s, push i32 1 if left < right (ordered) else 0.
    F32Lt,
    /// Pop two F32s, push i32 1 if left > right (ordered) else 0.
    F32Gt,
    /// Pop two F32s, push i32 1 if left ≤ right (ordered) else 0.
    F32Le,
    /// Pop two F32s, push i32 1 if left ≥ right (ordered) else 0.
    F32Ge,
    /// Load f32 from linear memory: pop addr, push
    /// `*(mem_base + addr + offset)` as F32.
    F32Load(u32),
    /// Store f32 to linear memory: pop value (f32), pop addr (i32),
    /// write value to `mem_base + addr + offset`.
    F32Store(u32),
    /// Push a 64-bit IEEE-754 float constant onto the stack.
    F64Const(f64),
    /// Pop two F64s, push sum.
    F64Add,
    /// Pop two F64s, push (left − right).
    F64Sub,
    /// Pop two F64s, push (left × right).
    F64Mul,
    /// Pop two F64s, push (left / right).
    F64Div,
    /// Pop two F64s, push i32 1 if equal else 0.
    F64Eq,
    /// Pop two F64s, push i32 1 if not equal (or unordered) else 0.
    F64Ne,
    /// Pop two F64s, push i32 1 if left < right (ordered) else 0.
    F64Lt,
    /// Pop two F64s, push i32 1 if left > right (ordered) else 0.
    F64Gt,
    /// Pop two F64s, push i32 1 if left ≤ right (ordered) else 0.
    F64Le,
    /// Pop two F64s, push i32 1 if left ≥ right (ordered) else 0.
    F64Ge,
    /// Load f64 from linear memory (8-aligned offset required).
    F64Load(u32),
    /// Store f64 to linear memory (8-aligned offset required).
    F64Store(u32),
    // ── Phase 15: conversions ─────────────────────────────────────
    // Sign-extensions (same-type, narrows then re-sign-extends).
    I32Extend8S,
    I32Extend16S,
    I64Extend8S,
    I64Extend16S,
    I64Extend32S,
    // FP → INT (round toward zero). `_s` = signed target, `_u` = unsigned.
    I32TruncF32S, I32TruncF32U,
    I32TruncF64S, I32TruncF64U,
    I64TruncF32S, I64TruncF32U,
    I64TruncF64S, I64TruncF64U,
    // INT → FP.
    F32ConvertI32S, F32ConvertI32U,
    F32ConvertI64S, F32ConvertI64U,
    F64ConvertI32S, F64ConvertI32U,
    F64ConvertI64S, F64ConvertI64U,
    // FP ↔ FP width conversions.
    F32DemoteF64,
    F64PromoteF32,
    // Bit-cast reinterprets — no numeric conversion, just register bank
    // swap via the existing FMOV encoders.
    I32ReinterpretF32,
    I64ReinterpretF64,
    F32ReinterpretI32,
    F64ReinterpretI64,
    // ── SIMD / v128 (Phase SIMD/1 — minimal set) ───────────────────
    /// Push an inline 16-byte v128 constant. The 128-bit value is
    /// materialized via a tiny literal pool embedded in the code
    /// stream: a forward `B` jumps over the data, which is emitted
    /// 16-byte-aligned, then a PC-relative `LDR Q` pulls it into the
    /// destination register. One lowered v128.const costs roughly
    /// 32 bytes (4 B + up to 12 B pad + 16 B data + 4 B LDR).
    V128Const(u128),
    /// Load 16 bytes from linear memory as a v128 — pops i32 addr,
    /// pushes a V128 slot. Offset must be 16-aligned for LDR Q.
    V128Load(u32),
    /// Pop v128 value, pop i32 addr, store 16 bytes. 16-aligned.
    V128Store(u32),
    /// Pop two V128s, push their lane-wise f32 sum (`FADD Vd.4S`).
    F32x4Add,
    /// Pop two V128s, push their lane-wise f32 product.
    F32x4Mul,
    /// Pop one V128, push lane `N` (0..=3) as a scalar F32.
    F32x4ExtractLane(u8),
    /// Pop F32 scalar, push V128 with all four lanes = the scalar.
    F32x4Splat,
    /// Pop I32 scalar, push V128 with all four lanes = the scalar.
    I32x4Splat,
    /// Pop two V128s, push lane-wise i32 sum (`ADD Vd.4S`).
    I32x4Add,
    /// Pop two V128s, push lane-wise i32 difference.
    I32x4Sub,
    /// Pop two V128s, push lane-wise i32 product (low 32 bits).
    I32x4Mul,
    /// Pop V128, push lane `N` as scalar I32 (`UMOV Wd, Vn.S[N]`).
    I32x4ExtractLane(u8),
    /// Pop two V128s, push lane-wise f32 difference.
    F32x4Sub,
    /// Pop two V128s, push lane-wise f32 quotient.
    F32x4Div,
    /// Fused multiply-add: pops `acc, a, b` (stack top = `b`), pushes
    /// `acc + a*b` lane-wise. Single rounding — matches WASM's
    /// relaxed-SIMD `f32x4.relaxed_madd` semantics. No parser opcode
    /// yet; constructed directly by the lowerer client (the relaxed-
    /// SIMD proposal's 0xFD 0x85 0x01 multi-byte encoding will plug
    /// in later when the parser grows support).
    F32x4Fma,
    /// Horizontal sum: pop one V128, push F32 equal to the sum of
    /// all four lanes. Folkering-specific extension — WASM SIMD has
    /// no portable reduction op. Implemented as two FADDPs
    /// (pairwise vector + pairwise scalar).
    F32x4HorizontalSum,
    /// Copy local `n` onto the stack.
    LocalGet(u32),
    /// Pop stack top, store into local `n`.
    LocalSet(u32),
    /// Start a labeled structured block.  The matching `end` is the
    /// forward branch target for any `br`/`br_if` targeting this
    /// label.
    Block,
    /// Start a labeled loop.  The `loop` position itself is the
    /// backward branch target (so `br` branches to the top).
    Loop,
    /// Branch to the N-th enclosing label (N=0 is innermost).
    Br(u32),
    /// Branch to the N-th enclosing label if the popped condition
    /// is non-zero.
    BrIf(u32),
    /// Start a conditional block. Pops an i32 condition; if zero,
    /// execution continues at the matching `else` (or `end` if no
    /// `else` is emitted). Otherwise falls through into the
    /// then-branch.
    If,
    /// Delimiter between the then- and else-branches of an enclosing
    /// `if`. The then-branch emits an unconditional branch to the
    /// matching `end` so it skips the else-branch at runtime; the
    /// `if`'s CBZ is patched to point at the instruction right
    /// after this branch (the start of the else-branch).
    Else,
    /// Call function at index `n` — the lowerer looks up the target's
    /// absolute address in the call-table supplied at construction
    /// and emits a MOVZ/MOVK chain into X16 followed by BLR X16.
    /// The function's i32 return value lands in X0; we push that
    /// onto the stack as the call's result.
    Call(u32),
    /// Indirect call through a function-reference table. The operand
    /// stack holds the args (shallowest = rightmost) followed by an
    /// i32 table index on top. `type_id` selects the signature in the
    /// lowerer's `indirect_sigs` list — it determines how many params
    /// to marshal and what result type (if any) to push after the
    /// call. See [`Lowerer::new_function_with_table`] for table setup.
    CallIndirect(u32),
    /// Explicit `return` — always jumps to the function end, regardless
    /// of the label stack.  Moves stack top into X0 if needed.
    Return,
    /// Structural end — matches `block`/`loop`/`if` when the label
    /// stack is non-empty, or ends the function body when empty.
    End,
    /// Load 32-bit int from linear memory: pop addr, push
    /// `*(mem_base + addr + offset)`. Requires the lowerer to be
    /// built with `new_function_with_memory`.
    I32Load(u32),
    /// Store 32-bit int to linear memory: pop value, pop addr, write
    /// `value` to `mem_base + addr + offset`. Same memory-aware
    /// lowerer requirement.
    I32Store(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowerError {
    /// Pushed past the 16-slot register budget.
    StackOverflow,
    /// Popped when empty — malformed WASM.
    StackUnderflow,
    /// Function `end` left the stack with ≠ 1 value.
    StackNotSingleton,
    /// Local index out of range (we preallocate a fixed count).
    LocalOutOfRange,
    /// Local count exceeded our X19..X28 budget.
    TooManyLocals,
    /// `br` depth refers to a label that doesn't exist.
    LabelOutOfRange,
    /// `end` with no open block/loop and no function return shape.
    UnbalancedEnd,
    /// Branch offset didn't fit in the ±1 MiB range (CBNZ) or ±128
    /// MiB (B).  Triggered only by pathologically large functions.
    BranchOutOfRange,
    /// `else` outside an `if` block, or a second `else` in the same if.
    ElseWithoutIf,
    /// Call referenced a function index not present in the call table.
    CallTargetMissing,
    /// i32.load / i32.store used on a lowerer without a configured
    /// memory base. Use `new_function_with_memory` instead.
    MemoryNotConfigured,
    /// `call_indirect` used on a lowerer without a configured table.
    /// Use `new_function_with_table` instead.
    TableNotConfigured,
    /// `call_indirect` referenced a type index not present in the
    /// signature list.
    IndirectTypeMissing,
    /// `call_indirect` signature has more integer parameters than the
    /// AAPCS64 integer argument-register band (X0..X7) can hold. Float
    /// params aren't supported in this phase at all.
    IndirectArityUnsupported,
    /// `call_indirect` signature uses a param or result type the MVP
    /// marshalling doesn't support (today: f32/f64 params and results).
    IndirectTypeUnsupported,
    /// `call` target has more than 8 integer parameters (beyond the
    /// AAPCS64 integer arg-register band). Stack-passed args are a
    /// follow-up.
    CallArityUnsupported,
    /// `call` signature uses a param or result type the MVP
    /// marshalling doesn't support (today: f32/f64 params and results).
    CallTypeUnsupported,
    /// V128 locals not supported in this phase of the SIMD lowering.
    V128LocalsUnsupported,
    /// Attempted to return a V128 value from a function. Callers
    /// must extract a lane (or store to memory) before `end`.
    V128ReturnUnsupported,
    /// An op expected a particular stack-top type but saw a different
    /// one — e.g. `i32.add` with an f32 on top, or `f32.add` with
    /// an i32 argument. Catches WASM validation errors the lowerer
    /// would otherwise silently miscompile.
    TypeMismatch {
        expected: ValType,
        got: ValType,
    },
    /// Per-type stack capacity exceeded. The i32 bank is X0..X15
    /// (16 slots); the f32 bank is S0..S15 (16 slots). Mixed stacks
    /// are fine until *either* band fills up.
    TypedStackOverflow(ValType),
    /// Underlying encoder rejected an instruction.
    Encode(EncodeError),
}

impl From<EncodeError> for LowerError {
    fn from(e: EncodeError) -> Self { LowerError::Encode(e) }
}

/// Tag used by the lowerer to pick the right A64 instruction for
/// an arithmetic WASM op. Keeps `lower_binop` single-shape while
/// giving each WASM opcode its own compile-time constant.
#[derive(Clone, Copy)]
enum BinOp {
    Add, Sub, Mul, DivS, DivU,
    And, Or, Xor, Shl, ShrS, ShrU,
    /// Comparison op — the inner Condition is the "result is true"
    /// predicate (e.g. `Cmp(Eq)` sets result=1 when operands equal).
    Cmp(Condition),
}

/// Subset of arithmetic binops that operate on F32 SIMD registers.
#[derive(Clone, Copy)]
enum FBinOp { Add, Sub, Mul, Div }

/// i64 arithmetic + bitwise ops. All route through 64-bit X-width
/// instructions; the lowerer uses the typed i64 stack helpers so
/// the operand-stack type tag stays correct.
#[derive(Clone, Copy)]
enum I64Op {
    Add, Sub, Mul, DivS, DivU,
    And, Or, Xor, Shl, ShrS, ShrU,
}

/// Narrow-source width for `i64.extend{8,16,32}_s` lowering.
#[derive(Clone, Copy)]
enum ExtendWidth { B8, B16, B32 }

/// Maximum WASM operand-stack depth in i32 slots (X0..X15).
const MAX_I32_STACK: usize = 16;
/// Maximum operand-stack depth in f32 slots (S0..S15).
/// V0..V7 are caller-saved so we can clobber them freely; V8..V15
/// are callee-saved, so using those at depth ≥ 8 would require
/// saving them in the prologue. For Phase 9 MVP we cap at 16 but
/// lowering only uses V0..V7 without a save/restore — good enough
/// for every expression we can actually write today.
const MAX_F32_STACK: usize = 16;
/// Maximum number of I32 locals hosted without spilling (X19..X27).
/// One fewer than pre-Phase-4C — X28 is reserved for memory base.
pub const MAX_I32_LOCALS: usize = 9;
/// Maximum number of F32 locals hosted without spilling (V16..V23).
/// V16..V31 are caller-saved under AAPCS64 so we can clobber them
/// without adding prologue save code. Cap at 8 to leave scratch room.
pub const MAX_F32_LOCALS: usize = 8;
/// Alias kept for existing i32-only callers and docs.
pub const MAX_LOCALS: usize = MAX_I32_LOCALS;
/// Register number where the I32 locals band begins (X19).
const LOCAL_I32_BASE_REG: u8 = 19;
/// Register number where the F32 locals band begins (V16).
const LOCAL_F32_BASE_REG: u8 = 16;
/// Register reserved for the linear-memory base pointer when a
/// memory-aware function is built. Callee-saved under AAPCS64, so
/// we save/restore it in the function's extended prologue.
const MEM_BASE_REG: Reg = Reg(28);

/// Mapping from a WASM local index to its host register. The
/// lowerer holds one of these per local, populated at construction.
/// I32 and I64 share the X-bank — the variant tells callers which
/// instruction width to use.
#[derive(Debug, Clone, Copy)]
enum LocalLoc {
    I32(Reg),
    I64(Reg),
    F32(Vreg),
    F64(Vreg),
}

#[derive(Debug, Clone, Copy)]
enum LabelKind {
    /// Forward-target: branches jump to the matching `end`.
    Block,
    /// Backward-target: branches jump to the `loop` itself.
    Loop,
    /// Open `if` block. The CBZ at `cond_branch_pos` needs patching
    /// — either to the `else` branch (if one appears) or directly
    /// to `end` (if no `else`).
    If { cond_branch_pos: usize },
    /// `if` with a resolved else clause. The CBZ from the opening
    /// `if` has been patched to point at the else-branch start;
    /// now `else_skip_pos` is the position of the unconditional `B`
    /// that jumps from end-of-then over the else-branch.
    IfElse { else_skip_pos: usize },
}

/// A pending forward branch whose offset must be patched once the
/// target label is resolved.  Each variant captures the emission
/// position and, for conditional branches, the register involved —
/// enough to recompute the opcode word in place.
#[derive(Debug, Clone, Copy)]
enum PendingPatch {
    /// Unconditional B at `pos`.
    B { pos: usize },
    /// CBNZ Wr at `pos` with register `r`.
    CbnzW { pos: usize, rt: Reg },
}

#[derive(Debug)]
struct Label {
    kind: LabelKind,
    /// For `Loop`: the byte offset of the first instruction in the
    /// loop body, i.e. the target for backward branches.
    loop_start: Option<usize>,
    /// Forward branches awaiting resolution at `end`.
    pending: Vec<PendingPatch>,
    /// Operand-stack depth at label entry.  We don't enforce WASM's
    /// block-signature validation yet, but we record it so future
    /// phases can verify stack-balance.
    #[allow(dead_code)]
    entry_depth: usize,
}

#[derive(Debug)]
pub struct Lowerer {
    enc: Encoder,
    /// Per-slot type tag, ordered bottom-first. `stack.len()` is the
    /// total operand-stack depth; each element tells the lowerer
    /// which register bank to reach for at that slot.
    stack: Vec<ValType>,
    /// Count of live I32 slots (= next free X index). Incremented on
    /// `push_i32`, decremented on pop of an I32.
    /// Count of live integer slots (I32 + I64 combined, same X bank).
    int_depth: usize,
    /// Count of live SIMD/FP slots (F32 + later F64, same V bank).
    fp_depth: usize,
    /// Per-local host-register mapping, indexed by WASM local index.
    locals: Vec<LocalLoc>,
    label_stack: Vec<Label>,
    /// Absolute addresses of callable functions, indexed by WASM
    /// function index. Empty when `Call` is not expected.
    call_targets: Vec<u64>,
    /// Parallel signature list for `call_targets`. `call_sigs[i]` is
    /// the AAPCS64-relevant signature of `call_targets[i]`. When the
    /// list is shorter than `call_targets` (or empty), `lower_call`
    /// treats missing entries as 0-arg / i32-return — preserves the
    /// Phase 4A contract for existing callers that didn't know sigs.
    call_sigs: Vec<FnSig>,
    /// True if `new_function` emitted a prologue; controls whether
    /// function-level `End` emits an epilogue (`LDP X29/X30` + RET)
    /// or just RET.
    has_frame: bool,
    /// True if the function frame includes a save slot for X28 and
    /// the prologue loaded the linear-memory base into X28. When set,
    /// `i32.load` and `i32.store` compile; otherwise they error.
    has_memory: bool,
    /// Size of the linear-memory buffer in bytes, as reported by the
    /// host (e.g. Pi daemon HELLO frame). The lowerer emits a runtime
    /// bounds check on every load/store that compares the dynamic
    /// address against `mem_size - offset - access_size`; addresses
    /// outside the buffer route to an inline trap block that sets
    /// X0 = -1 (exit code 0xFF) and RETs. Defaults to 64 KiB.
    mem_size: u32,
    /// Absolute address of the function-reference table, or `None` if
    /// `call_indirect` is not configured. Each 16-byte entry holds
    /// `addr: u64` at offset 0 and `type_id: u32` at offset 8 (with
    /// 4 bytes of reserved padding). Typically placed in the caller-
    /// visible linear-memory region, but can be any valid pointer.
    table_base: Option<u64>,
    /// Signatures indexed by WASM type index, used at `call_indirect`
    /// lowering to determine how many params to marshal and what
    /// return type to push. Empty when table-based calls aren't in use.
    indirect_sigs: Vec<FnSig>,
}

/// Function signature used for `call_indirect` marshalling. WASM MVP
/// allows at most one return; multi-value results are a follow-up.
#[derive(Debug, Clone, PartialEq)]
pub struct FnSig {
    pub params: Vec<ValType>,
    pub result: Option<ValType>,
}

impl Lowerer {
    /// Build a lowerer for a function with no locals.
    pub fn new() -> Self {
        Self::new_with_locals(0).expect("0 locals always fits")
    }

    /// Build a lowerer with `n` 32-bit locals preallocated in
    /// X19..X(19+n-1).  Each local is zero-initialised by emitting
    /// a `MOVZ` instruction, matching WASM's spec (locals start at
    /// zero before the body runs).  Returns `TooManyLocals` if
    /// `n > MAX_LOCALS`.
    pub fn new_with_locals(n: usize) -> Result<Self, LowerError> {
        let types = vec![ValType::I32; n];
        Self::new_with_typed_locals(&types)
    }

    /// Build a lowerer with per-local type information. Each i32
    /// local gets a fresh X19..X27 slot; each f32 local gets V16..V23.
    /// Both banks zero-initialize so locals start at 0 per WASM.
    pub fn new_with_typed_locals(types: &[ValType]) -> Result<Self, LowerError> {
        let mut enc = Encoder::new();
        let locals = Self::allocate_locals(&mut enc, types, false)?;
        Ok(Self {
            enc,
            stack: Vec::new(),
            int_depth: 0,
            fp_depth: 0,
            locals,
            label_stack: Vec::new(),
            call_targets: Vec::new(),
            has_frame: false,
            has_memory: false,
            mem_size: 64 * 1024,
            table_base: None,
            indirect_sigs: Vec::new(),
            call_sigs: Vec::new(),
        })
    }

    /// Internal: walk a list of local types, allocating per-bank
    /// register indices (X19..X27 for I32, V16..V23 for F32) and
    /// emitting zero-initialization for each. Used by every lowerer
    /// constructor.
    fn allocate_locals(
        enc: &mut Encoder,
        types: &[ValType],
        _frame: bool,
    ) -> Result<Vec<LocalLoc>, LowerError> {
        // I32 and I64 locals share X19..X27 — same physical register
        // file, different instruction widths. `int_idx` counts either.
        let mut int_idx: u8 = 0;
        let mut fp_idx: u8 = 0;
        let mut out = Vec::with_capacity(types.len());
        for &ty in types {
            match ty {
                ValType::I32 => {
                    if (int_idx as usize) >= MAX_I32_LOCALS {
                        return Err(LowerError::TooManyLocals);
                    }
                    let r = Reg(LOCAL_I32_BASE_REG + int_idx);
                    enc.movz(r, 0, MovShift::Lsl0)?;
                    out.push(LocalLoc::I32(r));
                    int_idx += 1;
                }
                ValType::I64 => {
                    if (int_idx as usize) >= MAX_I32_LOCALS {
                        return Err(LowerError::TooManyLocals);
                    }
                    let r = Reg(LOCAL_I32_BASE_REG + int_idx);
                    // MOVZ X (64-bit) clears the full 64 bits.
                    enc.movz(r, 0, MovShift::Lsl0)?;
                    out.push(LocalLoc::I64(r));
                    int_idx += 1;
                }
                ValType::F32 => {
                    if (fp_idx as usize) >= MAX_F32_LOCALS {
                        return Err(LowerError::TooManyLocals);
                    }
                    let v = Vreg(LOCAL_F32_BASE_REG + fp_idx);
                    // Zero-init: FMOV Sv, WZR.
                    enc.fmov_s_from_w(v, Reg::ZR)?;
                    out.push(LocalLoc::F32(v));
                    fp_idx += 1;
                }
                ValType::F64 => {
                    if (fp_idx as usize) >= MAX_F32_LOCALS {
                        return Err(LowerError::TooManyLocals);
                    }
                    let v = Vreg(LOCAL_F32_BASE_REG + fp_idx);
                    // Zero-init: FMOV Dv, XZR — clears all 64 bits.
                    enc.fmov_d_from_x(v, Reg::ZR)?;
                    out.push(LocalLoc::F64(v));
                    fp_idx += 1;
                }
                ValType::V128 => {
                    // V128 locals would need a 128-bit zero-init
                    // (MOVI Vd.2D, #0). Not wired in this sprint —
                    // v128 values live on the operand stack only.
                    return Err(LowerError::V128LocalsUnsupported);
                }
            }
        }
        Ok(out)
    }

    /// Build a lowerer with a standard AAPCS64 function frame: the
    /// prologue saves X29/X30 to a new 16-byte stack slot and sets
    /// the frame pointer; the matching epilogue runs at function-
    /// level `End`.  Required for any function that will make calls
    /// (BLR clobbers X30 and the prologue preserves the original
    /// return address).
    ///
    /// `call_targets` maps WASM function index → absolute address.
    /// Pass an empty Vec if the function has no `Call` opcodes.
    pub fn new_function(
        n_locals: usize,
        call_targets: Vec<u64>,
    ) -> Result<Self, LowerError> {
        let types = vec![ValType::I32; n_locals];
        Self::new_function_typed(&types, call_targets)
    }

    /// Variant of `new_function` with per-local types.
    pub fn new_function_typed(
        local_types: &[ValType],
        call_targets: Vec<u64>,
    ) -> Result<Self, LowerError> {
        let mut enc = Encoder::new();
        enc.stp_pre_indexed_64(Reg::X29, Reg::X30, Reg::SP, -16)?;
        let locals = Self::allocate_locals(&mut enc, local_types, true)?;
        Ok(Self {
            enc,
            stack: Vec::new(),
            int_depth: 0,
            fp_depth: 0,
            locals,
            label_stack: Vec::new(),
            call_targets,
            has_frame: true,
            has_memory: false,
            mem_size: 64 * 1024,
            table_base: None,
            indirect_sigs: Vec::new(),
            call_sigs: Vec::new(),
        })
    }

    /// Build a lowerer with the standard function frame PLUS a linear-
    /// memory base pinned in X28. Prologue layout:
    ///
    /// ```text
    /// STP  X29, X30, [SP, #-32]!     ; 32-byte frame, save X29/X30 at offset 0
    /// STR  X28,      [SP, #16]       ; save caller's X28 at offset 16
    /// MOVZ X28, #<lo>                ; load `mem_base` into X28 (up to 4 movs)
    /// MOVK X28, #<hi1>, LSL #16
    /// MOVK X28, #<hi2>, LSL #32
    /// MOVK X28, #<hi3>, LSL #48
    /// ```
    ///
    /// Every subsequent `i32.load`/`i32.store` computes its effective
    /// address as `X28 + zero_ext(Waddr) + offset`.
    pub fn new_function_with_memory(
        n_locals: usize,
        call_targets: Vec<u64>,
        mem_base: u64,
    ) -> Result<Self, LowerError> {
        if n_locals > MAX_I32_LOCALS {
            return Err(LowerError::TooManyLocals);
        }
        let mut enc = Encoder::new();
        enc.stp_pre_indexed_64(Reg::X29, Reg::X30, Reg::SP, -32)?;
        enc.str_imm(MEM_BASE_REG, Reg::SP, 16)?;
        enc.movz(MEM_BASE_REG, (mem_base & 0xFFFF) as u16, MovShift::Lsl0)?;
        let h1 = ((mem_base >> 16) & 0xFFFF) as u16;
        if h1 != 0 { enc.movk(MEM_BASE_REG, h1, MovShift::Lsl16)?; }
        let h2 = ((mem_base >> 32) & 0xFFFF) as u16;
        if h2 != 0 { enc.movk(MEM_BASE_REG, h2, MovShift::Lsl32)?; }
        let h3 = ((mem_base >> 48) & 0xFFFF) as u16;
        if h3 != 0 { enc.movk(MEM_BASE_REG, h3, MovShift::Lsl48)?; }
        let types = vec![ValType::I32; n_locals];
        let locals = Self::allocate_locals(&mut enc, &types, true)?;
        Ok(Self {
            enc,
            stack: Vec::new(),
            int_depth: 0,
            fp_depth: 0,
            locals,
            label_stack: Vec::new(),
            call_targets,
            has_frame: true,
            has_memory: true,
            mem_size: 64 * 1024,
            table_base: None,
            indirect_sigs: Vec::new(),
            call_sigs: Vec::new(),
        })
    }

    /// Override the linear-memory size used for bounds checks on
    /// subsequent load/store lowerings. Default is 64 KiB. The host's
    /// HELLO frame typically provides the authoritative value.
    pub fn set_mem_size(&mut self, size: u32) {
        self.mem_size = size;
    }

    /// Attach AAPCS64 signatures for the direct-call targets. Must be
    /// called after construction and before any `Call` op is lowered.
    /// `sigs[i]` is the signature of `call_targets[i]`. Passing a
    /// shorter list is allowed — entries beyond `sigs.len()` default
    /// to 0-arg / i32-return for backward compatibility with the
    /// Phase 4A callers who didn't know their helpers' sigs.
    pub fn set_call_sigs(&mut self, sigs: Vec<FnSig>) {
        self.call_sigs = sigs;
    }

    /// Builder-style variant of [`Self::set_call_sigs`]. Useful for
    /// chaining off `new_function(...)?`.
    pub fn with_call_sigs(mut self, sigs: Vec<FnSig>) -> Self {
        self.call_sigs = sigs;
        self
    }

    /// Build a lowerer for a function that uses `call_indirect`.
    /// Extends [`Self::new_function_with_memory`] by also wiring up a
    /// function-reference table at `table_base`. `sigs` is indexed by
    /// WASM type index; each `CallIndirect(type_id)` op looks up the
    /// corresponding signature to drive param/result marshalling.
    ///
    /// The table layout is 16 bytes per entry:
    /// ```text
    ///   bytes 0..8   addr: u64   // callable function address
    ///   bytes 8..12  type_id: u32 // (reserved for runtime type-check;
    ///                              ignored by this lowering today)
    ///   bytes 12..16 padding
    /// ```
    pub fn new_function_with_table(
        n_locals: usize,
        call_targets: Vec<u64>,
        mem_base: u64,
        table_base: u64,
        sigs: Vec<FnSig>,
    ) -> Result<Self, LowerError> {
        let mut lw = Self::new_function_with_memory(n_locals, call_targets, mem_base)?;
        lw.table_base = Some(table_base);
        lw.indirect_sigs = sigs;
        Ok(lw)
    }

    /// Lower a single op.
    pub fn lower_op(&mut self, op: WasmOp) -> Result<(), LowerError> {
        match op {
            WasmOp::I32Const(c) => self.lower_const(c),
            WasmOp::I32Add => self.lower_binop(BinOp::Add),
            WasmOp::I32Sub => self.lower_binop(BinOp::Sub),
            WasmOp::I32Mul => self.lower_binop(BinOp::Mul),
            WasmOp::I32DivS => self.lower_binop(BinOp::DivS),
            WasmOp::I32DivU => self.lower_binop(BinOp::DivU),
            WasmOp::I32Eq   => self.lower_binop(BinOp::Cmp(Condition::Eq)),
            WasmOp::I32Ne   => self.lower_binop(BinOp::Cmp(Condition::Ne)),
            WasmOp::I32LtS  => self.lower_binop(BinOp::Cmp(Condition::Lt)),
            WasmOp::I32GtS  => self.lower_binop(BinOp::Cmp(Condition::Gt)),
            WasmOp::I32LeS  => self.lower_binop(BinOp::Cmp(Condition::Le)),
            WasmOp::I32GeS  => self.lower_binop(BinOp::Cmp(Condition::Ge)),
            WasmOp::I32LtU  => self.lower_binop(BinOp::Cmp(Condition::Lo)),
            WasmOp::I32GtU  => self.lower_binop(BinOp::Cmp(Condition::Hi)),
            WasmOp::I32LeU  => self.lower_binop(BinOp::Cmp(Condition::Ls)),
            WasmOp::I32GeU  => self.lower_binop(BinOp::Cmp(Condition::Hs)),
            WasmOp::I32Eqz  => self.lower_eqz(),
            WasmOp::I64Const(c) => self.lower_i64_const(c),
            WasmOp::I64Add => self.lower_i64_binop(I64Op::Add),
            WasmOp::I64Sub => self.lower_i64_binop(I64Op::Sub),
            WasmOp::I64Mul => self.lower_i64_binop(I64Op::Mul),
            WasmOp::I64Eqz => self.lower_i64_eqz(),
            WasmOp::I64Eq => self.lower_i64_cmp(Condition::Eq),
            WasmOp::I64Ne => self.lower_i64_cmp(Condition::Ne),
            WasmOp::I64LtS => self.lower_i64_cmp(Condition::Lt),
            WasmOp::I64GtS => self.lower_i64_cmp(Condition::Gt),
            WasmOp::I64LtU => self.lower_i64_cmp(Condition::Lo),
            WasmOp::I64GtU => self.lower_i64_cmp(Condition::Hi),
            WasmOp::I64LeS => self.lower_i64_cmp(Condition::Le),
            WasmOp::I64LeU => self.lower_i64_cmp(Condition::Ls),
            WasmOp::I64GeS => self.lower_i64_cmp(Condition::Ge),
            WasmOp::I64GeU => self.lower_i64_cmp(Condition::Hs),
            WasmOp::I64DivS => self.lower_i64_binop(I64Op::DivS),
            WasmOp::I64DivU => self.lower_i64_binop(I64Op::DivU),
            WasmOp::I64And => self.lower_i64_binop(I64Op::And),
            WasmOp::I64Or => self.lower_i64_binop(I64Op::Or),
            WasmOp::I64Xor => self.lower_i64_binop(I64Op::Xor),
            WasmOp::I64Shl => self.lower_i64_binop(I64Op::Shl),
            WasmOp::I64ShrS => self.lower_i64_binop(I64Op::ShrS),
            WasmOp::I64ShrU => self.lower_i64_binop(I64Op::ShrU),
            WasmOp::I64Load(off) => self.lower_i64_load(off),
            WasmOp::I64Store(off) => self.lower_i64_store(off),
            WasmOp::I32WrapI64 => self.lower_wrap_i64(),
            WasmOp::I64ExtendI32S => self.lower_extend_i32(true),
            WasmOp::I64ExtendI32U => self.lower_extend_i32(false),
            WasmOp::I32And  => self.lower_binop(BinOp::And),
            WasmOp::I32Or   => self.lower_binop(BinOp::Or),
            WasmOp::I32Xor  => self.lower_binop(BinOp::Xor),
            WasmOp::I32Shl  => self.lower_binop(BinOp::Shl),
            WasmOp::I32ShrS => self.lower_binop(BinOp::ShrS),
            WasmOp::I32ShrU => self.lower_binop(BinOp::ShrU),
            WasmOp::F32Const(f) => self.lower_f32_const(f),
            WasmOp::F32Add => self.lower_f32_binop(FBinOp::Add),
            WasmOp::F32Sub => self.lower_f32_binop(FBinOp::Sub),
            WasmOp::F32Mul => self.lower_f32_binop(FBinOp::Mul),
            WasmOp::F32Div => self.lower_f32_binop(FBinOp::Div),
            WasmOp::F32Eq => self.lower_f32_cmp(Condition::Eq),
            WasmOp::F32Ne => self.lower_f32_cmp(Condition::Ne),
            // For non-NaN operands, the FP flag encoding matches the
            // signed-integer conditions: FCMP sets N for "less" and
            // Z for "equal", same as a signed SUBS. NaN (unordered)
            // sets V=1, which would make some conditions fire
            // incorrectly — full WASM NaN semantics is Phase 11+.
            WasmOp::F32Lt => self.lower_f32_cmp(Condition::Lt),
            WasmOp::F32Gt => self.lower_f32_cmp(Condition::Gt),
            WasmOp::F32Le => self.lower_f32_cmp(Condition::Le),
            WasmOp::F32Ge => self.lower_f32_cmp(Condition::Ge),
            WasmOp::F32Load(off) => self.lower_f32_load(off),
            WasmOp::F32Store(off) => self.lower_f32_store(off),
            WasmOp::F64Const(f) => self.lower_f64_const(f),
            WasmOp::F64Add => self.lower_f64_binop(FBinOp::Add),
            WasmOp::F64Sub => self.lower_f64_binop(FBinOp::Sub),
            WasmOp::F64Mul => self.lower_f64_binop(FBinOp::Mul),
            WasmOp::F64Div => self.lower_f64_binop(FBinOp::Div),
            WasmOp::F64Eq => self.lower_f64_cmp(Condition::Eq),
            WasmOp::F64Ne => self.lower_f64_cmp(Condition::Ne),
            WasmOp::F64Lt => self.lower_f64_cmp(Condition::Lt),
            WasmOp::F64Gt => self.lower_f64_cmp(Condition::Gt),
            WasmOp::F64Le => self.lower_f64_cmp(Condition::Le),
            WasmOp::F64Ge => self.lower_f64_cmp(Condition::Ge),
            WasmOp::F64Load(off) => self.lower_f64_load(off),
            WasmOp::F64Store(off) => self.lower_f64_store(off),
            // ── SIMD ─────────────────────────────────────────────
            WasmOp::V128Const(bits) => self.lower_v128_const(bits),
            WasmOp::V128Load(off) => self.lower_v128_load(off),
            WasmOp::V128Store(off) => self.lower_v128_store(off),
            WasmOp::F32x4Add => self.lower_f32x4_add(),
            WasmOp::F32x4Mul => self.lower_f32x4_mul(),
            WasmOp::F32x4ExtractLane(lane) => self.lower_f32x4_extract_lane(lane),
            WasmOp::F32x4Splat => self.lower_f32x4_splat(),
            WasmOp::I32x4Splat => self.lower_i32x4_splat(),
            WasmOp::I32x4Add => self.lower_i32x4_add(),
            WasmOp::I32x4Sub => self.lower_i32x4_sub(),
            WasmOp::I32x4Mul => self.lower_i32x4_mul(),
            WasmOp::I32x4ExtractLane(lane) => self.lower_i32x4_extract_lane(lane),
            WasmOp::F32x4Sub => self.lower_f32x4_sub(),
            WasmOp::F32x4Div => self.lower_f32x4_div(),
            WasmOp::F32x4Fma => self.lower_f32x4_fma(),
            WasmOp::F32x4HorizontalSum => self.lower_f32x4_horizontal_sum(),
            // Phase 15 conversions.
            WasmOp::I32Extend8S => self.lower_i32_extend_narrow(true, false),
            WasmOp::I32Extend16S => self.lower_i32_extend_narrow(false, false),
            WasmOp::I64Extend8S => self.lower_i64_extend_narrow(ExtendWidth::B8),
            WasmOp::I64Extend16S => self.lower_i64_extend_narrow(ExtendWidth::B16),
            WasmOp::I64Extend32S => self.lower_i64_extend_narrow(ExtendWidth::B32),
            WasmOp::I32TruncF32S => self.lower_trunc_f32_i32(true),
            WasmOp::I32TruncF32U => self.lower_trunc_f32_i32(false),
            WasmOp::I32TruncF64S => self.lower_trunc_f64_i32(true),
            WasmOp::I32TruncF64U => self.lower_trunc_f64_i32(false),
            WasmOp::I64TruncF32S => self.lower_trunc_f32_i64(true),
            WasmOp::I64TruncF32U => self.lower_trunc_f32_i64(false),
            WasmOp::I64TruncF64S => self.lower_trunc_f64_i64(true),
            WasmOp::I64TruncF64U => self.lower_trunc_f64_i64(false),
            WasmOp::F32ConvertI32S => self.lower_convert_i32_f32(true),
            WasmOp::F32ConvertI32U => self.lower_convert_i32_f32(false),
            WasmOp::F32ConvertI64S => self.lower_convert_i64_f32(true),
            WasmOp::F32ConvertI64U => self.lower_convert_i64_f32(false),
            WasmOp::F64ConvertI32S => self.lower_convert_i32_f64(true),
            WasmOp::F64ConvertI32U => self.lower_convert_i32_f64(false),
            WasmOp::F64ConvertI64S => self.lower_convert_i64_f64(true),
            WasmOp::F64ConvertI64U => self.lower_convert_i64_f64(false),
            WasmOp::F32DemoteF64 => self.lower_f32_demote_f64(),
            WasmOp::F64PromoteF32 => self.lower_f64_promote_f32(),
            WasmOp::I32ReinterpretF32 => self.lower_i32_reinterpret_f32(),
            WasmOp::I64ReinterpretF64 => self.lower_i64_reinterpret_f64(),
            WasmOp::F32ReinterpretI32 => self.lower_f32_reinterpret_i32(),
            WasmOp::F64ReinterpretI64 => self.lower_f64_reinterpret_i64(),
            WasmOp::LocalGet(i) => self.lower_local_get(i),
            WasmOp::LocalSet(i) => self.lower_local_set(i),
            WasmOp::Block => self.lower_block(),
            WasmOp::Loop => self.lower_loop(),
            WasmOp::Br(n) => self.lower_br(n),
            WasmOp::BrIf(n) => self.lower_br_if(n),
            WasmOp::If => self.lower_if(),
            WasmOp::Else => self.lower_else(),
            WasmOp::Call(n) => self.lower_call(n),
            WasmOp::CallIndirect(t) => self.lower_call_indirect(t),
            WasmOp::I32Load(off) => self.lower_load(off),
            WasmOp::I32Store(off) => self.lower_store(off),
            WasmOp::Return => self.lower_explicit_return(),
            WasmOp::End => {
                if self.label_stack.is_empty() {
                    self.lower_function_end()
                } else {
                    self.lower_block_end()
                }
            }
        }
    }

    /// Lower every op in order.
    pub fn lower_all(&mut self, ops: &[WasmOp]) -> Result<(), LowerError> {
        for &op in ops {
            self.lower_op(op)?;
        }
        Ok(())
    }

    /// Consume and return the emitted bytes.
    pub fn finish(self) -> Vec<u8> {
        self.enc.into_bytes()
    }

    /// Borrow the current bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.enc.as_bytes()
    }

    /// Current operand-stack depth.
    pub fn stack_depth(&self) -> usize { self.stack.len() }

    /// Current top-of-stack type, if any. Used by tests and by
    /// function-end lowering to decide between RET-with-X0 and
    /// RET-with-bitcast-from-S0.
    pub fn stack_top_type(&self) -> Option<ValType> { self.stack.last().copied() }

    /// Currently open block/loop labels.
    pub fn open_labels(&self) -> usize { self.label_stack.len() }

    // ── Stack helpers ───────────────────────────────────────────────

    /// Push an I32 slot; returns the X-bank register that holds it.
    fn push_i32_slot(&mut self) -> Result<Reg, LowerError> {
        if self.int_depth >= MAX_I32_STACK {
            return Err(LowerError::TypedStackOverflow(ValType::I32));
        }
        let r = Reg::new(self.int_depth as u8)
            .ok_or(LowerError::TypedStackOverflow(ValType::I32))?;
        self.int_depth += 1;
        self.stack.push(ValType::I32);
        Ok(r)
    }

    /// Pop an I32 from the top of the stack. Errors if the stack is
    /// empty or the top isn't an I32.
    fn pop_i32_slot(&mut self) -> Result<Reg, LowerError> {
        let ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        if ty != ValType::I32 {
            return Err(LowerError::TypeMismatch {
                expected: ValType::I32,
                got: ty,
            });
        }
        self.stack.pop();
        self.int_depth -= 1;
        Reg::new(self.int_depth as u8).ok_or(LowerError::StackUnderflow)
    }

    /// Push an F32 slot; returns the V-bank register.
    fn push_f32_slot(&mut self) -> Result<Vreg, LowerError> {
        if self.fp_depth >= MAX_F32_STACK {
            return Err(LowerError::TypedStackOverflow(ValType::F32));
        }
        let v = Vreg::new(self.fp_depth as u8)
            .ok_or(LowerError::TypedStackOverflow(ValType::F32))?;
        self.fp_depth += 1;
        self.stack.push(ValType::F32);
        Ok(v)
    }

    /// Pop an F32 from the top of the stack.
    fn pop_f32_slot(&mut self) -> Result<Vreg, LowerError> {
        let ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        if ty != ValType::F32 {
            return Err(LowerError::TypeMismatch {
                expected: ValType::F32,
                got: ty,
            });
        }
        self.stack.pop();
        self.fp_depth -= 1;
        Vreg::new(self.fp_depth as u8).ok_or(LowerError::StackUnderflow)
    }

    /// Push an F64 slot. Shares the V-bank with F32 — the instruction
    /// width picks which half (low-32 Si vs full-64 Di) the op touches.
    fn push_f64_slot(&mut self) -> Result<Vreg, LowerError> {
        if self.fp_depth >= MAX_F32_STACK {
            return Err(LowerError::TypedStackOverflow(ValType::F64));
        }
        let v = Vreg::new(self.fp_depth as u8)
            .ok_or(LowerError::TypedStackOverflow(ValType::F64))?;
        self.fp_depth += 1;
        self.stack.push(ValType::F64);
        Ok(v)
    }

    fn pop_f64_slot(&mut self) -> Result<Vreg, LowerError> {
        let ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        if ty != ValType::F64 {
            return Err(LowerError::TypeMismatch {
                expected: ValType::F64,
                got: ty,
            });
        }
        self.stack.pop();
        self.fp_depth -= 1;
        Vreg::new(self.fp_depth as u8).ok_or(LowerError::StackUnderflow)
    }

    /// Push a V128 slot — NEON 128-bit (Qn). Shares the V-register
    /// file with F32/F64 via `fp_depth`; a single Vreg index names
    /// the same physical register whether we touch it as Sn, Dn, or
    /// Qn. A V128 write clobbers all 128 bits — any prior scalar
    /// content at the same index is toast, which matches WASM's
    /// type-system guarantee that you can't hold a v128 and an f32
    /// in the same slot simultaneously.
    fn push_v128_slot(&mut self) -> Result<Vreg, LowerError> {
        if self.fp_depth >= MAX_F32_STACK {
            return Err(LowerError::TypedStackOverflow(ValType::V128));
        }
        let v = Vreg::new(self.fp_depth as u8)
            .ok_or(LowerError::TypedStackOverflow(ValType::V128))?;
        self.fp_depth += 1;
        self.stack.push(ValType::V128);
        Ok(v)
    }

    fn pop_v128_slot(&mut self) -> Result<Vreg, LowerError> {
        let ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        if ty != ValType::V128 {
            return Err(LowerError::TypeMismatch {
                expected: ValType::V128,
                got: ty,
            });
        }
        self.stack.pop();
        self.fp_depth -= 1;
        Vreg::new(self.fp_depth as u8).ok_or(LowerError::StackUnderflow)
    }

    /// Push an I64 slot; returns the X-bank register. Shares the
    /// `int_depth` counter with I32 since both types live in the
    /// same physical register file — the width distinction is in
    /// the instruction, not the register.
    fn push_i64_slot(&mut self) -> Result<Reg, LowerError> {
        if self.int_depth >= MAX_I32_STACK {
            return Err(LowerError::TypedStackOverflow(ValType::I64));
        }
        let r = Reg::new(self.int_depth as u8)
            .ok_or(LowerError::TypedStackOverflow(ValType::I64))?;
        self.int_depth += 1;
        self.stack.push(ValType::I64);
        Ok(r)
    }

    fn pop_i64_slot(&mut self) -> Result<Reg, LowerError> {
        let ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        if ty != ValType::I64 {
            return Err(LowerError::TypeMismatch {
                expected: ValType::I64,
                got: ty,
            });
        }
        self.stack.pop();
        self.int_depth -= 1;
        Reg::new(self.int_depth as u8).ok_or(LowerError::StackUnderflow)
    }

    // ── Op-specific lowering ────────────────────────────────────────

    fn lower_const(&mut self, c: i32) -> Result<(), LowerError> {
        // WASM i32 values are 32-bit bit patterns; the upper 32 bits
        // of the hosting register are irrelevant to i32 ops. Encode
        // as a u32 using MOVZ (low half) and optionally MOVK (high
        // half). This covers all 32-bit values including negatives.
        let bits = c as u32;
        let lo = (bits & 0xFFFF) as u16;
        let hi = ((bits >> 16) & 0xFFFF) as u16;
        let r = self.push_i32_slot()?;
        self.enc.movz(r, lo, MovShift::Lsl0)?;
        if hi != 0 {
            self.enc.movk(r, hi, MovShift::Lsl16)?;
        }
        Ok(())
    }

    /// Lower `f32.const c` — materialize the IEEE-754 bit pattern
    /// into a W register via MOVZ/MOVK, then bit-cast into an S
    /// register via FMOV. Two-step lowering keeps the encoder's FP
    /// surface minimal (no need for a full immediate-encoding table
    /// that handles only certain floats natively).
    fn lower_f32_const(&mut self, c: f32) -> Result<(), LowerError> {
        let bits = c.to_bits();
        // Temporary X-bank register — use one beyond the i32 stack
        // so we don't perturb i32 slots. X16 is AAPCS64 IP scratch;
        // always safe to clobber here.
        let tmp = Reg::X16;
        let lo = (bits & 0xFFFF) as u16;
        let hi = ((bits >> 16) & 0xFFFF) as u16;
        self.enc.movz(tmp, lo, MovShift::Lsl0)?;
        if hi != 0 {
            self.enc.movk(tmp, hi, MovShift::Lsl16)?;
        }
        let dst = self.push_f32_slot()?;
        self.enc.fmov_s_from_w(dst, tmp)?;
        Ok(())
    }

    fn lower_f32_binop(&mut self, op: FBinOp) -> Result<(), LowerError> {
        let rhs = self.pop_f32_slot()?;
        let lhs = self.pop_f32_slot()?;
        let dst = self.push_f32_slot()?;
        debug_assert_eq!(dst.0, lhs.0);
        match op {
            FBinOp::Add => self.enc.fadd_s(dst, lhs, rhs)?,
            FBinOp::Sub => self.enc.fsub_s(dst, lhs, rhs)?,
            FBinOp::Mul => self.enc.fmul_s(dst, lhs, rhs)?,
            FBinOp::Div => self.enc.fdiv_s(dst, lhs, rhs)?,
        }
        Ok(())
    }

    /// Lower `f32.<cmp>` — pops two F32 operands, pushes an I32
    /// result. Note the *type transition*: FCMP sets flags, CSET
    /// into an X register. This is the first op in the lowerer
    /// that produces a different type than it consumes, and is
    /// exactly the WASM model: comparisons of any type yield i32.
    fn lower_f32_cmp(&mut self, cond: Condition) -> Result<(), LowerError> {
        let rhs = self.pop_f32_slot()?;
        let lhs = self.pop_f32_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.fcmp_s(lhs, rhs)?;
        self.enc.cset(dst, cond)?;
        Ok(())
    }

    /// Lower `f64.const c` — materialize the 64-bit IEEE-754 bit pattern
    /// into X16 via MOVZ + MOVK×3, then bit-cast into a D register via
    /// FMOV Dd, Xn. Mirrors `lower_f32_const` but with 64-bit width.
    fn lower_f64_const(&mut self, c: f64) -> Result<(), LowerError> {
        let bits = c.to_bits();
        let tmp = Reg::X16;
        self.enc.movz(tmp, (bits & 0xFFFF) as u16, MovShift::Lsl0)?;
        let h1 = ((bits >> 16) & 0xFFFF) as u16;
        if h1 != 0 { self.enc.movk(tmp, h1, MovShift::Lsl16)?; }
        let h2 = ((bits >> 32) & 0xFFFF) as u16;
        if h2 != 0 { self.enc.movk(tmp, h2, MovShift::Lsl32)?; }
        let h3 = ((bits >> 48) & 0xFFFF) as u16;
        if h3 != 0 { self.enc.movk(tmp, h3, MovShift::Lsl48)?; }
        let dst = self.push_f64_slot()?;
        self.enc.fmov_d_from_x(dst, tmp)?;
        Ok(())
    }

    fn lower_f64_binop(&mut self, op: FBinOp) -> Result<(), LowerError> {
        let rhs = self.pop_f64_slot()?;
        let lhs = self.pop_f64_slot()?;
        let dst = self.push_f64_slot()?;
        debug_assert_eq!(dst.0, lhs.0);
        match op {
            FBinOp::Add => self.enc.fadd_d(dst, lhs, rhs)?,
            FBinOp::Sub => self.enc.fsub_d(dst, lhs, rhs)?,
            FBinOp::Mul => self.enc.fmul_d(dst, lhs, rhs)?,
            FBinOp::Div => self.enc.fdiv_d(dst, lhs, rhs)?,
        }
        Ok(())
    }

    /// Lower `f64.<cmp>` — pops two F64s, pushes i32 boolean.
    /// Same cross-type pattern as `lower_f32_cmp`.
    fn lower_f64_cmp(&mut self, cond: Condition) -> Result<(), LowerError> {
        let rhs = self.pop_f64_slot()?;
        let lhs = self.pop_f64_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.fcmp_d(lhs, rhs)?;
        self.enc.cset(dst, cond)?;
        Ok(())
    }

    fn lower_f64_load(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let addr = self.pop_i32_slot()?;
        self.emit_bounds_check(addr, 8, offset)?;
        let dst = self.push_f64_slot()?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.ldr_d_imm(dst, Reg::X16, offset)?;
        Ok(())
    }

    fn lower_f64_store(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let val = self.pop_f64_slot()?;
        let addr = self.pop_i32_slot()?;
        self.emit_bounds_check(addr, 8, offset)?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.str_d_imm(val, Reg::X16, offset)?;
        Ok(())
    }

    // ── SIMD / v128 lowerings ───────────────────────────────────────

    /// Lower `v128.const <16 bytes>` — materialize an inline 128-bit
    /// literal via a tiny PC-relative literal pool:
    ///
    /// ```text
    ///   B  skip_pool          ; 4 bytes, jumps past the data
    ///   (NOP padding to 16-align the data, 0..12 bytes)
    ///   <16 bytes of the constant, little-endian>
    /// skip_pool:
    ///   LDR Q_dst, [PC, #-16] ; 4 bytes, loads the data we just passed
    /// ```
    ///
    /// Cost: 4 B (B) + 0-12 B (pad) + 16 B (data) + 4 B (LDR) = 24-36
    /// bytes per v128.const. Single-shot, no shared pool across
    /// multiple constants yet — a later sprint can dedup by
    /// collecting all constants and emitting one pool at function end.
    fn lower_v128_const(&mut self, bits: u128) -> Result<(), LowerError> {
        let dst = self.push_v128_slot()?;

        // LDR Q requires its source to be 16-byte-aligned. The B is
        // 4 bytes; after B the position is (pos + 4). Compute the
        // NOP padding needed to 16-align the constant's start.
        let pos_after_b = self.enc.pos() + 4;
        let padding = (16 - (pos_after_b % 16)) % 16;

        // B jumps over the pad + 16-byte data, landing directly on
        // the LDR. Forward branch offset (from B's own address) =
        // 4 (size of B) + padding + 16 (data) bytes.
        let branch_bytes = 4 + padding as i32 + 16;
        self.enc.b(branch_bytes)?;

        // Emit NOP padding to push the position to 16-byte alignment.
        for _ in 0..(padding / 4) {
            self.enc.nop();
        }

        // Emit the 16-byte constant as 4 little-endian u32 words.
        // Same byte-order as the Encoder's normal emission, so the
        // constant's u128 little-endian layout is preserved.
        let le = bits.to_le_bytes();
        for chunk in le.chunks_exact(4) {
            let w = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            self.enc.emit_raw_word(w);
        }

        // LDR Q_dst, [PC, #-16] — the data is now 16 bytes before
        // the current PC (which points at the LDR we're about to emit).
        self.enc.ldr_q_literal(dst, -16)?;

        Ok(())
    }

    /// Lower `v128.load off` — pop i32 addr, push a V128 slot loaded
    /// from `mem_base + addr + offset`. 16-byte access needs
    /// 16-byte-aligned offset (LDR Q requires `offset % 16 == 0`).
    fn lower_v128_load(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let addr = self.pop_i32_slot()?;
        self.emit_bounds_check(addr, 16, offset)?;
        let dst = self.push_v128_slot()?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.ldr_q_imm(dst, Reg::X16, offset)?;
        Ok(())
    }

    fn lower_v128_store(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let val = self.pop_v128_slot()?;
        let addr = self.pop_i32_slot()?;
        self.emit_bounds_check(addr, 16, offset)?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.str_q_imm(val, Reg::X16, offset)?;
        Ok(())
    }

    /// Lower `f32x4.add` / `f32x4.mul` — pop two V128, push one V128
    /// with element-wise sum/product across the 4 f32 lanes.
    fn lower_f32x4_add(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.fadd_4s(dst, lhs, rhs)?;
        Ok(())
    }

    fn lower_f32x4_mul(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.fmul_4s(dst, lhs, rhs)?;
        Ok(())
    }

    /// Lower `f32x4.extract_lane N` — pop V128, push F32 holding
    /// lane N as a scalar. `DUP Sd, Vn.S[N]` copies the lane into a
    /// scalar S register (zeroing upper bits of Vd), which is what
    /// F32-consuming downstream ops expect.
    fn lower_f32x4_extract_lane(&mut self, lane: u8) -> Result<(), LowerError> {
        let src = self.pop_v128_slot()?;
        let dst = self.push_f32_slot()?;
        // Same physical register by construction — source V and dest
        // F share the bank and the slot counter. Using explicit DUP
        // still makes sense: if the extracted lane is N != 0, the
        // instruction actually moves bits. Lane 0 happens to
        // degenerate to a no-op on top of a V128 write (low S is the
        // first lane), but we emit the DUP unconditionally so the
        // semantics are obvious from the disassembly.
        self.enc.dup_s_from_v_s_lane(dst, src, lane)?;
        Ok(())
    }

    /// Lower `f32x4.splat` — pop F32, push V128 with the scalar
    /// replicated across all 4 lanes. DUP Vd.4S, Vn.S[0] broadcasts
    /// lane 0 of the source V register (where the scalar lives).
    fn lower_f32x4_splat(&mut self) -> Result<(), LowerError> {
        let src = self.pop_f32_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.dup_4s_from_vs_lane0(dst, src)?;
        Ok(())
    }

    /// Lower `i32x4.splat` — pop I32, push V128. Bank crossing:
    /// the scalar lives in the X bank, target is V. `DUP Vd.4S, Wn`
    /// handles it in one instruction.
    fn lower_i32x4_splat(&mut self) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.dup_4s_from_w(dst, src)?;
        Ok(())
    }

    /// Lower `i32x4.add/sub/mul` — integer vector arithmetic.
    /// Structurally identical to the f32x4 variants, just a
    /// different AdvSIMD opcode (ADD/SUB/MUL vs FADD/FMUL).
    fn lower_i32x4_add(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.add_4s_vector(dst, lhs, rhs)?;
        Ok(())
    }

    fn lower_i32x4_sub(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.sub_4s_vector(dst, lhs, rhs)?;
        Ok(())
    }

    fn lower_i32x4_mul(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.mul_4s_vector(dst, lhs, rhs)?;
        Ok(())
    }

    /// Lower `i32x4.extract_lane N` — pop V128, push I32 scalar.
    /// UMOV Wd, Vn.S[N] — zero-extends the 32-bit lane into the
    /// full X register, matching the i32 slot semantics.
    fn lower_i32x4_extract_lane(&mut self, lane: u8) -> Result<(), LowerError> {
        let src = self.pop_v128_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.umov_w_from_vs_lane(dst, src, lane)?;
        Ok(())
    }

    fn lower_f32x4_sub(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.fsub_4s(dst, lhs, rhs)?;
        Ok(())
    }

    fn lower_f32x4_div(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.fdiv_4s(dst, lhs, rhs)?;
        Ok(())
    }

    /// Lower `f32x4.fma` — fused multiply-add. Operand stack order:
    /// bottom `acc`, then `a`, then `b` (top). Each pop reveals the
    /// next register index going down. FMLA Vd.4S, Vn.4S, Vm.4S does
    /// `Vd += Vn * Vm` in-place; we emit it so Vd names the `acc`
    /// register (deepest), Vn names `a`, Vm names `b`. After the op,
    /// we push the result back at the `acc` slot position so the
    /// caller sees one V128 on the stack where three were.
    fn lower_f32x4_fma(&mut self) -> Result<(), LowerError> {
        let b = self.pop_v128_slot()?;   // topmost
        let a = self.pop_v128_slot()?;   // middle
        let acc = self.pop_v128_slot()?; // deepest
        let dst = self.push_v128_slot()?;
        debug_assert_eq!(
            dst.0, acc.0,
            "FMA should write back to the acc slot"
        );
        self.enc.fmla_4s(dst, a, b)?;
        Ok(())
    }

    /// Lower `f32x4.horizontal_sum` — reduce a 4-lane f32 vector to
    /// a single f32 total. Two-stage FADDP (vector pairwise +
    /// scalar pairwise):
    ///
    ///   input  Vn.4S = [a, b, c, d]
    ///   FADDP Vn.4S, Vn.4S, Vn.4S  → Vn.4S = [a+b, c+d, a+b, c+d]
    ///   FADDP Sd,    Vn.2S         → Sd    = (a+b) + (c+d)
    ///
    /// Pops V128, pushes F32 at the same slot index. The caller's
    /// downstream F32 arith/cmp/store all see a normal f32 scalar.
    fn lower_f32x4_horizontal_sum(&mut self) -> Result<(), LowerError> {
        let src = self.pop_v128_slot()?;
        let dst = self.push_f32_slot()?;
        debug_assert_eq!(
            dst.0, src.0,
            "horizontal_sum reuses the V128 slot for the scalar result",
        );
        // Stage 1: pairwise vector — folds the 4 lanes into 2 pairs.
        // Same src for Vn and Vm so the result's upper 64 bits are
        // just a duplicate of the lower 64 (harmless; the second
        // FADDP ignores them).
        self.enc.faddp_4s(src, src, src)?;
        // Stage 2: pairwise scalar — reduces the pair into one lane.
        self.enc.faddp_s_from_2s_scalar(dst, src)?;
        Ok(())
    }

    /// Lower `i64.const c`. 64-bit values need up to 4 halfwords;
    /// `MOVZ` sets bits 0..15 and clears the rest, then up to three
    /// `MOVK`s patch in each non-zero high halfword.
    fn lower_i64_const(&mut self, c: i64) -> Result<(), LowerError> {
        let bits = c as u64;
        let r = self.push_i64_slot()?;
        let h0 = (bits & 0xFFFF) as u16;
        self.enc.movz(r, h0, MovShift::Lsl0)?;
        let h1 = ((bits >> 16) & 0xFFFF) as u16;
        if h1 != 0 { self.enc.movk(r, h1, MovShift::Lsl16)?; }
        let h2 = ((bits >> 32) & 0xFFFF) as u16;
        if h2 != 0 { self.enc.movk(r, h2, MovShift::Lsl32)?; }
        let h3 = ((bits >> 48) & 0xFFFF) as u16;
        if h3 != 0 { self.enc.movk(r, h3, MovShift::Lsl48)?; }
        Ok(())
    }

    fn lower_i64_binop(&mut self, op: I64Op) -> Result<(), LowerError> {
        let rhs = self.pop_i64_slot()?;
        let lhs = self.pop_i64_slot()?;
        let dst = self.push_i64_slot()?;
        match op {
            I64Op::Add  => self.enc.add(dst, lhs, rhs)?,
            I64Op::Sub  => self.enc.sub(dst, lhs, rhs)?,
            I64Op::Mul  => self.enc.mul(dst, lhs, rhs)?,
            I64Op::DivS => self.enc.sdiv(dst, lhs, rhs)?,
            I64Op::DivU => self.enc.udiv(dst, lhs, rhs)?,
            I64Op::And  => self.enc.and_x(dst, lhs, rhs)?,
            I64Op::Or   => self.enc.orr_x(dst, lhs, rhs)?,
            I64Op::Xor  => self.enc.eor_x(dst, lhs, rhs)?,
            I64Op::Shl  => self.enc.lsl_x(dst, lhs, rhs)?,
            I64Op::ShrS => self.enc.asr_x(dst, lhs, rhs)?,
            I64Op::ShrU => self.enc.lsr_x(dst, lhs, rhs)?,
        }
        Ok(())
    }

    /// Lower `i64.load off` — pop i32 addr, push i64 value from
    /// `mem_base + addr + offset`. LDR Xt requires 8-aligned offset.
    fn lower_i64_load(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let addr = self.pop_i32_slot()?;
        self.emit_bounds_check(addr, 8, offset)?;
        let dst = self.push_i64_slot()?;
        // Effective-address computation into X16 (caller-saved IP)
        // since `addr` was an i32 slot and `dst` is an i64 slot; they
        // may alias physically but we need the full X register as the
        // LDR destination, so compute separately.
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.ldr_imm(dst, Reg::X16, offset)?;
        Ok(())
    }

    /// Lower `i64.store off` — pop i64 value, pop i32 addr, write the
    /// full 64-bit value. STR Xt requires 8-aligned offset.
    fn lower_i64_store(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let val = self.pop_i64_slot()?;
        let addr = self.pop_i32_slot()?;
        self.emit_bounds_check(addr, 8, offset)?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.str_imm(val, Reg::X16, offset)?;
        Ok(())
    }

    /// Lower `i64.eqz` — unary. CMP X + CSET Xd, EQ. The result is
    /// i32, so the i64 slot is popped and an i32 slot is pushed.
    fn lower_i64_eqz(&mut self) -> Result<(), LowerError> {
        let src = self.pop_i64_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.cmp_x(src, Reg::ZR)?;
        self.enc.cset(dst, Condition::Eq)?;
        Ok(())
    }

    /// Lower `i64.<cmp>`. Like `lower_f32_cmp`, this pops two values
    /// of one type (i64) and pushes an i32 boolean result — a cross-
    /// type stack transition.
    fn lower_i64_cmp(&mut self, cond: Condition) -> Result<(), LowerError> {
        let rhs = self.pop_i64_slot()?;
        let lhs = self.pop_i64_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.cmp_x(lhs, rhs)?;
        self.enc.cset(dst, cond)?;
        Ok(())
    }

    /// Lower `i32.wrap_i64` — the top i64 becomes an i32. Physically
    /// the same X register, but we zero the upper 32 bits so that
    /// subsequent 64-bit ops on its low-32-bit value don't see stale
    /// high bits. `AND Wd, Wn, Wn` trivially zeros the upper 32 of
    /// the hosting X via 32-bit-write-zeroes semantics.
    fn lower_wrap_i64(&mut self) -> Result<(), LowerError> {
        let src = self.pop_i64_slot()?;
        let dst = self.push_i32_slot()?;
        // dst will be the SAME physical register as src — we popped
        // the i64 (decrementing int_depth) and pushed an i32 at the
        // same slot. AND Wd, Wn, Wn clears upper 32 of the parent X.
        self.enc.and_w(dst, src, src)?;
        Ok(())
    }

    /// Lower `i64.extend_i32_s/_u`. Signed uses SXTW; unsigned
    /// leverages the fact that `AND Wd, Wn, Wn` already zero-
    /// extends to 64 bits via the ISA's 32-bit-write rule.
    fn lower_extend_i32(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_i64_slot()?;
        if signed {
            self.enc.sxtw(dst, src)?;
        } else {
            // `mov wd, wn` via 32-bit AND — zero-extends to X.
            self.enc.and_w(dst, src, src)?;
        }
        Ok(())
    }

    // ── Phase 15 conversion lowerings ───────────────────────────────

    /// `i32.extend8_s` / `i32.extend16_s` — sign-extend narrow view
    /// of an i32 back to a full i32. Same register slot (stack type
    /// unchanged), just rewrites the upper bits.
    fn lower_i32_extend_narrow(&mut self, is_8: bool, _unused: bool) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_i32_slot()?;
        debug_assert_eq!(dst.0, src.0);
        if is_8 {
            self.enc.sxtb_w(dst, src)?;
        } else {
            self.enc.sxth_w(dst, src)?;
        }
        Ok(())
    }

    /// `i64.extend{8,16,32}_s` — sign-extend narrow view of an i64.
    fn lower_i64_extend_narrow(&mut self, width: ExtendWidth) -> Result<(), LowerError> {
        let src = self.pop_i64_slot()?;
        let dst = self.push_i64_slot()?;
        debug_assert_eq!(dst.0, src.0);
        match width {
            ExtendWidth::B8 => self.enc.sxtb_x(dst, src)?,
            ExtendWidth::B16 => self.enc.sxth_x(dst, src)?,
            ExtendWidth::B32 => self.enc.sxtw(dst, src)?,
        }
        Ok(())
    }

    /// `i32.trunc_f32_{s,u}` — pop f32, push i32. FCVTZS/FCVTZU round
    /// toward zero (WASM's trunc semantics). Cross-bank: V → X slot.
    fn lower_trunc_f32_i32(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_f32_slot()?;
        let dst = self.push_i32_slot()?;
        if signed {
            self.enc.fcvtzs_w_s(dst, src)?;
        } else {
            self.enc.fcvtzu_w_s(dst, src)?;
        }
        Ok(())
    }

    fn lower_trunc_f64_i32(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_f64_slot()?;
        let dst = self.push_i32_slot()?;
        if signed {
            self.enc.fcvtzs_w_d(dst, src)?;
        } else {
            self.enc.fcvtzu_w_d(dst, src)?;
        }
        Ok(())
    }

    fn lower_trunc_f32_i64(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_f32_slot()?;
        let dst = self.push_i64_slot()?;
        if signed {
            self.enc.fcvtzs_x_s(dst, src)?;
        } else {
            self.enc.fcvtzu_x_s(dst, src)?;
        }
        Ok(())
    }

    fn lower_trunc_f64_i64(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_f64_slot()?;
        let dst = self.push_i64_slot()?;
        if signed {
            self.enc.fcvtzs_x_d(dst, src)?;
        } else {
            self.enc.fcvtzu_x_d(dst, src)?;
        }
        Ok(())
    }

    /// `f32.convert_i32_{s,u}` — pop i32, push f32. SCVTF/UCVTF.
    fn lower_convert_i32_f32(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_f32_slot()?;
        if signed {
            self.enc.scvtf_s_w(dst, src)?;
        } else {
            self.enc.ucvtf_s_w(dst, src)?;
        }
        Ok(())
    }

    fn lower_convert_i64_f32(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_i64_slot()?;
        let dst = self.push_f32_slot()?;
        if signed {
            self.enc.scvtf_s_x(dst, src)?;
        } else {
            self.enc.ucvtf_s_x(dst, src)?;
        }
        Ok(())
    }

    fn lower_convert_i32_f64(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_f64_slot()?;
        if signed {
            self.enc.scvtf_d_w(dst, src)?;
        } else {
            self.enc.ucvtf_d_w(dst, src)?;
        }
        Ok(())
    }

    fn lower_convert_i64_f64(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_i64_slot()?;
        let dst = self.push_f64_slot()?;
        if signed {
            self.enc.scvtf_d_x(dst, src)?;
        } else {
            self.enc.ucvtf_d_x(dst, src)?;
        }
        Ok(())
    }

    /// `f32.demote_f64` — pop f64, push f32. Same V-register slot
    /// physically; lossy rounding from double to single precision.
    fn lower_f32_demote_f64(&mut self) -> Result<(), LowerError> {
        let src = self.pop_f64_slot()?;
        let dst = self.push_f32_slot()?;
        debug_assert_eq!(dst.0, src.0);
        self.enc.fcvt_s_d(dst, src)?;
        Ok(())
    }

    /// `f64.promote_f32` — pop f32, push f64. Exact (f32 values are
    /// a subset of f64).
    fn lower_f64_promote_f32(&mut self) -> Result<(), LowerError> {
        let src = self.pop_f32_slot()?;
        let dst = self.push_f64_slot()?;
        debug_assert_eq!(dst.0, src.0);
        self.enc.fcvt_d_s(dst, src)?;
        Ok(())
    }

    /// Bit-cast reinterprets — no numeric conversion, only a bank swap.
    /// All four use FMOV under the hood; free as far as the hardware
    /// is concerned.
    fn lower_i32_reinterpret_f32(&mut self) -> Result<(), LowerError> {
        let src = self.pop_f32_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.fmov_w_from_s(dst, src)?;
        Ok(())
    }

    fn lower_i64_reinterpret_f64(&mut self) -> Result<(), LowerError> {
        let src = self.pop_f64_slot()?;
        let dst = self.push_i64_slot()?;
        self.enc.fmov_x_from_d(dst, src)?;
        Ok(())
    }

    fn lower_f32_reinterpret_i32(&mut self) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_f32_slot()?;
        self.enc.fmov_s_from_w(dst, src)?;
        Ok(())
    }

    fn lower_f64_reinterpret_i64(&mut self) -> Result<(), LowerError> {
        let src = self.pop_i64_slot()?;
        let dst = self.push_f64_slot()?;
        self.enc.fmov_d_from_x(dst, src)?;
        Ok(())
    }

    /// Lower `i32.eqz` — unary "is zero". `cmp_w` against WZR
    /// (register 31 in 32-bit form reads as zero) sets the Z flag,
    /// then `cset Xd, EQ` converts Z=1 to a 1 in Xd.
    fn lower_eqz(&mut self) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.cmp_w(src, Reg::ZR)?;
        self.enc.cset(dst, Condition::Eq)?;
        Ok(())
    }

    fn lower_binop(&mut self, op: BinOp) -> Result<(), LowerError> {
        let rhs = self.pop_i32_slot()?;
        let lhs = self.pop_i32_slot()?;
        let dst = self.push_i32_slot()?;
        debug_assert_eq!(dst.0, lhs.0);
        match op {
            BinOp::Add  => self.enc.add(dst, lhs, rhs)?,
            BinOp::Sub  => self.enc.sub(dst, lhs, rhs)?,
            BinOp::Mul  => self.enc.mul(dst, lhs, rhs)?,
            BinOp::DivS => self.enc.sdiv(dst, lhs, rhs)?,
            BinOp::DivU => self.enc.udiv(dst, lhs, rhs)?,
            // Bitops use 32-bit W-variants so WASM i32 semantics
            // hold exactly (shifts mod 32, not mod 64, etc). The
            // 32-bit form zero-writes the upper 32 of the hosting
            // X register — consistent with how MOVZ/MOVK fill in
            // our constant lowering.
            BinOp::And  => self.enc.and_w(dst, lhs, rhs)?,
            BinOp::Or   => self.enc.orr_w(dst, lhs, rhs)?,
            BinOp::Xor  => self.enc.eor_w(dst, lhs, rhs)?,
            BinOp::Shl  => self.enc.lsl_w(dst, lhs, rhs)?,
            BinOp::ShrS => self.enc.asr_w(dst, lhs, rhs)?,
            BinOp::ShrU => self.enc.lsr_w(dst, lhs, rhs)?,
            // 32-bit compare: use CMP W (low 32 bits) so i32-semantic
            // comparisons ignore any upper-bit residue left by prior
            // arithmetic. CSET converts the flag result into 0 or 1.
            BinOp::Cmp(cond) => {
                self.enc.cmp_w(lhs, rhs)?;
                self.enc.cset(dst, cond)?;
            }
        }
        Ok(())
    }

    /// Look up a local's storage, returning its type-erased handle.
    /// Callers dispatch on the variant to pick ADD/MOV vs FMOV.
    fn local_loc(&self, idx: u32) -> Result<LocalLoc, LowerError> {
        let i = idx as usize;
        self.locals.get(i).copied().ok_or(LowerError::LocalOutOfRange)
    }

    fn lower_local_get(&mut self, idx: u32) -> Result<(), LowerError> {
        match self.local_loc(idx)? {
            LocalLoc::I32(local) => {
                let dst = self.push_i32_slot()?;
                // A64 has no "MOV Xd, Xn"; idiom is `ADD Xd, XZR, Xn`.
                self.enc.add(dst, Reg::ZR, local)?;
            }
            LocalLoc::I64(local) => {
                let dst = self.push_i64_slot()?;
                // Same encoder — ADD X is already 64-bit.
                self.enc.add(dst, Reg::ZR, local)?;
            }
            LocalLoc::F32(local) => {
                let dst = self.push_f32_slot()?;
                self.enc.fmov_s_s(dst, local)?;
            }
            LocalLoc::F64(local) => {
                let dst = self.push_f64_slot()?;
                self.enc.fmov_d_d(dst, local)?;
            }
        }
        Ok(())
    }

    fn lower_local_set(&mut self, idx: u32) -> Result<(), LowerError> {
        match self.local_loc(idx)? {
            LocalLoc::I32(local) => {
                let src = self.pop_i32_slot()?;
                self.enc.add(local, Reg::ZR, src)?;
            }
            LocalLoc::I64(local) => {
                let src = self.pop_i64_slot()?;
                self.enc.add(local, Reg::ZR, src)?;
            }
            LocalLoc::F32(local) => {
                let src = self.pop_f32_slot()?;
                self.enc.fmov_s_s(local, src)?;
            }
            LocalLoc::F64(local) => {
                let src = self.pop_f64_slot()?;
                self.enc.fmov_d_d(local, src)?;
            }
        }
        Ok(())
    }

    fn lower_block(&mut self) -> Result<(), LowerError> {
        self.label_stack.push(Label {
            kind: LabelKind::Block,
            loop_start: None,
            pending: Vec::new(),
            entry_depth: self.stack.len(),
        });
        Ok(())
    }

    fn lower_loop(&mut self) -> Result<(), LowerError> {
        self.label_stack.push(Label {
            kind: LabelKind::Loop,
            loop_start: Some(self.enc.pos()),
            pending: Vec::new(),
            entry_depth: self.stack.len(),
        });
        Ok(())
    }

    fn label_index(&self, depth: u32) -> Result<usize, LowerError> {
        let n = self.label_stack.len();
        let d = depth as usize;
        if d >= n {
            return Err(LowerError::LabelOutOfRange);
        }
        Ok(n - 1 - d)
    }

    fn lower_br(&mut self, depth: u32) -> Result<(), LowerError> {
        let idx = self.label_index(depth)?;
        match self.label_stack[idx].kind {
            LabelKind::Loop => {
                // Backward branch — target is already known.
                let target = self.label_stack[idx]
                    .loop_start
                    .expect("loop label has start pos");
                let here = self.enc.pos();
                let offset = target as i32 - here as i32;
                self.enc.b(offset)?;
            }
            // Block, If, and IfElse are all forward-targeting labels —
            // their end is where a `br` lands. Add the patch to the
            // label's pending list; `lower_block_end` will walk it
            // when the end is reached.
            LabelKind::Block | LabelKind::If { .. } | LabelKind::IfElse { .. } => {
                let pos = self.enc.pos();
                self.enc.b(0)?;
                self.label_stack[idx]
                    .pending
                    .push(PendingPatch::B { pos });
            }
        }
        Ok(())
    }

    fn lower_br_if(&mut self, depth: u32) -> Result<(), LowerError> {
        // Pop condition (i32). We check the low 32 bits via CBNZ Wr.
        let cond = self.pop_i32_slot()?;
        let idx = self.label_index(depth)?;
        match self.label_stack[idx].kind {
            LabelKind::Loop => {
                let target = self.label_stack[idx]
                    .loop_start
                    .expect("loop label has start pos");
                let here = self.enc.pos();
                let offset = target as i32 - here as i32;
                self.enc.cbnz_w(cond, offset)?;
            }
            LabelKind::Block | LabelKind::If { .. } | LabelKind::IfElse { .. } => {
                let pos = self.enc.pos();
                self.enc.cbnz_w(cond, 0)?;
                self.label_stack[idx]
                    .pending
                    .push(PendingPatch::CbnzW { pos, rt: cond });
            }
        }
        Ok(())
    }

    fn lower_block_end(&mut self) -> Result<(), LowerError> {
        let label = self.label_stack.pop().ok_or(LowerError::UnbalancedEnd)?;
        let target = self.enc.pos();

        match label.kind {
            LabelKind::Block => {
                // Patch every pending forward br/br_if to point at
                // the byte after the end.
                for patch in &label.pending {
                    self.patch_pending(*patch, target)?;
                }
            }
            LabelKind::Loop => {
                // Backward branches were resolved at emission — nothing
                // to do here.
            }
            LabelKind::If { cond_branch_pos } => {
                // `if` without an `else`: the condition-false path
                // falls through to here, so the CBZ jumps directly
                // to end.
                let offset = target as i32 - cond_branch_pos as i32;
                let word = encode_cbz_w(
                    // Rt was encoded into the original word; we
                    // re-encode with the same register by reading
                    // the 5 low bits of the stored word.
                    rt_from_cbz_at(&self.enc, cond_branch_pos),
                    offset,
                )
                .map_err(|_| LowerError::BranchOutOfRange)?;
                self.enc.patch_word(cond_branch_pos, word);
                // Also patch any br/br_if from inside the then-branch.
                for patch in &label.pending {
                    self.patch_pending(*patch, target)?;
                }
            }
            LabelKind::IfElse { else_skip_pos } => {
                // Patch the B from end-of-then to jump here (the
                // byte after the else-branch).
                let offset = target as i32 - else_skip_pos as i32;
                let word = encode_b(offset)
                    .map_err(|_| LowerError::BranchOutOfRange)?;
                self.enc.patch_word(else_skip_pos, word);
                // And patch any br/br_if from inside either branch.
                for patch in &label.pending {
                    self.patch_pending(*patch, target)?;
                }
            }
        }
        Ok(())
    }

    /// Patch a single pending forward-branch placeholder to target
    /// the given code offset.
    fn patch_pending(&mut self, patch: PendingPatch, target: usize) -> Result<(), LowerError> {
        match patch {
            PendingPatch::B { pos } => {
                let offset = target as i32 - pos as i32;
                let word = encode_b(offset)
                    .map_err(|_| LowerError::BranchOutOfRange)?;
                self.enc.patch_word(pos, word);
            }
            PendingPatch::CbnzW { pos, rt } => {
                let offset = target as i32 - pos as i32;
                let word = encode_cbnz_w(rt, offset)
                    .map_err(|_| LowerError::BranchOutOfRange)?;
                self.enc.patch_word(pos, word);
            }
        }
        Ok(())
    }

    fn lower_if(&mut self) -> Result<(), LowerError> {
        // Pop the i32 condition and emit a CBZ to the "false" target.
        // Placeholder offset; patched at `else` or `end` depending on
        // whether the block has an else-branch.
        let cond = self.pop_i32_slot()?;
        let pos = self.enc.pos();
        self.enc.cbz_w(cond, 0)?;
        self.label_stack.push(Label {
            kind: LabelKind::If { cond_branch_pos: pos },
            loop_start: None,
            pending: Vec::new(),
            entry_depth: self.stack.len(),
        });
        Ok(())
    }

    fn lower_else(&mut self) -> Result<(), LowerError> {
        // Must be inside an open `if`. Upgrade the label to IfElse,
        // emit the then→end skip branch, and patch the original CBZ
        // to point at the start of the else-branch.
        let label = self.label_stack.last_mut().ok_or(LowerError::ElseWithoutIf)?;
        let cond_branch_pos = match label.kind {
            LabelKind::If { cond_branch_pos } => cond_branch_pos,
            _ => return Err(LowerError::ElseWithoutIf),
        };

        // Emit the B that end-of-then uses to skip the else-branch.
        let skip_pos = self.enc.pos();
        self.enc.b(0)?;

        // Patch the `if`'s CBZ to target the first instruction of
        // the else-branch — that's the current position (right
        // after the B we just emitted).
        let else_target = self.enc.pos();
        let offset = else_target as i32 - cond_branch_pos as i32;
        let word = encode_cbz_w(
            rt_from_cbz_at(&self.enc, cond_branch_pos),
            offset,
        )
        .map_err(|_| LowerError::BranchOutOfRange)?;
        self.enc.patch_word(cond_branch_pos, word);

        // Update the label to IfElse so `end` knows to patch the B.
        // Also reset the operand stack to the if-entry depth: WASM
        // semantics say the else-branch sees the same stack as the
        // then-branch entry, not whatever the then-branch left.
        let label = self.label_stack.last_mut().unwrap();
        let entry = label.entry_depth;
        label.kind = LabelKind::IfElse { else_skip_pos: skip_pos };
        self.truncate_stack_to(entry);
        Ok(())
    }

    /// Pop slots until `stack.len() == target`, updating per-type
    /// counters along the way. Used for `else` depth-reset.
    fn truncate_stack_to(&mut self, target: usize) {
        while self.stack.len() > target {
            let ty = self.stack.pop().unwrap();
            if ty.is_int() {
                self.int_depth -= 1;
            } else {
                self.fp_depth -= 1;
            }
        }
    }

    /// Lower a direct `call funcidx`.
    ///
    /// AAPCS64 arg-marshalling: the signature (looked up in
    /// `call_sigs[idx]`, defaulting to 0-arg/i32-return when absent)
    /// tells us how many operand-stack slots to pop as args and which
    /// target registers they land in. Staging through X9..X(8+N)
    /// avoids clobbering AAPCS64 arg regs when the source-register
    /// band overlaps the target band (which happens whenever the
    /// stack has anything below the args).
    ///
    /// BLR clobbers all caller-saved registers (X0..X18 in the int
    /// bank, V0..V7/V16..V31 in the FP bank) — any *live* slot below
    /// the args is therefore logically destroyed by the call. The
    /// lowerer does not save them; programs must be structured so
    /// the stack at call time holds only the args. Compiled WASM
    /// naturally respects this, which is why we don't enforce it.
    fn lower_call(&mut self, idx: u32) -> Result<(), LowerError> {
        let target = *self
            .call_targets
            .get(idx as usize)
            .ok_or(LowerError::CallTargetMissing)?;
        // Signature lookup — fall back to 0-arg/i32-return for
        // pre-sigs callers (Phase 4A helpers, the SSH-era tests).
        let sig = self
            .call_sigs
            .get(idx as usize)
            .cloned()
            .unwrap_or(FnSig { params: Vec::new(), result: Some(ValType::I32) });

        // Only integer params supported — FP args would require
        // dual-bank staging through V0..V7 per AAPCS64. Add when a
        // test case needs it.
        for p in &sig.params {
            if !p.is_int() {
                return Err(LowerError::CallTypeUnsupported);
            }
        }
        let n_args = sig.params.len();
        if n_args > 8 {
            return Err(LowerError::CallArityUnsupported);
        }

        // Step 1: pop args in reverse order (top of stack = rightmost
        // arg in WASM semantics). Record source register for each.
        let mut arg_src: Vec<Reg> = Vec::with_capacity(n_args);
        for p in sig.params.iter().rev() {
            let src = match p {
                ValType::I32 => self.pop_i32_slot()?,
                ValType::I64 => self.pop_i64_slot()?,
                _ => unreachable!("filtered above"),
            };
            arg_src.push(src);
        }
        arg_src.reverse();

        // Step 2: synthesize the callee address into X16 (AAPCS64
        // intra-procedure scratch). Constant MOVZ + up-to-three
        // MOVKs covers any 64-bit value; emitting only the non-zero
        // halves keeps the common case (low addresses) compact.
        let x16 = Reg::X16;
        self.enc.movz(x16, (target & 0xFFFF) as u16, MovShift::Lsl0)?;
        let h1 = ((target >> 16) & 0xFFFF) as u16;
        if h1 != 0 { self.enc.movk(x16, h1, MovShift::Lsl16)?; }
        let h2 = ((target >> 32) & 0xFFFF) as u16;
        if h2 != 0 { self.enc.movk(x16, h2, MovShift::Lsl32)?; }
        let h3 = ((target >> 48) & 0xFFFF) as u16;
        if h3 != 0 { self.enc.movk(x16, h3, MovShift::Lsl48)?; }

        // Step 3: stage args into X9..X(8+N), then copy to X0..X(N-1).
        // The two-phase copy avoids clobbers when arg source regs
        // alias target regs (e.g. arg 0 sits in X3, arg 1 in X4;
        // naive order `mov X0, X3; mov X1, X4` is fine, but if arg
        // 0 sat in X1 we'd overwrite it before reading arg 1's source
        // if we did `mov X0, X1; mov X1, X?`). Using 64-bit MOV via
        // `add Xd, XZR, Xn` preserves both i32 (upper bits already
        // zero from our lowerer) and i64 args uniformly.
        for (i, src) in arg_src.iter().enumerate() {
            let scratch = Reg((9 + i) as u8);
            self.enc.add(scratch, Reg::ZR, *src)?;
        }
        for i in 0..n_args {
            let scratch = Reg((9 + i) as u8);
            let target_reg = Reg(i as u8);
            self.enc.add(target_reg, Reg::ZR, scratch)?;
        }

        // Step 4: the call itself.
        self.enc.blr(x16)?;

        // Step 5: push result slot per signature. Result arrives in
        // X0 (i32 via W0 zero-extends, i64 via full X0). Moving to
        // the push slot's register preserves int_depth convention.
        match sig.result {
            None => {}
            Some(ValType::I32) => {
                let dst = self.push_i32_slot()?;
                if dst.0 != 0 {
                    self.enc.and_w(dst, Reg::X0, Reg::X0)?;
                } else {
                    // Even when the push slot IS X0, run AND W to zero
                    // the upper 32 bits — keeps i32-slot semantics
                    // consistent regardless of what the callee left
                    // in the high half of X0.
                    self.enc.and_w(Reg::X0, Reg::X0, Reg::X0)?;
                }
            }
            Some(ValType::I64) => {
                let dst = self.push_i64_slot()?;
                if dst.0 != 0 {
                    self.enc.add(dst, Reg::ZR, Reg::X0)?;
                }
            }
            Some(_) => return Err(LowerError::CallTypeUnsupported),
        }
        Ok(())
    }

    /// Lower `call_indirect type_id`. The operand stack must hold the
    /// args for the signature identified by `type_id` followed by an
    /// i32 table index at the top. This lowering:
    ///
    ///   1. Pops the table index.
    ///   2. Computes entry address = `table_base + (idx << 4)` in X17
    ///      with a single `ADD X17, X17, Widx, UXTW #4`.
    ///   3. Loads the callable address from `[X17]` into X16.
    ///   4. Pops the args and stages them through X9..X(8+N) scratch
    ///      (avoiding clobbers when arg sources overlap X0..X(N-1)),
    ///      then moves into X0..X(N-1) per AAPCS64.
    ///   5. BLR X16.
    ///   6. Pushes a result slot of the signature's result type,
    ///      moving from X0 if the new slot's register isn't X0.
    ///
    /// Bounds checking and runtime type-checking are deliberately
    /// out of scope for this phase — they require a trap epilogue and
    /// a dedicated test-harness signature. Hosts that build the table
    /// must supply valid entries matching the compile-time `type_id`.
    fn lower_call_indirect(&mut self, type_id: u32) -> Result<(), LowerError> {
        let table_base = self.table_base.ok_or(LowerError::TableNotConfigured)?;
        let sig = self
            .indirect_sigs
            .get(type_id as usize)
            .ok_or(LowerError::IndirectTypeMissing)?
            .clone();

        // Only integer params supported in this phase; FP args would
        // need separate AAPCS64 slots (V0..V7) and multi-bank
        // scratch staging. Add later once there's a test case.
        for p in &sig.params {
            if !p.is_int() {
                return Err(LowerError::IndirectTypeUnsupported);
            }
        }
        let n_args = sig.params.len();
        if n_args > 8 {
            return Err(LowerError::IndirectArityUnsupported);
        }

        // Step 1: pop the table index.
        let idx_reg = self.pop_i32_slot()?;

        // Step 2: synthesize table_base into X17 (IP1 scratch), then
        // add the scaled index.
        let x17 = Reg::X17;
        self.enc.movz(x17, (table_base & 0xFFFF) as u16, MovShift::Lsl0)?;
        let h1 = ((table_base >> 16) & 0xFFFF) as u16;
        if h1 != 0 { self.enc.movk(x17, h1, MovShift::Lsl16)?; }
        let h2 = ((table_base >> 32) & 0xFFFF) as u16;
        if h2 != 0 { self.enc.movk(x17, h2, MovShift::Lsl32)?; }
        let h3 = ((table_base >> 48) & 0xFFFF) as u16;
        if h3 != 0 { self.enc.movk(x17, h3, MovShift::Lsl48)?; }
        // X17 = X17 + UXTW(Widx) << 4  (idx * 16-byte entries)
        self.enc.add_ext_uxtw_shifted(x17, x17, idx_reg, 4)?;

        // Step 3: load the callable address.
        self.enc.ldr_imm(Reg::X16, x17, 0)?;

        // Step 4: pop args, stage into X9..X(8+N), then move to
        // X0..X(N-1). Stage-then-copy avoids source-clobber aliasing
        // when the arg source regs overlap the target register band.
        // Arg order in WASM: deepest is arg 0. Pop pops top (last arg
        // first); record in reverse and re-reverse for calling order.
        let mut arg_src: Vec<Reg> = Vec::with_capacity(n_args);
        for p in sig.params.iter().rev() {
            let src = match p {
                ValType::I32 => self.pop_i32_slot()?,
                ValType::I64 => self.pop_i64_slot()?,
                _ => unreachable!("filtered above"),
            };
            arg_src.push(src);
        }
        arg_src.reverse();

        // Stage: X(9+i) ← arg_src[i].  Use 64-bit MOV (ADD X, XZR, X)
        // to preserve the full width — if the callee expects a W
        // param the upper bits are ignored by the W instructions it
        // uses, and if it expects an X everything is already there.
        for (i, src) in arg_src.iter().enumerate() {
            let scratch = Reg((9 + i) as u8);
            self.enc.add(scratch, Reg::ZR, *src)?;
        }
        // Copy scratch into AAPCS64 arg regs.
        for i in 0..n_args {
            let scratch = Reg((9 + i) as u8);
            let target = Reg(i as u8);
            self.enc.add(target, Reg::ZR, scratch)?;
        }

        // Step 5: indirect call.
        self.enc.blr(Reg::X16)?;

        // Step 6: push result slot and move X0 into it if needed.
        match sig.result {
            None => {}
            Some(ValType::I32) => {
                let dst = self.push_i32_slot()?;
                if dst.0 != 0 {
                    // AND W zeros upper 32 so the i32 slot matches WASM
                    // i32 semantics regardless of what the callee left
                    // in the high half of X0.
                    self.enc.and_w(dst, Reg::X0, Reg::X0)?;
                } else {
                    // Slot IS X0; still mask the upper 32 bits to keep
                    // consistency with other i32-producing lowerings.
                    self.enc.and_w(Reg::X0, Reg::X0, Reg::X0)?;
                }
            }
            Some(ValType::I64) => {
                let dst = self.push_i64_slot()?;
                if dst.0 != 0 {
                    self.enc.add(dst, Reg::ZR, Reg::X0)?;
                }
            }
            Some(_) => return Err(LowerError::IndirectTypeUnsupported),
        }
        Ok(())
    }

    fn lower_function_end(&mut self) -> Result<(), LowerError> {
        // Function returns exactly one value. If it's an F32, bit-
        // cast into W0 so callers see the float's bit pattern as
        // a 32-bit integer in X0 (useful for exit-code harnesses).
        match self.stack.len() {
            1 => match self.stack_top_type().unwrap() {
                ValType::I32 => {
                    self.pop_i32_slot()?;
                }
                ValType::I64 => {
                    // 64-bit result already sits in X0 by design; just
                    // pop the slot bookkeeping.
                    self.pop_i64_slot()?;
                }
                ValType::F32 => {
                    let s = self.pop_f32_slot()?;
                    self.enc.fmov_w_from_s(Reg::X0, s)?;
                }
                ValType::F64 => {
                    let d = self.pop_f64_slot()?;
                    self.enc.fmov_x_from_d(Reg::X0, d)?;
                }
                ValType::V128 => {
                    // X0 is 64-bit — can't return a full v128 to a
                    // scalar caller. Programs that want to surface a
                    // vector must extract a lane first (see
                    // `f32x4.extract_lane`) or store to memory that
                    // the host reads after exec.
                    return Err(LowerError::V128ReturnUnsupported);
                }
            },
            _ => return Err(LowerError::StackNotSingleton),
        }
        if self.has_frame {
            // Epilogue mirrors the prologue: if we saved X28 too
            // (memory mode), restore it and pop a 32-byte frame;
            // otherwise just pop the 16-byte frame.
            if self.has_memory {
                self.enc.ldr_imm(MEM_BASE_REG, Reg::SP, 16)?;
                self.enc.ldp_post_indexed_64(Reg::X29, Reg::X30, Reg::SP, 32)?;
            } else {
                self.enc.ldp_post_indexed_64(Reg::X29, Reg::X30, Reg::SP, 16)?;
            }
        }
        self.enc.ret(Reg::X30)?;
        Ok(())
    }

    // ── Bounds-check + trap emission ────────────────────────────────
    //
    // Every load/store gets a runtime bounds check against `mem_size`.
    // If the address is out of range, we jump to an inline trap block
    // that sets X0 = -1 (exit code 0xFF as seen by the Pi daemon /
    // harness) and returns cleanly — frame pointer restored, saved
    // MEM_BASE_REG reloaded, stack realigned. Without these checks a
    // single buggy WASM program would segfault the server process;
    // with them the worst outcome is a trap that the daemon can log
    // and accept another CODE frame.

    /// Emit a CMP + B.LS + trap sequence. After this returns the
    /// hardware flags have been consumed; the next instructions are
    /// the normal load/store. If access is statically out-of-range
    /// (offset + size > mem_size or u32 overflow), emits an
    /// unconditional trap instead — saves the bounds-check code and
    /// guarantees the bad access never happens at runtime.
    fn emit_bounds_check(
        &mut self,
        addr_reg: Reg,
        access_size: u32,
        offset: u32,
    ) -> Result<(), LowerError> {
        let Some(access_end) = offset.checked_add(access_size) else {
            return self.emit_trap();
        };
        if access_end > self.mem_size {
            return self.emit_trap();
        }
        let max_valid = self.mem_size - access_end;

        // Materialize max_valid in X16 (AAPCS64 IP0 scratch — same
        // register the f32/f64 load/store paths use right after, but
        // only after this bounds check completes, so no conflict).
        let lo = (max_valid & 0xFFFF) as u16;
        let hi = ((max_valid >> 16) & 0xFFFF) as u16;
        self.enc.movz(Reg::X16, lo, MovShift::Lsl0)?;
        if hi != 0 {
            self.enc.movk(Reg::X16, hi, MovShift::Lsl16)?;
        }
        self.enc.cmp_w(addr_reg, Reg::X16)?;

        // Emit B.LS with a 0-offset placeholder; we'll patch after
        // the trap block so the branch skips over the trap when the
        // address is in range.
        let b_cond_pos = self.enc.pos();
        self.enc.b_cond(Condition::Ls, 0)?;

        self.emit_trap()?;

        // Patch the B.LS offset to point at the instruction right
        // after the trap block — i.e. the real load/store that will
        // be emitted next.
        let after_trap = self.enc.pos();
        let skip_offset = (after_trap as i32) - (b_cond_pos as i32);
        let patched = Encoder::encode_b_cond(Condition::Ls, skip_offset)?;
        self.enc.patch_word(b_cond_pos, patched);
        Ok(())
    }

    /// Emit the trap block. Sets X0 = -1 (all ones — exit code 0xFF),
    /// restores the saved MEM_BASE_REG and frame pointer if they were
    /// pushed in the prologue, and RETs. Safe to call from the
    /// middle of a function because it doesn't fall through.
    fn emit_trap(&mut self) -> Result<(), LowerError> {
        // MOVN Xd, #0 sets Xd = ~0 = 0xFFFF_FFFF_FFFF_FFFF. The low
        // 8 bits (0xFF) show up as the process exit code on the Pi
        // harness — distinctive signature that's easy to grep for.
        self.enc.movn(Reg::X0, 0, MovShift::Lsl0)?;
        if self.has_memory {
            // Restore the caller's X28 from the frame save slot.
            self.enc.ldr_imm(MEM_BASE_REG, Reg::SP, 16)?;
        }
        if self.has_frame {
            let frame_size = if self.has_memory { 32 } else { 16 };
            self.enc
                .ldp_post_indexed_64(Reg::X29, Reg::X30, Reg::SP, frame_size)?;
        }
        self.enc.ret(Reg::X30)?;
        Ok(())
    }

    /// Lower `i32.load off` — pop addr, compute effective address as
    /// `X28 + zero_ext(addr) + off`, load 32-bit word into a fresh
    /// stack slot. LDR Wt automatically zero-extends the upper 32
    /// bits of the hosting X register, matching WASM i32 semantics.
    fn lower_load(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let addr = self.pop_i32_slot()?;
        self.emit_bounds_check(addr, 4, offset)?;
        let dst = self.push_i32_slot()?;
        // Xdst = X28 + UXTW(Waddr). Safe to use dst as the effective-
        // address register because we don't touch its upper bits
        // after the LDR (which itself zero-extends).
        self.enc.add_ext_uxtw(dst, MEM_BASE_REG, addr)?;
        self.enc.ldr_w_imm(dst, dst, offset)?;
        Ok(())
    }

    /// Lower `f32.load off` — pop i32 addr, push f32 value from
    /// `mem_base + addr + offset`. Uses a scratch X-bank register
    /// (X16 = IP) to compute the effective address, then LDR Si.
    fn lower_f32_load(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let addr = self.pop_i32_slot()?;
        self.emit_bounds_check(addr, 4, offset)?;
        let dst = self.push_f32_slot()?;
        // Can't reuse dst (V-bank) as the effective-address reg —
        // those are different banks. Use X16 (caller-saved IP).
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.ldr_s_imm(dst, Reg::X16, offset)?;
        Ok(())
    }

    /// Lower `f32.store off` — pop f32 value, pop i32 addr, write
    /// the 32-bit bit pattern into memory. X16 holds the computed
    /// effective address.
    fn lower_f32_store(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let val = self.pop_f32_slot()?;
        let addr = self.pop_i32_slot()?;
        self.emit_bounds_check(addr, 4, offset)?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.str_s_imm(val, Reg::X16, offset)?;
        Ok(())
    }

    /// Lower `i32.store off` — pop value, pop addr, write value at
    /// `X28 + zero_ext(addr) + off`. Repurposes the addr register
    /// as the effective-address scratch; that's safe because it's
    /// already been popped from the operand stack.
    fn lower_store(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let val = self.pop_i32_slot()?;
        let addr = self.pop_i32_slot()?;
        self.emit_bounds_check(addr, 4, offset)?;
        self.enc.add_ext_uxtw(addr, MEM_BASE_REG, addr)?;
        self.enc.str_w_imm(val, addr, offset)?;
        Ok(())
    }

    fn lower_explicit_return(&mut self) -> Result<(), LowerError> {
        // `return` exits the function immediately. Move the top-of-
        // stack value into X0 (bit-casting from S if needed), then
        // RET. We don't pop — subsequent code on the stack is
        // unreachable.
        match self.stack_top_type() {
            None => return Err(LowerError::StackNotSingleton),
            Some(ValType::I32) | Some(ValType::I64) => {
                let top = Reg::new((self.int_depth - 1) as u8).unwrap();
                if top.0 != 0 {
                    self.enc.add(Reg::X0, Reg::ZR, top)?;
                }
            }
            Some(ValType::F32) => {
                let s = Vreg::new((self.fp_depth - 1) as u8).unwrap();
                self.enc.fmov_w_from_s(Reg::X0, s)?;
            }
            Some(ValType::F64) => {
                let d = Vreg::new((self.fp_depth - 1) as u8).unwrap();
                self.enc.fmov_x_from_d(Reg::X0, d)?;
            }
            Some(ValType::V128) => {
                return Err(LowerError::V128ReturnUnsupported);
            }
        }
        self.enc.ret(Reg::X30)?;
        Ok(())
    }
}

impl Default for Lowerer {
    fn default() -> Self { Self::new() }
}

/// Read the Rt (low-5) field of the CBZ/CBNZ instruction stored at
/// `pos` in the encoder's buffer. Used to re-encode the branch once
/// its offset is known without re-threading the Reg through label
/// state.
fn rt_from_cbz_at(enc: &Encoder, pos: usize) -> Reg {
    let bytes = enc.as_bytes();
    let word = u32::from_le_bytes([
        bytes[pos],
        bytes[pos + 1],
        bytes[pos + 2],
        bytes[pos + 3],
    ]);
    // Rt is bits 4..0 — we know it was an X0..X30 scratch, so safe to unwrap.
    Reg::new((word & 0x1F) as u8).unwrap_or(Reg::X0)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(ops: &[WasmOp]) -> Vec<u8> {
        let mut lw = Lowerer::new();
        lw.lower_all(ops).expect("lower");
        lw.finish()
    }

    fn compile_with_locals(n: usize, ops: &[WasmOp]) -> Vec<u8> {
        let mut lw = Lowerer::new_with_locals(n).expect("new_with_locals");
        lw.lower_all(ops).expect("lower");
        lw.finish()
    }

    fn bytes_as_u32s(bytes: &[u8]) -> Vec<u32> {
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    // ── Phase 2.0 regression ────────────────────────────────────────

    #[test]
    fn return_42_matches_phase_1() {
        let bytes = compile(&[WasmOp::I32Const(42), WasmOp::End]);
        assert_eq!(
            bytes,
            vec![
                0x40, 0x05, 0x80, 0xD2, // movz x0, #42
                0xC0, 0x03, 0x5F, 0xD6, // ret
            ]
        );
    }

    #[test]
    fn const_add_const() {
        let bytes = compile(&[
            WasmOp::I32Const(10),
            WasmOp::I32Const(20),
            WasmOp::I32Add,
            WasmOp::End,
        ]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words, vec![0xD2800140, 0xD2800281, 0x8B010000, 0xD65F03C0]);
    }

    #[test]
    fn nested_arithmetic() {
        let bytes = compile(&[
            WasmOp::I32Const(1),
            WasmOp::I32Const(2),
            WasmOp::I32Const(3),
            WasmOp::I32Add,
            WasmOp::I32Add,
            WasmOp::End,
        ]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words[0], 0xD2800020);
        assert_eq!(words[1], 0xD2800041);
        assert_eq!(words[2], 0xD2800062);
        assert_eq!(words[3], 0x8B020021);
        assert_eq!(words[4], 0x8B010000);
        assert_eq!(words[5], 0xD65F03C0);
    }

    // ── Phase 2.1: negative consts, locals ──────────────────────────

    #[test]
    fn negative_const_encodes_as_u32() {
        // i32.const -1 → MOVZ X0, #0xFFFF ; MOVK X0, #0xFFFF, LSL #16
        //              → 0xD29FFFE0 ; 0xF2BFFFE0 ; RET
        let bytes = compile(&[WasmOp::I32Const(-1), WasmOp::End]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words, vec![0xD29FFFE0, 0xF2BFFFE0, 0xD65F03C0]);
    }

    #[test]
    fn negative_small_const() {
        // i32.const -42 → -42 as u32 = 0xFFFFFFD6
        //   MOVZ X0, #0xFFD6 ; MOVK X0, #0xFFFF, LSL #16 ; RET
        let bytes = compile(&[WasmOp::I32Const(-42), WasmOp::End]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words, vec![0xD29FFAC0, 0xF2BFFFE0, 0xD65F03C0]);
    }

    #[test]
    fn locals_init_and_get() {
        // (func (local i32) local.get 0 end)
        // Prologue: MOVZ X19, #0
        // Body: ADD X0, XZR, X19 ; RET
        let bytes = compile_with_locals(1, &[WasmOp::LocalGet(0), WasmOp::End]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words[0], 0xD2800013); // movz x19, #0
        assert_eq!(words[1], 0x8B1303E0); // add x0, xzr, x19
        assert_eq!(words[2], 0xD65F03C0); // ret
    }

    #[test]
    fn local_set_then_get() {
        // (func (local i32)
        //   i32.const 7
        //   local.set 0
        //   local.get 0
        //   end)
        let bytes = compile_with_locals(
            1,
            &[
                WasmOp::I32Const(7),
                WasmOp::LocalSet(0),
                WasmOp::LocalGet(0),
                WasmOp::End,
            ],
        );
        let words = bytes_as_u32s(&bytes);
        // movz x19, #0
        // movz x0,  #7       ; push const
        // add  x19, xzr, x0  ; local.set 0
        // add  x0, xzr, x19  ; local.get 0
        // ret
        assert_eq!(words[0], 0xD2800013);
        assert_eq!(words[1], 0xD28000E0);
        assert_eq!(words[2], 0x8B0003F3);
        assert_eq!(words[3], 0x8B1303E0);
        assert_eq!(words[4], 0xD65F03C0);
    }

    #[test]
    fn too_many_locals_rejected() {
        let err = Lowerer::new_with_locals(MAX_LOCALS + 1).unwrap_err();
        assert_eq!(err, LowerError::TooManyLocals);
    }

    #[test]
    fn local_out_of_range() {
        let mut lw = Lowerer::new_with_locals(1).unwrap();
        assert_eq!(
            lw.lower_op(WasmOp::LocalGet(5)),
            Err(LowerError::LocalOutOfRange)
        );
    }

    // ── Phase 2.2: control flow ─────────────────────────────────────

    #[test]
    fn block_end_is_noop_when_empty() {
        // An empty block followed by a return should emit only the
        // constant load + RET (the block/end don't generate anything
        // on their own).
        let mut lw = Lowerer::new();
        lw.lower_op(WasmOp::Block).unwrap();
        lw.lower_op(WasmOp::End).unwrap(); // close block
        lw.lower_op(WasmOp::I32Const(1)).unwrap();
        lw.lower_op(WasmOp::End).unwrap(); // function end
        let words = bytes_as_u32s(&lw.finish());
        // movz x0, #1 ; ret
        assert_eq!(words, vec![0xD2800020, 0xD65F03C0]);
    }

    #[test]
    fn br_from_block_emits_patched_forward_branch() {
        // (func (result i32)
        //   i32.const 5
        //   block
        //     br 0             ;; unconditional jump to block end
        //   end
        //   end)
        //
        // Our compile-time stack tracking doesn't model reachability,
        // so anything after `br` in the same block would confuse the
        // function-end depth check.  Real WASM would let the verifier
        // discard unreachable ops; we keep the test simple and put
        // `br` last in the block.
        //
        // Expected bytes:
        //   movz x0, #5          ; push 5 (stack → [X0=5])
        //   b    +4              ; br 0 — patched to next insn (end-of-block)
        //   ret                  ; function end, X0 already holds result
        let bytes = compile(&[
            WasmOp::I32Const(5),
            WasmOp::Block,
            WasmOp::Br(0),
            WasmOp::End, // close block — patches the B placeholder
            WasmOp::End, // function end
        ]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words[0], 0xD28000A0); // movz x0, #5
        assert_eq!(words[1], 0x14000001); // b +4 (one instruction forward)
        assert_eq!(words[2], 0xD65F03C0); // ret
    }

    #[test]
    fn br_from_loop_is_backward_branch() {
        // loop
        //   br 0   ;; infinite loop: jump to loop start
        // end
        // i32.const 0
        // end
        //
        // Emission:
        //   b .       ; backward offset 0 — B to itself (infinite loop)
        //   movz x0,#0
        //   ret
        let bytes = compile(&[
            WasmOp::Loop,
            WasmOp::Br(0),
            WasmOp::End, // close loop
            WasmOp::I32Const(0),
            WasmOp::End, // function end
        ]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words[0], 0x14000000); // b . (offset 0)
        assert_eq!(words[1], 0xD2800000); // movz x0, #0
        assert_eq!(words[2], 0xD65F03C0); // ret
    }

    #[test]
    fn br_if_from_block_patches_cbnz() {
        // (func (result i32) (local i32)
        //   block
        //     local.get 0      ;; push local 0 as condition
        //     br_if 0          ;; branch to end if non-zero
        //     i32.const 7      ;; (only reached if condition == 0)
        //     drop-equivalent? -- we'll just make it the block's fallthrough
        //   end
        //   i32.const 42
        //   end)
        //
        // For simplicity, require the block leaves stack balanced.
        // Emission trace:
        //   movz x19, #0           ; local 0 init
        //   add  x0, xzr, x19       ; local.get 0 (push to X0)
        //   cbnz w0, <end-of-block> ; br_if 0 (placeholder)
        //   movz x0, #7             ; fallthrough const
        //   ; block end — patch cbnz to here
        //   ; but now stack has X0 from either br_if (prior) or i32.const 7.
        //   ; For MVP we don't enforce block signatures — the patched
        //   ; target is right after `movz x0, #7`.
        //   movz x0, #42            ; WAIT — we can't re-push x0, depth conflict
        //
        // Simpler test: just verify CBNZ is emitted and patched correctly.
        let mut lw = Lowerer::new_with_locals(1).unwrap();
        lw.lower_all(&[
            WasmOp::Block,
            WasmOp::LocalGet(0),
            WasmOp::BrIf(0),
            WasmOp::End, // close block — patches CBNZ
            WasmOp::I32Const(42),
            WasmOp::End, // function end
        ])
        .unwrap();
        let words = bytes_as_u32s(&lw.finish());
        // Word 0: movz x19, #0 (local init)
        assert_eq!(words[0], 0xD2800013);
        // Word 1: add x0, xzr, x19 (local.get 0)
        assert_eq!(words[1], 0x8B1303E0);
        // Word 2: cbnz w0, +4 (branch 4 bytes forward, to the movz at word 3)
        // imm19 = 1 (4/4), rt = 0 → 0x35000020
        assert_eq!(words[2], 0x35000020);
        // Word 3: movz x0, #42 (after block)
        assert_eq!(words[3], 0xD2800540);
        // Word 4: ret
        assert_eq!(words[4], 0xD65F03C0);
    }

    #[test]
    fn unbalanced_end_without_block() {
        let mut lw = Lowerer::new();
        // function end with empty stack — StackNotSingleton (not UnbalancedEnd,
        // because no open label exists, we fall through to lower_function_end).
        assert_eq!(
            lw.lower_op(WasmOp::End),
            Err(LowerError::StackNotSingleton)
        );
    }

    #[test]
    fn br_out_of_range() {
        let mut lw = Lowerer::new();
        lw.lower_op(WasmOp::Block).unwrap();
        assert_eq!(
            lw.lower_op(WasmOp::Br(5)),
            Err(LowerError::LabelOutOfRange)
        );
    }

    // ── Phase 2.0 regressions (kept) ────────────────────────────────

    #[test]
    fn stack_underflow_on_lonely_add() {
        let mut lw = Lowerer::new();
        assert_eq!(
            lw.lower_op(WasmOp::I32Add),
            Err(LowerError::StackUnderflow)
        );
    }

    #[test]
    fn end_rejects_multi_value_stack() {
        let mut lw = Lowerer::new();
        lw.lower_op(WasmOp::I32Const(1)).unwrap();
        lw.lower_op(WasmOp::I32Const(2)).unwrap();
        assert_eq!(
            lw.lower_op(WasmOp::End),
            Err(LowerError::StackNotSingleton)
        );
    }

    #[test]
    fn stack_overflow_at_16() {
        let mut lw = Lowerer::new();
        for _ in 0..16 {
            lw.lower_op(WasmOp::I32Const(0)).unwrap();
        }
        assert_eq!(
            lw.lower_op(WasmOp::I32Const(0)),
            Err(LowerError::TypedStackOverflow(ValType::I32))
        );
    }

    // ── Phase 4B: MUL / SDIV / UDIV ─────────────────────────────────

    #[test]
    fn mul_basic() {
        // (func (result i32) i32.const 6 i32.const 7 i32.mul end) → 42
        let bytes = compile(&[
            WasmOp::I32Const(6),
            WasmOp::I32Const(7),
            WasmOp::I32Mul,
            WasmOp::End,
        ]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words[0], 0xD28000C0); // movz x0, #6
        assert_eq!(words[1], 0xD28000E1); // movz x1, #7
        assert_eq!(words[2], 0x9B017C00); // mul x0, x0, x1
        assert_eq!(words[3], 0xD65F03C0); // ret
    }

    #[test]
    fn sdiv_basic() {
        let bytes = compile(&[
            WasmOp::I32Const(20),
            WasmOp::I32Const(4),
            WasmOp::I32DivS,
            WasmOp::End,
        ]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words[0], 0xD2800280); // movz x0, #20
        assert_eq!(words[1], 0xD2800081); // movz x1, #4
        assert_eq!(words[2], 0x9AC10C00); // sdiv x0, x0, x1
        assert_eq!(words[3], 0xD65F03C0); // ret
    }

    // ── Phase 4C: memory (I32Load / I32Store) ──────────────────────

    #[test]
    fn store_then_load_roundtrip() {
        // Builds: write 42 to mem[0], read it back, return it.
        //   i32.const 0    ; addr for store
        //   i32.const 42   ; value
        //   i32.store 0
        //   i32.const 0    ; addr for load
        //   i32.load 0
        //   end
        let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), 0xCAFEBABE).unwrap();
        lw.lower_all(&[
            WasmOp::I32Const(0),
            WasmOp::I32Const(42),
            WasmOp::I32Store(0),
            WasmOp::I32Const(0),
            WasmOp::I32Load(0),
            WasmOp::End,
        ])
        .unwrap();
        // Not checking every word; just sanity-check the prologue
        // saves X28 and the mem-base MOVZ/MOVK chain is emitted.
        let words = bytes_as_u32s(&lw.as_bytes());
        // Word 0: stp x29, x30, [sp, #-32]!  →  a9be7bfd
        assert_eq!(words[0], 0xA9BE7BFD);
        // Word 1: str x28, [sp, #16]         →  f90013fc  (imm12=2, Rn=31, Rt=28)
        //   base 0xF9000000 | (2<<10) | (31<<5) | 28
        //   = 0xF9000000 | 0x800 | 0x3E0 | 0x1C = 0xF9000BFC
        assert_eq!(words[1], 0xF9000BFC);
        // Word 2: movz x28, #0xBABE (low halfword of 0xCAFEBABE)
        //   base 0xD2800000 | (0xBABE << 5) | 28
        //   = 0xD2800000 | 0x17D7C0 | 0x1C = 0xD29757DC
        assert_eq!(words[2], 0xD29757DC);
    }

    #[test]
    fn load_without_memory_rejected() {
        let mut lw = Lowerer::new();
        lw.lower_op(WasmOp::I32Const(0)).unwrap();
        assert_eq!(
            lw.lower_op(WasmOp::I32Load(0)),
            Err(LowerError::MemoryNotConfigured)
        );
    }

    #[test]
    fn store_without_memory_rejected() {
        let mut lw = Lowerer::new();
        lw.lower_op(WasmOp::I32Const(0)).unwrap();
        lw.lower_op(WasmOp::I32Const(0)).unwrap();
        assert_eq!(
            lw.lower_op(WasmOp::I32Store(0)),
            Err(LowerError::MemoryNotConfigured)
        );
    }

    // ── Phase 5: comparisons ────────────────────────────────────────

    #[test]
    fn eq_basic() {
        // i32.const 5 ; i32.const 5 ; i32.eq ; end  →  1
        let bytes = compile(&[
            WasmOp::I32Const(5),
            WasmOp::I32Const(5),
            WasmOp::I32Eq,
            WasmOp::End,
        ]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words[0], 0xD28000A0); // movz x0, #5
        assert_eq!(words[1], 0xD28000A1); // movz x1, #5
        assert_eq!(words[2], 0x6B01001F); // cmp w0, w1
        assert_eq!(words[3], 0x9A9F17E0); // cset x0, eq
        assert_eq!(words[4], 0xD65F03C0); // ret
    }

    #[test]
    fn lt_s_basic() {
        let bytes = compile(&[
            WasmOp::I32Const(3),
            WasmOp::I32Const(5),
            WasmOp::I32LtS,
            WasmOp::End,
        ]);
        let words = bytes_as_u32s(&bytes);
        // movz x0, #3 ; movz x1, #5 ; cmp w0, w1 ; cset x0, lt ; ret
        assert_eq!(words[3], 0x9A9FA7E0); // cset x0, lt
    }

    #[test]
    fn gt_s_basic() {
        let bytes = compile(&[
            WasmOp::I32Const(5),
            WasmOp::I32Const(3),
            WasmOp::I32GtS,
            WasmOp::End,
        ]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words[3], 0x9A9FD7E0); // cset x0, gt
    }

    #[test]
    fn ne_basic() {
        let bytes = compile(&[
            WasmOp::I32Const(5),
            WasmOp::I32Const(3),
            WasmOp::I32Ne,
            WasmOp::End,
        ]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words[3], 0x9A9F07E0); // cset x0, ne
    }

    #[test]
    fn udiv_basic() {
        let bytes = compile(&[
            WasmOp::I32Const(100),
            WasmOp::I32Const(7),
            WasmOp::I32DivU,
            WasmOp::End,
        ]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words[2], 0x9AC10800); // udiv x0, x0, x1
    }

    // ── Phase 2.4: If / Else ────────────────────────────────────────

    #[test]
    fn if_without_else() {
        // (func (result i32) (local i32)
        //   local.get 0
        //   if
        //     i32.const 1
        //     local.set 0     ;; set local to 1 if cond was true
        //   end
        //   local.get 0
        //   end)
        let mut lw = Lowerer::new_with_locals(1).unwrap();
        lw.lower_all(&[
            WasmOp::LocalGet(0),
            WasmOp::If,
            WasmOp::I32Const(1),
            WasmOp::LocalSet(0),
            WasmOp::End, // close if (no else)
            WasmOp::LocalGet(0),
            WasmOp::End, // function end
        ])
        .unwrap();
        let words = bytes_as_u32s(&lw.finish());
        assert_eq!(words[0], 0xD2800013); // movz x19, #0 (local init)
        assert_eq!(words[1], 0x8B1303E0); // add x0, xzr, x19 (local.get 0 → cond)
        // Word 2: cbz w0, +12 (3 instructions forward) → imm19=3, rt=0
        // Target is right after the two instructions in the then-branch.
        assert_eq!(words[2], 0x34000060);
        assert_eq!(words[3], 0xD2800020); // movz x0, #1
        assert_eq!(words[4], 0x8B0003F3); // add x19, xzr, x0 (local.set 0)
        assert_eq!(words[5], 0x8B1303E0); // add x0, xzr, x19 (local.get 0)
        assert_eq!(words[6], 0xD65F03C0); // ret
    }

    #[test]
    fn if_else_two_branches() {
        // (func (result i32)
        //   i32.const 1
        //   if
        //     i32.const 10
        //   else
        //     i32.const 20
        //   end
        //   end)
        let bytes = compile(&[
            WasmOp::I32Const(1),
            WasmOp::If,
            WasmOp::I32Const(10),
            WasmOp::Else,
            WasmOp::I32Const(20),
            WasmOp::End, // close if/else
            WasmOp::End, // function end
        ]);
        let words = bytes_as_u32s(&bytes);
        assert_eq!(words[0], 0xD2800020); // movz x0, #1
        // Word 1: cbz w0, +12 (3 instr forward → else branch start)
        assert_eq!(words[1], 0x34000060);
        assert_eq!(words[2], 0xD2800140); // movz x0, #10 (then)
        // Word 3: b +8 (2 instr forward → past else)
        assert_eq!(words[3], 0x14000002);
        assert_eq!(words[4], 0xD2800280); // movz x0, #20 (else)
        assert_eq!(words[5], 0xD65F03C0); // ret
    }

    #[test]
    fn else_without_if_rejected() {
        let mut lw = Lowerer::new();
        lw.lower_op(WasmOp::I32Const(1)).unwrap();
        assert_eq!(
            lw.lower_op(WasmOp::Else),
            Err(LowerError::ElseWithoutIf)
        );
    }

    // ── Phase 2.3: Call + function frame ───────────────────────────

    #[test]
    fn new_function_emits_prologue_and_epilogue() {
        let mut lw = Lowerer::new_function(0, Vec::new()).unwrap();
        lw.lower_all(&[WasmOp::I32Const(0), WasmOp::End]).unwrap();
        let words = bytes_as_u32s(&lw.finish());
        assert_eq!(words[0], 0xA9BF7BFD); // stp x29, x30, [sp, #-16]!
        assert_eq!(words[1], 0xD2800000); // movz x0, #0
        assert_eq!(words[2], 0xA8C17BFD); // ldp x29, x30, [sp], #16
        assert_eq!(words[3], 0xD65F03C0); // ret
    }

    #[test]
    fn call_emits_movz_chain_blr_and_pushes_result() {
        // Target address 0x0000_1234_5678_ABCD — exercises h0, h1, h2
        // halfwords of the MOVZ/MOVK chain but not h3 (which is 0).
        let addr: u64 = 0x0000_1234_5678_ABCD;
        let mut lw = Lowerer::new_function(0, vec![addr]).unwrap();
        lw.lower_all(&[WasmOp::Call(0), WasmOp::End]).unwrap();
        let words = bytes_as_u32s(&lw.finish());
        // Word 0: prologue STP
        assert_eq!(words[0], 0xA9BF7BFD);
        // Word 1: movz x16, #0xABCD
        //   0xD2800000 | (0xABCD << 5) | 16 = 0xD29579B0
        assert_eq!(words[1], 0xD29579B0);
        // Word 2: movk x16, #0x5678, LSL #16
        //   0xF2800000 | (1 << 21) | (0x5678 << 5) | 16 = 0xF2AACF10
        assert_eq!(words[2], 0xF2AACF10);
        // Word 3: movk x16, #0x1234, LSL #32
        //   0xF2800000 | (2 << 21) | (0x1234 << 5) | 16 = 0xF2C24690
        assert_eq!(words[3], 0xF2C24690);
        // Word 4: blr x16
        assert_eq!(words[4], 0xD63F0200);
        // Word 5: and w0, w0, w0 — Phase 4A upper-32-bit mask on the
        // i32 result slot (emitted even when push slot IS X0, to
        // normalise whatever the callee left in the high half).
        //   0x0A000000 (Rd=0, Rn=0, Rm=0).
        assert_eq!(words[5], 0x0A000000);
        // Word 6: epilogue LDP
        assert_eq!(words[6], 0xA8C17BFD);
        // Word 7: ret
        assert_eq!(words[7], 0xD65F03C0);
    }

    #[test]
    fn call_rejects_missing_target() {
        let mut lw = Lowerer::new_function(0, Vec::new()).unwrap();
        assert_eq!(
            lw.lower_op(WasmOp::Call(5)),
            Err(LowerError::CallTargetMissing)
        );
    }

    #[test]
    fn call_high_addresses_emit_all_movk() {
        // Address with every halfword non-zero — confirms all 4
        // MOVZ/MOVK slots are emitted.
        let addr: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let mut lw = Lowerer::new_function(0, vec![addr]).unwrap();
        lw.lower_op(WasmOp::Call(0)).unwrap();
        // Prologue (1) + MOVZ+3×MOVK (4) + BLR (1) + AND W0 (1) = 7.
        assert_eq!(lw.as_bytes().len(), 7 * 4);
    }
}
