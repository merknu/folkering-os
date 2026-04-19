//! Control-flow lowerings — block, loop, br, br_if, if/else/end,
//! and forward-branch patch resolution.

use crate::{encode_b, encode_cbz_w, encode_cbnz_w, Encoder, Reg};
use super::*;

fn rt_from_cbz_at(enc: &Encoder, pos: usize) -> Reg {
    let bytes = enc.as_bytes();
    let word = u32::from_le_bytes([
        bytes[pos],
        bytes[pos + 1],
        bytes[pos + 2],
        bytes[pos + 3],
    ]);
    Reg::new((word & 0x1F) as u8).unwrap_or(Reg::X0)
}

impl Lowerer {
    pub(super) fn lower_block(&mut self) -> Result<(), LowerError> {
        self.label_stack.push(Label {
            kind: LabelKind::Block,
            loop_start: None,
            pending: Vec::new(),
            entry_depth: self.stack.len(),
        });
        Ok(())
    }

    pub(super) fn lower_loop(&mut self) -> Result<(), LowerError> {
        self.label_stack.push(Label {
            kind: LabelKind::Loop,
            loop_start: Some(self.enc.pos()),
            pending: Vec::new(),
            entry_depth: self.stack.len(),
        });
        Ok(())
    }

    pub(super) fn label_index(&self, depth: u32) -> Result<usize, LowerError> {
        let n = self.label_stack.len();
        let d = depth as usize;
        if d >= n {
            return Err(LowerError::LabelOutOfRange);
        }
        Ok(n - 1 - d)
    }

    pub(super) fn lower_br(&mut self, depth: u32) -> Result<(), LowerError> {
        let idx = self.label_index(depth)?;
        match self.label_stack[idx].kind {
            LabelKind::Loop => {
                let target = self.label_stack[idx]
                    .loop_start
                    .expect("loop label has start pos");
                let here = self.enc.pos();
                let offset = target as i32 - here as i32;
                self.enc.b(offset)?;
            }
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

    pub(super) fn lower_br_if(&mut self, depth: u32) -> Result<(), LowerError> {
        let cond = self.pop_i32_slot()?;
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

    pub(super) fn lower_block_end(&mut self) -> Result<(), LowerError> {
        let label = self.label_stack.pop().ok_or(LowerError::UnbalancedEnd)?;
        let target = self.enc.pos();

        match label.kind {
            LabelKind::Block => {
                for patch in &label.pending {
                    self.patch_pending(*patch, target)?;
                }
            }
            LabelKind::Loop => {}
            LabelKind::If { cond_branch_pos } => {
                let offset = target as i32 - cond_branch_pos as i32;
                let word = encode_cbz_w(
                    rt_from_cbz_at(&self.enc, cond_branch_pos),
                    offset,
                )
                .map_err(|_| LowerError::BranchOutOfRange)?;
                self.enc.patch_word(cond_branch_pos, word);
                for patch in &label.pending {
                    self.patch_pending(*patch, target)?;
                }
            }
            LabelKind::IfElse { else_skip_pos } => {
                let offset = target as i32 - else_skip_pos as i32;
                let word = encode_b(offset)
                    .map_err(|_| LowerError::BranchOutOfRange)?;
                self.enc.patch_word(else_skip_pos, word);
                for patch in &label.pending {
                    self.patch_pending(*patch, target)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn patch_pending(&mut self, patch: PendingPatch, target: usize) -> Result<(), LowerError> {
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

    pub(super) fn lower_if(&mut self) -> Result<(), LowerError> {
        let cond = self.pop_i32_slot()?;
        let pos = self.enc.pos();
        self.enc.cbz_w(cond, 0)?;
        self.label_stack.push(Label {
            kind: LabelKind::If { cond_branch_pos: pos },
            loop_start: None,
            pending: Vec::new(),
            entry_depth: self.stack.len(),
        });
        Ok(())
    }

    pub(super) fn lower_else(&mut self) -> Result<(), LowerError> {
        let label = self.label_stack.last_mut().ok_or(LowerError::ElseWithoutIf)?;
        let cond_branch_pos = match label.kind {
            LabelKind::If { cond_branch_pos } => cond_branch_pos,
            _ => return Err(LowerError::ElseWithoutIf),
        };

        let skip_pos = self.enc.pos();
        self.enc.b(0)?;

        let else_target = self.enc.pos();
        let offset = else_target as i32 - cond_branch_pos as i32;
        let word = encode_cbz_w(
            rt_from_cbz_at(&self.enc, cond_branch_pos),
            offset,
        )
        .map_err(|_| LowerError::BranchOutOfRange)?;
        self.enc.patch_word(cond_branch_pos, word);

        let label = self.label_stack.last_mut().unwrap();
        let entry = label.entry_depth;
        label.kind = LabelKind::IfElse { else_skip_pos: skip_pos };
        self.truncate_stack_to(entry);
        Ok(())
    }

    pub(super) fn truncate_stack_to(&mut self, target: usize) {
        while self.stack.len() > target {
            let ty = self.stack.pop().unwrap();
            if ty.is_int() {
                self.int_depth -= 1;
                self.int_sym_stack.pop();
            } else {
                self.fp_depth -= 1;
            }
        }
        debug_assert_eq!(self.int_sym_stack.len(), self.int_depth,
            "int_sym_stack and int_depth diverged after truncate_stack_to");
    }
}
