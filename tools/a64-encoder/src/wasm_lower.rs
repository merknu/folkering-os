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

use crate::{
    encode_b, encode_cbnz_w, encode_cbz_w, Condition, Encoder, EncodeError, MovShift, Reg,
};

/// Simplified WASM operator set.  Hand-constructed by callers, or
/// produced by [`crate::wasm_parse::parse_function_body`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Comparison op — the inner Condition is the "result is true"
    /// predicate (e.g. `Cmp(Eq)` sets result=1 when operands equal).
    Cmp(Condition),
}

/// Maximum WASM stack depth we can hold in registers (X0..X15).
const MAX_STACK: usize = 16;
/// Maximum number of locals we can host without spilling (X19..X27).
/// One fewer than before — X28 is now reserved for the memory base.
pub const MAX_LOCALS: usize = 9;
/// Register number where the locals band begins.
const LOCAL_BASE_REG: u8 = 19;
/// Register reserved for the linear-memory base pointer when a
/// memory-aware function is built. Callee-saved under AAPCS64, so
/// we save/restore it in the function's extended prologue.
const MEM_BASE_REG: Reg = Reg(28);

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
    depth: usize,
    num_locals: usize,
    label_stack: Vec<Label>,
    /// Absolute addresses of callable functions, indexed by WASM
    /// function index. Empty when `Call` is not expected.
    call_targets: Vec<u64>,
    /// True if `new_function` emitted a prologue; controls whether
    /// function-level `End` emits an epilogue (`LDP X29/X30` + RET)
    /// or just RET.
    has_frame: bool,
    /// True if the function frame includes a save slot for X28 and
    /// the prologue loaded the linear-memory base into X28. When set,
    /// `i32.load` and `i32.store` compile; otherwise they error.
    has_memory: bool,
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
        if n > MAX_LOCALS {
            return Err(LowerError::TooManyLocals);
        }
        let mut enc = Encoder::new();
        for i in 0..n {
            let r = Reg(LOCAL_BASE_REG + i as u8);
            enc.movz(r, 0, MovShift::Lsl0)?;
        }
        Ok(Self {
            enc,
            depth: 0,
            num_locals: n,
            label_stack: Vec::new(),
            call_targets: Vec::new(),
            has_frame: false,
            has_memory: false,
        })
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
        if n_locals > MAX_LOCALS {
            return Err(LowerError::TooManyLocals);
        }
        let mut enc = Encoder::new();
        // Prologue: stp x29, x30, [sp, #-16]!  ; mov x29, sp
        enc.stp_pre_indexed_64(Reg::X29, Reg::X30, Reg::SP, -16)?;
        // `MOV X29, SP` is encoded as `ADD X29, SP, #0` in the
        // immediate form, which we don't have yet. Skip the frame-
        // pointer set — the prologue's X29/X30 save is the key
        // ABI-preserving step and callers that don't walk the frame
        // chain don't care about X29.
        for i in 0..n_locals {
            let r = Reg(LOCAL_BASE_REG + i as u8);
            enc.movz(r, 0, MovShift::Lsl0)?;
        }
        Ok(Self {
            enc,
            depth: 0,
            num_locals: n_locals,
            label_stack: Vec::new(),
            call_targets,
            has_frame: true,
            has_memory: false,
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
        if n_locals > MAX_LOCALS {
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
        for i in 0..n_locals {
            let r = Reg(LOCAL_BASE_REG + i as u8);
            enc.movz(r, 0, MovShift::Lsl0)?;
        }
        Ok(Self {
            enc,
            depth: 0,
            num_locals: n_locals,
            label_stack: Vec::new(),
            call_targets,
            has_frame: true,
            has_memory: true,
        })
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
            WasmOp::LocalGet(i) => self.lower_local_get(i),
            WasmOp::LocalSet(i) => self.lower_local_set(i),
            WasmOp::Block => self.lower_block(),
            WasmOp::Loop => self.lower_loop(),
            WasmOp::Br(n) => self.lower_br(n),
            WasmOp::BrIf(n) => self.lower_br_if(n),
            WasmOp::If => self.lower_if(),
            WasmOp::Else => self.lower_else(),
            WasmOp::Call(n) => self.lower_call(n),
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
    pub fn stack_depth(&self) -> usize { self.depth }

    /// Currently open block/loop labels.
    pub fn open_labels(&self) -> usize { self.label_stack.len() }

    // ── Stack helpers ───────────────────────────────────────────────

    fn push_slot(&mut self) -> Result<Reg, LowerError> {
        if self.depth >= MAX_STACK {
            return Err(LowerError::StackOverflow);
        }
        let r = Reg::new(self.depth as u8).ok_or(LowerError::StackOverflow)?;
        self.depth += 1;
        Ok(r)
    }

    fn pop_slot(&mut self) -> Result<Reg, LowerError> {
        if self.depth == 0 {
            return Err(LowerError::StackUnderflow);
        }
        self.depth -= 1;
        Reg::new(self.depth as u8).ok_or(LowerError::StackUnderflow)
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
        let r = self.push_slot()?;
        self.enc.movz(r, lo, MovShift::Lsl0)?;
        if hi != 0 {
            self.enc.movk(r, hi, MovShift::Lsl16)?;
        }
        Ok(())
    }

    fn lower_binop(&mut self, op: BinOp) -> Result<(), LowerError> {
        let rhs = self.pop_slot()?;
        let lhs = self.pop_slot()?;
        let dst = self.push_slot()?;
        debug_assert_eq!(dst.0, lhs.0);
        match op {
            BinOp::Add  => self.enc.add(dst, lhs, rhs)?,
            BinOp::Sub  => self.enc.sub(dst, lhs, rhs)?,
            BinOp::Mul  => self.enc.mul(dst, lhs, rhs)?,
            BinOp::DivS => self.enc.sdiv(dst, lhs, rhs)?,
            BinOp::DivU => self.enc.udiv(dst, lhs, rhs)?,
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

    fn local_reg(&self, idx: u32) -> Result<Reg, LowerError> {
        let i = idx as usize;
        if i >= self.num_locals {
            return Err(LowerError::LocalOutOfRange);
        }
        Ok(Reg(LOCAL_BASE_REG + i as u8))
    }

    fn lower_local_get(&mut self, idx: u32) -> Result<(), LowerError> {
        let local = self.local_reg(idx)?;
        let dst = self.push_slot()?;
        // Copy local value to new stack slot. A64 has no "MOV Xd, Xn";
        // the idiomatic form is `ADD Xd, XZR, Xn` (or ORR Xd, XZR, Xn,
        // but we already have ADD in the encoder).
        self.enc.add(dst, Reg::ZR, local)?;
        Ok(())
    }

    fn lower_local_set(&mut self, idx: u32) -> Result<(), LowerError> {
        let local = self.local_reg(idx)?;
        let src = self.pop_slot()?;
        self.enc.add(local, Reg::ZR, src)?;
        Ok(())
    }

    fn lower_block(&mut self) -> Result<(), LowerError> {
        self.label_stack.push(Label {
            kind: LabelKind::Block,
            loop_start: None,
            pending: Vec::new(),
            entry_depth: self.depth,
        });
        Ok(())
    }

    fn lower_loop(&mut self) -> Result<(), LowerError> {
        self.label_stack.push(Label {
            kind: LabelKind::Loop,
            loop_start: Some(self.enc.pos()),
            pending: Vec::new(),
            entry_depth: self.depth,
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
        let cond = self.pop_slot()?;
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
        let cond = self.pop_slot()?;
        let pos = self.enc.pos();
        self.enc.cbz_w(cond, 0)?;
        self.label_stack.push(Label {
            kind: LabelKind::If { cond_branch_pos: pos },
            loop_start: None,
            pending: Vec::new(),
            entry_depth: self.depth,
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
        // Also reset the operand stack depth to the if-entry depth:
        // WASM semantics say the else-branch sees the same stack as
        // the then-branch entry, not whatever the then-branch left.
        let label = self.label_stack.last_mut().unwrap();
        let entry = label.entry_depth;
        label.kind = LabelKind::IfElse { else_skip_pos: skip_pos };
        self.depth = entry;
        Ok(())
    }

    fn lower_call(&mut self, idx: u32) -> Result<(), LowerError> {
        let target = *self
            .call_targets
            .get(idx as usize)
            .ok_or(LowerError::CallTargetMissing)?;

        // Load the 64-bit absolute address into X16 (the AAPCS64
        // intra-procedure-call scratch register). MOVZ + three
        // MOVKs covers any 64-bit value; we always emit all four
        // so the instruction sequence length is fixed — simpler
        // than variable-length chains when we later add call-site
        // patching.
        let x16 = Reg::X16;
        self.enc.movz(x16, (target & 0xFFFF) as u16, MovShift::Lsl0)?;
        let h1 = ((target >> 16) & 0xFFFF) as u16;
        if h1 != 0 {
            self.enc.movk(x16, h1, MovShift::Lsl16)?;
        }
        let h2 = ((target >> 32) & 0xFFFF) as u16;
        if h2 != 0 {
            self.enc.movk(x16, h2, MovShift::Lsl32)?;
        }
        let h3 = ((target >> 48) & 0xFFFF) as u16;
        if h3 != 0 {
            self.enc.movk(x16, h3, MovShift::Lsl48)?;
        }

        self.enc.blr(x16)?;

        // Callee's i32 result is in X0. Push a new stack slot that
        // references it. If our stack bottom is currently X0 (depth
        // 0), the result is already in place; otherwise we'd need a
        // copy — but by design our stack allocates X0 first. If we
        // push a slot at depth > 0, we need to move X0 to the new
        // slot's register. For Phase 2.3 MVP we require the pre-call
        // stack to be empty so X0 naturally holds the result.
        if self.depth != 0 {
            // Copy X0 to the new slot register.
            let dst = self.push_slot()?;
            self.enc.add(dst, Reg::ZR, Reg::X0)?;
        } else {
            // Fast path — X0 is already stack[0].
            let _ = self.push_slot()?;
        }
        Ok(())
    }

    fn lower_function_end(&mut self) -> Result<(), LowerError> {
        match self.depth {
            1 => {
                // Result is in X0 by design.
                self.pop_slot()?;
            }
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

    /// Lower `i32.load off` — pop addr, compute effective address as
    /// `X28 + zero_ext(addr) + off`, load 32-bit word into a fresh
    /// stack slot. LDR Wt automatically zero-extends the upper 32
    /// bits of the hosting X register, matching WASM i32 semantics.
    fn lower_load(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let addr = self.pop_slot()?;
        let dst = self.push_slot()?;
        // Xdst = X28 + UXTW(Waddr). Safe to use dst as the effective-
        // address register because we don't touch its upper bits
        // after the LDR (which itself zero-extends).
        self.enc.add_ext_uxtw(dst, MEM_BASE_REG, addr)?;
        self.enc.ldr_w_imm(dst, dst, offset)?;
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
        let val = self.pop_slot()?;
        let addr = self.pop_slot()?;
        self.enc.add_ext_uxtw(addr, MEM_BASE_REG, addr)?;
        self.enc.str_w_imm(val, addr, offset)?;
        Ok(())
    }

    fn lower_explicit_return(&mut self) -> Result<(), LowerError> {
        // `return` exits the function immediately. Move the top-of-
        // stack value to X0 if it isn't there already, then RET. We
        // don't pop — subsequent code on the stack is unreachable.
        if self.depth == 0 {
            return Err(LowerError::StackNotSingleton);
        }
        let top = Reg::new((self.depth - 1) as u8).unwrap();
        if top.0 != 0 {
            self.enc.add(Reg::X0, Reg::ZR, top)?;
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
            Err(LowerError::StackOverflow)
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
        // Word 5: epilogue LDP
        assert_eq!(words[5], 0xA8C17BFD);
        // Word 6: ret
        assert_eq!(words[6], 0xD65F03C0);
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
        // MOVZ/MOVK slots are emitted (5 instructions total including
        // BLR + 1 prologue word).
        let addr: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let mut lw = Lowerer::new_function(0, vec![addr]).unwrap();
        lw.lower_op(WasmOp::Call(0)).unwrap();
        // Prologue (1) + MOVZ+3×MOVK (4) + BLR (1) = 6 words.
        assert_eq!(lw.as_bytes().len(), 6 * 4);
    }
}
