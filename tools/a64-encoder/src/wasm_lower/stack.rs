//! Operand-stack management — push/pop for all types with spill support.
//!
//! Three allocation bands for integers (I32/I64):
//!   Primary  (0..15)       → X0..X15
//!   Extended (16..max-1)   → X(19+N_locals)..X27 (callee-saved overflow)
//!   Spill    (max..max+15) → frame memory via X14/X15 alternating scratch
//!
//! FP types (F32/F64/V128):
//!   Primary  (0..15)       → V0..V15
//!   Spill    (16..23)      → frame memory via V30/V31 alternating scratch

use crate::{Reg, Vreg};
use super::*;

impl Lowerer {
    /// Map integer depth to a physical register, or None for spill.
    pub(super) fn int_depth_to_reg(&self, depth: usize) -> Option<Reg> {
        if depth < MAX_PRIMARY_INT {
            Some(Reg(depth as u8))
        } else if depth < self.max_reg_int {
            let overflow_base = LOCAL_I32_BASE_REG + self.n_int_locals as u8;
            let overflow_idx = (depth - MAX_PRIMARY_INT) as u8;
            Some(Reg(overflow_base + overflow_idx))
        } else {
            None
        }
    }

    /// Frame offset for an integer spill slot at `depth`.
    pub(super) fn spill_offset(&self, depth: usize) -> u32 {
        let spill_idx = depth - self.max_reg_int;
        self.spill_base + (spill_idx as u32) * SPILL_SLOT_BYTES
    }

    /// Frame offset for an FP spill slot at `depth`.
    pub(super) fn fp_spill_offset(&self, depth: usize) -> u32 {
        let spill_idx = depth - MAX_F32_STACK;
        self.fp_spill_base + (spill_idx as u32) * FP_SPILL_SLOT_BYTES
    }

    /// Flush any pending integer spill-push STR.
    pub(super) fn flush_pending_spill(&mut self) -> Result<(), LowerError> {
        if let Some(depth) = self.pending_spill_depth.take() {
            let off = self.spill_offset(depth);
            self.enc.str_imm(SPILL_SCRATCH_A, Reg::SP, off)?;
        }
        Ok(())
    }

    /// Flush any pending FP spill-push STR Q.
    pub(super) fn flush_pending_fp_spill(&mut self) -> Result<(), LowerError> {
        if let Some(depth) = self.pending_fp_spill_depth.take() {
            let off = self.fp_spill_offset(depth);
            self.enc.str_q_imm(FP_SPILL_SCRATCH_A, Reg::SP, off)?;
        }
        Ok(())
    }

    // ── I32 ────────────────────────────────────────────────────────

    pub(super) fn push_i32_slot(&mut self) -> Result<Reg, LowerError> {
        self.flush_pending_spill()?;
        let depth = self.int_depth;
        if let Some(r) = self.int_depth_to_reg(depth) {
            self.int_depth += 1;
            self.stack.push(ValType::I32);
            Ok(r)
        } else if self.has_spill && depth < self.max_reg_int + MAX_FRAME_SPILL {
            self.pending_spill_depth = Some(depth);
            self.int_depth += 1;
            self.stack.push(ValType::I32);
            Ok(SPILL_SCRATCH_A)
        } else {
            Err(LowerError::TypedStackOverflow(ValType::I32))
        }
    }

    pub(super) fn pop_i32_slot(&mut self) -> Result<Reg, LowerError> {
        self.flush_pending_spill()?;
        let ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        if ty != ValType::I32 {
            return Err(LowerError::TypeMismatch { expected: ValType::I32, got: ty });
        }
        self.stack.pop();
        self.int_depth -= 1;
        let depth = self.int_depth;
        if let Some(r) = self.int_depth_to_reg(depth) {
            Ok(r)
        } else {
            let scratch = if self.spill_pop_toggle { SPILL_SCRATCH_B } else { SPILL_SCRATCH_A };
            self.spill_pop_toggle = !self.spill_pop_toggle;
            let off = self.spill_offset(depth);
            self.enc.ldr_imm(scratch, Reg::SP, off)?;
            Ok(scratch)
        }
    }

    // ── I64 (shares int-bank with I32) ─────────────────────────────

    pub(super) fn push_i64_slot(&mut self) -> Result<Reg, LowerError> {
        self.flush_pending_spill()?;
        let depth = self.int_depth;
        if let Some(r) = self.int_depth_to_reg(depth) {
            self.int_depth += 1;
            self.stack.push(ValType::I64);
            Ok(r)
        } else if self.has_spill && depth < self.max_reg_int + MAX_FRAME_SPILL {
            self.pending_spill_depth = Some(depth);
            self.int_depth += 1;
            self.stack.push(ValType::I64);
            Ok(SPILL_SCRATCH_A)
        } else {
            Err(LowerError::TypedStackOverflow(ValType::I64))
        }
    }

    pub(super) fn pop_i64_slot(&mut self) -> Result<Reg, LowerError> {
        self.flush_pending_spill()?;
        let ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        if ty != ValType::I64 {
            return Err(LowerError::TypeMismatch { expected: ValType::I64, got: ty });
        }
        self.stack.pop();
        self.int_depth -= 1;
        let depth = self.int_depth;
        if let Some(r) = self.int_depth_to_reg(depth) {
            Ok(r)
        } else {
            let scratch = if self.spill_pop_toggle { SPILL_SCRATCH_B } else { SPILL_SCRATCH_A };
            self.spill_pop_toggle = !self.spill_pop_toggle;
            let off = self.spill_offset(depth);
            self.enc.ldr_imm(scratch, Reg::SP, off)?;
            Ok(scratch)
        }
    }

    // ── F32 (V-bank, with spill) ───────────────────────────────────

    pub(super) fn push_f32_slot(&mut self) -> Result<Vreg, LowerError> {
        self.flush_pending_fp_spill()?;
        let depth = self.fp_depth;
        if depth < MAX_F32_STACK {
            let v = Vreg::new(depth as u8)
                .ok_or(LowerError::TypedStackOverflow(ValType::F32))?;
            self.fp_depth += 1;
            self.stack.push(ValType::F32);
            Ok(v)
        } else if self.has_spill && depth < MAX_F32_STACK + MAX_FP_SPILL {
            self.pending_fp_spill_depth = Some(depth);
            self.fp_depth += 1;
            self.stack.push(ValType::F32);
            Ok(FP_SPILL_SCRATCH_A)
        } else {
            Err(LowerError::TypedStackOverflow(ValType::F32))
        }
    }

    pub(super) fn pop_f32_slot(&mut self) -> Result<Vreg, LowerError> {
        self.flush_pending_fp_spill()?;
        let ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        if ty != ValType::F32 {
            return Err(LowerError::TypeMismatch { expected: ValType::F32, got: ty });
        }
        self.stack.pop();
        self.fp_depth -= 1;
        let depth = self.fp_depth;
        if depth < MAX_F32_STACK {
            Vreg::new(depth as u8).ok_or(LowerError::StackUnderflow)
        } else {
            let scratch = if self.fp_spill_pop_toggle { FP_SPILL_SCRATCH_B } else { FP_SPILL_SCRATCH_A };
            self.fp_spill_pop_toggle = !self.fp_spill_pop_toggle;
            let off = self.fp_spill_offset(depth);
            self.enc.ldr_q_imm(scratch, Reg::SP, off)?;
            Ok(scratch)
        }
    }

    // ── F64 ────────────────────────────────────────────────────────

    pub(super) fn push_f64_slot(&mut self) -> Result<Vreg, LowerError> {
        self.flush_pending_fp_spill()?;
        let depth = self.fp_depth;
        if depth < MAX_F32_STACK {
            let v = Vreg::new(depth as u8)
                .ok_or(LowerError::TypedStackOverflow(ValType::F64))?;
            self.fp_depth += 1;
            self.stack.push(ValType::F64);
            Ok(v)
        } else if self.has_spill && depth < MAX_F32_STACK + MAX_FP_SPILL {
            self.pending_fp_spill_depth = Some(depth);
            self.fp_depth += 1;
            self.stack.push(ValType::F64);
            Ok(FP_SPILL_SCRATCH_A)
        } else {
            Err(LowerError::TypedStackOverflow(ValType::F64))
        }
    }

    pub(super) fn pop_f64_slot(&mut self) -> Result<Vreg, LowerError> {
        self.flush_pending_fp_spill()?;
        let ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        if ty != ValType::F64 {
            return Err(LowerError::TypeMismatch { expected: ValType::F64, got: ty });
        }
        self.stack.pop();
        self.fp_depth -= 1;
        let depth = self.fp_depth;
        if depth < MAX_F32_STACK {
            Vreg::new(depth as u8).ok_or(LowerError::StackUnderflow)
        } else {
            let scratch = if self.fp_spill_pop_toggle { FP_SPILL_SCRATCH_B } else { FP_SPILL_SCRATCH_A };
            self.fp_spill_pop_toggle = !self.fp_spill_pop_toggle;
            let off = self.fp_spill_offset(depth);
            self.enc.ldr_q_imm(scratch, Reg::SP, off)?;
            Ok(scratch)
        }
    }

    // ── V128 ───────────────────────────────────────────────────────

    pub(super) fn push_v128_slot(&mut self) -> Result<Vreg, LowerError> {
        self.flush_pending_fp_spill()?;
        let depth = self.fp_depth;
        if depth < MAX_F32_STACK {
            let v = Vreg::new(depth as u8)
                .ok_or(LowerError::TypedStackOverflow(ValType::V128))?;
            self.fp_depth += 1;
            self.stack.push(ValType::V128);
            Ok(v)
        } else if self.has_spill && depth < MAX_F32_STACK + MAX_FP_SPILL {
            self.pending_fp_spill_depth = Some(depth);
            self.fp_depth += 1;
            self.stack.push(ValType::V128);
            Ok(FP_SPILL_SCRATCH_A)
        } else {
            Err(LowerError::TypedStackOverflow(ValType::V128))
        }
    }

    pub(super) fn pop_v128_slot(&mut self) -> Result<Vreg, LowerError> {
        self.flush_pending_fp_spill()?;
        let ty = self.stack.last().copied().ok_or(LowerError::StackUnderflow)?;
        if ty != ValType::V128 {
            return Err(LowerError::TypeMismatch { expected: ValType::V128, got: ty });
        }
        self.stack.pop();
        self.fp_depth -= 1;
        let depth = self.fp_depth;
        if depth < MAX_F32_STACK {
            Vreg::new(depth as u8).ok_or(LowerError::StackUnderflow)
        } else {
            let scratch = if self.fp_spill_pop_toggle { FP_SPILL_SCRATCH_B } else { FP_SPILL_SCRATCH_A };
            self.fp_spill_pop_toggle = !self.fp_spill_pop_toggle;
            let off = self.fp_spill_offset(depth);
            self.enc.ldr_q_imm(scratch, Reg::SP, off)?;
            Ok(scratch)
        }
    }
}
