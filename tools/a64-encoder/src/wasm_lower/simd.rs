//! SIMD lowering — all v128/f32x4/f64x2/i32x4/i16x8/i8x16 ops.

use crate::{Encoder, EncodeError, Reg, Vreg};
use super::*;

impl Lowerer {
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
    pub(super) fn lower_v128_const(&mut self, bits: u128) -> Result<(), LowerError> {
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
        // Register the byte range as a data region so the peephole
        // optimizer doesn't mistake constant bytes for instructions.
        let data_start = self.enc.pos();
        let le = bits.to_le_bytes();
        for chunk in le.chunks_exact(4) {
            let w = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            self.enc.emit_raw_word(w);
        }
        self.data_regions.push(crate::peephole::DataRegion {
            start: data_start,
            end: self.enc.pos(),
        });

        // LDR Q_dst, [PC, #-16] — the data is now 16 bytes before
        // the current PC (which points at the LDR we're about to emit).
        self.enc.ldr_q_literal(dst, -16)?;

        Ok(())
    }

    /// Lower `v128.load off` — pop i32 addr, push a V128 slot loaded
    /// from `mem_base + addr + offset`. 16-byte access needs
    /// 16-byte-aligned offset (LDR Q requires `offset % 16 == 0`).
    pub(super) fn lower_v128_load(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        // Snapshot the address sym BEFORE the pop, so the elider sees
        // it (pop_i32_slot drops the symbolic info along with the slot).
        let addr_sym = self.peek_top_sym();
        let addr = self.pop_i32_slot()?;
        self.maybe_bounds_check(addr, 16, offset, false, addr_sym)?;
        let dst = self.push_v128_slot()?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.ldr_q_imm(dst, Reg::X16, offset)?;
        Ok(())
    }

    pub(super) fn lower_v128_store(&mut self, offset: u32) -> Result<(), LowerError> {
        if !self.has_memory {
            return Err(LowerError::MemoryNotConfigured);
        }
        // Stack: [..., addr (i32), val (v128)]. val lives on the V
        // bank; the address is the int-stack top, so peek_top_sym
        // sees it directly.
        let addr_sym = self.peek_top_sym();
        let val = self.pop_v128_slot()?;
        let addr = self.pop_i32_slot()?;
        self.maybe_bounds_check(addr, 16, offset, true, addr_sym)?;
        self.enc.add_ext_uxtw(Reg::X16, MEM_BASE_REG, addr)?;
        self.enc.str_q_imm(val, Reg::X16, offset)?;
        Ok(())
    }

    /// Lower `f32x4.add` / `f32x4.mul` — pop two V128, push one V128
    /// with element-wise sum/product across the 4 f32 lanes.
    pub(super) fn lower_f32x4_add(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.fadd_4s(dst, lhs, rhs)?;
        Ok(())
    }

    pub(super) fn lower_f32x4_mul(&mut self) -> Result<(), LowerError> {
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
    pub(super) fn lower_f32x4_extract_lane(&mut self, lane: u8) -> Result<(), LowerError> {
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
    pub(super) fn lower_f32x4_splat(&mut self) -> Result<(), LowerError> {
        let src = self.pop_f32_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.dup_4s_from_vs_lane0(dst, src)?;
        Ok(())
    }

    /// Lower `i32x4.splat` — pop I32, push V128. Bank crossing:
    /// the scalar lives in the X bank, target is V. `DUP Vd.4S, Wn`
    /// handles it in one instruction.
    pub(super) fn lower_i32x4_splat(&mut self) -> Result<(), LowerError> {
        let src = self.pop_i32_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.dup_4s_from_w(dst, src)?;
        Ok(())
    }

    /// Lower `i32x4.add/sub/mul` — integer vector arithmetic.
    /// Structurally identical to the f32x4 variants, just a
    /// different AdvSIMD opcode (ADD/SUB/MUL vs FADD/FMUL).
    pub(super) fn lower_i32x4_add(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.add_4s_vector(dst, lhs, rhs)?;
        Ok(())
    }

    pub(super) fn lower_i32x4_sub(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.sub_4s_vector(dst, lhs, rhs)?;
        Ok(())
    }

    pub(super) fn lower_i32x4_mul(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.mul_4s_vector(dst, lhs, rhs)?;
        Ok(())
    }

    /// Lower `i32x4.dot_i8x16_signed` — Folkering extension over
    /// AArch64 SDOT. Stack: [acc i32x4, a i8x16, b i8x16] →
    /// [acc + dot(a, b) i32x4]. SDOT accumulates into Vd, so we
    /// move the acc into the destination first, then SDOT against
    /// the two source vectors.
    pub(super) fn lower_i32x4_dot_i8x16_signed(&mut self) -> Result<(), LowerError> {
        let b = self.pop_v128_slot()?;
        let a = self.pop_v128_slot()?;
        let acc = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        // Move acc into dst (SDOT reads-modifies-writes Vd).
        // ORR Vd.16b, Vacc.16b, Vacc.16b is the canonical "MOV V"
        // — already used by the operand-stack mover. If dst == acc
        // we skip.
        if dst != acc {
            self.enc.orr_16b_vec(dst, acc, acc)?;
        }
        self.enc.sdot_4s_16b(dst, a, b)?;
        Ok(())
    }

    pub(super) fn lower_i32x4_dot_i8x16_unsigned(&mut self) -> Result<(), LowerError> {
        let b = self.pop_v128_slot()?;
        let a = self.pop_v128_slot()?;
        let acc = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        if dst != acc {
            self.enc.orr_16b_vec(dst, acc, acc)?;
        }
        self.enc.udot_4s_16b(dst, a, b)?;
        Ok(())
    }

    /// Lower `i32x4.extract_lane N` — pop V128, push I32 scalar.
    /// UMOV Wd, Vn.S[N] — zero-extends the 32-bit lane into the
    /// full X register, matching the i32 slot semantics.
    pub(super) fn lower_i32x4_extract_lane(&mut self, lane: u8) -> Result<(), LowerError> {
        let src = self.pop_v128_slot()?;
        let dst = self.push_i32_slot()?;
        self.enc.umov_w_from_vs_lane(dst, src, lane)?;
        Ok(())
    }

    pub(super) fn lower_f32x4_sub(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.fsub_4s(dst, lhs, rhs)?;
        Ok(())
    }

    pub(super) fn lower_f32x4_div(&mut self) -> Result<(), LowerError> {
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
    pub(super) fn lower_f32x4_fma(&mut self) -> Result<(), LowerError> {
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

    /// Generic unary f32x4 op dispatcher. `emit` takes an encoder
    /// reference and the (dst, src) Vreg pair. Used for abs/neg/sqrt
    /// — all share the same stack-transition shape (pop V128 →
    /// push V128) and differ only in which NEON instruction they
    /// emit.
    pub(super) fn lower_f32x4_unary<F>(&mut self, emit: F) -> Result<(), LowerError>
    where
        F: FnOnce(&mut Encoder, Vreg, Vreg) -> Result<(), EncodeError>,
    {
        let src = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        debug_assert_eq!(dst.0, src.0, "unary f32x4 reuses the slot");
        emit(&mut self.enc, dst, src)?;
        Ok(())
    }

    /// Generic binary v128 → v128 op. Used by f64x2 arith to avoid
    /// duplicating the pop-pop-push-emit pattern for each opcode.
    pub(super) fn lower_v128_binop<F>(&mut self, emit: F) -> Result<(), LowerError>
    where
        F: FnOnce(&mut Encoder, Vreg, Vreg, Vreg) -> Result<(), EncodeError>,
    {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        emit(&mut self.enc, dst, lhs, rhs)?;
        Ok(())
    }

    /// f64x2.splat — pop F64 scalar, broadcast to both lanes.
    /// The scalar lives in a D register (low 64 of V). We use
    /// DUP Vd.2D, Xn to broadcast from the X bank, so first
    /// FMOV Xn, Dn to get the bits into an X register.
    pub(super) fn lower_f64x2_splat(&mut self) -> Result<(), LowerError> {
        let src = self.pop_f64_slot()?;
        let dst = self.push_v128_slot()?;
        // FMOV X17, Dn — move the f64 bits to an X scratch
        self.enc.fmov_x_from_d(Reg::X17, src)?;
        // DUP Vd.2D, X17
        self.enc.dup_2d_from_x(dst, Reg::X17)?;
        Ok(())
    }

    /// f64x2.extract_lane N — pop V128, push F64 from lane N (0 or 1).
    pub(super) fn lower_f64x2_extract_lane(&mut self, lane: u8) -> Result<(), LowerError> {
        let src = self.pop_v128_slot()?;
        let dst = self.push_f64_slot()?;
        self.enc.dup_d_from_v_d_lane(dst, src, lane)?;
        Ok(())
    }

    pub(super) fn lower_f32x4_eq(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.fcmeq_4s(dst, lhs, rhs)?;
        Ok(())
    }

    pub(super) fn lower_f32x4_gt(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.fcmgt_4s(dst, lhs, rhs)?;
        Ok(())
    }

    /// Lower `v128.bitselect` — the canonical masked-choose op.
    ///
    /// Stack discipline: bottom `v1`, middle `v2`, top `mask`.
    /// WASM semantics: `result = (v1 AND mask) OR (v2 AND NOT mask)`.
    /// That is — where `mask[bit]=1`, take v1; else take v2.
    ///
    /// AArch64 `BSL Vd, Vn, Vm` does
    /// `Vd = (Vd AND Vn) OR (NOT Vd AND Vm)`, with **Vd as the
    /// mask register, both read and written**. For our stack
    /// layout, that means the result lands at the mask's slot
    /// (fp_depth − 1) — *above* where we need it (fp_depth − 3
    /// after popping all three).
    ///
    /// Solution: emit BSL in place, then MOV the result down to
    /// the push slot via `ORR Vdst, Vmask, Vmask` (the canonical
    /// AdvSIMD register copy). Two instructions; neither fights
    /// the register file because mask and dst live on the same
    /// V bank.
    pub(super) fn lower_v128_bitselect(&mut self) -> Result<(), LowerError> {
        let mask = self.pop_v128_slot()?;
        let v2 = self.pop_v128_slot()?;
        let v1 = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        // BSL Vmask, Vv1, Vv2 — writes the selected result into the
        // mask register (Vd). Vn selected where Vd=1, Vm where Vd=0.
        self.enc.bsl_16b(mask, v1, v2)?;
        // Copy mask's now-result contents to the push slot, unless
        // they're the same physical register (i.e., dst == mask).
        // For our slot allocator the push slot after pop(3) + push(1)
        // is at the bottom — Vreg(fp_depth - 1) = v1's old position,
        // NOT mask's. So the registers differ and the MOV is real.
        if dst.0 != mask.0 {
            self.enc.orr_16b_vec(dst, mask, mask)?;
        }
        Ok(())
    }

    pub(super) fn lower_f32x4_max(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.fmax_4s(dst, lhs, rhs)?;
        Ok(())
    }

    pub(super) fn lower_f32x4_min(&mut self) -> Result<(), LowerError> {
        let rhs = self.pop_v128_slot()?;
        let lhs = self.pop_v128_slot()?;
        let dst = self.push_v128_slot()?;
        self.enc.fmin_4s(dst, lhs, rhs)?;
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
    pub(super) fn lower_f32x4_horizontal_sum(&mut self) -> Result<(), LowerError> {
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
}
