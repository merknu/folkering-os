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

mod types;
mod stack;
mod simd;
mod memory;
mod convert;
mod control;
mod scalar;
mod call;

pub use types::{ValType, WasmOp, LowerError, FnSig, MAX_I32_LOCALS, MAX_F32_LOCALS, MAX_LOCALS};
use types::*;

use alloc::{vec, vec::Vec};

use crate::{
    Condition, Encoder, MovShift, Reg, Vreg,
};

#[derive(Debug)]
pub struct Lowerer {
    pub(super) enc: Encoder,
    /// Per-slot type tag, ordered bottom-first. `stack.len()` is the
    /// total operand-stack depth; each element tells the lowerer
    /// which register bank to reach for at that slot.
    pub(super) stack: Vec<ValType>,
    /// Count of live I32 slots (= next free X index). Incremented on
    /// `push_i32`, decremented on pop of an I32.
    /// Count of live integer slots (I32 + I64 combined, same X bank).
    pub(super) int_depth: usize,
    /// Count of live SIMD/FP slots (F32 + later F64, same V bank).
    pub(super) fp_depth: usize,
    /// Per-local host-register mapping, indexed by WASM local index.
    pub(super) locals: Vec<LocalLoc>,
    pub(super) label_stack: Vec<Label>,
    /// Absolute addresses of callable functions, indexed by WASM
    /// function index. Empty when `Call` is not expected.
    pub(super) call_targets: Vec<u64>,
    /// Parallel signature list for `call_targets`. `call_sigs[i]` is
    /// the AAPCS64-relevant signature of `call_targets[i]`. When the
    /// list is shorter than `call_targets` (or empty), `lower_call`
    /// treats missing entries as 0-arg / i32-return — preserves the
    /// Phase 4A contract for existing callers that didn't know sigs.
    pub(super) call_sigs: Vec<FnSig>,
    /// True if `new_function` emitted a prologue; controls whether
    /// function-level `End` emits an epilogue (`LDP X29/X30` + RET)
    /// or just RET.
    pub(super) has_frame: bool,
    /// True if the function frame includes a save slot for X28 and
    /// the prologue loaded the linear-memory base into X28. When set,
    /// `i32.load` and `i32.store` compile; otherwise they error.
    pub(super) has_memory: bool,
    /// Number of callee-saved STP pairs saved in the prologue
    /// (X19..X27, pairs of 2). Used by the epilogue + trap handler
    /// to emit the matching LDP restore sequence.
    pub(super) saved_int_pairs: usize,
    /// Frame size in bytes (without memory-base save). Derived from
    /// 16 (X29+X30) + saved_int_pairs * 16.
    pub(super) frame_size_base: i16,
    /// Size of the linear-memory buffer in bytes, as reported by the
    /// host (e.g. Pi daemon HELLO frame). The lowerer emits a runtime
    /// bounds check on every load/store that compares the dynamic
    /// address against `mem_size - offset - access_size`; addresses
    /// outside the buffer route to an inline trap block that sets
    /// X0 = -1 (exit code 0xFF) and RETs. Defaults to 64 KiB.
    pub(super) mem_size: u32,
    /// Absolute address of the function-reference table, or `None` if
    /// `call_indirect` is not configured. Each 16-byte entry holds
    /// `addr: u64` at offset 0 and `type_id: u32` at offset 8 (with
    /// 4 bytes of reserved padding). Typically placed in the caller-
    /// visible linear-memory region, but can be any valid pointer.
    pub(super) table_base: Option<u64>,
    /// Signatures indexed by WASM type index, used at `call_indirect`
    /// lowering to determine how many params to marshal and what
    /// return type to push. Empty when table-based calls aren't in use.
    pub(super) indirect_sigs: Vec<FnSig>,
    /// Byte ranges in the encoder buffer that contain literal-pool
    /// data (from v128.const). The peephole optimizer skips these
    /// so it doesn't corrupt data that happens to match an
    /// instruction pattern.
    pub(super) data_regions: Vec<crate::peephole::DataRegion>,
    /// Number of integer locals (used to compute overflow-band
    /// base: X(19 + n_int_locals) .. X27).
    pub(super) n_int_locals: usize,
    /// Total direct+extended register capacity for this function.
    /// = MAX_PRIMARY_INT + (9 - n_int_locals).
    pub(super) max_reg_int: usize,
    /// Byte offset from SP where spill[0] starts (past all
    /// callee-saved saves). 0 if no spill area.
    pub(super) spill_base: u32,
    /// Whether the frame has a spill area.
    pub(super) has_spill: bool,
    /// Pending spill: depth of the most recent push that went to
    /// spill and hasn't been STR'd to the frame yet. None = no
    /// pending. Flushed at the start of every push/pop/end.
    pub(super) pending_spill_depth: Option<usize>,
    /// Toggle for alternating scratch registers on consecutive
    /// spill pops. false = X14 next, true = X15 next.
    pub(super) spill_pop_toggle: bool,
    /// Byte offset from SP where FP spill[0] starts (after int spill area).
    pub(super) fp_spill_base: u32,
    /// Pending FP spill depth (same pattern as integer pending_spill_depth).
    pub(super) pending_fp_spill_depth: Option<usize>,
    /// Toggle for alternating FP scratch registers on consecutive pops.
    pub(super) fp_spill_pop_toggle: bool,
    /// Value of the most recent I32Const push, if the const is still
    /// the stack top (no intervening ops). Used by memory load/store
    /// to eliminate bounds checks when the address is statically known.
    /// Cleared at the start of every `lower_op` except I32Const.
    pub(super) last_i32_const_value: Option<i32>,
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
            saved_int_pairs: 0,
            frame_size_base: 0,
            mem_size: 64 * 1024,
            table_base: None,
            indirect_sigs: Vec::new(),
            call_sigs: Vec::new(),
            data_regions: Vec::new(),
            n_int_locals: 0,
            max_reg_int: MAX_PRIMARY_INT,
            spill_base: 0,
            has_spill: false,
            pending_spill_depth: None,
            spill_pop_toggle: false,
            fp_spill_base: 0,
            pending_fp_spill_depth: None,
            fp_spill_pop_toggle: false,
            last_i32_const_value: None,
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
        let n_int_locals = local_types.iter().filter(|t| t.is_int()).count();
        // Always save ALL callee-saved X19..X27 (5 STP pairs) so
        // the extended register band is AAPCS64-safe. Functions
        // that use X19..X(18+N) for locals need them restored;
        // functions that use X(19+N)..X27 for operand-stack
        // overflow also need them. Saving all 5 pairs
        // unconditionally keeps the prologue simple.
        let save_pairs = 5; // (X19,X20), (X21,X22), (X23,X24), (X25,X26), (X27,ZR)
        let callee_save_bytes = save_pairs * 16; // 80
        let spill_base_off = (16 + callee_save_bytes) as u32;
        let fp_spill_base_off = spill_base_off + SPILL_AREA_BYTES;
        let frame_size = (16 + callee_save_bytes + SPILL_AREA_BYTES as usize + FP_SPILL_AREA_BYTES as usize) as i16;
        enc.stp_pre_indexed_64(Reg::X29, Reg::X30, Reg::SP, -frame_size)?;
        // Save callee-saved registers used for locals at fixed offsets
        // within the frame, starting at SP+16. STP handles pairs.
        for pair in 0..save_pairs {
            let r1 = Reg(LOCAL_I32_BASE_REG + (pair * 2) as u8);
            let r2 = if pair * 2 + 1 < n_int_locals {
                Reg(LOCAL_I32_BASE_REG + (pair * 2 + 1) as u8)
            } else {
                Reg::ZR
            };
            let off = (16 + pair * 16) as i16;
            enc.stp_offset_64(r1, r2, Reg::SP, off)?;
        }
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
            saved_int_pairs: save_pairs,
            n_int_locals: n_int_locals,
            max_reg_int: MAX_PRIMARY_INT + (9 - n_int_locals),
            spill_base: spill_base_off,
            has_spill: true,
            pending_spill_depth: None,
            spill_pop_toggle: false,
            fp_spill_base: fp_spill_base_off,
            pending_fp_spill_depth: None,
            fp_spill_pop_toggle: false,
            last_i32_const_value: None,
            frame_size_base: frame_size,
            mem_size: 64 * 1024,
            table_base: None,
            indirect_sigs: Vec::new(),
            call_sigs: Vec::new(),
            data_regions: Vec::new(),
        })
    }

    /// Build a lowerer with the full function frame PLUS linear-memory
    /// base pinned in X28. Combines the spill-capable frame from
    /// `new_function_typed` with memory access.
    ///
    /// Frame layout (368 bytes):
    /// ```text
    /// SP+0:   X29, X30 save       (16B)
    /// SP+16:  X19..X27 save       (80B, 5 STP pairs)
    /// SP+96:  int spill area      (128B, 16 × 8B slots)
    /// SP+224: FP spill area       (128B, 8 × 16B slots)
    /// SP+352: X28 save            (16B, 8B value + 8B padding)
    /// ```
    pub fn new_function_with_memory(
        n_locals: usize,
        call_targets: Vec<u64>,
        mem_base: u64,
    ) -> Result<Self, LowerError> {
        let types = vec![ValType::I32; n_locals];
        Self::new_function_with_memory_typed(&types, call_targets, mem_base)
    }

    /// Typed-locals variant of `new_function_with_memory`.
    pub fn new_function_with_memory_typed(
        local_types: &[ValType],
        call_targets: Vec<u64>,
        mem_base: u64,
    ) -> Result<Self, LowerError> {
        let mut enc = Encoder::new();
        let n_int_locals = local_types.iter().filter(|t| t.is_int()).count();
        let save_pairs = 5;
        let callee_save_bytes = save_pairs * 16; // 80
        let spill_base_off = (16 + callee_save_bytes) as u32; // 96
        let fp_spill_base_off = spill_base_off + SPILL_AREA_BYTES; // 224
        let x28_save_off = fp_spill_base_off + FP_SPILL_AREA_BYTES; // 352
        let frame_size = (x28_save_off as usize + 16) as i16; // 368

        enc.stp_pre_indexed_64(Reg::X29, Reg::X30, Reg::SP, -frame_size)?;
        for pair in 0..save_pairs {
            let r1 = Reg(LOCAL_I32_BASE_REG + (pair * 2) as u8);
            let r2 = if pair * 2 + 1 < n_int_locals {
                Reg(LOCAL_I32_BASE_REG + (pair * 2 + 1) as u8)
            } else {
                Reg::ZR
            };
            let off = (16 + pair * 16) as i16;
            enc.stp_offset_64(r1, r2, Reg::SP, off)?;
        }
        enc.str_imm(MEM_BASE_REG, Reg::SP, x28_save_off)?;
        enc.movz(MEM_BASE_REG, (mem_base & 0xFFFF) as u16, MovShift::Lsl0)?;
        let h1 = ((mem_base >> 16) & 0xFFFF) as u16;
        if h1 != 0 { enc.movk(MEM_BASE_REG, h1, MovShift::Lsl16)?; }
        let h2 = ((mem_base >> 32) & 0xFFFF) as u16;
        if h2 != 0 { enc.movk(MEM_BASE_REG, h2, MovShift::Lsl32)?; }
        let h3 = ((mem_base >> 48) & 0xFFFF) as u16;
        if h3 != 0 { enc.movk(MEM_BASE_REG, h3, MovShift::Lsl48)?; }
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
            has_memory: true,
            saved_int_pairs: save_pairs,
            frame_size_base: frame_size,
            mem_size: 64 * 1024,
            table_base: None,
            indirect_sigs: Vec::new(),
            call_sigs: Vec::new(),
            data_regions: Vec::new(),
            n_int_locals,
            max_reg_int: MAX_PRIMARY_INT + (9 - n_int_locals),
            spill_base: spill_base_off,
            has_spill: true,
            pending_spill_depth: None,
            spill_pop_toggle: false,
            fp_spill_base: fp_spill_base_off,
            pending_fp_spill_depth: None,
            fp_spill_pop_toggle: false,
            last_i32_const_value: None,
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
        // Memory loads/stores consume the const addr — they read
        // last_i32_const_value internally, then it's stale after.
        // All other non-const ops clear it immediately.
        let is_memory_load = matches!(op,
            WasmOp::I32Load(_) | WasmOp::I64Load(_) |
            WasmOp::F32Load(_) | WasmOp::F64Load(_) |
            WasmOp::V128Load(_)
        );
        let is_memory_consumer = is_memory_load;
        if !matches!(op, WasmOp::I32Const(_)) && !is_memory_consumer {
            self.last_i32_const_value = None;
        }
        let result = match op {
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
            // f64x2
            WasmOp::F64x2Add => self.lower_v128_binop(|e, d, l, r| e.fadd_2d(d, l, r)),
            WasmOp::F64x2Sub => self.lower_v128_binop(|e, d, l, r| e.fsub_2d(d, l, r)),
            WasmOp::F64x2Mul => self.lower_v128_binop(|e, d, l, r| e.fmul_2d(d, l, r)),
            WasmOp::F64x2Div => self.lower_v128_binop(|e, d, l, r| e.fdiv_2d(d, l, r)),
            WasmOp::F64x2Min => self.lower_v128_binop(|e, d, l, r| e.fmin_2d(d, l, r)),
            WasmOp::F64x2Max => self.lower_v128_binop(|e, d, l, r| e.fmax_2d(d, l, r)),
            WasmOp::F64x2Sqrt => self.lower_f32x4_unary(|e, d, s| e.fsqrt_2d(d, s)),
            WasmOp::F64x2Abs => self.lower_f32x4_unary(|e, d, s| e.fabs_2d(d, s)),
            WasmOp::F64x2Neg => self.lower_f32x4_unary(|e, d, s| e.fneg_2d(d, s)),
            WasmOp::F64x2Splat => self.lower_f64x2_splat(),
            WasmOp::F64x2ExtractLane(lane) => self.lower_f64x2_extract_lane(lane),
            // i8x16
            WasmOp::I8x16Add => self.lower_v128_binop(|e, d, l, r| e.add_16b_vector(d, l, r)),
            WasmOp::I8x16Sub => self.lower_v128_binop(|e, d, l, r| e.sub_16b_vector(d, l, r)),
            WasmOp::I8x16Splat => { let s = self.pop_i32_slot()?; let d = self.push_v128_slot()?; self.enc.dup_16b_from_w(d, s)?; Ok(()) }
            WasmOp::I8x16ExtractLaneU(lane) => { let s = self.pop_v128_slot()?; let d = self.push_i32_slot()?; self.enc.umov_w_from_vb_lane(d, s, lane)?; Ok(()) }
            // i16x8
            WasmOp::I16x8Add => self.lower_v128_binop(|e, d, l, r| e.add_8h_vector(d, l, r)),
            WasmOp::I16x8Sub => self.lower_v128_binop(|e, d, l, r| e.sub_8h_vector(d, l, r)),
            WasmOp::I16x8Mul => self.lower_v128_binop(|e, d, l, r| e.mul_8h_vector(d, l, r)),
            WasmOp::I16x8Splat => { let s = self.pop_i32_slot()?; let d = self.push_v128_slot()?; self.enc.dup_8h_from_w(d, s)?; Ok(()) }
            WasmOp::I16x8ExtractLaneU(lane) => { let s = self.pop_v128_slot()?; let d = self.push_i32_slot()?; self.enc.umov_w_from_vh_lane(d, s, lane)?; Ok(()) }
            WasmOp::F32x4Sub => self.lower_f32x4_sub(),
            WasmOp::F32x4Div => self.lower_f32x4_div(),
            WasmOp::F32x4Fma => self.lower_f32x4_fma(),
            WasmOp::F32x4HorizontalSum => self.lower_f32x4_horizontal_sum(),
            WasmOp::F32x4Max => self.lower_f32x4_max(),
            WasmOp::F32x4Min => self.lower_f32x4_min(),
            WasmOp::F32x4Abs => self.lower_f32x4_unary(|e, d, s| e.fabs_4s(d, s)),
            WasmOp::F32x4Neg => self.lower_f32x4_unary(|e, d, s| e.fneg_4s(d, s)),
            WasmOp::F32x4Sqrt => self.lower_f32x4_unary(|e, d, s| e.fsqrt_4s(d, s)),
            WasmOp::F32x4Eq => self.lower_f32x4_eq(),
            WasmOp::F32x4Gt => self.lower_f32x4_gt(),
            WasmOp::V128Bitselect => self.lower_v128_bitselect(),
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
            WasmOp::Drop => self.lower_drop(),
            WasmOp::Select => self.lower_select(),
            WasmOp::LocalGet(i) => self.lower_local_get(i),
            WasmOp::LocalSet(i) => self.lower_local_set(i),
            WasmOp::LocalTee(i) => self.lower_local_tee(i),
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
        };
        if is_memory_consumer {
            self.last_i32_const_value = None;
        }
        result
    }

    /// Lower every op in order.
    pub fn lower_all(&mut self, ops: &[WasmOp]) -> Result<(), LowerError> {
        for &op in ops {
            self.lower_op(op)?;
        }
        Ok(())
    }

    /// Consume and return the emitted bytes after running the
    /// peephole optimizer. Eliminates self-MOVs and self-ANDs
    /// that the lowerer emits defensively (e.g., `AND W0, W0, W0`
    /// after a call that already left the result in X0).
    pub fn finish(self) -> Vec<u8> {
        let mut bytes = self.enc.into_bytes();
        let _eliminated = crate::peephole::optimize(&mut bytes, &self.data_regions);
        bytes
    }

    /// Consume and return the emitted bytes WITHOUT running the
    /// peephole pass. Useful for byte-exact test assertions that
    /// check the raw lowerer output.
    pub fn finish_raw(self) -> Vec<u8> {
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

}

impl Default for Lowerer {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests;

