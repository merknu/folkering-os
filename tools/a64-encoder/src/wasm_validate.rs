//! WASM validation pass — type-checks a function body before lowering.
//!
//! Tracks the real operand stack (ValType) and validates against actual
//! local types and function signatures. Catches malformed bytecode that
//! would otherwise panic or miscompile in the lowerer.

use alloc::vec::Vec;

use crate::wasm_lower::{FnSig, ValType, WasmOp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub op_index: usize,
    pub message: &'static str,
}

impl core::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "op[{}]: {}", self.op_index, self.message)
    }
}

/// Validate a function body with full type information.
///
/// - `local_types`: per-local ValType (from the code section's local decl)
/// - `ops`: instruction sequence ending with `End`
/// - `call_sigs`: signatures indexed by function index (for `Call`)
/// - `indirect_sigs`: signatures indexed by type index (for `CallIndirect`)
pub fn validate(
    local_types: &[ValType],
    ops: &[WasmOp],
    call_sigs: &[FnSig],
    indirect_sigs: &[FnSig],
) -> Result<(), ValidationError> {
    const MAX_STACK_DEPTH: usize = 4096;

    let mut stack: Vec<ValType> = Vec::new();
    let mut label_depth: u32 = 0;
    let mut unreachable = false;

    for (i, op) in ops.iter().enumerate() {
        let err = |msg: &'static str| ValidationError { op_index: i, message: msg };

        // After an unconditional branch or return, code is unreachable
        // until the next End or Else resets the context.
        if unreachable {
            match op {
                WasmOp::End => {
                    unreachable = false;
                    if label_depth > 0 {
                        label_depth -= 1;
                    }
                    continue;
                }
                WasmOp::Else => {
                    unreachable = false;
                    continue;
                }
                WasmOp::Block | WasmOp::Loop | WasmOp::If => {
                    label_depth += 1;
                    continue;
                }
                _ => continue,
            }
        }

        if stack.len() > MAX_STACK_DEPTH {
            return Err(err("operand stack depth exceeded (max 4096)"));
        }

        match op {
            // ── Constants ──────────────────────────────────────────
            WasmOp::I32Const(_) => stack.push(ValType::I32),
            WasmOp::I64Const(_) => stack.push(ValType::I64),
            WasmOp::F32Const(_) => stack.push(ValType::F32),
            WasmOp::F64Const(_) => stack.push(ValType::F64),
            WasmOp::V128Const(_) => stack.push(ValType::V128),

            // ── Binary i32 ────────────────────────────────────────
            WasmOp::I32Add | WasmOp::I32Sub | WasmOp::I32Mul
            | WasmOp::I32DivS | WasmOp::I32DivU
            | WasmOp::I32And | WasmOp::I32Or | WasmOp::I32Xor
            | WasmOp::I32Shl | WasmOp::I32ShrS | WasmOp::I32ShrU
            | WasmOp::I32Eq | WasmOp::I32Ne
            | WasmOp::I32LtS | WasmOp::I32GtS | WasmOp::I32LeS | WasmOp::I32GeS
            | WasmOp::I32LtU | WasmOp::I32GtU | WasmOp::I32LeU | WasmOp::I32GeU => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::I32);
            }

            // ── Binary i64 ────────────────────────────────────────
            WasmOp::I64Add | WasmOp::I64Sub | WasmOp::I64Mul
            | WasmOp::I64DivS | WasmOp::I64DivU
            | WasmOp::I64And | WasmOp::I64Or | WasmOp::I64Xor
            | WasmOp::I64Shl | WasmOp::I64ShrS | WasmOp::I64ShrU => {
                pop_expect(&mut stack, ValType::I64, &err)?;
                pop_expect(&mut stack, ValType::I64, &err)?;
                stack.push(ValType::I64);
            }

            // ── i64 comparisons (2 i64 → 1 i32) ──────────────────
            WasmOp::I64Eq | WasmOp::I64Ne
            | WasmOp::I64LtS | WasmOp::I64GtS | WasmOp::I64LeS | WasmOp::I64GeS
            | WasmOp::I64LtU | WasmOp::I64GtU | WasmOp::I64LeU | WasmOp::I64GeU => {
                pop_expect(&mut stack, ValType::I64, &err)?;
                pop_expect(&mut stack, ValType::I64, &err)?;
                stack.push(ValType::I32);
            }

            // ── Unary ─────────────────────────────────────────────
            WasmOp::I32Eqz => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::I32);
            }
            WasmOp::I64Eqz => {
                pop_expect(&mut stack, ValType::I64, &err)?;
                stack.push(ValType::I32);
            }

            // ── f32 binary / compare ──────────────────────────────
            WasmOp::F32Add | WasmOp::F32Sub | WasmOp::F32Mul | WasmOp::F32Div => {
                pop_expect(&mut stack, ValType::F32, &err)?;
                pop_expect(&mut stack, ValType::F32, &err)?;
                stack.push(ValType::F32);
            }
            WasmOp::F32Eq | WasmOp::F32Ne | WasmOp::F32Lt
            | WasmOp::F32Gt | WasmOp::F32Le | WasmOp::F32Ge => {
                pop_expect(&mut stack, ValType::F32, &err)?;
                pop_expect(&mut stack, ValType::F32, &err)?;
                stack.push(ValType::I32);
            }

            // ── f64 binary / compare ──────────────────────────────
            WasmOp::F64Add | WasmOp::F64Sub | WasmOp::F64Mul | WasmOp::F64Div => {
                pop_expect(&mut stack, ValType::F64, &err)?;
                pop_expect(&mut stack, ValType::F64, &err)?;
                stack.push(ValType::F64);
            }
            WasmOp::F64Eq | WasmOp::F64Ne | WasmOp::F64Lt
            | WasmOp::F64Gt | WasmOp::F64Le | WasmOp::F64Ge => {
                pop_expect(&mut stack, ValType::F64, &err)?;
                pop_expect(&mut stack, ValType::F64, &err)?;
                stack.push(ValType::I32);
            }

            // ── Memory ops ────────────────────────────────────────
            WasmOp::I32Load(_) => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::I32);
            }
            WasmOp::I32Store(_) => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                pop_expect(&mut stack, ValType::I32, &err)?;
            }
            WasmOp::I64Load(_) => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::I64);
            }
            WasmOp::I64Store(_) => {
                pop_expect(&mut stack, ValType::I64, &err)?;
                pop_expect(&mut stack, ValType::I32, &err)?;
            }
            WasmOp::F32Load(_) => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::F32);
            }
            WasmOp::F32Store(_) => {
                pop_expect(&mut stack, ValType::F32, &err)?;
                pop_expect(&mut stack, ValType::I32, &err)?;
            }
            WasmOp::F64Load(_) => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::F64);
            }
            WasmOp::F64Store(_) => {
                pop_expect(&mut stack, ValType::F64, &err)?;
                pop_expect(&mut stack, ValType::I32, &err)?;
            }
            WasmOp::V128Load(_) => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::V128);
            }
            WasmOp::V128Store(_) => {
                pop_expect(&mut stack, ValType::V128, &err)?;
                pop_expect(&mut stack, ValType::I32, &err)?;
            }

            // ── Locals (real types) ───────────────────────────────
            WasmOp::LocalGet(idx) => {
                let idx = *idx as usize;
                if idx >= local_types.len() {
                    return Err(err("local index out of range"));
                }
                stack.push(local_types[idx]);
            }
            WasmOp::LocalSet(idx) => {
                let idx = *idx as usize;
                if idx >= local_types.len() {
                    return Err(err("local index out of range"));
                }
                pop_expect(&mut stack, local_types[idx], &err)?;
            }
            WasmOp::LocalTee(idx) => {
                let idx = *idx as usize;
                if idx >= local_types.len() {
                    return Err(err("local index out of range"));
                }
                let top = stack.last().copied().ok_or(err("stack underflow"))?;
                if top != local_types[idx] {
                    return Err(err("type mismatch"));
                }
            }
            WasmOp::Drop => {
                if stack.is_empty() {
                    return Err(err("stack underflow"));
                }
                stack.pop();
            }
            WasmOp::Select => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                let b = stack.pop().ok_or(err("stack underflow"))?;
                let a = stack.pop().ok_or(err("stack underflow"))?;
                if a != b {
                    return Err(err("select: both values must have same type"));
                }
                stack.push(a);
            }

            // ── Control flow ──────────────────────────────────────
            WasmOp::Block | WasmOp::Loop | WasmOp::If => {
                if matches!(op, WasmOp::If) {
                    pop_expect(&mut stack, ValType::I32, &err)?;
                }
                label_depth += 1;
            }
            WasmOp::Else => {}
            WasmOp::Br(d) | WasmOp::BrIf(d) => {
                if *d >= label_depth {
                    return Err(err("br/br_if depth exceeds label stack"));
                }
                if matches!(op, WasmOp::BrIf(_)) {
                    pop_expect(&mut stack, ValType::I32, &err)?;
                } else {
                    unreachable = true;
                }
            }
            WasmOp::End => {
                if label_depth > 0 {
                    label_depth -= 1;
                }
            }
            WasmOp::Return => {
                if stack.is_empty() {
                    return Err(err("return with empty stack"));
                }
                unreachable = true;
            }

            // ── Conversions ───────────────────────────────────────
            WasmOp::I32WrapI64 => {
                pop_expect(&mut stack, ValType::I64, &err)?;
                stack.push(ValType::I32);
            }
            WasmOp::I64ExtendI32S | WasmOp::I64ExtendI32U => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::I64);
            }
            WasmOp::I32Extend8S | WasmOp::I32Extend16S => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::I32);
            }
            WasmOp::I64Extend8S | WasmOp::I64Extend16S | WasmOp::I64Extend32S => {
                pop_expect(&mut stack, ValType::I64, &err)?;
                stack.push(ValType::I64);
            }
            WasmOp::I32TruncF32S | WasmOp::I32TruncF32U => {
                pop_expect(&mut stack, ValType::F32, &err)?;
                stack.push(ValType::I32);
            }
            WasmOp::I32TruncF64S | WasmOp::I32TruncF64U => {
                pop_expect(&mut stack, ValType::F64, &err)?;
                stack.push(ValType::I32);
            }
            WasmOp::I64TruncF32S | WasmOp::I64TruncF32U => {
                pop_expect(&mut stack, ValType::F32, &err)?;
                stack.push(ValType::I64);
            }
            WasmOp::I64TruncF64S | WasmOp::I64TruncF64U => {
                pop_expect(&mut stack, ValType::F64, &err)?;
                stack.push(ValType::I64);
            }
            WasmOp::F32ConvertI32S | WasmOp::F32ConvertI32U => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::F32);
            }
            WasmOp::F32ConvertI64S | WasmOp::F32ConvertI64U => {
                pop_expect(&mut stack, ValType::I64, &err)?;
                stack.push(ValType::F32);
            }
            WasmOp::F64ConvertI32S | WasmOp::F64ConvertI32U => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::F64);
            }
            WasmOp::F64ConvertI64S | WasmOp::F64ConvertI64U => {
                pop_expect(&mut stack, ValType::I64, &err)?;
                stack.push(ValType::F64);
            }
            WasmOp::F32DemoteF64 => {
                pop_expect(&mut stack, ValType::F64, &err)?;
                stack.push(ValType::F32);
            }
            WasmOp::F64PromoteF32 => {
                pop_expect(&mut stack, ValType::F32, &err)?;
                stack.push(ValType::F64);
            }
            WasmOp::I32ReinterpretF32 => {
                pop_expect(&mut stack, ValType::F32, &err)?;
                stack.push(ValType::I32);
            }
            WasmOp::I64ReinterpretF64 => {
                pop_expect(&mut stack, ValType::F64, &err)?;
                stack.push(ValType::I64);
            }
            WasmOp::F32ReinterpretI32 => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::F32);
            }
            WasmOp::F64ReinterpretI64 => {
                pop_expect(&mut stack, ValType::I64, &err)?;
                stack.push(ValType::F64);
            }

            // ── Calls (real signatures) ───────────────────────────
            WasmOp::Call(idx) => {
                let idx = *idx as usize;
                if idx >= call_sigs.len() {
                    return Err(err("call target index out of range"));
                }
                let sig = &call_sigs[idx];
                for p in sig.params.iter().rev() {
                    pop_expect(&mut stack, *p, &err)?;
                }
                if let Some(r) = sig.result {
                    stack.push(r);
                }
            }
            WasmOp::CallIndirect(type_id) => {
                pop_expect(&mut stack, ValType::I32, &err)?; // table index
                let type_id = *type_id as usize;
                if type_id >= indirect_sigs.len() {
                    return Err(err("call_indirect type index out of range"));
                }
                let sig = &indirect_sigs[type_id];
                for p in sig.params.iter().rev() {
                    pop_expect(&mut stack, *p, &err)?;
                }
                if let Some(r) = sig.result {
                    stack.push(r);
                }
            }

            // ── SIMD binary v128 → v128 ───────────────────────────
            WasmOp::F32x4Add | WasmOp::F32x4Sub | WasmOp::F32x4Mul
            | WasmOp::F32x4Div | WasmOp::F32x4Max | WasmOp::F32x4Min
            | WasmOp::I32x4Add | WasmOp::I32x4Sub | WasmOp::I32x4Mul
            | WasmOp::F32x4Eq | WasmOp::F32x4Gt
            | WasmOp::F64x2Add | WasmOp::F64x2Sub | WasmOp::F64x2Mul
            | WasmOp::F64x2Div | WasmOp::F64x2Min | WasmOp::F64x2Max
            | WasmOp::I8x16Add | WasmOp::I8x16Sub
            | WasmOp::I16x8Add | WasmOp::I16x8Sub | WasmOp::I16x8Mul => {
                pop_expect(&mut stack, ValType::V128, &err)?;
                pop_expect(&mut stack, ValType::V128, &err)?;
                stack.push(ValType::V128);
            }

            // ── SIMD unary v128 → v128 ────────────────────────────
            WasmOp::F32x4Abs | WasmOp::F32x4Neg | WasmOp::F32x4Sqrt
            | WasmOp::F64x2Abs | WasmOp::F64x2Neg | WasmOp::F64x2Sqrt => {
                pop_expect(&mut stack, ValType::V128, &err)?;
                stack.push(ValType::V128);
            }

            // ── SIMD ternary (3 v128 → 1 v128) ───────────────────
            WasmOp::F32x4Fma | WasmOp::V128Bitselect => {
                pop_expect(&mut stack, ValType::V128, &err)?;
                pop_expect(&mut stack, ValType::V128, &err)?;
                pop_expect(&mut stack, ValType::V128, &err)?;
                stack.push(ValType::V128);
            }

            // ── SIMD splat ────────────────────────────────────────
            WasmOp::F32x4Splat => {
                pop_expect(&mut stack, ValType::F32, &err)?;
                stack.push(ValType::V128);
            }
            WasmOp::F64x2Splat => {
                pop_expect(&mut stack, ValType::F64, &err)?;
                stack.push(ValType::V128);
            }
            WasmOp::I32x4Splat | WasmOp::I8x16Splat | WasmOp::I16x8Splat => {
                pop_expect(&mut stack, ValType::I32, &err)?;
                stack.push(ValType::V128);
            }

            // ── SIMD extract_lane ─────────────────────────────────
            WasmOp::F32x4ExtractLane(_) => {
                pop_expect(&mut stack, ValType::V128, &err)?;
                stack.push(ValType::F32);
            }
            WasmOp::F64x2ExtractLane(_) => {
                pop_expect(&mut stack, ValType::V128, &err)?;
                stack.push(ValType::F64);
            }
            WasmOp::I32x4ExtractLane(_) | WasmOp::I8x16ExtractLaneU(_)
            | WasmOp::I16x8ExtractLaneU(_) => {
                pop_expect(&mut stack, ValType::V128, &err)?;
                stack.push(ValType::I32);
            }

            // ── SIMD horizontal sum ───────────────────────────────
            WasmOp::F32x4HorizontalSum => {
                pop_expect(&mut stack, ValType::V128, &err)?;
                stack.push(ValType::F32);
            }
        }
    }

    if label_depth != 0 {
        return Err(ValidationError {
            op_index: ops.len(),
            message: "unclosed block/loop at function end",
        });
    }
    if stack.len() != 1 {
        return Err(ValidationError {
            op_index: ops.len(),
            message: "function must leave exactly 1 value on the stack",
        });
    }
    Ok(())
}

fn pop_expect<F>(
    stack: &mut Vec<ValType>,
    expected: ValType,
    err: &F,
) -> Result<ValType, ValidationError>
where
    F: Fn(&'static str) -> ValidationError,
{
    match stack.pop() {
        None => Err(err("stack underflow")),
        Some(got) if got != expected => Err(err("type mismatch")),
        Some(got) => Ok(got),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn v(locals: &[ValType], ops: &[WasmOp]) -> Result<(), ValidationError> {
        validate(locals, ops, &[], &[])
    }

    // ── Basic ops ─────────────────────────────────────────────────

    #[test]
    fn valid_const_42() {
        assert!(v(&[], &[WasmOp::I32Const(42), WasmOp::End]).is_ok());
    }

    #[test]
    fn valid_arith() {
        assert!(v(&[], &[
            WasmOp::I32Const(3), WasmOp::I32Const(4), WasmOp::I32Add, WasmOp::End,
        ]).is_ok());
    }

    #[test]
    fn underflow_detected() {
        let r = v(&[], &[WasmOp::I32Add, WasmOp::End]);
        assert_eq!(r.unwrap_err().message, "stack underflow");
    }

    #[test]
    fn type_mismatch_detected() {
        let r = v(&[], &[WasmOp::I32Const(1), WasmOp::F32Const(2.0), WasmOp::I32Add, WasmOp::End]);
        assert_eq!(r.unwrap_err().message, "type mismatch");
    }

    // ── Locals (typed) ────────────────────────────────────────────

    #[test]
    fn local_out_of_range() {
        let r = v(&[ValType::I32], &[WasmOp::LocalGet(5), WasmOp::End]);
        assert_eq!(r.unwrap_err().message, "local index out of range");
    }

    #[test]
    fn local_get_pushes_real_type() {
        assert!(v(
            &[ValType::F64],
            &[WasmOp::LocalGet(0), WasmOp::End],
        ).is_ok());
    }

    #[test]
    fn local_get_f64_rejects_i32_add() {
        let r = v(
            &[ValType::F64],
            &[WasmOp::LocalGet(0), WasmOp::LocalGet(0), WasmOp::I32Add, WasmOp::End],
        );
        assert_eq!(r.unwrap_err().message, "type mismatch");
    }

    #[test]
    fn local_set_type_mismatch() {
        let r = v(
            &[ValType::I64],
            &[WasmOp::I32Const(1), WasmOp::LocalSet(0), WasmOp::I32Const(0), WasmOp::End],
        );
        assert_eq!(r.unwrap_err().message, "type mismatch");
    }

    #[test]
    fn local_set_correct_type() {
        assert!(v(
            &[ValType::I64],
            &[WasmOp::I64Const(1), WasmOp::LocalSet(0), WasmOp::I32Const(0), WasmOp::End],
        ).is_ok());
    }

    // ── Control flow ──────────────────────────────────────────────

    #[test]
    fn br_depth_out_of_range() {
        let r = v(&[], &[WasmOp::I32Const(0), WasmOp::BrIf(0), WasmOp::I32Const(1), WasmOp::End]);
        assert_eq!(r.unwrap_err().message, "br/br_if depth exceeds label stack");
    }

    #[test]
    fn unclosed_block() {
        assert!(v(&[], &[WasmOp::Block, WasmOp::I32Const(1), WasmOp::End]).is_ok());
    }

    #[test]
    fn empty_stack_at_end() {
        let r = v(&[], &[WasmOp::End]);
        assert_eq!(r.unwrap_err().message, "function must leave exactly 1 value on the stack");
    }

    // ── Calls (typed) ─────────────────────────────────────────────

    #[test]
    fn call_pops_args_and_pushes_result() {
        let sig = FnSig { params: vec![ValType::I32, ValType::I32], result: Some(ValType::I64) };
        assert!(validate(
            &[],
            &[WasmOp::I32Const(1), WasmOp::I32Const(2), WasmOp::Call(0), WasmOp::End],
            &[sig],
            &[],
        ).is_ok());
    }

    #[test]
    fn call_wrong_arg_type() {
        let sig = FnSig { params: vec![ValType::I64], result: Some(ValType::I32) };
        let r = validate(
            &[],
            &[WasmOp::I32Const(1), WasmOp::Call(0), WasmOp::End],
            &[sig],
            &[],
        );
        assert_eq!(r.unwrap_err().message, "type mismatch");
    }

    #[test]
    fn call_missing_args() {
        let sig = FnSig { params: vec![ValType::I32, ValType::I32], result: Some(ValType::I32) };
        let r = validate(
            &[],
            &[WasmOp::I32Const(1), WasmOp::Call(0), WasmOp::End],
            &[sig],
            &[],
        );
        assert_eq!(r.unwrap_err().message, "stack underflow");
    }

    #[test]
    fn call_target_out_of_range() {
        let r = validate(
            &[],
            &[WasmOp::Call(5), WasmOp::End],
            &[],
            &[],
        );
        assert_eq!(r.unwrap_err().message, "call target index out of range");
    }

    #[test]
    fn call_void_return() {
        let sig = FnSig { params: vec![], result: None };
        let r = validate(
            &[],
            &[WasmOp::Call(0), WasmOp::End],
            &[sig],
            &[],
        );
        assert_eq!(r.unwrap_err().message, "function must leave exactly 1 value on the stack");
    }

    #[test]
    fn call_indirect_pops_index_and_args() {
        let sig = FnSig { params: vec![ValType::I32], result: Some(ValType::I32) };
        assert!(validate(
            &[],
            &[WasmOp::I32Const(42), WasmOp::I32Const(0), WasmOp::CallIndirect(0), WasmOp::End],
            &[],
            &[sig],
        ).is_ok());
    }

    #[test]
    fn call_indirect_type_out_of_range() {
        let r = validate(
            &[],
            &[WasmOp::I32Const(0), WasmOp::CallIndirect(9), WasmOp::End],
            &[],
            &[],
        );
        assert_eq!(r.unwrap_err().message, "call_indirect type index out of range");
    }

    #[test]
    fn call_indirect_wrong_arg_type() {
        let sig = FnSig { params: vec![ValType::I64], result: Some(ValType::I32) };
        let r = validate(
            &[],
            &[WasmOp::I32Const(1), WasmOp::I32Const(0), WasmOp::CallIndirect(0), WasmOp::End],
            &[],
            &[sig],
        );
        assert_eq!(r.unwrap_err().message, "type mismatch");
    }

    // ── SIMD type checking ────────────────────────────────────────

    #[test]
    fn simd_type_checked() {
        let r = v(&[], &[WasmOp::I32Const(1), WasmOp::I32Const(2), WasmOp::F32x4Add, WasmOp::End]);
        assert_eq!(r.unwrap_err().message, "type mismatch");
    }

    #[test]
    fn simd_splat_wrong_type() {
        let r = v(&[], &[WasmOp::I32Const(1), WasmOp::F32x4Splat, WasmOp::End]);
        assert_eq!(r.unwrap_err().message, "type mismatch");
    }

    #[test]
    fn simd_splat_correct_type() {
        assert!(v(&[], &[WasmOp::F32Const(1.0), WasmOp::F32x4Splat, WasmOp::End]).is_ok());
    }

    // ── Mixed-type locals loop ────────────────────────────────────

    #[test]
    fn valid_loop_with_typed_locals() {
        assert!(v(
            &[ValType::I32, ValType::I32],
            &[
                WasmOp::I32Const(1),
                WasmOp::LocalSet(1),
                WasmOp::Block,
                WasmOp::Loop,
                WasmOp::LocalGet(1),
                WasmOp::I32Const(11),
                WasmOp::I32GeS,
                WasmOp::BrIf(1),
                WasmOp::LocalGet(0),
                WasmOp::LocalGet(1),
                WasmOp::I32Add,
                WasmOp::LocalSet(0),
                WasmOp::LocalGet(1),
                WasmOp::I32Const(1),
                WasmOp::I32Add,
                WasmOp::LocalSet(1),
                WasmOp::Br(0),
                WasmOp::End,
                WasmOp::End,
                WasmOp::LocalGet(0),
                WasmOp::End,
            ],
        ).is_ok());
    }

    // ── Unreachability ────────────────────────────────────────────

    #[test]
    fn unreachable_code_after_br_skipped() {
        // Code after unconditional br is unreachable — the f32.add
        // with i32 operands would normally fail, but it's skipped.
        assert!(v(&[], &[
            WasmOp::I32Const(5),
            WasmOp::Block,
            WasmOp::Br(0),
            WasmOp::I32Const(1), // unreachable
            WasmOp::I32Const(2), // unreachable
            WasmOp::F32Add,      // unreachable — would be type error if checked
            WasmOp::End,
            WasmOp::End,
        ]).is_ok());
    }

    #[test]
    fn unreachable_code_after_return_skipped() {
        assert!(v(&[], &[
            WasmOp::I32Const(42),
            WasmOp::Return,
            WasmOp::F64Const(1.0), // unreachable
            WasmOp::F64Const(2.0), // unreachable
            WasmOp::I32Add,        // unreachable — wrong types
            WasmOp::End,
        ]).is_ok());
    }

    #[test]
    fn br_if_does_not_mark_unreachable() {
        // br_if is conditional — code after it IS reachable.
        let r = v(&[], &[
            WasmOp::I32Const(1),
            WasmOp::Block,
            WasmOp::I32Const(1),
            WasmOp::BrIf(0),
            WasmOp::F32Const(1.0), // reachable — should be type-checked
            WasmOp::I32Add,        // type error: f32 vs i32 expected
            WasmOp::End,
            WasmOp::End,
        ]);
        assert_eq!(r.unwrap_err().message, "type mismatch");
    }

    #[test]
    fn else_resets_unreachable() {
        // Br in then-branch makes rest unreachable, but else resets it.
        assert!(v(&[], &[
            WasmOp::I32Const(1),
            WasmOp::If,
            WasmOp::Br(0),
            WasmOp::I32Const(99), // unreachable
            WasmOp::Else,         // resets unreachable
            WasmOp::I32Const(42), // reachable again
            WasmOp::End,
            WasmOp::End,
        ]).is_ok());
    }

    #[test]
    fn nested_blocks_in_unreachable() {
        // Nested block/end inside unreachable region must still
        // balance label_depth correctly.
        assert!(v(&[], &[
            WasmOp::I32Const(5),
            WasmOp::Block,
            WasmOp::Br(0),
            WasmOp::Block,        // unreachable — but depth still tracked
            WasmOp::End,          // closes inner block
            WasmOp::End,          // closes outer block
            WasmOp::End,
        ]).is_ok());
    }

    // ── Stack depth limit ─────────────────────────────────────────

    #[test]
    fn stack_depth_limit_enforced() {
        let mut ops: Vec<WasmOp> = Vec::new();
        for _ in 0..4098 {
            ops.push(WasmOp::I32Const(0));
        }
        ops.push(WasmOp::End);
        let r = validate(&[], &ops, &[], &[]);
        assert_eq!(r.unwrap_err().message, "operand stack depth exceeded (max 4096)");
    }
}
