//! Multi-function module lowering — the two-pass linker.
//!
//! Takes a parsed WASM [`Module`](crate::Module) and produces ONE
//! contiguous AArch64 blob containing every function, with internal
//! `Call(idx)` sites patched to PC-relative BL offsets.
//!
//! The daemon mmaps one CODE region and calls it at offset 0; WASM
//! fn 0 is conventionally our entrypoint, so it lives at blob
//! offset 0.
//!
//! Algorithm:
//!   1. For each function, create a fresh [`Lowerer`] with module
//!      context (knows the other functions' signatures). Lower the
//!      body; internal calls emit placeholder BL #0 + record a
//!      relocation at the encoder-local byte offset.
//!   2. Concatenate every function's code into one blob, tracking
//!      each function's final offset. Adjust relocations from
//!      per-function offsets to blob offsets.
//!   3. Patch each relocation's BL instruction in the blob with the
//!      correct delta (`target_fn_offset - site_offset`).

use alloc::{vec, vec::Vec};

use super::*;
use crate::wasm_module::{Module, FunctionBody};

/// Output of [`compile_module`] — ready to ship as a CODE frame.
pub struct ModuleLayout {
    /// Combined AArch64 blob: all functions concatenated, relocations
    /// patched.
    pub code: Vec<u8>,
    /// Byte offset of each function's first instruction within `code`.
    /// `function_offsets[i]` is where `module.bodies[i]` starts.
    pub function_offsets: Vec<u32>,
    /// Where the daemon should jump to (= `function_offsets[0]`
    /// unless overridden). Zero for the simple case where fn 0 is
    /// the entrypoint.
    pub entrypoint_offset: u32,
}

fn valtype_from_byte(b: u8) -> Result<ValType, LowerError> {
    match b {
        0x7F => Ok(ValType::I32),
        0x7E => Ok(ValType::I64),
        0x7D => Ok(ValType::F32),
        0x7C => Ok(ValType::F64),
        _ => Err(LowerError::CallTypeUnsupported),
    }
}

/// Build a FnSig from the module's Type section using the declared
/// type index. Only i32/i64/f32/f64 params and 0-or-1 result are
/// supported in this phase; anything else is `CallTypeUnsupported`.
fn build_sig(module: &Module, fn_idx: u32) -> Result<FnSig, LowerError> {
    let type_idx = module
        .func_types
        .get(fn_idx as usize)
        .copied()
        .ok_or(LowerError::CallTargetMissing)?;
    let ty = module
        .types
        .get(type_idx as usize)
        .ok_or(LowerError::CallTargetMissing)?;
    let params: Result<Vec<ValType>, LowerError> =
        ty.params.iter().map(|&b| valtype_from_byte(b)).collect();
    let result = match ty.results.len() {
        0 => None,
        1 => Some(valtype_from_byte(ty.results[0])?),
        _ => return Err(LowerError::CallTypeUnsupported),
    };
    Ok(FnSig { params: params?, result })
}

/// Compile a whole module. The function at `entrypoint_fn_idx`
/// emits global-init code before its body runs; all other functions
/// skip that prologue (the globals are already set by the time
/// they're called). Rust's wasm linker typically places exported
/// functions last, so the right `entrypoint_fn_idx` is usually
/// looked up via the Export section rather than just `0`.
///
/// The daemon still jumps to offset 0 in the returned blob; we lay
/// the entrypoint out first so that's always correct.
///
/// `mem_base` is the daemon's linear-memory base (from HELLO).
/// `mem_size` is the daemon's linear-memory size.
pub fn compile_module(
    module: &Module,
    mem_base: u64,
    mem_size: u32,
    entrypoint_fn_idx: u32,
) -> Result<ModuleLayout, LowerError> {
    let n = module.bodies.len();
    let entry = entrypoint_fn_idx as usize;
    if entry >= n {
        return Err(LowerError::CallTargetMissing);
    }
    if module.bodies.is_empty() {
        return Err(LowerError::CallTargetMissing);
    }

    // Build per-function signatures from the Type section. Both the
    // internal-call path and the callee-side arg setup need these.
    let mut fn_sigs: Vec<FnSig> = Vec::with_capacity(n);
    for i in 0..n {
        fn_sigs.push(build_sig(module, i as u32)?);
    }

    // Layout order: entrypoint first (so it lands at blob offset 0,
    // where the daemon jumps), then the rest in their original
    // WASM order. We build a permutation `layout_order[blob_pos] =
    // original_fn_idx` and its inverse so internal-call relocations
    // can map target fn_idx → blob position.
    let mut layout_order: Vec<usize> = Vec::with_capacity(n);
    layout_order.push(entry);
    for i in 0..n {
        if i != entry { layout_order.push(i); }
    }
    let mut fn_idx_to_layout_pos: Vec<usize> = vec![0; n];
    for (pos, &orig) in layout_order.iter().enumerate() {
        fn_idx_to_layout_pos[orig] = pos;
    }

    // Global metadata is the same for every function in the module.
    let global_types: Vec<ValType> = module
        .globals
        .iter()
        .map(|g| valtype_from_byte(g.valtype))
        .collect::<Result<_, _>>()?;
    let global_mutable: Vec<bool> = module.globals.iter().map(|g| g.mutable).collect();
    let global_inits: Vec<[u8; 8]> = module.globals.iter().map(|g| g.init_bytes).collect();

    // ── Pass 1: lower each function into its own blob + relocations ──
    // Lower in LAYOUT ORDER so that function_offsets[pos] is the
    // blob offset of layout_order[pos].
    let mut per_fn_code: Vec<Vec<u8>> = Vec::with_capacity(n);
    let mut per_fn_relocations: Vec<Vec<(u32, u32)>> = Vec::with_capacity(n);

    for &orig_fn_idx in &layout_order {
        let body = &module.bodies[orig_fn_idx];
        let code = lower_one_function(
            body,
            &fn_sigs[orig_fn_idx],
            &fn_sigs,
            &global_types,
            &global_mutable,
            &global_inits,
            orig_fn_idx == entry,
            mem_base,
            mem_size,
        )?;
        per_fn_code.push(code.bytes);
        per_fn_relocations.push(code.relocations);
    }

    // ── Pass 2: stitch, adjust relocations to blob offsets ──
    // `function_offsets_by_orig[orig_fn_idx]` = blob offset of that
    // function in the stitched output.
    let mut function_offsets_by_orig: Vec<u32> = vec![0; n];
    let mut blob: Vec<u8> = Vec::new();
    let mut global_relocations: Vec<(u32, u32)> = Vec::new();

    for (pos, (code_bytes, relocs)) in
        per_fn_code.iter().zip(per_fn_relocations.iter()).enumerate()
    {
        let orig_fn_idx = layout_order[pos];
        let fn_start = blob.len() as u32;
        function_offsets_by_orig[orig_fn_idx] = fn_start;
        blob.extend_from_slice(code_bytes);
        for &(site, target) in relocs {
            global_relocations.push((fn_start + site, target));
        }
    }

    // ── Pass 3: patch every BL placeholder with the real PC-relative offset ──
    for (site, target_idx) in global_relocations {
        let target_off = function_offsets_by_orig[target_idx as usize];
        let delta = target_off as i64 - site as i64;
        if delta % 4 != 0 {
            return Err(LowerError::BranchOutOfRange);
        }
        let imm26 = delta >> 2;
        if !(-(1 << 25)..(1 << 25)).contains(&imm26) {
            return Err(LowerError::BranchOutOfRange);
        }
        let word: u32 = 0x9400_0000 | ((imm26 as u32) & 0x03FF_FFFF);
        let site = site as usize;
        blob[site..site + 4].copy_from_slice(&word.to_le_bytes());
    }

    Ok(ModuleLayout {
        code: blob,
        function_offsets: function_offsets_by_orig,
        entrypoint_offset: 0,
    })
}

struct LoweredFunction {
    bytes: Vec<u8>,
    relocations: Vec<(u32, u32)>,
}

// Private helper threading every per-function context through the
// pass-1 lowering. Bundling these into a struct would be a real
// refactor (callers compose them per function), so we accept the
// 9-arg signature behind an explicit allow rather than hide it.
#[allow(clippy::too_many_arguments)]
fn lower_one_function(
    body: &FunctionBody,
    own_sig: &FnSig,
    module_fn_sigs: &[FnSig],
    global_types: &[ValType],
    global_mutable: &[bool],
    global_inits: &[[u8; 8]],
    is_entrypoint: bool,
    mem_base: u64,
    mem_size: u32,
) -> Result<LoweredFunction, LowerError> {
    // WASM local numbering: indices 0..N-1 are PARAMETERS (from the
    // function's signature), indices N.. are declared locals (from
    // the Code section). Both banks share the same `LocalGet/Set`
    // opcode. The Lowerer allocates a register slot per local, so
    // we must combine params + declared locals into one vec.
    let mut all_locals: Vec<ValType> = Vec::with_capacity(
        own_sig.params.len() + body.local_types.len(),
    );
    all_locals.extend_from_slice(&own_sig.params);
    for &b in &body.local_types {
        all_locals.push(valtype_from_byte(b)?);
    }

    let mut lw = Lowerer::new_function_with_memory_typed(
        &all_locals,
        Vec::new(),
        mem_base,
    )?;
    lw.set_mem_size(mem_size);
    lw.set_globals(global_types.to_vec(), global_mutable.to_vec())?;
    lw.set_module_fn_sigs(module_fn_sigs.to_vec());

    if is_entrypoint && !global_inits.is_empty() {
        lw.emit_global_inits(global_inits)?;
    }

    // Copy incoming AAPCS64 argument registers (W0-W7 / S0-S7) into
    // the first N local slots that correspond to params. The Lowerer
    // zero-initialised them earlier — this overrides that with the
    // real arg values the caller passed.
    if !own_sig.params.is_empty() {
        lw.emit_param_rehydration(&own_sig.params)?;
    }

    lw.lower_all(&body.ops)?;
    let relocations = lw.take_relocations();
    let bytes = lw.finish();

    Ok(LoweredFunction { bytes, relocations })
}
