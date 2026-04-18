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
        self.last_i32_const_value = Some(c);
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
        match self.local_loc(idx)? {
            LocalLoc::I32(local) => {
                let dst = self.push_i32_slot()?;
                self.enc.add(dst, Reg::ZR, local)?;
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
}
