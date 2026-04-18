//! `global.get` / `global.set` lowering.
//!
//! Globals live in the last `GLOBAL_AREA_SIZE` bytes of linear
//! memory (conventionally 256 B = 32 slots of 8 B each). Each slot
//! has a fixed byte offset from the memory base, computed by
//! [`Lowerer::global_mem_offset`]. That means `global.get/set`
//! reduces to a load/store through X28 (the memory-base register
//! set up by the function prologue when `has_memory` is true).
//!
//! Encoding:
//!   * i32/f32 — LDR W / STR W on a 32-bit load/store (upper 4 B of
//!     the slot stay zero or whatever the previous store left)
//!   * i64/f64 — LDR X / STR X over the full 8 B slot
//!   * i32/i64 globals land on the integer operand-stack bank
//!   * f32/f64 globals land on the FP operand-stack bank
//!
//! Offsets are immediates on the LDR/STR instructions (up to 4095 B
//! scaled for 32-bit, 32760 B scaled for 64-bit), which is plenty
//! for a 256 B globals area.
//!
//! `global.set` on a const global returns `LowerError::GlobalNotMutable`.

use super::*;

impl Lowerer {
    fn global_entry(&self, idx: u32) -> Result<(ValType, bool, u32), LowerError> {
        let i = idx as usize;
        if i >= self.global_types.len() {
            return Err(LowerError::GlobalOutOfRange);
        }
        Ok((
            self.global_types[i],
            self.global_mutable[i],
            self.global_mem_offset(idx),
        ))
    }

    /// Materialise `mem_base + global_off` into X16, the per-call
    /// address scratch we use everywhere else for memory ops.
    /// Required because globals live near the top of linear memory
    /// (offset > 16 KiB), beyond what LDR/STR's immediate-offset
    /// encoding can reach (max 4095 × scale, so 16 380 B for 32-bit
    /// access). We always use MOVZ + ADD because every offset in
    /// our 256 B globals area fits in a single 16-bit immediate.
    fn materialize_global_addr(&mut self, off: u32) -> Result<(), LowerError> {
        debug_assert!(off <= 0xFFFF, "global offset must fit MOVZ imm16");
        self.enc.movz(Reg::X16, off as u16, MovShift::Lsl0)?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, Reg::X16)?;
        Ok(())
    }

    pub(super) fn lower_global_get(&mut self, idx: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            // Globals are stored in linear memory; a function that
            // doesn't have X28 = mem_base can't address them.
            return Err(LowerError::MemoryNotConfigured);
        }
        let (ty, _mutable, off) = self.global_entry(idx)?;
        self.materialize_global_addr(off)?;
        match ty {
            ValType::I32 => {
                let dst = self.push_i32_slot()?;
                self.enc.ldr_w_imm(dst, Reg::X16, 0)?;
            }
            ValType::I64 => {
                let dst = self.push_i64_slot()?;
                self.enc.ldr_imm(dst, Reg::X16, 0)?;
            }
            ValType::F32 => {
                let dst = self.push_f32_slot()?;
                self.enc.ldr_s_imm(dst, Reg::X16, 0)?;
            }
            ValType::F64 => {
                let dst = self.push_f64_slot()?;
                self.enc.ldr_d_imm(dst, Reg::X16, 0)?;
            }
            ValType::V128 => {
                return Err(LowerError::V128LocalsUnsupported);
            }
        }
        Ok(())
    }

    pub(super) fn lower_global_set(&mut self, idx: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        let (ty, mutable, off) = self.global_entry(idx)?;
        if !mutable {
            return Err(LowerError::GlobalNotMutable);
        }
        // Pop FIRST, then materialise — popping might emit code
        // that uses X16 (spill scratch), which would clobber any
        // pre-materialised address.
        match ty {
            ValType::I32 => {
                let src = self.pop_i32_slot()?;
                self.materialize_global_addr(off)?;
                self.enc.str_w_imm(src, Reg::X16, 0)?;
            }
            ValType::I64 => {
                let src = self.pop_i64_slot()?;
                self.materialize_global_addr(off)?;
                self.enc.str_imm(src, Reg::X16, 0)?;
            }
            ValType::F32 => {
                let src = self.pop_f32_slot()?;
                self.materialize_global_addr(off)?;
                self.enc.str_s_imm(src, Reg::X16, 0)?;
            }
            ValType::F64 => {
                let src = self.pop_f64_slot()?;
                self.materialize_global_addr(off)?;
                self.enc.str_d_imm(src, Reg::X16, 0)?;
            }
            ValType::V128 => {
                return Err(LowerError::V128LocalsUnsupported);
            }
        }
        Ok(())
    }

    /// Emit code at the top of the function that stores each
    /// global's declared init value into its memory slot. Called
    /// once from the module entrypoint prologue so Rust's reliance
    /// on `__stack_pointer` being initialised works without any
    /// runtime setup from the host.
    ///
    /// `init_values[i]` is the 8-byte little-endian raw value of
    /// global `i`, as parsed from the WASM module.
    pub fn emit_global_inits(
        &mut self,
        init_values: &[[u8; 8]],
    ) -> Result<(), LowerError> {
        if init_values.len() != self.global_types.len() {
            return Err(LowerError::GlobalOutOfRange);
        }
        for (idx, init) in init_values.iter().enumerate() {
            let off = self.global_mem_offset(idx as u32);
            let ty = self.global_types[idx];
            // Pattern for each global: load value into X17, compute
            // dest address into X16, store. We use X16 for the
            // address (consistent with materialize_global_addr) and
            // X17 for the value because both are scratch registers
            // in our ABI and don't participate in operand-stack
            // allocation.
            match ty {
                ValType::I32 | ValType::F32 => {
                    let mut val = u32::from_le_bytes([init[0], init[1], init[2], init[3]]);
                    if matches!(ty, ValType::I32)
                        && val > self.mem_size.saturating_sub(GLOBAL_AREA_SIZE)
                    {
                        // Rust-compiled WASM initialises __stack_pointer
                        // (and __data_end / __heap_base) to 1 MiB by
                        // default. Clamp anything that points outside
                        // our 64 KiB linear memory to a safe top-of-
                        // stack value so SP-relative spills land in
                        // [STACK_POINTER_INIT_FALLBACK .. mem_size -
                        // GLOBAL_AREA_SIZE].
                        val = STACK_POINTER_INIT_FALLBACK
                            .min(self.mem_size.saturating_sub(GLOBAL_AREA_SIZE));
                    }
                    self.load_u32_into_reg(Reg::X17, val)?;
                    self.enc.movz(Reg::X16, off as u16, MovShift::Lsl0)?;
                    self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, Reg::X16)?;
                    self.enc.str_w_imm(Reg::X17, Reg::X16, 0)?;
                }
                ValType::I64 | ValType::F64 => {
                    let val = u64::from_le_bytes(*init);
                    self.load_u64_into_reg(Reg::X17, val)?;
                    self.enc.movz(Reg::X16, off as u16, MovShift::Lsl0)?;
                    self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, Reg::X16)?;
                    self.enc.str_imm(Reg::X17, Reg::X16, 0)?;
                }
                ValType::V128 => {
                    return Err(LowerError::V128LocalsUnsupported);
                }
            }
        }
        Ok(())
    }

    fn load_u32_into_reg(&mut self, dst: Reg, val: u32) -> Result<(), LowerError> {
        self.enc.movz(dst, (val & 0xFFFF) as u16, MovShift::Lsl0)?;
        let hi = ((val >> 16) & 0xFFFF) as u16;
        if hi != 0 {
            self.enc.movk(dst, hi, MovShift::Lsl16)?;
        }
        Ok(())
    }

    fn load_u64_into_reg(&mut self, dst: Reg, val: u64) -> Result<(), LowerError> {
        self.enc.movz(dst, (val & 0xFFFF) as u16, MovShift::Lsl0)?;
        let parts = [
            ((val >> 16) & 0xFFFF) as u16,
            ((val >> 32) & 0xFFFF) as u16,
            ((val >> 48) & 0xFFFF) as u16,
        ];
        let shifts = [MovShift::Lsl16, MovShift::Lsl32, MovShift::Lsl48];
        for (&p, &s) in parts.iter().zip(shifts.iter()) {
            if p != 0 {
                self.enc.movk(dst, p, s)?;
            }
        }
        Ok(())
    }
}
