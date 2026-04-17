use super::*;

fn compile(ops: &[WasmOp]) -> Vec<u8> {
    let mut lw = Lowerer::new();
    lw.lower_all(ops).expect("lower");
    lw.finish()
}

fn compile_with_locals(n: usize, ops: &[WasmOp]) -> Vec<u8> {
    let mut lw = Lowerer::new_with_locals(n).expect("new_with_locals");
    lw.lower_all(ops).expect("lower");
    lw.finish()
}

fn bytes_as_u32s(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// ── Phase 2.0 regression ────────────────────────────────────────

#[test]
fn return_42_matches_phase_1() {
    let bytes = compile(&[WasmOp::I32Const(42), WasmOp::End]);
    assert_eq!(
        bytes,
        vec![
            0x40, 0x05, 0x80, 0xD2, // movz x0, #42
            0xC0, 0x03, 0x5F, 0xD6, // ret
        ]
    );
}

#[test]
fn const_add_const() {
    let bytes = compile(&[
        WasmOp::I32Const(10),
        WasmOp::I32Const(20),
        WasmOp::I32Add,
        WasmOp::End,
    ]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words, vec![0xD2800140, 0xD2800281, 0x8B010000, 0xD65F03C0]);
}

#[test]
fn nested_arithmetic() {
    let bytes = compile(&[
        WasmOp::I32Const(1),
        WasmOp::I32Const(2),
        WasmOp::I32Const(3),
        WasmOp::I32Add,
        WasmOp::I32Add,
        WasmOp::End,
    ]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words[0], 0xD2800020);
    assert_eq!(words[1], 0xD2800041);
    assert_eq!(words[2], 0xD2800062);
    assert_eq!(words[3], 0x8B020021);
    assert_eq!(words[4], 0x8B010000);
    assert_eq!(words[5], 0xD65F03C0);
}

// ── Phase 2.1: negative consts, locals ──────────────────────────

#[test]
fn negative_const_encodes_as_u32() {
    // i32.const -1 → MOVZ X0, #0xFFFF ; MOVK X0, #0xFFFF, LSL #16
    //              → 0xD29FFFE0 ; 0xF2BFFFE0 ; RET
    let bytes = compile(&[WasmOp::I32Const(-1), WasmOp::End]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words, vec![0xD29FFFE0, 0xF2BFFFE0, 0xD65F03C0]);
}

#[test]
fn negative_small_const() {
    // i32.const -42 → -42 as u32 = 0xFFFFFFD6
    //   MOVZ X0, #0xFFD6 ; MOVK X0, #0xFFFF, LSL #16 ; RET
    let bytes = compile(&[WasmOp::I32Const(-42), WasmOp::End]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words, vec![0xD29FFAC0, 0xF2BFFFE0, 0xD65F03C0]);
}

#[test]
fn locals_init_and_get() {
    // (func (local i32) local.get 0 end)
    // Prologue: MOVZ X19, #0
    // Body: ADD X0, XZR, X19 ; RET
    let bytes = compile_with_locals(1, &[WasmOp::LocalGet(0), WasmOp::End]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words[0], 0xD2800013); // movz x19, #0
    assert_eq!(words[1], 0x8B1303E0); // add x0, xzr, x19
    assert_eq!(words[2], 0xD65F03C0); // ret
}

#[test]
fn local_set_then_get() {
    // (func (local i32)
    //   i32.const 7
    //   local.set 0
    //   local.get 0
    //   end)
    let bytes = compile_with_locals(
        1,
        &[
            WasmOp::I32Const(7),
            WasmOp::LocalSet(0),
            WasmOp::LocalGet(0),
            WasmOp::End,
        ],
    );
    let words = bytes_as_u32s(&bytes);
    // movz x19, #0
    // movz x0,  #7       ; push const
    // add  x19, xzr, x0  ; local.set 0
    // add  x0, xzr, x19  ; local.get 0
    // ret
    assert_eq!(words[0], 0xD2800013);
    assert_eq!(words[1], 0xD28000E0);
    assert_eq!(words[2], 0x8B0003F3);
    assert_eq!(words[3], 0x8B1303E0);
    assert_eq!(words[4], 0xD65F03C0);
}

#[test]
fn too_many_locals_rejected() {
    let err = Lowerer::new_with_locals(MAX_LOCALS + 1).unwrap_err();
    assert_eq!(err, LowerError::TooManyLocals);
}

#[test]
fn local_out_of_range() {
    let mut lw = Lowerer::new_with_locals(1).unwrap();
    assert_eq!(
        lw.lower_op(WasmOp::LocalGet(5)),
        Err(LowerError::LocalOutOfRange)
    );
}

// ── Phase 2.2: control flow ─────────────────────────────────────

#[test]
fn block_end_is_noop_when_empty() {
    // An empty block followed by a return should emit only the
    // constant load + RET (the block/end don't generate anything
    // on their own).
    let mut lw = Lowerer::new();
    lw.lower_op(WasmOp::Block).unwrap();
    lw.lower_op(WasmOp::End).unwrap(); // close block
    lw.lower_op(WasmOp::I32Const(1)).unwrap();
    lw.lower_op(WasmOp::End).unwrap(); // function end
    let words = bytes_as_u32s(&lw.finish());
    // movz x0, #1 ; ret
    assert_eq!(words, vec![0xD2800020, 0xD65F03C0]);
}

#[test]
fn br_from_block_emits_patched_forward_branch() {
    // (func (result i32)
    //   i32.const 5
    //   block
    //     br 0             ;; unconditional jump to block end
    //   end
    //   end)
    //
    // Our compile-time stack tracking doesn't model reachability,
    // so anything after `br` in the same block would confuse the
    // function-end depth check.  Real WASM would let the verifier
    // discard unreachable ops; we keep the test simple and put
    // `br` last in the block.
    //
    // Expected bytes:
    //   movz x0, #5          ; push 5 (stack → [X0=5])
    //   b    +4              ; br 0 — patched to next insn (end-of-block)
    //   ret                  ; function end, X0 already holds result
    let bytes = compile(&[
        WasmOp::I32Const(5),
        WasmOp::Block,
        WasmOp::Br(0),
        WasmOp::End, // close block — patches the B placeholder
        WasmOp::End, // function end
    ]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words[0], 0xD28000A0); // movz x0, #5
    assert_eq!(words[1], 0x14000001); // b +4 (one instruction forward)
    assert_eq!(words[2], 0xD65F03C0); // ret
}

#[test]
fn br_from_loop_is_backward_branch() {
    // loop
    //   br 0   ;; infinite loop: jump to loop start
    // end
    // i32.const 0
    // end
    //
    // Emission:
    //   b .       ; backward offset 0 — B to itself (infinite loop)
    //   movz x0,#0
    //   ret
    let bytes = compile(&[
        WasmOp::Loop,
        WasmOp::Br(0),
        WasmOp::End, // close loop
        WasmOp::I32Const(0),
        WasmOp::End, // function end
    ]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words[0], 0x14000000); // b . (offset 0)
    assert_eq!(words[1], 0xD2800000); // movz x0, #0
    assert_eq!(words[2], 0xD65F03C0); // ret
}

#[test]
fn br_if_from_block_patches_cbnz() {
    // (func (result i32) (local i32)
    //   block
    //     local.get 0      ;; push local 0 as condition
    //     br_if 0          ;; branch to end if non-zero
    //     i32.const 7      ;; (only reached if condition == 0)
    //     drop-equivalent? -- we'll just make it the block's fallthrough
    //   end
    //   i32.const 42
    //   end)
    //
    // For simplicity, require the block leaves stack balanced.
    // Emission trace:
    //   movz x19, #0           ; local 0 init
    //   add  x0, xzr, x19       ; local.get 0 (push to X0)
    //   cbnz w0, <end-of-block> ; br_if 0 (placeholder)
    //   movz x0, #7             ; fallthrough const
    //   ; block end — patch cbnz to here
    //   ; but now stack has X0 from either br_if (prior) or i32.const 7.
    //   ; For MVP we don't enforce block signatures — the patched
    //   ; target is right after `movz x0, #7`.
    //   movz x0, #42            ; WAIT — we can't re-push x0, depth conflict
    //
    // Simpler test: just verify CBNZ is emitted and patched correctly.
    let mut lw = Lowerer::new_with_locals(1).unwrap();
    lw.lower_all(&[
        WasmOp::Block,
        WasmOp::LocalGet(0),
        WasmOp::BrIf(0),
        WasmOp::End, // close block — patches CBNZ
        WasmOp::I32Const(42),
        WasmOp::End, // function end
    ])
    .unwrap();
    let words = bytes_as_u32s(&lw.finish());
    // Word 0: movz x19, #0 (local init)
    assert_eq!(words[0], 0xD2800013);
    // Word 1: add x0, xzr, x19 (local.get 0)
    assert_eq!(words[1], 0x8B1303E0);
    // Word 2: cbnz w0, +4 (branch 4 bytes forward, to the movz at word 3)
    // imm19 = 1 (4/4), rt = 0 → 0x35000020
    assert_eq!(words[2], 0x35000020);
    // Word 3: movz x0, #42 (after block)
    assert_eq!(words[3], 0xD2800540);
    // Word 4: ret
    assert_eq!(words[4], 0xD65F03C0);
}

#[test]
fn unbalanced_end_without_block() {
    let mut lw = Lowerer::new();
    // function end with empty stack — StackNotSingleton (not UnbalancedEnd,
    // because no open label exists, we fall through to lower_function_end).
    assert_eq!(
        lw.lower_op(WasmOp::End),
        Err(LowerError::StackNotSingleton)
    );
}

#[test]
fn br_out_of_range() {
    let mut lw = Lowerer::new();
    lw.lower_op(WasmOp::Block).unwrap();
    assert_eq!(
        lw.lower_op(WasmOp::Br(5)),
        Err(LowerError::LabelOutOfRange)
    );
}

// ── Phase 2.0 regressions (kept) ────────────────────────────────

#[test]
fn stack_underflow_on_lonely_add() {
    let mut lw = Lowerer::new();
    assert_eq!(
        lw.lower_op(WasmOp::I32Add),
        Err(LowerError::StackUnderflow)
    );
}

#[test]
fn end_rejects_multi_value_stack() {
    let mut lw = Lowerer::new();
    lw.lower_op(WasmOp::I32Const(1)).unwrap();
    lw.lower_op(WasmOp::I32Const(2)).unwrap();
    assert_eq!(
        lw.lower_op(WasmOp::End),
        Err(LowerError::StackNotSingleton)
    );
}

#[test]
fn stack_overflow_frameless_at_14() {
    // Frameless lowerer has no spill area → overflow at 14.
    let mut lw = Lowerer::new();
    for _ in 0..MAX_PRIMARY_INT {
        lw.lower_op(WasmOp::I32Const(0)).unwrap();
    }
    assert_eq!(
        lw.lower_op(WasmOp::I32Const(0)),
        Err(LowerError::TypedStackOverflow(ValType::I32))
    );
}

#[test]
fn extended_band_allows_deep_stack() {
    // Function with 0 locals: 16 primary + 9 callee-saved = 25 slots.
    let mut lw = Lowerer::new_function(0, Vec::new()).unwrap();
    for i in 0..20 {
        lw.lower_op(WasmOp::I32Const(i as i32)).unwrap();
    }
    // 20 pushes: 16 direct (X0..X15) + 4 extended (X19..X22).
    // All in registers, no memory spill.
    for _ in 0..19 {
        lw.lower_op(WasmOp::I32Add).unwrap();
    }
    lw.lower_op(WasmOp::End).unwrap();
    let _bytes = lw.finish();
}

#[test]
fn extended_band_with_locals() {
    // 2 locals → X19, X20 used for locals → extended starts at X21.
    // Available extended: X21..X27 = 7 slots.
    // Total: 16 + 7 = 23.
    let mut lw = Lowerer::new_function(2, Vec::new()).unwrap();
    for i in 0..20 {
        lw.lower_op(WasmOp::I32Const(i as i32)).unwrap();
    }
    for _ in 0..19 {
        lw.lower_op(WasmOp::I32Add).unwrap();
    }
    lw.lower_op(WasmOp::End).unwrap();
    let _bytes = lw.finish();
}

#[test]
fn extended_band_overflow_detected() {
    // 0 locals → 25 register + 16 spill = 41 total. Push 42 → error.
    let mut lw = Lowerer::new_function(0, Vec::new()).unwrap();
    for _ in 0..41 {
        lw.lower_op(WasmOp::I32Const(0)).unwrap();
    }
    assert_eq!(
        lw.lower_op(WasmOp::I32Const(0)),
        Err(LowerError::TypedStackOverflow(ValType::I32))
    );
}

#[test]
fn spill_to_memory_roundtrip() {
    // Push 30 values (16 primary + 9 extended + 5 spill), then
    // sum them all via I32Add. Exercises the full push→spill→
    // pop→reload pipeline. Should compile without error.
    let mut lw = Lowerer::new_function(0, Vec::new()).unwrap();
    for i in 0..30 {
        lw.lower_op(WasmOp::I32Const(i as i32)).unwrap();
    }
    for _ in 0..29 {
        lw.lower_op(WasmOp::I32Add).unwrap();
    }
    lw.lower_op(WasmOp::End).unwrap();
    let _bytes = lw.finish();
}

// ── select / drop / local.tee ───────────────────────────────────

#[test]
fn select_picks_true_branch() {
    // select(10, 20, 1) → 10  (cond != 0 → pick val_true)
    let bytes = compile(&[
        WasmOp::I32Const(10),  // val_true
        WasmOp::I32Const(20),  // val_false
        WasmOp::I32Const(1),   // cond (nonzero)
        WasmOp::Select,
        WasmOp::End,
    ]);
    let words = bytes_as_u32s(&bytes);
    let has_csel = words.iter().any(|&w| (w & 0xFFE0_0C00) == 0x9A80_0000);
    assert!(has_csel, "CSEL instruction not found: {:08X?}", words);
}

#[test]
fn select_f32_uses_fcsel() {
    let mut lw = Lowerer::new();
    lw.lower_all(&[
        WasmOp::F32Const(1.0),
        WasmOp::F32Const(2.0),
        WasmOp::I32Const(0),
        WasmOp::Select,
        WasmOp::End,
    ]).unwrap();
    let words = bytes_as_u32s(&lw.finish_raw());
    let has_fcsel = words.iter().any(|&w| (w & 0xFF20_0C00) == 0x1E20_0C00);
    assert!(has_fcsel, "FCSEL instruction not found");
}

#[test]
fn drop_pops_value() {
    let mut lw = Lowerer::new();
    lw.lower_all(&[
        WasmOp::I32Const(1),
        WasmOp::I32Const(2),
        WasmOp::Drop,
        WasmOp::End,
    ]).unwrap();
    assert_eq!(lw.stack_depth(), 0);
}

#[test]
fn local_tee_keeps_value() {
    let mut lw = Lowerer::new_with_locals(1).unwrap();
    lw.lower_op(WasmOp::I32Const(42)).unwrap();
    lw.lower_op(WasmOp::LocalTee(0)).unwrap();
    assert_eq!(lw.stack_depth(), 1);
    lw.lower_op(WasmOp::End).unwrap();
    let _bytes = lw.finish();
}

// ── FP-bank spill ───────────────────────────────────────────────

#[test]
fn fp_spill_roundtrip() {
    // Push 20 f32 constants (16 register + 4 spill), sum them all.
    let mut lw = Lowerer::new_function(0, Vec::new()).unwrap();
    for i in 0..20 {
        lw.lower_op(WasmOp::F32Const(i as f32)).unwrap();
    }
    for _ in 0..19 {
        lw.lower_op(WasmOp::F32Add).unwrap();
    }
    lw.lower_op(WasmOp::End).unwrap();
    let _bytes = lw.finish();
}

#[test]
fn fp_spill_overflow_detected() {
    // 16 register + 8 spill = 24 total. Push 25 → error.
    let mut lw = Lowerer::new_function(0, Vec::new()).unwrap();
    for _ in 0..24 {
        lw.lower_op(WasmOp::F32Const(1.0)).unwrap();
    }
    assert_eq!(
        lw.lower_op(WasmOp::F32Const(1.0)),
        Err(LowerError::TypedStackOverflow(ValType::F32))
    );
}

// ── Phase 4B: MUL / SDIV / UDIV ─────────────────────────────────

#[test]
fn mul_basic() {
    // (func (result i32) i32.const 6 i32.const 7 i32.mul end) → 42
    let bytes = compile(&[
        WasmOp::I32Const(6),
        WasmOp::I32Const(7),
        WasmOp::I32Mul,
        WasmOp::End,
    ]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words[0], 0xD28000C0); // movz x0, #6
    assert_eq!(words[1], 0xD28000E1); // movz x1, #7
    assert_eq!(words[2], 0x9B017C00); // mul x0, x0, x1
    assert_eq!(words[3], 0xD65F03C0); // ret
}

#[test]
fn sdiv_basic() {
    let bytes = compile(&[
        WasmOp::I32Const(20),
        WasmOp::I32Const(4),
        WasmOp::I32DivS,
        WasmOp::End,
    ]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words[0], 0xD2800280); // movz x0, #20
    assert_eq!(words[1], 0xD2800081); // movz x1, #4
    assert_eq!(words[2], 0x9AC10C00); // sdiv x0, x0, x1
    assert_eq!(words[3], 0xD65F03C0); // ret
}

// ── Phase 4C: memory (I32Load / I32Store) ──────────────────────

#[test]
fn store_then_load_roundtrip() {
    // Builds: write 42 to mem[0], read it back, return it.
    //   i32.const 0    ; addr for store
    //   i32.const 42   ; value
    //   i32.store 0
    //   i32.const 0    ; addr for load
    //   i32.load 0
    //   end
    let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), 0xCAFEBABE).unwrap();
    lw.lower_all(&[
        WasmOp::I32Const(0),
        WasmOp::I32Const(42),
        WasmOp::I32Store(0),
        WasmOp::I32Const(0),
        WasmOp::I32Load(0),
        WasmOp::End,
    ])
    .unwrap();
    let words = bytes_as_u32s(&lw.as_bytes());
    // Prologue must contain STP (pre-indexed) as first word.
    assert_eq!(words[0] & 0xFFC0_0000, 0xA980_0000, "first word must be STP pre-indexed");
    // Must contain MOVZ X28, #0xBABE somewhere in the prologue.
    assert!(words.iter().any(|&w| w == 0xD29757DC), "MOVZ X28, #0xBABE not found");
    // Must end with RET.
    assert_eq!(*words.last().unwrap(), 0xD65F03C0, "must end with RET");
}

#[test]
fn load_without_memory_rejected() {
    let mut lw = Lowerer::new();
    lw.lower_op(WasmOp::I32Const(0)).unwrap();
    assert_eq!(
        lw.lower_op(WasmOp::I32Load(0)),
        Err(LowerError::MemoryNotConfigured)
    );
}

#[test]
fn store_without_memory_rejected() {
    let mut lw = Lowerer::new();
    lw.lower_op(WasmOp::I32Const(0)).unwrap();
    lw.lower_op(WasmOp::I32Const(0)).unwrap();
    assert_eq!(
        lw.lower_op(WasmOp::I32Store(0)),
        Err(LowerError::MemoryNotConfigured)
    );
}

#[test]
fn memory_with_deep_stack() {
    // Proves memory + spill work together: push 20 values (needs spill),
    // store/load from memory, all in one function.
    let mut lw = Lowerer::new_function_with_memory(0, Vec::new(), 0x1000).unwrap();
    for i in 0..20 {
        lw.lower_op(WasmOp::I32Const(i)).unwrap();
    }
    for _ in 0..19 {
        lw.lower_op(WasmOp::I32Add).unwrap();
    }
    // Store result to memory, load it back
    lw.lower_op(WasmOp::I32Const(0)).unwrap(); // addr
    lw.lower_op(WasmOp::I32Store(0)).unwrap();
    lw.lower_op(WasmOp::I32Const(0)).unwrap();
    lw.lower_op(WasmOp::I32Load(0)).unwrap();
    lw.lower_op(WasmOp::End).unwrap();
    let _bytes = lw.finish();
}

#[test]
fn memory_typed_with_fp_locals() {
    // new_function_with_memory_typed: f32 locals + memory access
    let types = vec![ValType::F32; 4];
    let mut lw = Lowerer::new_function_with_memory_typed(&types, Vec::new(), 0x2000).unwrap();
    lw.lower_all(&[
        WasmOp::F32Const(3.14),
        WasmOp::LocalSet(0),
        WasmOp::I32Const(0),
        WasmOp::I32Load(0),
        WasmOp::End,
    ]).unwrap();
    let _bytes = lw.finish();
}

// ── Phase 5: comparisons ────────────────────────────────────────

#[test]
fn eq_basic() {
    // i32.const 5 ; i32.const 5 ; i32.eq ; end  →  1
    let bytes = compile(&[
        WasmOp::I32Const(5),
        WasmOp::I32Const(5),
        WasmOp::I32Eq,
        WasmOp::End,
    ]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words[0], 0xD28000A0); // movz x0, #5
    assert_eq!(words[1], 0xD28000A1); // movz x1, #5
    assert_eq!(words[2], 0x6B01001F); // cmp w0, w1
    assert_eq!(words[3], 0x9A9F17E0); // cset x0, eq
    assert_eq!(words[4], 0xD65F03C0); // ret
}

#[test]
fn lt_s_basic() {
    let bytes = compile(&[
        WasmOp::I32Const(3),
        WasmOp::I32Const(5),
        WasmOp::I32LtS,
        WasmOp::End,
    ]);
    let words = bytes_as_u32s(&bytes);
    // movz x0, #3 ; movz x1, #5 ; cmp w0, w1 ; cset x0, lt ; ret
    assert_eq!(words[3], 0x9A9FA7E0); // cset x0, lt
}

#[test]
fn gt_s_basic() {
    let bytes = compile(&[
        WasmOp::I32Const(5),
        WasmOp::I32Const(3),
        WasmOp::I32GtS,
        WasmOp::End,
    ]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words[3], 0x9A9FD7E0); // cset x0, gt
}

#[test]
fn ne_basic() {
    let bytes = compile(&[
        WasmOp::I32Const(5),
        WasmOp::I32Const(3),
        WasmOp::I32Ne,
        WasmOp::End,
    ]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words[3], 0x9A9F07E0); // cset x0, ne
}

#[test]
fn udiv_basic() {
    let bytes = compile(&[
        WasmOp::I32Const(100),
        WasmOp::I32Const(7),
        WasmOp::I32DivU,
        WasmOp::End,
    ]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words[2], 0x9AC10800); // udiv x0, x0, x1
}

// ── Phase 2.4: If / Else ────────────────────────────────────────

#[test]
fn if_without_else() {
    // (func (result i32) (local i32)
    //   local.get 0
    //   if
    //     i32.const 1
    //     local.set 0     ;; set local to 1 if cond was true
    //   end
    //   local.get 0
    //   end)
    let mut lw = Lowerer::new_with_locals(1).unwrap();
    lw.lower_all(&[
        WasmOp::LocalGet(0),
        WasmOp::If,
        WasmOp::I32Const(1),
        WasmOp::LocalSet(0),
        WasmOp::End, // close if (no else)
        WasmOp::LocalGet(0),
        WasmOp::End, // function end
    ])
    .unwrap();
    let words = bytes_as_u32s(&lw.finish());
    assert_eq!(words[0], 0xD2800013); // movz x19, #0 (local init)
    assert_eq!(words[1], 0x8B1303E0); // add x0, xzr, x19 (local.get 0 → cond)
    // Word 2: cbz w0, +12 (3 instructions forward) → imm19=3, rt=0
    // Target is right after the two instructions in the then-branch.
    assert_eq!(words[2], 0x34000060);
    assert_eq!(words[3], 0xD2800020); // movz x0, #1
    assert_eq!(words[4], 0x8B0003F3); // add x19, xzr, x0 (local.set 0)
    assert_eq!(words[5], 0x8B1303E0); // add x0, xzr, x19 (local.get 0)
    assert_eq!(words[6], 0xD65F03C0); // ret
}

#[test]
fn if_else_two_branches() {
    // (func (result i32)
    //   i32.const 1
    //   if
    //     i32.const 10
    //   else
    //     i32.const 20
    //   end
    //   end)
    let bytes = compile(&[
        WasmOp::I32Const(1),
        WasmOp::If,
        WasmOp::I32Const(10),
        WasmOp::Else,
        WasmOp::I32Const(20),
        WasmOp::End, // close if/else
        WasmOp::End, // function end
    ]);
    let words = bytes_as_u32s(&bytes);
    assert_eq!(words[0], 0xD2800020); // movz x0, #1
    // Word 1: cbz w0, +12 (3 instr forward → else branch start)
    assert_eq!(words[1], 0x34000060);
    assert_eq!(words[2], 0xD2800140); // movz x0, #10 (then)
    // Word 3: b +8 (2 instr forward → past else)
    assert_eq!(words[3], 0x14000002);
    assert_eq!(words[4], 0xD2800280); // movz x0, #20 (else)
    assert_eq!(words[5], 0xD65F03C0); // ret
}

#[test]
fn else_without_if_rejected() {
    let mut lw = Lowerer::new();
    lw.lower_op(WasmOp::I32Const(1)).unwrap();
    assert_eq!(
        lw.lower_op(WasmOp::Else),
        Err(LowerError::ElseWithoutIf)
    );
}

// ── Phase 2.3: Call + function frame ───────────────────────────

#[test]
fn new_function_emits_prologue_and_epilogue() {
    let mut lw = Lowerer::new_function(0, Vec::new()).unwrap();
    lw.lower_all(&[WasmOp::I32Const(0), WasmOp::End]).unwrap();
    let words = bytes_as_u32s(&lw.finish_raw());
    // Prologue: STP X29, X30 (first word) + 5 STP pairs for
    // callee-saved (words 1-5) + MOVZ X0, #0 (word 6).
    // Epilogue: 5 LDP pairs (words 7-11) + LDP X29, X30 (word 12) + RET.
    // Just check first and last words:
    assert_eq!(words[0] & 0xFF00_0000, 0xA900_0000); // STP X pre-index
    assert_eq!(*words.last().unwrap(), 0xD65F03C0); // RET
}

#[test]
fn call_emits_movz_chain_blr_and_pushes_result() {
    // Verify call lowering produces valid code (BLR present,
    // RET at end). Exact byte offsets depend on prologue size
    // (which changes with callee-saved save count), so we
    // check for the presence of key instructions rather than
    // fixed offsets.
    let addr: u64 = 0x0000_1234_5678_ABCD;
    let mut lw = Lowerer::new_function(0, vec![addr]).unwrap();
    lw.lower_all(&[WasmOp::Call(0), WasmOp::End]).unwrap();
    let words = bytes_as_u32s(&lw.finish_raw());
    // BLR X16 must be somewhere in the output.
    assert!(words.contains(&0xD63F0200), "BLR X16 not found");
    // RET must be the last word.
    assert_eq!(*words.last().unwrap(), 0xD65F03C0, "last word must be RET");
}

#[test]
fn call_rejects_missing_target() {
    let mut lw = Lowerer::new_function(0, Vec::new()).unwrap();
    assert_eq!(
        lw.lower_op(WasmOp::Call(5)),
        Err(LowerError::CallTargetMissing)
    );
}

#[test]
fn call_high_addresses_emit_all_movk() {
    // Address with every halfword non-zero — check MOVZ + 3×MOVK
    // are present in the output (exact position depends on
    // prologue size).
    let addr: u64 = 0xDEAD_BEEF_CAFE_BABE;
    let mut lw = Lowerer::new_function(0, vec![addr]).unwrap();
    lw.lower_op(WasmOp::Call(0)).unwrap();
    let words = bytes_as_u32s(lw.as_bytes());
    // Check that MOVZ X16 and 3 MOVKs are in the output.
    let movz_count = words.iter().filter(|&&w| w & 0xFF80_0000 == 0xD280_0000).count();
    let movk_count = words.iter().filter(|&&w| w & 0xFF80_0000 == 0xF280_0000).count();
    assert!(movz_count >= 1, "need at least 1 MOVZ");
    assert!(movk_count >= 3, "need 3 MOVKs for 4-halfword addr");
}
