//! Memory access lowerings — loads, stores, bounds checks, traps, and epilogue.
//!
//! Every load/store gets a runtime bounds check against `mem_size`.
//! If the address is out of range we jump to an inline trap block
//! that sets X0 = -(kind+1) and returns cleanly — frame pointer
//! restored, callee-saved regs reloaded, stack realigned.

use crate::{Condition, Encoder, MovShift, Reg};
use super::*;

impl Lowerer {
    // ── Epilogue ────────────────────────────────────────────────────

    pub(super) fn emit_epilogue(&mut self) -> Result<(), LowerError> {
        if !self.has_frame {
            return Ok(());
        }
        for pair in 0..self.saved_int_pairs {
            let r1 = Reg(LOCAL_I32_BASE_REG + (pair * 2) as u8);
            let n_int = self.saved_int_pairs * 2;
            let r2 = if pair * 2 + 1 < n_int {
                Reg(LOCAL_I32_BASE_REG + (pair * 2 + 1) as u8)
            } else {
                Reg::ZR
            };
            let off = (16 + pair * 16) as i16;
            self.enc.ldp_offset_64(r1, r2, Reg::SP, off)?;
        }
        if self.has_memory {
            // X28 is saved at frame_size - 16 (last 16B slot before frame end)
            let x28_off = (self.frame_size_base - 16) as u32;
            self.enc.ldr_imm(MEM_BASE_REG, Reg::SP, x28_off)?;
        }
        self.enc.ldp_post_indexed_64(
            Reg::X29,
            Reg::X30,
            Reg::SP,
            self.frame_size_base,
        )?;
        Ok(())
    }

    // ── Bounds check + trap ─────────────────────────────────────────

    pub(super) fn emit_bounds_check(
        &mut self,
        addr_reg: Reg,
        access_size: u32,
        offset: u32,
        is_store: bool,
    ) -> Result<(), LowerError> {
        let trap_kind: u16 = if is_store { 2 } else { 1 };

        let Some(access_end) = offset.checked_add(access_size) else {
            return self.emit_trap_kind(trap_kind);
        };
        if access_end > self.mem_size {
            return self.emit_trap_kind(trap_kind);
        }
        let max_valid = self.mem_size - access_end;

        let lo = (max_valid & 0xFFFF) as u16;
        let hi = ((max_valid >> 16) & 0xFFFF) as u16;
        self.enc.movz(Reg::X16, lo, MovShift::Lsl0)?;
        if hi != 0 {
            self.enc.movk(Reg::X16, hi, MovShift::Lsl16)?;
        }
        self.enc.cmp_w(addr_reg, Reg::X16)?;

        let b_cond_pos = self.enc.pos();
        self.enc.b_cond(Condition::Ls, 0)?;

        self.emit_trap_kind(trap_kind)?;

        let after_trap = self.enc.pos();
        let skip_offset = (after_trap as i32) - (b_cond_pos as i32);
        let patched = Encoder::encode_b_cond(Condition::Ls, skip_offset)?;
        self.enc.patch_word(b_cond_pos, patched);
        Ok(())
    }

    pub(super) fn emit_trap_kind(&mut self, kind: u16) -> Result<(), LowerError> {
        self.enc.movn(Reg::X0, kind, MovShift::Lsl0)?;
        self.emit_epilogue()?;
        self.enc.ret(Reg::X30)?;
        Ok(())
    }

    pub(super) fn emit_trap(&mut self) -> Result<(), LowerError> {
        self.emit_trap_kind(0)
    }

    /// Check if the address was a compile-time constant and the access
    /// is statically within bounds. Returns true if the runtime check
    /// can be skipped entirely.
    fn static_bounds_ok(&mut self, access_size: u32, offset: u32) -> bool {
        if let Some(addr_val) = self.last_i32_const_value {
            let addr = addr_val as u32 as u64;
            let end = addr + offset as u64 + access_size as u64;
            return end <= self.mem_size as u64;
        }
        false
    }

    /// Emit bounds check only if the access can't be statically proven safe.
    fn maybe_bounds_check(&mut self, addr_reg: Reg, access_size: u32, offset: u32, is_store: bool) -> Result<(), LowerError> {
        if self.static_bounds_ok(access_size, offset) {
            return Ok(());
        }
        self.emit_bounds_check(addr_reg, access_size, offset, is_store)
    }

    // ── i32 load/store ──────────────────────────────────────────────

    pub(super) fn lower_load(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let addr = self.pop_i32_slot()?;
        self.maybe_bounds_check(addr, 4, offset, false)?;
        let dst = self.push_i32_slot()?;
        self.enc.add_ext_uxtw(dst, MEM_BASE_REG, addr)?;
        self.enc.ldr_w_imm(dst, dst, offset)?;
        Ok(())
    }

    pub(super) fn lower_store(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let val = self.pop_i32_slot()?;
        let addr = self.pop_i32_slot()?;
        self.maybe_bounds_check(addr, 4, offset, true)?;
        self.enc.add_ext_uxtw(addr, MEM_BASE_REG, addr)?;
        self.enc.str_w_imm(val, addr, offset)?;
        Ok(())
    }

    // ── f32 load/store ──────────────────────────────────────────────

    pub(super) fn lower_f32_load(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let addr = self.pop_i32_slot()?;
        self.maybe_bounds_check(addr, 4, offset, false)?;
        let dst = self.push_f32_slot()?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.ldr_s_imm(dst, Reg::X16, offset)?;
        Ok(())
    }

    pub(super) fn lower_f32_store(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let val = self.pop_f32_slot()?;
        let addr = self.pop_i32_slot()?;
        self.maybe_bounds_check(addr, 4, offset, true)?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.str_s_imm(val, Reg::X16, offset)?;
        Ok(())
    }

    // ── f64 load/store ──────────────────────────────────────────────

    pub(super) fn lower_f64_load(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let addr = self.pop_i32_slot()?;
        self.maybe_bounds_check(addr, 8, offset, false)?;
        let dst = self.push_f64_slot()?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.ldr_d_imm(dst, Reg::X16, offset)?;
        Ok(())
    }

    pub(super) fn lower_f64_store(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let val = self.pop_f64_slot()?;
        let addr = self.pop_i32_slot()?;
        self.maybe_bounds_check(addr, 8, offset, true)?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.str_d_imm(val, Reg::X16, offset)?;
        Ok(())
    }

    // ── i64 load/store ──────────────────────────────────────────────

    pub(super) fn lower_i64_load(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let addr = self.pop_i32_slot()?;
        self.maybe_bounds_check(addr, 8, offset, false)?;
        let dst = self.push_i64_slot()?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.ldr_imm(dst, Reg::X16, offset)?;
        Ok(())
    }

    pub(super) fn lower_i64_store(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let val = self.pop_i64_slot()?;
        let addr = self.pop_i32_slot()?;
        self.maybe_bounds_check(addr, 8, offset, true)?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.str_imm(val, Reg::X16, offset)?;
        Ok(())
    }
}
