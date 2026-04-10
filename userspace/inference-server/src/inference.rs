//! `InferenceEngine` — holds all per-process inference state.
//!
//! Phase B3 strategic improvement: `logits_buf` is **pre-allocated** at init
//! via dedicated mmap region (1 MB at LOGITS_BUF_VADDR). The hot-path sampler
//! reuses it instead of bumping the BumpArena per token, eliminating ~712
//! `vocab_size × 4` allocations per request.

use libtensor::arena::BumpArena;
use libtensor::kv_cache::KvCacheManager;
use libtensor::transformer::{LayerWeights, ModelConfig, ModelWeights};

use crate::weights::{LayerDataVec, WeightsData};

/// Holds all state needed for inference across IPC requests.
///
/// All `&[u8]` references point into the mmap'd model data which lives
/// for the entire process lifetime, so we use 'static.
pub struct InferenceEngine {
    pub config: ModelConfig,
    pub weights_data: WeightsData,
    pub layer_data: LayerDataVec,
    pub kv_cache: KvCacheManager,
    /// Raw GGUF data for tokenizer reconstruction
    pub model_data: &'static [u8],
    pub vocab_offset: usize,
    pub vocab_size: usize,
    pub bos_id: u32,
    pub eos_id: u32,
    pub merges_offset: usize,
    pub merges_count: usize,
    pub unknown_token_id: u32,
    pub token_type_offset: usize,
    /// ULTRA 39: ChatML stop token IDs (u32::MAX = not found)
    pub im_end_id: u32,
    pub im_start_id: u32,
    /// ULTRA 42: Reentrancy guard — true while generating
    pub is_generating: bool,
    /// Phase B3: Pre-allocated logits work buffer (vocab_size × f32).
    /// Lives at LOGITS_BUF_VADDR for the entire process lifetime.
    /// Sampler borrows this instead of allocating from BumpArena per token.
    pub logits_buf: &'static mut [f32],
}

/// Build ModelWeights + LayerWeights for a single forward pass.
///
/// LayerWeights contain &[f32] references for norm weights, which requires
/// casting from the raw &[u8] GGUF data.
pub fn build_weights_for_forward<'a>(
    eng: &InferenceEngine,
    arena: &'a BumpArena,
) -> Option<(ModelWeights<'a>, &'a [LayerWeights<'a>])> {
    let config = &eng.config;
    let n_layers = config.n_layers;

    // Allocate LayerWeights array in arena
    let layer_weights = arena.alloc_slice::<LayerWeights>(n_layers)?;

    for i in 0..n_layers {
        let ld = eng.layer_data.get(i);

        layer_weights[i] = LayerWeights {
            attn_norm: bytes_as_f32(ld.attn_norm),
            wq: ld.wq,
            wk: ld.wk,
            wv: ld.wv,
            q_norm: bytes_as_f32(ld.q_norm),
            k_norm: bytes_as_f32(ld.k_norm),
            wo: ld.wo,
            ffn_norm: bytes_as_f32(ld.ffn_norm),
            w_gate: ld.w_gate,
            w_up: ld.w_up,
            w_down: ld.w_down,
            w_down_quant: ld.w_down_quant,
        };
    }

    let weights = ModelWeights {
        token_embed: eng.weights_data.token_embed,
        layers: layer_weights,
        final_norm: bytes_as_f32(eng.weights_data.final_norm),
        output_weight: eng.weights_data.output_weight,
        output_quant: eng.weights_data.output_quant,
    };

    Some((weights, layer_weights))
}

/// Cast a &[u8] slice to &[f32] (GGUF guarantees alignment for F32 tensors).
#[inline]
pub fn bytes_as_f32(data: &[u8]) -> &[f32] {
    let ptr = data.as_ptr() as *const f32;
    let len = data.len() / 4;
    unsafe { core::slice::from_raw_parts(ptr, len) }
}
