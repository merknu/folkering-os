//! Type-conversion lowerings — wrap, extend, trunc, convert, demote,
//! promote, and bit-cast reinterpret between I32/I64/F32/F64.

use super::*;

impl Lowerer {
    // ── Wrap / Extend (integer ↔ integer) ───────────────────────────

    pub(super) fn lower_wrap_i64(&mut self) -> Result<(), LowerError> {
        // i32.wrap_i64 truncates to the low 32 bits. If the source was
        // bounded by ≤ u32::MAX, that bound survives the truncation.
        // Larger bounds collapse to None (the result could be anything
        // in [0, u32::MAX]).
        let src_sym = self.peek_top_sym();
        let src = self.pop_i64_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.and_w(dst, src, src)?;
        let sym = match src_sym {
            Some(SymAddr::Const(c)) => Some(SymAddr::Const(c)),
            Some(SymAddr::Bounded { max_inclusive }) if max_inclusive <= u32::MAX as u64 =>
                Some(SymAddr::Bounded { max_inclusive }),
            _ => None,
        };
        self.set_top_sym(sym);
        Ok(())
    }

    pub(super) fn lower_extend_i32(&mut self, signed: bool) -> Result<(), LowerError> {
        // Unsigned extend always preserves the value (zero-extends to
        // 64 bits). Signed extend preserves it only if we can prove
        // the i32 source was non-negative — i.e. the bound is ≤
        // i32::MAX. Otherwise sign-extension flips bits and the
        // resulting u64 is unrelated to the original bound.
        let src_sym = self.peek_top_sym();
        let src = self.pop_i32_slot()?;
        let dst = self.push_i64_slot()?;
        if signed {
            self.enc.sxtw(dst, src)?;
        } else {
            self.enc.and_w(dst, src, src)?;
        }
        let sym = match src_sym {
            Some(SymAddr::Const(c)) => Some(SymAddr::Const(c)),
            Some(SymAddr::Bounded { max_inclusive })
                if !signed || max_inclusive <= i32::MAX as u64 =>
                Some(SymAddr::Bounded { max_inclusive }),
            _ => None,
        };
        self.set_top_sym(sym);
        Ok(())
    }

    pub(super) fn lower_i32_extend_narrow(&mut self, is_8: bool, _unused: bool) -> Result<(), LowerError> {
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

    pub(super) fn lower_i64_extend_narrow(&mut self, width: ExtendWidth) -> Result<(), LowerError> {
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

    // ── Trunc (float → integer) ─────────────────────────────────────

    pub(super) fn lower_trunc_f32_i32(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_f32_slot()?;
        let dst = self.push_i32_slot()?;
        if signed {
            self.enc.fcvtzs_w_s(dst, src)?;
        } else {
            self.enc.fcvtzu_w_s(dst, src)?;
        }
        Ok(())
    }

    pub(super) fn lower_trunc_f64_i32(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_f64_slot()?;
        let dst = self.push_i32_slot()?;
        if signed {
            self.enc.fcvtzs_w_d(dst, src)?;
        } else {
            self.enc.fcvtzu_w_d(dst, src)?;
        }
        Ok(())
    }

    pub(super) fn lower_trunc_f32_i64(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_f32_slot()?;
        let dst = self.push_i64_slot()?;
        if signed {
            self.enc.fcvtzs_x_s(dst, src)?;
        } else {
            self.enc.fcvtzu_x_s(dst, src)?;
        }
        Ok(())
    }

    pub(super) fn lower_trunc_f64_i64(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_f64_slot()?;
        let dst = self.push_i64_slot()?;
        if signed {
            self.enc.fcvtzs_x_d(dst, src)?;
        } else {
            self.enc.fcvtzu_x_d(dst, src)?;
        }
        Ok(())
    }

    // ── Convert (integer → float) ───────────────────────────────────

    pub(super) fn lower_convert_i32_f32(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_f32_slot()?;
        if signed {
            self.enc.scvtf_s_w(dst, src)?;
        } else {
            self.enc.ucvtf_s_w(dst, src)?;
        }
        Ok(())
    }

    pub(super) fn lower_convert_i64_f32(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_i64_slot()?;
        let dst = self.push_f32_slot()?;
        if signed {
            self.enc.scvtf_s_x(dst, src)?;
        } else {
            self.enc.ucvtf_s_x(dst, src)?;
        }
        Ok(())
    }

    pub(super) fn lower_convert_i32_f64(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_f64_slot()?;
        if signed {
            self.enc.scvtf_d_w(dst, src)?;
        } else {
            self.enc.ucvtf_d_w(dst, src)?;
        }
        Ok(())
    }

    pub(super) fn lower_convert_i64_f64(&mut self, signed: bool) -> Result<(), LowerError> {
        let src = self.pop_i64_slot()?;
        let dst = self.push_f64_slot()?;
        if signed {
            self.enc.scvtf_d_x(dst, src)?;
        } else {
            self.enc.ucvtf_d_x(dst, src)?;
        }
        Ok(())
    }

    // ── Demote / Promote (float ↔ float) ────────────────────────────

    pub(super) fn lower_f32_demote_f64(&mut self) -> Result<(), LowerError> {
        let src = self.pop_f64_slot()?;
        let dst = self.push_f32_slot()?;
        debug_assert_eq!(dst.0, src.0);
        self.enc.fcvt_s_d(dst, src)?;
        Ok(())
    }

    pub(super) fn lower_f64_promote_f32(&mut self) -> Result<(), LowerError> {
        let src = self.pop_f32_slot()?;
        let dst = self.push_f64_slot()?;
        debug_assert_eq!(dst.0, src.0);
        self.enc.fcvt_d_s(dst, src)?;
        Ok(())
    }

    // ── Reinterpret (bit-cast, no numeric conversion) ───────────────

    pub(super) fn lower_i32_reinterpret_f32(&mut self) -> Result<(), LowerError> {
        let src = self.pop_f32_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.fmov_w_from_s(dst, src)?;
        Ok(())
    }

    pub(super) fn lower_i64_reinterpret_f64(&mut self) -> Result<(), LowerError> {
        let src = self.pop_f64_slot()?;
        let dst = self.push_i64_slot()?;
        self.enc.fmov_x_from_d(dst, src)?;
        Ok(())
    }

    pub(super) fn lower_f32_reinterpret_i32(&mut self) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_f32_slot()?;
        self.enc.fmov_s_from_w(dst, src)?;
        Ok(())
    }

    pub(super) fn lower_f64_reinterpret_i64(&mut self) -> Result<(), LowerError> {
        let src = self.pop_i64_slot()?;
        let dst = self.push_f64_slot()?;
        self.enc.fmov_d_from_x(dst, src)?;
        Ok(())
    }
}
