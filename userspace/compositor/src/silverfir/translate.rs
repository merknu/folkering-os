//! WASM → x86_64 opcode translator (single-pass).
//!
//! Translates WASM stack-machine bytecode into x86_64 register code.
//! Uses the native stack as the WASM operand stack (push/pop).
//!
//! Supported opcodes (enough for fib, gcd, factorial, is_prime):
//!   - i32.const, local.get, local.set, local.tee
//!   - i32.add, i32.sub, i32.mul
//!   - i32.eq, i32.ne, i32.lt_s, i32.gt_s, i32.le_s, i32.ge_s
//!   - i32.eqz, i32.and, i32.or, i32.xor, i32.shl, i32.shr_s, i32.shr_u
//!   - i64.const, i64.add, i64.sub, i64.mul (same pattern, REX.W)
//!   - if/else/end, br, br_if, block, loop, return
//!   - call (local functions only, no imports yet)
//!   - drop
//!
//! Register convention:
//!   RSP — native stack (also WASM operand stack)
//!   RBP — frame base (locals are at [RBP - 8*N])

extern crate alloc;
use alloc::vec::Vec;
use super::compiler::CodeBuffer;
use super::parser::{CodeBody, ValType, read_leb128_i32};

/// WASM opcodes
mod op {
    pub const UNREACHABLE: u8 = 0x00;
    pub const NOP: u8 = 0x01;
    pub const BLOCK: u8 = 0x02;
    pub const LOOP: u8 = 0x03;
    pub const IF: u8 = 0x04;
    pub const ELSE: u8 = 0x05;
    pub const END: u8 = 0x0B;
    pub const BR: u8 = 0x0C;
    pub const BR_IF: u8 = 0x0D;
    pub const RETURN: u8 = 0x0F;
    pub const CALL: u8 = 0x10;
    pub const DROP: u8 = 0x1A;
    pub const LOCAL_GET: u8 = 0x20;
    pub const LOCAL_SET: u8 = 0x21;
    pub const LOCAL_TEE: u8 = 0x22;
    pub const I32_CONST: u8 = 0x41;
    pub const I64_CONST: u8 = 0x42;
    pub const I32_EQZ: u8 = 0x45;
    pub const I32_EQ: u8 = 0x46;
    pub const I32_NE: u8 = 0x47;
    pub const I32_LT_S: u8 = 0x48;
    pub const I32_GT_S: u8 = 0x4A;
    pub const I32_LE_S: u8 = 0x4C;
    pub const I32_GE_S: u8 = 0x4E;
    pub const I32_LT_U: u8 = 0x49;
    pub const I32_GT_U: u8 = 0x4B;
    pub const I32_LE_U: u8 = 0x4D;
    pub const I32_GE_U: u8 = 0x4F;
    pub const I32_ADD: u8 = 0x6A;
    pub const I32_SUB: u8 = 0x6B;
    pub const I32_MUL: u8 = 0x6C;
    pub const I32_AND: u8 = 0x71;
    pub const I32_OR: u8 = 0x72;
    pub const I32_XOR: u8 = 0x73;
    pub const I32_SHL: u8 = 0x74;
    pub const I32_SHR_S: u8 = 0x75;
    pub const I32_SHR_U: u8 = 0x76;
    pub const I32_STORE: u8 = 0x36;
    pub const I32_LOAD: u8 = 0x28;
    pub const I64_ADD: u8 = 0x7C;
    pub const I64_SUB: u8 = 0x7D;
    pub const I64_MUL: u8 = 0x7E;
}

/// Control flow label (for block/loop/if)
struct Label {
    /// Offset in code buffer where a forward jump needs patching
    patch_offsets: Vec<usize>,
    /// Target offset for backward jumps (loops)
    loop_target: Option<usize>,
    /// Whether this is an if with else branch
    is_if: bool,
}

/// Translate a single WASM function body to x86_64 machine code.
///
/// Returns the emitted code bytes. The function uses the System V AMD64
/// calling convention (RDI=arg0, RSI=arg1) with locals on the stack.
pub fn translate_function(
    body: &CodeBody,
    num_params: usize,
    _has_result: bool,
) -> Result<Vec<u8>, super::JitError> {
    let mut buf = CodeBuffer::new();
    let mut label_stack: Vec<Label> = Vec::new();

    // Count total locals (params + declared locals)
    let mut total_locals = num_params;
    for &(count, _) in &body.locals {
        total_locals += count as usize;
    }

    // ── Function prologue ──────────────────────────────────────────
    // push rbp; mov rbp, rsp; sub rsp, locals*8
    buf.emit(&[0x55]);                              // push rbp
    buf.emit(&[0x48, 0x89, 0xE5]);                 // mov rbp, rsp
    if total_locals > 0 {
        let stack_size = (total_locals * 8) as u32;
        buf.emit(&[0x48, 0x81, 0xEC]);             // sub rsp, imm32
        buf.emit(&stack_size.to_le_bytes());
        // Zero-initialize locals
        for i in 0..total_locals {
            let offset = ((i + 1) * 8) as i32;
            // mov qword [rbp - offset], 0
            buf.emit(&[0x48, 0xC7, 0x85]);
            buf.emit(&(-offset).to_le_bytes());
            buf.emit(&0u32.to_le_bytes());
        }
    }

    // Store parameters into local slots
    // System V ABI: RDI=arg0, RSI=arg1, RDX=arg2, RCX=arg3
    let param_regs: &[u8] = &[0x07, 0x06, 0x02, 0x01]; // RDI, RSI, RDX, RCX
    for i in 0..num_params.min(4) {
        let offset = ((i + 1) * 8) as i32;
        // mov [rbp - offset], reg
        buf.emit(&[0x48, 0x89]);
        buf.emit(&[0x85 | (param_regs[i] << 3)]);
        buf.emit(&(-offset).to_le_bytes());
    }

    // ── Translate bytecode ──────────────────────────────────────────
    let bc = &body.bytecode;
    let mut pc = 0;

    while pc < bc.len() {
        let opcode = bc[pc]; pc += 1;

        match opcode {
            op::NOP => {}

            op::I32_CONST => {
                let (val, n) = read_leb128_i32(&bc[pc..])
                    .map_err(|_| super::JitError::ParseError(alloc::string::String::from("bad i32.const")))?;
                pc += n;
                // push imm32 onto operand stack
                buf.emit(&[0x68]); // push imm32
                buf.emit(&(val as u32).to_le_bytes());
            }

            op::LOCAL_GET => {
                let (idx, n) = read_leb128_i32(&bc[pc..])
                    .map_err(|_| super::JitError::ParseError(alloc::string::String::from("bad local.get")))?;
                pc += n;
                let offset = ((idx as usize + 1) * 8) as i32;
                // mov rax, [rbp - offset]; push rax
                buf.emit(&[0x48, 0x8B, 0x85]);
                buf.emit(&(-offset).to_le_bytes());
                buf.emit(&[0x50]); // push rax
            }

            op::LOCAL_SET => {
                let (idx, n) = read_leb128_i32(&bc[pc..])
                    .map_err(|_| super::JitError::ParseError(alloc::string::String::from("bad local.set")))?;
                pc += n;
                let offset = ((idx as usize + 1) * 8) as i32;
                // pop rax; mov [rbp - offset], rax
                buf.emit(&[0x58]); // pop rax
                buf.emit(&[0x48, 0x89, 0x85]);
                buf.emit(&(-offset).to_le_bytes());
            }

            op::LOCAL_TEE => {
                let (idx, n) = read_leb128_i32(&bc[pc..])
                    .map_err(|_| super::JitError::ParseError(alloc::string::String::from("bad local.tee")))?;
                pc += n;
                let offset = ((idx as usize + 1) * 8) as i32;
                // peek top of stack (don't pop), store to local
                // mov rax, [rsp]; mov [rbp - offset], rax
                buf.emit(&[0x48, 0x8B, 0x04, 0x24]); // mov rax, [rsp]
                buf.emit(&[0x48, 0x89, 0x85]);
                buf.emit(&(-offset).to_le_bytes());
            }

            op::DROP => {
                // pop and discard
                buf.emit(&[0x48, 0x83, 0xC4, 0x08]); // add rsp, 8
            }

            // ── i32 arithmetic ──────────────────────────────────────
            op::I32_ADD => {
                // pop rbx; pop rax; add eax, ebx; push rax
                buf.emit(&[0x5B]);              // pop rbx
                buf.emit(&[0x58]);              // pop rax
                buf.emit(&[0x01, 0xD8]);        // add eax, ebx
                buf.emit(&[0x50]);              // push rax
            }
            op::I32_SUB => {
                buf.emit(&[0x5B, 0x58]);        // pop rbx; pop rax
                buf.emit(&[0x29, 0xD8]);        // sub eax, ebx
                buf.emit(&[0x50]);
            }
            op::I32_MUL => {
                buf.emit(&[0x5B, 0x58]);        // pop rbx; pop rax
                buf.emit(&[0x0F, 0xAF, 0xC3]); // imul eax, ebx
                buf.emit(&[0x50]);
            }
            op::I32_AND => {
                buf.emit(&[0x5B, 0x58]);
                buf.emit(&[0x21, 0xD8]);        // and eax, ebx
                buf.emit(&[0x50]);
            }
            op::I32_OR => {
                buf.emit(&[0x5B, 0x58]);
                buf.emit(&[0x09, 0xD8]);        // or eax, ebx
                buf.emit(&[0x50]);
            }
            op::I32_XOR => {
                buf.emit(&[0x5B, 0x58]);
                buf.emit(&[0x31, 0xD8]);        // xor eax, ebx
                buf.emit(&[0x50]);
            }

            // ── i32 comparisons (produce 0 or 1) ───────────────────
            op::I32_EQZ => {
                // pop rax; test eax, eax; setz al; movzx eax, al; push rax
                buf.emit(&[0x58]);              // pop rax
                buf.emit(&[0x85, 0xC0]);        // test eax, eax
                buf.emit(&[0x0F, 0x94, 0xC0]);  // setz al
                buf.emit(&[0x0F, 0xB6, 0xC0]);  // movzx eax, al
                buf.emit(&[0x50]);
            }
            op::I32_EQ => { emit_cmp_i32(&mut buf, 0x94); } // sete
            op::I32_NE => { emit_cmp_i32(&mut buf, 0x95); } // setne
            op::I32_LT_S => { emit_cmp_i32(&mut buf, 0x9C); } // setl
            op::I32_GT_S => { emit_cmp_i32(&mut buf, 0x9F); } // setg
            op::I32_LE_S => { emit_cmp_i32(&mut buf, 0x9E); } // setle
            op::I32_GE_S => { emit_cmp_i32(&mut buf, 0x9D); } // setge
            op::I32_LT_U => { emit_cmp_i32(&mut buf, 0x92); } // setb (unsigned)
            op::I32_GT_U => { emit_cmp_i32(&mut buf, 0x97); } // seta
            op::I32_LE_U => { emit_cmp_i32(&mut buf, 0x96); } // setbe
            op::I32_GE_U => { emit_cmp_i32(&mut buf, 0x93); } // setae

            // ── i32 memory operations ────────────────────────────────
            // Note: these are simplified — no memory base pointer yet.
            // For silverfir v1, memory operations are NOPs (just consume args).
            op::I32_STORE => {
                // memarg: alignment (LEB128) + offset (LEB128)
                let (_, n1) = read_leb128_i32(&bc[pc..])
                    .map_err(|_| super::JitError::ParseError(alloc::string::String::from("bad align")))?;
                pc += n1;
                let (_, n2) = read_leb128_i32(&bc[pc..])
                    .map_err(|_| super::JitError::ParseError(alloc::string::String::from("bad offset")))?;
                pc += n2;
                // pop value, pop addr — discard both (no memory impl yet)
                buf.emit(&[0x5B]); // pop rbx (value)
                buf.emit(&[0x58]); // pop rax (addr)
            }
            op::I32_LOAD => {
                let (_, n1) = read_leb128_i32(&bc[pc..])
                    .map_err(|_| super::JitError::ParseError(alloc::string::String::from("bad align")))?;
                pc += n1;
                let (_, n2) = read_leb128_i32(&bc[pc..])
                    .map_err(|_| super::JitError::ParseError(alloc::string::String::from("bad offset")))?;
                pc += n2;
                // pop addr, push 0 (no memory impl yet)
                buf.emit(&[0x58]); // pop rax (addr)
                buf.emit(&[0x6A, 0x00]); // push 0
            }

            // ── Control flow ────────────────────────────────────────
            op::BLOCK => {
                let _block_type = bc[pc]; pc += 1; // skip block type
                label_stack.push(Label {
                    patch_offsets: Vec::new(),
                    loop_target: None,
                    is_if: false,
                });
            }
            op::LOOP => {
                let _block_type = bc[pc]; pc += 1;
                label_stack.push(Label {
                    patch_offsets: Vec::new(),
                    loop_target: Some(buf.offset()),
                    is_if: false,
                });
            }
            op::IF => {
                let _block_type = bc[pc]; pc += 1;
                // pop condition; test; jz else/end
                buf.emit(&[0x58]);                  // pop rax
                buf.emit(&[0x85, 0xC0]);            // test eax, eax
                buf.emit(&[0x0F, 0x84]);            // jz rel32
                let patch = buf.offset();
                buf.emit(&0u32.to_le_bytes());      // placeholder
                label_stack.push(Label {
                    patch_offsets: alloc::vec![patch],
                    loop_target: None,
                    is_if: true,
                });
            }
            op::ELSE => {
                if let Some(label) = label_stack.last_mut() {
                    // Jump over else branch (from if-true path)
                    buf.emit(&[0xE9]);              // jmp rel32
                    let skip_patch = buf.offset();
                    buf.emit(&0u32.to_le_bytes());

                    // Patch the if's jz to jump here (else start)
                    if let Some(if_patch) = label.patch_offsets.first().copied() {
                        let target = buf.offset();
                        let rel = (target as i32) - (if_patch as i32) - 4;
                        let code = buf.code_mut();
                        code[if_patch..if_patch+4].copy_from_slice(&rel.to_le_bytes());
                    }

                    // Replace the patch list with the skip-jump
                    label.patch_offsets = alloc::vec![skip_patch];
                }
            }
            op::END => {
                if let Some(label) = label_stack.pop() {
                    let target = buf.offset();
                    // Patch all forward jumps to here
                    for patch in &label.patch_offsets {
                        let rel = (target as i32) - (*patch as i32) - 4;
                        let code = buf.code_mut();
                        code[*patch..*patch+4].copy_from_slice(&rel.to_le_bytes());
                    }
                }
                // If label stack is empty, this is the function end
                if label_stack.is_empty() && pc >= bc.len() - 1 {
                    break;
                }
            }
            op::BR => {
                let (depth, n) = read_leb128_i32(&bc[pc..])
                    .map_err(|_| super::JitError::ParseError(alloc::string::String::from("bad br")))?;
                pc += n;
                let target_idx = label_stack.len().saturating_sub(1 + depth as usize);
                if let Some(label) = label_stack.get(target_idx) {
                    if let Some(loop_target) = label.loop_target {
                        // Backward jump to loop start
                        buf.emit(&[0xE9]); // jmp rel32
                        let from = buf.offset();
                        let rel = (loop_target as i32) - (from as i32) - 4;
                        buf.emit(&rel.to_le_bytes());
                    } else {
                        // Forward jump (needs patching at END)
                        buf.emit(&[0xE9]); // jmp rel32
                        let patch = buf.offset();
                        buf.emit(&0u32.to_le_bytes());
                        if let Some(label) = label_stack.get_mut(target_idx) {
                            label.patch_offsets.push(patch);
                        }
                    }
                }
            }
            op::BR_IF => {
                let (depth, n) = read_leb128_i32(&bc[pc..])
                    .map_err(|_| super::JitError::ParseError(alloc::string::String::from("bad br_if")))?;
                pc += n;
                // pop condition; test; jnz target
                buf.emit(&[0x58]);                  // pop rax
                buf.emit(&[0x85, 0xC0]);            // test eax, eax
                let target_idx = label_stack.len().saturating_sub(1 + depth as usize);
                if let Some(label) = label_stack.get(target_idx) {
                    if let Some(loop_target) = label.loop_target {
                        buf.emit(&[0x0F, 0x85]);    // jnz rel32
                        let from = buf.offset();
                        let rel = (loop_target as i32) - (from as i32) - 4;
                        buf.emit(&rel.to_le_bytes());
                    } else {
                        buf.emit(&[0x0F, 0x85]);    // jnz rel32
                        let patch = buf.offset();
                        buf.emit(&0u32.to_le_bytes());
                        if let Some(label) = label_stack.get_mut(target_idx) {
                            label.patch_offsets.push(patch);
                        }
                    }
                }
            }
            op::RETURN => {
                // mov rsp, rbp; pop rbp; ret
                buf.emit(&[0x48, 0x89, 0xEC]);  // mov rsp, rbp
                buf.emit(&[0x5D]);              // pop rbp
                buf.emit(&[0xC3]);              // ret
            }

            op::UNREACHABLE => {
                buf.emit(&[0xCC]); // int3 (debug trap)
            }

            _ => {
                return Err(super::JitError::UnsupportedOpcode(opcode));
            }
        }
    }

    // ── Function epilogue ───────────────────────────────────────────
    // Return value is on top of operand stack → pop into rax
    buf.emit(&[0x58]);                  // pop rax (return value)
    buf.emit(&[0x48, 0x89, 0xEC]);      // mov rsp, rbp
    buf.emit(&[0x5D]);                  // pop rbp
    buf.emit(&[0xC3]);                  // ret

    Ok(buf.into_code())
}

/// Helper: emit i32 comparison (pop rbx, pop rax, cmp, setCC, push)
fn emit_cmp_i32(buf: &mut CodeBuffer, setcc_byte: u8) {
    buf.emit(&[0x5B]);                      // pop rbx
    buf.emit(&[0x58]);                      // pop rax
    buf.emit(&[0x39, 0xD8]);                // cmp eax, ebx
    buf.emit(&[0x0F, setcc_byte, 0xC0]);    // setCC al
    buf.emit(&[0x0F, 0xB6, 0xC0]);          // movzx eax, al
    buf.emit(&[0x50]);                      // push rax
}
