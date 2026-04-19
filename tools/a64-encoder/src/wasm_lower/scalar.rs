//! Scalar lowerings — i32/i64/f32/f64 constants, arithmetic,
//! comparisons, eqz, and local get/set.

use crate::{Condition, MovShift, Reg, Vreg};
use super::*;

impl Lowerer {
    // ── i32 ─────────────────────────────────────────────────────────

    pub(super) fn lower_const(&mut self, c: i32) -> Result<(), LowerError> {
        let bits = c as u32;
        let lo = (bits & 0xFFFF) as u16;
        let hi = ((bits >> 16) & 0xFFFF) as u16;
        let r = self.push_i32_slot()?;
        self.enc.movz(r, lo, MovShift::Lsl0)?;
        if hi != 0 {
            self.enc.movk(r, hi, MovShift::Lsl16)?;
        }
        // Track only non-negative constants — bounds-check elision
        // works on byte addresses, and a negative i32 sign-extends to
        // a huge u64 that we can never prove safe anyway.
        if c >= 0 {
            self.set_top_sym(Some(SymAddr::Const(c as u32)));
        }
        Ok(())
    }

    pub(super) fn lower_eqz(&mut self) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.cmp_w(src, Reg::ZR)?;
        self.enc.cset(dst, Condition::Eq)?;
        Ok(())
    }

    pub(super) fn lower_binop(&mut self, op: BinOp) -> Result<(), LowerError> {
        // Snapshot symbolic values for both operands before they pop —
        // rhs is the stack top, lhs is the slot just below. Used by
        // sym_combine to compute an upper bound on the result so the
        // canonical `local.get k ; i32.const 4 ; i32.mul ; <load> off`
        // pattern propagates a bound all the way to the load.
        let rhs_sym = self.peek_sym_at(0);
        let lhs_sym = self.peek_sym_at(1);
        let rhs = self.pop_i32_slot()?;
        let lhs = self.pop_i32_slot()?;
        let dst = self.push_i32_slot()?;
        match op {
            BinOp::Add  => self.enc.add(dst, lhs, rhs)?,
            BinOp::Sub  => self.enc.sub(dst, lhs, rhs)?,
            BinOp::Mul  => self.enc.mul(dst, lhs, rhs)?,
            BinOp::DivS => self.enc.sdiv(dst, lhs, rhs)?,
            BinOp::DivU => self.enc.udiv(dst, lhs, rhs)?,
            BinOp::And  => self.enc.and_w(dst, lhs, rhs)?,
            BinOp::Or   => self.enc.orr_w(dst, lhs, rhs)?,
            BinOp::Xor  => self.enc.eor_w(dst, lhs, rhs)?,
            BinOp::Shl  => self.enc.lsl_w(dst, lhs, rhs)?,
            BinOp::ShrS => self.enc.asr_w(dst, lhs, rhs)?,
            BinOp::ShrU => self.enc.lsr_w(dst, lhs, rhs)?,
            BinOp::Cmp(cond) => {
                self.enc.cmp_w(lhs, rhs)?;
                self.enc.cset(dst, cond)?;
            }
        }
        let sym = match op {
            BinOp::Add => sym_add(lhs_sym, rhs_sym),
            BinOp::Mul => sym_mul(lhs_sym, rhs_sym),
            BinOp::Shl => sym_shl(lhs_sym, rhs_sym),
            // Other ops can in principle preserve a bound (e.g. `& mask`
            // bounds the result by the mask), but address arithmetic in
            // the wild almost never uses them — keep the surface small.
            _ => None,
        };
        self.set_top_sym(sym);
        Ok(())
    }

    // ── i64 ─────────────────────────────────────────────────────────

    pub(super) fn lower_i64_const(&mut self, c: i64) -> Result<(), LowerError> {
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

    pub(super) fn lower_i64_binop(&mut self, op: I64Op) -> Result<(), LowerError> {
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

    pub(super) fn lower_i64_eqz(&mut self) -> Result<(), LowerError> {
        let src = self.pop_i64_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.cmp_x(src, Reg::ZR)?;
        self.enc.cset(dst, Condition::Eq)?;
        Ok(())
    }

    pub(super) fn lower_i64_cmp(&mut self, cond: Condition) -> Result<(), LowerError> {
        let rhs = self.pop_i64_slot()?;
        let lhs = self.pop_i64_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.cmp_x(lhs, rhs)?;
        self.enc.cset(dst, cond)?;
        Ok(())
    }

    // ── f32 ─────────────────────────────────────────────────────────

    pub(super) fn lower_f32_const(&mut self, c: f32) -> Result<(), LowerError> {
        let bits = c.to_bits();
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

    pub(super) fn lower_f32_binop(&mut self, op: FBinOp) -> Result<(), LowerError> {
        let rhs = self.pop_f32_slot()?;
        let lhs = self.pop_f32_slot()?;
        let dst = self.push_f32_slot()?;
        match op {
            FBinOp::Add => self.enc.fadd_s(dst, lhs, rhs)?,
            FBinOp::Sub => self.enc.fsub_s(dst, lhs, rhs)?,
            FBinOp::Mul => self.enc.fmul_s(dst, lhs, rhs)?,
            FBinOp::Div => self.enc.fdiv_s(dst, lhs, rhs)?,
        }
        Ok(())
    }

    pub(super) fn lower_f32_cmp(&mut self, cond: Condition) -> Result<(), LowerError> {
        let rhs = self.pop_f32_slot()?;
        let lhs = self.pop_f32_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.fcmp_s(lhs, rhs)?;
        self.enc.cset(dst, cond)?;
        Ok(())
    }

    // ── f64 ─────────────────────────────────────────────────────────

    pub(super) fn lower_f64_const(&mut self, c: f64) -> Result<(), LowerError> {
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

    pub(super) fn lower_f64_binop(&mut self, op: FBinOp) -> Result<(), LowerError> {
        let rhs = self.pop_f64_slot()?;
        let lhs = self.pop_f64_slot()?;
        let dst = self.push_f64_slot()?;
        match op {
            FBinOp::Add => self.enc.fadd_d(dst, lhs, rhs)?,
            FBinOp::Sub => self.enc.fsub_d(dst, lhs, rhs)?,
            FBinOp::Mul => self.enc.fmul_d(dst, lhs, rhs)?,
            FBinOp::Div => self.enc.fdiv_d(dst, lhs, rhs)?,
        }
        Ok(())
    }

    pub(super) fn lower_f64_cmp(&mut self, cond: Condition) -> Result<(), LowerError> {
        let rhs = self.pop_f64_slot()?;
        let lhs = self.pop_f64_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.fcmp_d(lhs, rhs)?;
        self.enc.cset(dst, cond)?;
        Ok(())
    }

    // ── Locals ──────────────────────────────────────────────────────

    pub(super) fn local_loc(&self, idx: u32) -> Result<LocalLoc, LowerError> {
        let i = idx as usize;
        self.locals.get(i).copied().ok_or(LowerError::LocalOutOfRange)
    }

    pub(super) fn lower_local_get(&mut self, idx: u32) -> Result<(), LowerError> {
        // If `idx` is the counter of an enclosing loop and that loop's
        // body hasn't written to it yet, the value is bounded by the
        // loop guard — propagate that bound to the operand stack so
        // downstream arithmetic and the eventual load can elide their
        // bounds checks.
        let sym = self.local_bound(idx)
            .map(|m| SymAddr::Bounded { max_inclusive: m });
        match self.local_loc(idx)? {
            LocalLoc::I32(local) => {
                let dst = self.push_i32_slot()?;
                self.enc.add(dst, Reg::ZR, local)?;
                self.set_top_sym(sym);
            }
            LocalLoc::I64(local) => {
                let dst = self.push_i64_slot()?;
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
            LocalLoc::V128(local) => {
                let dst = self.push_v128_slot()?;
                // ORR Vd.16B, Vn.16B, Vm.16B with Vn=Vm performs a
                // 128-bit MOV — same trick we use for v128 stack
                // moves elsewhere.
                self.enc.orr_16b_vec(dst, local, local)?;
            }
        }
        Ok(())
    }

    pub(super) fn lower_local_tee(&mut self, idx: u32) -> Result<(), LowerError> {
        // Tee writes the local — invalidate any active loop bound that
        // pinned this counter. (Tee leaves the value on the stack so
        // its symbolic info there is unaffected.)
        self.taint_local(idx);
        match self.local_loc(idx)? {
            LocalLoc::I32(local) => {
                let top = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
                if top != ValType::I32 {
                    return Err(LowerError::TypeMismatch { expected: ValType::I32, got: top });
                }
                let src_reg = Reg((self.int_depth - 1) as u8);
                self.enc.add(local, Reg::ZR, src_reg)?;
            }
            LocalLoc::I64(local) => {
                let top = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
                if top != ValType::I64 {
                    return Err(LowerError::TypeMismatch { expected: ValType::I64, got: top });
                }
                let src_reg = Reg((self.int_depth - 1) as u8);
                self.enc.add(local, Reg::ZR, src_reg)?;
            }
            LocalLoc::F32(local) => {
                let top = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
                if top != ValType::F32 {
                    return Err(LowerError::TypeMismatch { expected: ValType::F32, got: top });
                }
                let src_reg = Vreg((self.fp_depth - 1) as u8);
                self.enc.fmov_s_s(local, src_reg)?;
            }
            LocalLoc::F64(local) => {
                let top = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
                if top != ValType::F64 {
                    return Err(LowerError::TypeMismatch { expected: ValType::F64, got: top });
                }
                let src_reg = Vreg((self.fp_depth - 1) as u8);
                self.enc.fmov_d_d(local, src_reg)?;
            }
            LocalLoc::V128(local) => {
                let top = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
                if top != ValType::V128 {
                    return Err(LowerError::TypeMismatch { expected: ValType::V128, got: top });
                }
                let src_reg = Vreg((self.fp_depth - 1) as u8);
                self.enc.orr_16b_vec(local, src_reg, src_reg)?;
            }
        }
        Ok(())
    }

    pub(super) fn lower_drop(&mut self) -> Result<(), LowerError> {
        let ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        match ty {
            ValType::I32 => { self.pop_i32_slot()?; }
            ValType::I64 => { self.pop_i64_slot()?; }
            ValType::F32 => { self.pop_f32_slot()?; }
            ValType::F64 => { self.pop_f64_slot()?; }
            ValType::V128 => { self.pop_v128_slot()?; }
        }
        Ok(())
    }

    pub(super) fn lower_select(&mut self) -> Result<(), LowerError> {
        let cond = self.pop_i32_slot()?;
        self.enc.cmp_w(cond, Reg::ZR)?;

        let top_ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        match top_ty {
            ValType::I32 => {
                let val_false = self.pop_i32_slot()?;
                let val_true = self.pop_i32_slot()?;
                let dst = self.push_i32_slot()?;
                self.enc.csel(dst, val_true, val_false, Condition::Ne)?;
            }
            ValType::I64 => {
                let val_false = self.pop_i64_slot()?;
                let val_true = self.pop_i64_slot()?;
                let dst = self.push_i64_slot()?;
                self.enc.csel(dst, val_true, val_false, Condition::Ne)?;
            }
            ValType::F32 => {
                let val_false = self.pop_f32_slot()?;
                let val_true = self.pop_f32_slot()?;
                let dst = self.push_f32_slot()?;
                self.enc.fcsel_s(dst, val_true, val_false, Condition::Ne)?;
            }
            ValType::F64 => {
                let val_false = self.pop_f64_slot()?;
                let val_true = self.pop_f64_slot()?;
                let dst = self.push_f64_slot()?;
                self.enc.fcsel_d(dst, val_true, val_false, Condition::Ne)?;
            }
            ValType::V128 => return Err(LowerError::V128ReturnUnsupported),
        }
        Ok(())
    }

    pub(super) fn lower_local_set(&mut self, idx: u32) -> Result<(), LowerError> {
        // Set writes the local — invalidate any active loop bound that
        // pinned this counter so subsequent local.get of it doesn't
        // get a stale bound.
        self.taint_local(idx);
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
            LocalLoc::V128(local) => {
                let src = self.pop_v128_slot()?;
                self.enc.orr_16b_vec(local, src, src)?;
            }
        }
        Ok(())
    }

    // ── Symbolic-tracking helpers for bounds-check elision ──────────

    /// Maximum value local `idx` can hold inside the current loop
    /// body, or `None` if unbounded. Walks `active_loop_bounds`
    /// innermost-first — an inner loop that re-bounds the same
    /// counter shadows an outer one. A counter is considered bounded
    /// only if the loop body hasn't yet written to it (LocalSet/Tee
    /// inside the body invalidates the guard's invariant).
    pub(super) fn local_bound(&self, idx: u32) -> Option<u64> {
        let n = self.active_loop_bounds.len();
        for i in (0..n).rev() {
            if let Some(b) = self.active_loop_bounds[i] {
                if b.counter_local == idx {
                    if let Some(taints) = self.tainted_locals.get(i) {
                        if taints.contains(&idx) { return None; }
                    }
                    return Some(b.max_value as u64);
                }
            }
        }
        None
    }

    /// Mark local `idx` as written in every active loop scope. Once
    /// tainted, [`Self::local_bound`] returns `None` for it within
    /// that loop — the guard's `counter < M` invariant only holds at
    /// the top of the iteration, so any subsequent `local.get` of a
    /// freshly-written counter could see an out-of-bound value.
    pub(super) fn taint_local(&mut self, idx: u32) {
        for taints in self.tainted_locals.iter_mut() {
            taints.insert(idx);
        }
    }
}

// ── Free helpers: combine symbolic operand info under arithmetic ───

/// Result of `i32.add` on two symbolic values. Only fires when both
/// operands are tracked — partial info collapses to `None` because we
/// can't bound `unknown + bounded`.
pub(super) fn sym_add(a: Option<SymAddr>, b: Option<SymAddr>) -> Option<SymAddr> {
    match (a, b) {
        (Some(SymAddr::Const(x)), Some(SymAddr::Const(y))) =>
            Some(SymAddr::Const(x.saturating_add(y))),
        (Some(x), Some(y)) =>
            Some(SymAddr::Bounded { max_inclusive: x.max().saturating_add(y.max()) }),
        _ => None,
    }
}

/// Result of `i32.mul` on two symbolic values. The common case is
/// `Const(scale) * Bounded(counter_max)` for the `arr[k]` pattern
/// LLVM emits — that becomes `Bounded(scale * counter_max)`.
pub(super) fn sym_mul(a: Option<SymAddr>, b: Option<SymAddr>) -> Option<SymAddr> {
    match (a, b) {
        (Some(SymAddr::Const(x)), Some(SymAddr::Const(y))) =>
            Some(SymAddr::Const(x.saturating_mul(y))),
        (Some(x), Some(y)) =>
            Some(SymAddr::Bounded { max_inclusive: x.max().saturating_mul(y.max()) }),
        _ => None,
    }
}

/// Result of `i32.shl`. Only handled when the shift amount is a
/// known small constant — a Bounded shift amount could wrap modulo 32
/// in WASM semantics, so we can't soundly bound the result.
pub(super) fn sym_shl(a: Option<SymAddr>, b: Option<SymAddr>) -> Option<SymAddr> {
    let s = match b {
        Some(SymAddr::Const(s)) if s < 32 => s,
        _ => return None,
    };
    match a {
        Some(SymAddr::Const(x)) =>
            Some(SymAddr::Const(x.checked_shl(s).unwrap_or(u32::MAX))),
        Some(SymAddr::Bounded { max_inclusive }) =>
            Some(SymAddr::Bounded {
                max_inclusive: max_inclusive.checked_shl(s).unwrap_or(u64::MAX),
            }),
        None => None,
    }
}
