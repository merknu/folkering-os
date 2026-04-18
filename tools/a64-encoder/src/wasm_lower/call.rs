//! Call lowerings — direct `call`, `call_indirect`, function-end
//! epilogue, and explicit `return`.

use alloc::{vec::Vec};
use crate::{MovShift, Reg, Vreg};
use super::*;

impl Lowerer {
    pub(super) fn lower_call(&mut self, idx: u32) -> Result<(), LowerError> {
        let target = *self
            .call_targets
            .get(idx as usize)
            .ok_or(LowerError::CallTargetMissing)?;
        let sig = self
            .call_sigs
            .get(idx as usize)
            .cloned()
            .unwrap_or(FnSig { params: Vec::new(), result: Some(ValType::I32) });

        for p in &sig.params {
            if !p.is_int() {
                return Err(LowerError::CallTypeUnsupported);
            }
        }
        let n_args = sig.params.len();
        if n_args > 8 {
            return Err(LowerError::CallArityUnsupported);
        }

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

        let x16 = Reg::X16;
        self.enc.movz(x16, (target & 0xFFFF) as u16, MovShift::Lsl0)?;
        let h1 = ((target >> 16) & 0xFFFF) as u16;
        if h1 != 0 { self.enc.movk(x16, h1, MovShift::Lsl16)?; }
        let h2 = ((target >> 32) & 0xFFFF) as u16;
        if h2 != 0 { self.enc.movk(x16, h2, MovShift::Lsl32)?; }
        let h3 = ((target >> 48) & 0xFFFF) as u16;
        if h3 != 0 { self.enc.movk(x16, h3, MovShift::Lsl48)?; }

        for (i, src) in arg_src.iter().enumerate() {
            let scratch = Reg((9 + i) as u8);
            self.enc.add(scratch, Reg::ZR, *src)?;
        }
        for i in 0..n_args {
            let scratch = Reg((9 + i) as u8);
            let target_reg = Reg(i as u8);
            self.enc.add(target_reg, Reg::ZR, scratch)?;
        }

        self.enc.blr(x16)?;

        match sig.result {
            None => {}
            Some(ValType::I32) => {
                let dst = self.push_i32_slot()?;
                if dst.0 != 0 {
                    self.enc.and_w(dst, Reg::X0, Reg::X0)?;
                } else {
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

    pub(super) fn lower_call_indirect(&mut self, type_id: u32) -> Result<(), LowerError> {
        let table_base = self.table_base.ok_or(LowerError::TableNotConfigured)?;
        let sig = self
            .indirect_sigs
            .get(type_id as usize)
            .ok_or(LowerError::IndirectTypeMissing)?
            .clone();

        for p in &sig.params {
            if !p.is_int() {
                return Err(LowerError::IndirectTypeUnsupported);
            }
        }
        let n_args = sig.params.len();
        if n_args > 8 {
            return Err(LowerError::IndirectArityUnsupported);
        }

        let idx_reg = self.pop_i32_slot()?;

        let x17 = Reg::X17;
        self.enc.movz(x17, (table_base & 0xFFFF) as u16, MovShift::Lsl0)?;
        let h1 = ((table_base >> 16) & 0xFFFF) as u16;
        if h1 != 0 { self.enc.movk(x17, h1, MovShift::Lsl16)?; }
        let h2 = ((table_base >> 32) & 0xFFFF) as u16;
        if h2 != 0 { self.enc.movk(x17, h2, MovShift::Lsl32)?; }
        let h3 = ((table_base >> 48) & 0xFFFF) as u16;
        if h3 != 0 { self.enc.movk(x17, h3, MovShift::Lsl48)?; }
        self.enc.add_ext_uxtw_shifted(x17, x17, idx_reg, 4)?;

        self.enc.ldr_imm(Reg::X16, x17, 0)?;

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

        for (i, src) in arg_src.iter().enumerate() {
            let scratch = Reg((9 + i) as u8);
            self.enc.add(scratch, Reg::ZR, *src)?;
        }
        for i in 0..n_args {
            let scratch = Reg((9 + i) as u8);
            let target = Reg(i as u8);
            self.enc.add(target, Reg::ZR, scratch)?;
        }

        self.enc.blr(Reg::X16)?;

        match sig.result {
            None => {}
            Some(ValType::I32) => {
                let dst = self.push_i32_slot()?;
                if dst.0 != 0 {
                    self.enc.and_w(dst, Reg::X0, Reg::X0)?;
                } else {
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

    pub(super) fn lower_function_end(&mut self) -> Result<(), LowerError> {
        self.flush_pending_spill()?;
        self.flush_pending_fp_spill()?;
        match self.stack.len() {
            1 => match self.stack_top_type().unwrap() {
                ValType::I32 => {
                    self.pop_i32_slot()?;
                }
                ValType::I64 => {
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
                    return Err(LowerError::V128ReturnUnsupported);
                }
            },
            _ => return Err(LowerError::StackNotSingleton),
        }
        self.emit_epilogue()?;
        self.enc.ret(Reg::X30)?;
        Ok(())
    }

    pub(super) fn lower_explicit_return(&mut self) -> Result<(), LowerError> {
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
