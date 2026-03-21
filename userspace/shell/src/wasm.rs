//! M14: Minimal WASM Interpreter for Folkering OS
//!
//! Executes .wasm binaries loaded from VFS. Provides calculator logic
//! as a dynamically loaded module instead of hardcoded in Shell.
//!
//! Safety: All memory/stack access is bounds-checked. Instruction limit
//! prevents infinite loops. Any trap falls back to hardcoded logic.

use crate::AppState;

// === Constants ===
const WASM_MAX_CODE: usize = 4096;
const WASM_MEM_SIZE: usize = 65536; // 1 WASM page = 64KB
const WASM_STATE_BASE: usize = 1024; // AppState offset in linear memory
const WASM_STACK_SIZE: usize = 64;
const WASM_MAX_FUNCS: usize = 16;
const WASM_MAX_EXPORTS: usize = 8;
const WASM_MAX_FRAMES: usize = 8;
const WASM_MAX_LOCALS: usize = 16;
const WASM_MAX_INSTRUCTIONS: u32 = 10_000;
const WASM_MAX_LABELS: usize = 32;

// === Data Structures ===

#[derive(Copy, Clone)]
struct WasmFunc {
    type_param_count: u8,
    type_result_count: u8,
    code_offset: u16,
    code_len: u16,
    local_count: u8,
}

impl WasmFunc {
    const fn empty() -> Self {
        Self { type_param_count: 0, type_result_count: 0, code_offset: 0, code_len: 0, local_count: 0 }
    }
}

#[derive(Copy, Clone)]
struct WasmExport {
    name_hash: u32,
    func_idx: u8,
}

impl WasmExport {
    const fn empty() -> Self {
        Self { name_hash: 0, func_idx: 0 }
    }
}

#[derive(Copy, Clone)]
struct WasmFrame {
    return_pc: u16,
    return_code_end: u16,
    func_idx: u8,
    locals: [i64; WASM_MAX_LOCALS],
    local_count: u8,
    stack_base: u8,
    label_base: u8,
}

impl WasmFrame {
    const fn empty() -> Self {
        Self {
            return_pc: 0, return_code_end: 0, func_idx: 0,
            locals: [0i64; WASM_MAX_LOCALS], local_count: 0,
            stack_base: 0, label_base: 0,
        }
    }
}

/// Label entry for structured control flow (block/loop/if)
#[derive(Copy, Clone)]
struct Label {
    /// For loop: start of loop body. For block/if: position after matching end.
    target: u16,
    /// true = loop (br jumps to start), false = block/if (br jumps to end)
    is_loop: bool,
}

impl Label {
    const fn empty() -> Self {
        Self { target: 0, is_loop: false }
    }
}

struct WasmModule {
    funcs: [WasmFunc; WASM_MAX_FUNCS],
    func_count: u8,
    exports: [WasmExport; WASM_MAX_EXPORTS],
    export_count: u8,
    code: [u8; WASM_MAX_CODE],
    code_len: u16,
    memory: [u8; WASM_MEM_SIZE],
    stack: [i64; WASM_STACK_SIZE],
    sp: u8,
    frames: [WasmFrame; WASM_MAX_FRAMES],
    fp: u8,
    labels: [Label; WASM_MAX_LABELS],
    lp: u8, // label pointer (depth)
    loaded: bool,
    // Type section storage (param_count, result_count per type)
    types: [(u8, u8); WASM_MAX_FUNCS],
    type_count: u8,
}

static mut MODULE: WasmModule = WasmModule {
    funcs: [WasmFunc::empty(); WASM_MAX_FUNCS],
    func_count: 0,
    exports: [WasmExport::empty(); WASM_MAX_EXPORTS],
    export_count: 0,
    code: [0u8; WASM_MAX_CODE],
    code_len: 0,
    memory: [0u8; WASM_MEM_SIZE],
    stack: [0i64; WASM_STACK_SIZE],
    sp: 0,
    frames: [WasmFrame::empty(); WASM_MAX_FRAMES],
    fp: 0,
    labels: [Label::empty(); WASM_MAX_LABELS],
    lp: 0,
    loaded: false,
    types: [(0, 0); WASM_MAX_FUNCS],
    type_count: 0,
};

// === Public API ===

pub fn init() {
    unsafe {
        MODULE.loaded = false;
        MODULE.func_count = 0;
        MODULE.export_count = 0;
        MODULE.code_len = 0;
        MODULE.sp = 0;
        MODULE.fp = 0;
        MODULE.lp = 0;
        MODULE.type_count = 0;
    }
}

pub fn parse(data: &[u8]) -> bool {
    init();
    unsafe { wasm_parse(data) }
}

pub fn is_loaded() -> bool {
    unsafe { MODULE.loaded }
}

/// Copy AppState into WASM memory, call handle_event, copy state back.
pub fn call_handle_event(state: &mut AppState, action_id: u32) -> bool {
    if !is_loaded() {
        return false;
    }
    unsafe {
        let m = &mut MODULE.memory;
        // Copy state → WASM memory[1024..]
        m[1024..1032].copy_from_slice(&state.display.to_le_bytes());
        m[1032..1040].copy_from_slice(&state.accumulator.to_le_bytes());
        m[1040..1044].copy_from_slice(&(state.operator as i32).to_le_bytes());
        m[1044..1048].copy_from_slice(&(if state.fresh_digit { 1i32 } else { 0i32 }).to_le_bytes());

        let hash = fnv1a_hash(b"handle_event");
        if let Some(_) = wasm_call(hash, &[1024, action_id as i64]) {
            // Read back updated state
            state.display = i64::from_le_bytes([
                m[1024], m[1025], m[1026], m[1027], m[1028], m[1029], m[1030], m[1031],
            ]);
            state.accumulator = i64::from_le_bytes([
                m[1032], m[1033], m[1034], m[1035], m[1036], m[1037], m[1038], m[1039],
            ]);
            let op = i32::from_le_bytes([m[1040], m[1041], m[1042], m[1043]]);
            state.operator = op as u8;
            let fresh = i32::from_le_bytes([m[1044], m[1045], m[1046], m[1047]]);
            state.fresh_digit = fresh != 0;
            true
        } else {
            false
        }
    }
}

// === LEB128 Decoding ===

fn read_leb128_u32(data: &[u8], pos: &mut usize) -> Option<u32> {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    loop {
        if *pos >= data.len() {
            return None;
        }
        let byte = data[*pos];
        *pos += 1;
        result |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 35 {
            return None;
        }
    }
}

fn read_leb128_i32(data: &[u8], pos: &mut usize) -> Option<i32> {
    let mut result: i32 = 0;
    let mut shift: u32 = 0;
    loop {
        if *pos >= data.len() {
            return None;
        }
        let byte = data[*pos];
        *pos += 1;
        result |= ((byte & 0x7F) as i32) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            if shift < 32 && (byte & 0x40) != 0 {
                result |= !0i32 << shift;
            }
            return Some(result);
        }
        if shift >= 35 {
            return None;
        }
    }
}

fn read_leb128_i64(data: &[u8], pos: &mut usize) -> Option<i64> {
    let mut result: i64 = 0;
    let mut shift: u32 = 0;
    loop {
        if *pos >= data.len() {
            return None;
        }
        let byte = data[*pos];
        *pos += 1;
        result |= ((byte & 0x7F) as i64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            if shift < 64 && (byte & 0x40) != 0 {
                result |= !0i64 << shift;
            }
            return Some(result);
        }
        if shift >= 70 {
            return None;
        }
    }
}

// === FNV-1a Hash ===

fn fnv1a_hash(data: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for &b in data {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

// === Binary Parser ===

unsafe fn wasm_parse(data: &[u8]) -> bool {
    // Validate magic: \0asm
    if data.len() < 8 {
        return false;
    }
    if data[0] != 0x00 || data[1] != 0x61 || data[2] != 0x73 || data[3] != 0x6D {
        return false;
    }
    // Version 1
    if data[4] != 0x01 || data[5] != 0x00 || data[6] != 0x00 || data[7] != 0x00 {
        return false;
    }

    let mut pos = 8;

    while pos < data.len() {
        if pos >= data.len() {
            break;
        }
        let section_id = data[pos];
        pos += 1;

        let section_size = match read_leb128_u32(data, &mut pos) {
            Some(s) => s as usize,
            None => return false,
        };
        let section_end = pos + section_size;
        if section_end > data.len() {
            return false;
        }

        match section_id {
            1 => {
                // Type section
                if !parse_type_section(data, pos, section_end) {
                    return false;
                }
            }
            3 => {
                // Function section
                if !parse_function_section(data, pos, section_end) {
                    return false;
                }
            }
            5 => {
                // Memory section — acknowledge, we use fixed 64KB
            }
            7 => {
                // Export section
                if !parse_export_section(data, pos, section_end) {
                    return false;
                }
            }
            10 => {
                // Code section
                if !parse_code_section(data, pos, section_end) {
                    return false;
                }
            }
            _ => {
                // Skip unknown sections
            }
        }

        pos = section_end;
    }

    // Link function types
    for i in 0..MODULE.func_count as usize {
        let f = &mut MODULE.funcs[i];
        // type index was temporarily stored in type_param_count during function section parse
        let type_idx = f.type_param_count as usize;
        if type_idx < MODULE.type_count as usize {
            f.type_param_count = MODULE.types[type_idx].0;
            f.type_result_count = MODULE.types[type_idx].1;
        }
    }

    MODULE.loaded = true;
    true
}

unsafe fn parse_type_section(data: &[u8], mut pos: usize, end: usize) -> bool {
    let count = match read_leb128_u32(data, &mut pos) {
        Some(c) => c as usize,
        None => return false,
    };
    for i in 0..count {
        if pos >= end {
            return false;
        }
        if data[pos] != 0x60 {
            return false; // Must be func type
        }
        pos += 1;

        // Param count + types
        let param_count = match read_leb128_u32(data, &mut pos) {
            Some(c) => c,
            None => return false,
        };
        // Skip param type bytes
        for _ in 0..param_count {
            if pos >= end {
                return false;
            }
            pos += 1;
        }

        // Result count + types
        let result_count = match read_leb128_u32(data, &mut pos) {
            Some(c) => c,
            None => return false,
        };
        for _ in 0..result_count {
            if pos >= end {
                return false;
            }
            pos += 1;
        }

        if i < WASM_MAX_FUNCS {
            MODULE.types[i] = (param_count as u8, result_count as u8);
            MODULE.type_count = (i + 1) as u8;
        }
    }
    true
}

unsafe fn parse_function_section(data: &[u8], mut pos: usize, _end: usize) -> bool {
    let count = match read_leb128_u32(data, &mut pos) {
        Some(c) => c as usize,
        None => return false,
    };
    for i in 0..count {
        if i >= WASM_MAX_FUNCS {
            break;
        }
        let type_idx = match read_leb128_u32(data, &mut pos) {
            Some(t) => t,
            None => return false,
        };
        // Temporarily store type_idx in type_param_count; will be resolved after parse
        MODULE.funcs[i].type_param_count = type_idx as u8;
        MODULE.func_count = (i + 1) as u8;
    }
    true
}

unsafe fn parse_export_section(data: &[u8], mut pos: usize, end: usize) -> bool {
    let count = match read_leb128_u32(data, &mut pos) {
        Some(c) => c as usize,
        None => return false,
    };
    for _ in 0..count {
        // Name length + name
        let name_len = match read_leb128_u32(data, &mut pos) {
            Some(l) => l as usize,
            None => return false,
        };
        if pos + name_len > end {
            return false;
        }
        let name_hash = fnv1a_hash(&data[pos..pos + name_len]);
        pos += name_len;

        // Kind
        if pos >= end {
            return false;
        }
        let kind = data[pos];
        pos += 1;

        // Index
        let idx = match read_leb128_u32(data, &mut pos) {
            Some(i) => i,
            None => return false,
        };

        // Only store function exports
        if kind == 0 {
            let ei = MODULE.export_count as usize;
            if ei < WASM_MAX_EXPORTS {
                MODULE.exports[ei] = WasmExport { name_hash, func_idx: idx as u8 };
                MODULE.export_count += 1;
            }
        }
    }
    true
}

unsafe fn parse_code_section(data: &[u8], mut pos: usize, end: usize) -> bool {
    let count = match read_leb128_u32(data, &mut pos) {
        Some(c) => c as usize,
        None => return false,
    };
    for i in 0..count {
        if i >= MODULE.func_count as usize {
            break;
        }
        // Body size
        let body_size = match read_leb128_u32(data, &mut pos) {
            Some(s) => s as usize,
            None => return false,
        };
        let body_end = pos + body_size;
        if body_end > end {
            return false;
        }

        // Local declarations
        let local_decl_count = match read_leb128_u32(data, &mut pos) {
            Some(c) => c as usize,
            None => return false,
        };
        let mut total_locals: u32 = 0;
        for _ in 0..local_decl_count {
            let n = match read_leb128_u32(data, &mut pos) {
                Some(n) => n,
                None => return false,
            };
            // Skip type byte
            if pos >= body_end {
                return false;
            }
            pos += 1;
            total_locals += n;
        }

        // Remaining bytes are the bytecode (excluding the trailing 0x0B end)
        let code_start = pos;
        let code_bytes = body_end - code_start;

        // Copy bytecode into our code buffer
        let offset = MODULE.code_len as usize;
        if offset + code_bytes > WASM_MAX_CODE {
            return false;
        }
        MODULE.code[offset..offset + code_bytes].copy_from_slice(&data[code_start..code_start + code_bytes]);

        MODULE.funcs[i].code_offset = offset as u16;
        MODULE.funcs[i].code_len = code_bytes as u16;
        MODULE.funcs[i].local_count = total_locals as u8;
        MODULE.code_len = (offset + code_bytes) as u16;

        pos = body_end;
    }
    true
}

// === Execution Engine ===

unsafe fn wasm_call(export_hash: u32, args: &[i64]) -> Option<i64> {
    // Find export
    let mut func_idx: Option<u8> = None;
    for i in 0..MODULE.export_count as usize {
        if MODULE.exports[i].name_hash == export_hash {
            func_idx = Some(MODULE.exports[i].func_idx);
            break;
        }
    }
    let fi = func_idx? as usize;
    if fi >= MODULE.func_count as usize {
        return None;
    }

    // Reset execution state
    MODULE.sp = 0;
    MODULE.fp = 0;
    MODULE.lp = 0;

    // Setup initial frame
    let func = MODULE.funcs[fi];
    let frame = &mut MODULE.frames[0];
    frame.return_pc = 0;
    frame.return_code_end = 0;
    frame.func_idx = fi as u8;
    frame.stack_base = 0;
    frame.label_base = 0;
    frame.local_count = func.type_param_count + func.local_count;
    if frame.local_count as usize > WASM_MAX_LOCALS {
        return None;
    }
    // Zero locals
    for j in 0..WASM_MAX_LOCALS {
        frame.locals[j] = 0;
    }
    // Copy args to locals
    for (j, &arg) in args.iter().enumerate() {
        if j >= frame.local_count as usize {
            break;
        }
        frame.locals[j] = arg;
    }
    MODULE.fp = 1;

    execute(func.code_offset, func.code_offset + func.code_len, func.type_result_count > 0)
}

unsafe fn push(val: i64) -> bool {
    if (MODULE.sp as usize) >= WASM_STACK_SIZE {
        return false;
    }
    MODULE.stack[MODULE.sp as usize] = val;
    MODULE.sp += 1;
    true
}

unsafe fn pop() -> Option<i64> {
    let frame_base = if MODULE.fp > 0 {
        MODULE.frames[MODULE.fp as usize - 1].stack_base
    } else {
        0
    };
    if MODULE.sp <= frame_base {
        return None;
    }
    MODULE.sp -= 1;
    Some(MODULE.stack[MODULE.sp as usize])
}

unsafe fn peek() -> Option<i64> {
    let frame_base = if MODULE.fp > 0 {
        MODULE.frames[MODULE.fp as usize - 1].stack_base
    } else {
        0
    };
    if MODULE.sp <= frame_base {
        return None;
    }
    Some(MODULE.stack[MODULE.sp as usize - 1])
}

unsafe fn current_frame() -> &'static mut WasmFrame {
    &mut MODULE.frames[MODULE.fp as usize - 1]
}

unsafe fn push_label(target: u16, is_loop: bool) -> bool {
    if (MODULE.lp as usize) >= WASM_MAX_LABELS {
        return false;
    }
    MODULE.labels[MODULE.lp as usize] = Label { target, is_loop };
    MODULE.lp += 1;
    true
}

unsafe fn pop_label() -> Option<Label> {
    let frame_label_base = if MODULE.fp > 0 {
        MODULE.frames[MODULE.fp as usize - 1].label_base
    } else {
        0
    };
    if MODULE.lp <= frame_label_base {
        return None;
    }
    MODULE.lp -= 1;
    Some(MODULE.labels[MODULE.lp as usize])
}

/// Find the end position of a block/if/loop by forward-scanning for matching 'end'
unsafe fn find_end(code: &[u8], start: usize, code_end: usize) -> Option<usize> {
    let mut depth: u32 = 1;
    let mut p = start;
    while p < code_end {
        let op = code[p];
        p += 1;
        match op {
            0x02 | 0x03 | 0x04 => {
                // block, loop, if — increase depth
                depth += 1;
                // Skip block type byte
                if p < code_end {
                    // Block type: 0x40 = void, or a valtype
                    p += 1;
                }
            }
            0x0B => {
                // end
                depth -= 1;
                if depth == 0 {
                    return Some(p);
                }
            }
            0x05 => {
                // else — at depth 1 this is our else
                if depth == 1 {
                    return Some(p);
                }
            }
            // Skip operands for opcodes that have them
            0x0C | 0x0D => {
                // br / br_if — LEB128 label index
                let _ = read_leb128_u32(code, &mut p);
            }
            0x10 => {
                // call — LEB128 func index
                let _ = read_leb128_u32(code, &mut p);
            }
            0x20 | 0x21 | 0x22 => {
                // local.get/set/tee — LEB128 local index
                let _ = read_leb128_u32(code, &mut p);
            }
            0x28 | 0x29 => {
                // i32.load / i64.load — align + offset
                let _ = read_leb128_u32(code, &mut p);
                let _ = read_leb128_u32(code, &mut p);
            }
            0x36 | 0x37 => {
                // i32.store / i64.store — align + offset
                let _ = read_leb128_u32(code, &mut p);
                let _ = read_leb128_u32(code, &mut p);
            }
            0x41 => {
                // i32.const — LEB128 i32
                let _ = read_leb128_i32(code, &mut p);
            }
            0x42 => {
                // i64.const — LEB128 i64
                let _ = read_leb128_i64(code, &mut p);
            }
            _ => {
                // Most opcodes have no operands
            }
        }
    }
    None
}

/// Find the else clause position within an if block
unsafe fn find_else(code: &[u8], start: usize, code_end: usize) -> Option<usize> {
    let mut depth: u32 = 1;
    let mut p = start;
    while p < code_end {
        let op = code[p];
        p += 1;
        match op {
            0x02 | 0x03 | 0x04 => {
                depth += 1;
                if p < code_end {
                    p += 1;
                }
            }
            0x0B => {
                depth -= 1;
                if depth == 0 {
                    return None; // Hit end before else
                }
            }
            0x05 => {
                if depth == 1 {
                    return Some(p); // Found else at our depth
                }
            }
            0x0C | 0x0D | 0x10 | 0x20 | 0x21 | 0x22 => {
                let _ = read_leb128_u32(code, &mut p);
            }
            0x28 | 0x29 | 0x36 | 0x37 => {
                let _ = read_leb128_u32(code, &mut p);
                let _ = read_leb128_u32(code, &mut p);
            }
            0x41 => { let _ = read_leb128_i32(code, &mut p); }
            0x42 => { let _ = read_leb128_i64(code, &mut p); }
            _ => {}
        }
    }
    None
}

unsafe fn execute(code_start: u16, code_end: u16, has_result: bool) -> Option<i64> {
    let mut pc = code_start as usize;
    let end = code_end as usize;
    let mut ic: u32 = 0;

    while pc < end {
        ic += 1;
        if ic > WASM_MAX_INSTRUCTIONS {
            return None; // Instruction limit trap
        }

        if pc >= WASM_MAX_CODE {
            return None;
        }
        let opcode = MODULE.code[pc];
        pc += 1;

        match opcode {
            // === Control ===
            0x00 => {
                // unreachable
                return None;
            }
            0x01 => {
                // nop
            }
            0x02 => {
                // block — blocktype
                if pc >= end { return None; }
                let _block_type = MODULE.code[pc];
                pc += 1;
                // Find the end position for this block
                let end_pos = match find_end(&MODULE.code, pc, end) {
                    Some(p) => p,
                    None => return None,
                };
                // Push label: br jumps to end_pos (forward)
                if !push_label(end_pos as u16, false) {
                    return None;
                }
            }
            0x03 => {
                // loop — blocktype
                if pc >= end { return None; }
                let _block_type = MODULE.code[pc];
                pc += 1;
                // Push label: br jumps back to pc (loop start)
                if !push_label(pc as u16, true) {
                    return None;
                }
            }
            0x04 => {
                // if — blocktype
                if pc >= end { return None; }
                let _block_type = MODULE.code[pc];
                pc += 1;
                let cond = pop()?;

                // Find end of this if block
                let end_pos = match find_end(&MODULE.code, pc, end) {
                    Some(p) => p,
                    None => return None,
                };

                if cond != 0 {
                    // Take the if branch — push label for br
                    if !push_label(end_pos as u16, false) {
                        return None;
                    }
                } else {
                    // Look for else clause
                    let else_pos = find_else(&MODULE.code, pc, end);
                    if let Some(ep) = else_pos {
                        // Jump to else body, find the real end for this label
                        // find_end from pc found either else or end; we need the actual end
                        // The end_pos we found might be the else position
                        // We need to re-scan from else_pos to find the final end
                        let real_end = match find_end(&MODULE.code, ep, end) {
                            Some(p) => p,
                            None => end_pos,
                        };
                        pc = ep;
                        if !push_label(real_end as u16, false) {
                            return None;
                        }
                    } else {
                        // No else — skip to end
                        pc = end_pos;
                    }
                }
            }
            0x05 => {
                // else — skip to end of if block (we were in the if-true branch)
                // Pop the if label, jump to its target (end)
                let label = pop_label()?;
                pc = label.target as usize;
            }
            0x0B => {
                // end — pop label if there is one
                let frame_label_base = if MODULE.fp > 0 {
                    MODULE.frames[MODULE.fp as usize - 1].label_base
                } else {
                    0
                };
                if MODULE.lp > frame_label_base {
                    MODULE.lp -= 1;
                } else {
                    // Function end — return
                    if MODULE.fp <= 1 {
                        // Top-level function return
                        if has_result && MODULE.sp > 0 {
                            return Some(MODULE.stack[MODULE.sp as usize - 1]);
                        }
                        return Some(0);
                    }
                    // Return from called function
                    let result = if MODULE.funcs[current_frame().func_idx as usize].type_result_count > 0 {
                        pop()
                    } else {
                        None
                    };
                    MODULE.fp -= 1;
                    let ret_frame = &MODULE.frames[MODULE.fp as usize];
                    pc = ret_frame.return_pc as usize;
                    let new_end = ret_frame.return_code_end as usize;
                    MODULE.sp = ret_frame.stack_base;
                    MODULE.lp = ret_frame.label_base;
                    if let Some(r) = result {
                        if !push(r) { return None; }
                    }
                    // Continue execution in caller (use the caller's code_end)
                    // Actually, we return to the outer execute context
                    // For simplicity in M14, we handle this by updating end
                    let _ = new_end;
                }
            }
            0x0C => {
                // br label_idx
                let label_idx = read_leb128_u32(&MODULE.code, &mut pc)? as usize;
                let frame_label_base = current_frame().label_base as usize;
                let target_lp = MODULE.lp as usize;
                if label_idx >= target_lp - frame_label_base {
                    return None;
                }
                let label = MODULE.labels[target_lp - 1 - label_idx];
                // Pop labels up to and including the target
                MODULE.lp = (target_lp - 1 - label_idx) as u8;
                if label.is_loop {
                    // Re-push loop label for next iteration
                    if !push_label(label.target, true) {
                        return None;
                    }
                }
                pc = label.target as usize;
            }
            0x0D => {
                // br_if label_idx
                let label_idx = read_leb128_u32(&MODULE.code, &mut pc)? as usize;
                let cond = pop()?;
                if cond != 0 {
                    let frame_label_base = current_frame().label_base as usize;
                    let target_lp = MODULE.lp as usize;
                    if label_idx >= target_lp - frame_label_base {
                        return None;
                    }
                    let label = MODULE.labels[target_lp - 1 - label_idx];
                    MODULE.lp = (target_lp - 1 - label_idx) as u8;
                    if label.is_loop {
                        if !push_label(label.target, true) {
                            return None;
                        }
                    }
                    pc = label.target as usize;
                }
            }
            0x0F => {
                // return
                if MODULE.fp <= 1 {
                    if has_result && MODULE.sp > 0 {
                        return Some(MODULE.stack[MODULE.sp as usize - 1]);
                    }
                    return Some(0);
                }
                let result = if MODULE.funcs[current_frame().func_idx as usize].type_result_count > 0 {
                    pop()
                } else {
                    None
                };
                MODULE.fp -= 1;
                let ret_frame = &MODULE.frames[MODULE.fp as usize];
                pc = ret_frame.return_pc as usize;
                MODULE.sp = ret_frame.stack_base;
                MODULE.lp = ret_frame.label_base;
                if let Some(r) = result {
                    if !push(r) { return None; }
                }
            }
            0x10 => {
                // call func_idx
                let fi = read_leb128_u32(&MODULE.code, &mut pc)? as usize;
                if fi >= MODULE.func_count as usize {
                    return None;
                }
                let func = MODULE.funcs[fi];
                let param_count = func.type_param_count as usize;

                // Save return info
                if MODULE.fp as usize >= WASM_MAX_FRAMES {
                    return None;
                }

                // Pop arguments from stack
                let mut args = [0i64; WASM_MAX_LOCALS];
                let total_locals = param_count + func.local_count as usize;
                if total_locals > WASM_MAX_LOCALS {
                    return None;
                }
                // Pop args in reverse order
                for j in (0..param_count).rev() {
                    args[j] = pop()?;
                }

                // Push new frame
                let new_frame = &mut MODULE.frames[MODULE.fp as usize];
                new_frame.return_pc = pc as u16;
                new_frame.return_code_end = end as u16;
                new_frame.func_idx = fi as u8;
                new_frame.stack_base = MODULE.sp;
                new_frame.label_base = MODULE.lp;
                new_frame.local_count = total_locals as u8;
                new_frame.locals = [0i64; WASM_MAX_LOCALS];
                for j in 0..param_count {
                    new_frame.locals[j] = args[j];
                }
                MODULE.fp += 1;

                // Jump to callee
                pc = func.code_offset as usize;
                // Note: end stays as the outer end, but the callee's end opcode will handle return
            }
            0x1A => {
                // drop
                pop()?;
            }

            // === Locals ===
            0x20 => {
                // local.get
                let idx = read_leb128_u32(&MODULE.code, &mut pc)? as usize;
                if MODULE.fp == 0 { return None; }
                let frame = current_frame();
                if idx >= frame.local_count as usize { return None; }
                let val = frame.locals[idx];
                if !push(val) { return None; }
            }
            0x21 => {
                // local.set
                let idx = read_leb128_u32(&MODULE.code, &mut pc)? as usize;
                let val = pop()?;
                if MODULE.fp == 0 { return None; }
                let frame = current_frame();
                if idx >= frame.local_count as usize { return None; }
                frame.locals[idx] = val;
            }
            0x22 => {
                // local.tee
                let idx = read_leb128_u32(&MODULE.code, &mut pc)? as usize;
                let val = peek()?;
                if MODULE.fp == 0 { return None; }
                let frame = current_frame();
                if idx >= frame.local_count as usize { return None; }
                frame.locals[idx] = val;
            }

            // === Memory ===
            0x28 => {
                // i32.load
                let _align = read_leb128_u32(&MODULE.code, &mut pc)?;
                let offset = read_leb128_u32(&MODULE.code, &mut pc)? as usize;
                let base = pop()? as usize;
                let addr = base + offset;
                if addr + 4 > WASM_MEM_SIZE { return None; }
                let val = i32::from_le_bytes([
                    MODULE.memory[addr], MODULE.memory[addr+1],
                    MODULE.memory[addr+2], MODULE.memory[addr+3],
                ]);
                if !push(val as i64) { return None; }
            }
            0x29 => {
                // i64.load
                let _align = read_leb128_u32(&MODULE.code, &mut pc)?;
                let offset = read_leb128_u32(&MODULE.code, &mut pc)? as usize;
                let base = pop()? as usize;
                let addr = base + offset;
                if addr + 8 > WASM_MEM_SIZE { return None; }
                let val = i64::from_le_bytes([
                    MODULE.memory[addr], MODULE.memory[addr+1],
                    MODULE.memory[addr+2], MODULE.memory[addr+3],
                    MODULE.memory[addr+4], MODULE.memory[addr+5],
                    MODULE.memory[addr+6], MODULE.memory[addr+7],
                ]);
                if !push(val) { return None; }
            }
            0x36 => {
                // i32.store
                let _align = read_leb128_u32(&MODULE.code, &mut pc)?;
                let offset = read_leb128_u32(&MODULE.code, &mut pc)? as usize;
                let val = pop()? as i32;
                let base = pop()? as usize;
                let addr = base + offset;
                if addr + 4 > WASM_MEM_SIZE { return None; }
                let bytes = val.to_le_bytes();
                MODULE.memory[addr] = bytes[0];
                MODULE.memory[addr+1] = bytes[1];
                MODULE.memory[addr+2] = bytes[2];
                MODULE.memory[addr+3] = bytes[3];
            }
            0x37 => {
                // i64.store
                let _align = read_leb128_u32(&MODULE.code, &mut pc)?;
                let offset = read_leb128_u32(&MODULE.code, &mut pc)? as usize;
                let val = pop()?;
                let base = pop()? as usize;
                let addr = base + offset;
                if addr + 8 > WASM_MEM_SIZE { return None; }
                let bytes = val.to_le_bytes();
                for i in 0..8 {
                    MODULE.memory[addr + i] = bytes[i];
                }
            }

            // === Constants ===
            0x41 => {
                // i32.const
                let val = read_leb128_i32(&MODULE.code, &mut pc)?;
                if !push(val as i64) { return None; }
            }
            0x42 => {
                // i64.const
                let val = read_leb128_i64(&MODULE.code, &mut pc)?;
                if !push(val) { return None; }
            }

            // === i32 Comparisons ===
            0x45 => {
                // i32.eqz
                let a = pop()? as i32;
                if !push(if a == 0 { 1 } else { 0 }) { return None; }
            }
            0x46 => {
                // i32.eq
                let b = pop()? as i32;
                let a = pop()? as i32;
                if !push(if a == b { 1 } else { 0 }) { return None; }
            }
            0x47 => {
                // i32.ne
                let b = pop()? as i32;
                let a = pop()? as i32;
                if !push(if a != b { 1 } else { 0 }) { return None; }
            }
            0x48 => {
                // i32.lt_s
                let b = pop()? as i32;
                let a = pop()? as i32;
                if !push(if a < b { 1 } else { 0 }) { return None; }
            }
            0x49 => {
                // i32.lt_u
                let b = pop()? as u32;
                let a = pop()? as u32;
                if !push(if a < b { 1 } else { 0 }) { return None; }
            }
            0x4A => {
                // i32.gt_s
                let b = pop()? as i32;
                let a = pop()? as i32;
                if !push(if a > b { 1 } else { 0 }) { return None; }
            }
            0x4B => {
                // i32.gt_u
                let b = pop()? as u32;
                let a = pop()? as u32;
                if !push(if a > b { 1 } else { 0 }) { return None; }
            }
            0x4C => {
                // i32.le_s
                let b = pop()? as i32;
                let a = pop()? as i32;
                if !push(if a <= b { 1 } else { 0 }) { return None; }
            }
            0x4D => {
                // i32.le_u
                let b = pop()? as u32;
                let a = pop()? as u32;
                if !push(if a <= b { 1 } else { 0 }) { return None; }
            }
            0x4E => {
                // i32.ge_s
                let b = pop()? as i32;
                let a = pop()? as i32;
                if !push(if a >= b { 1 } else { 0 }) { return None; }
            }
            0x4F => {
                // i32.ge_u
                let b = pop()? as u32;
                let a = pop()? as u32;
                if !push(if a >= b { 1 } else { 0 }) { return None; }
            }

            // === i64 Comparisons ===
            0x50 => {
                // i64.eqz
                let a = pop()?;
                if !push(if a == 0 { 1 } else { 0 }) { return None; }
            }
            0x51 => {
                // i64.eq
                let b = pop()?;
                let a = pop()?;
                if !push(if a == b { 1 } else { 0 }) { return None; }
            }
            0x52 => {
                // i64.ne
                let b = pop()?;
                let a = pop()?;
                if !push(if a != b { 1 } else { 0 }) { return None; }
            }
            0x53 => {
                // i64.lt_s
                let b = pop()?;
                let a = pop()?;
                if !push(if a < b { 1 } else { 0 }) { return None; }
            }
            0x55 => {
                // i64.gt_s
                let b = pop()?;
                let a = pop()?;
                if !push(if a > b { 1 } else { 0 }) { return None; }
            }

            // === i32 Arithmetic ===
            0x6A => {
                // i32.add
                let b = pop()? as i32;
                let a = pop()? as i32;
                if !push(a.wrapping_add(b) as i64) { return None; }
            }
            0x6B => {
                // i32.sub
                let b = pop()? as i32;
                let a = pop()? as i32;
                if !push(a.wrapping_sub(b) as i64) { return None; }
            }
            0x6C => {
                // i32.mul
                let b = pop()? as i32;
                let a = pop()? as i32;
                if !push(a.wrapping_mul(b) as i64) { return None; }
            }
            0x6D => {
                // i32.div_s
                let b = pop()? as i32;
                let a = pop()? as i32;
                if b == 0 { return None; }
                if !push(a.wrapping_div(b) as i64) { return None; }
            }

            // === i64 Arithmetic ===
            0x7C => {
                // i64.add
                let b = pop()?;
                let a = pop()?;
                if !push(a.wrapping_add(b)) { return None; }
            }
            0x7D => {
                // i64.sub
                let b = pop()?;
                let a = pop()?;
                if !push(a.wrapping_sub(b)) { return None; }
            }
            0x7E => {
                // i64.mul
                let b = pop()?;
                let a = pop()?;
                if !push(a.wrapping_mul(b)) { return None; }
            }
            0x7F => {
                // i64.div_s
                let b = pop()?;
                let a = pop()?;
                if b == 0 { return None; }
                if !push(a.wrapping_div(b)) { return None; }
            }

            // === Type Conversions ===
            0xA7 => {
                // i32.wrap_i64
                let a = pop()?;
                if !push((a as i32) as i64) { return None; }
            }
            0xAC => {
                // i64.extend_i32_s
                let a = pop()? as i32;
                if !push(a as i64) { return None; }
            }
            0xAD => {
                // i64.extend_i32_u
                let a = pop()? as u32;
                if !push(a as i64) { return None; }
            }

            _ => {
                // Unknown opcode — trap
                return None;
            }
        }
    }

    // Fell off end of function
    if has_result && MODULE.sp > 0 {
        Some(MODULE.stack[MODULE.sp as usize - 1])
    } else {
        Some(0)
    }
}
