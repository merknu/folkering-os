//! Transformer Forward Pass (M41)
//!
//! SmolLM-135M / LLaMA-style architecture:
//! - Pre-norm (RMSNorm before attention and FFN)
//! - Multi-head self-attention with KV-cache
//! - SiLU-gated FFN (gate_proj * SiLU(up_proj))
//! - RoPE positional encoding
//!
//! All intermediate buffers allocated from BumpArena.
//! Cooperative yield between GEMM operations.

use crate::arena::BumpArena;
use crate::gemm;
use crate::ops;
use crate::kv_cache::KvCacheManager;
use crate::quantize;
use crate::FuseOp;

/// Model configuration (parsed from GGUF metadata)
#[derive(Clone, Copy)]
pub struct ModelConfig {
    pub n_layers: usize,
    pub n_heads: usize,
    pub n_kv_heads: usize,
    pub embed_dim: usize,
    pub head_dim: usize,
    pub intermediate_size: usize,
    pub vocab_size: usize,
    pub max_seq_len: usize,
    pub rope_base: f32,
    pub rms_norm_eps: f32,
}

/// References to model weight tensors (zero-copy from GGUF mmap).
/// Each &[u8] points directly into the mmap'd model file.
pub struct ModelWeights<'a> {
    /// Token embedding table: [vocab_size × embed_dim] in Q4_0 or Q8_0
    pub token_embed: &'a [u8],

    /// Per-layer weights
    pub layers: &'a [LayerWeights<'a>],

    /// Final RMSNorm weight: [embed_dim] in F32
    pub final_norm: &'a [f32],

    /// Output projection (LM head): [vocab_size × embed_dim] in Q4_0 or Q8_0
    /// Often shares weights with token_embed (tied embeddings)
    pub output_weight: &'a [u8],

    /// True if output_weight is Q8_0 (otherwise Q4_0)
    pub output_is_q8: bool,
}

/// Weights for a single transformer layer
#[derive(Clone, Copy)]
pub struct LayerWeights<'a> {
    /// Attention input norm: [embed_dim] f32
    pub attn_norm: &'a [f32],

    /// Q/K/V projection: [embed_dim × (n_heads * head_dim)] Q4_0
    pub wq: &'a [u8],
    pub wk: &'a [u8],
    pub wv: &'a [u8],

    /// Attention output projection: [(n_heads * head_dim) × embed_dim] Q4_0
    pub wo: &'a [u8],

    /// FFN input norm: [embed_dim] f32
    pub ffn_norm: &'a [f32],

    /// FFN gate projection: [embed_dim × intermediate_size] Q4_0
    pub w_gate: &'a [u8],
    /// FFN up projection: [embed_dim × intermediate_size] Q4_0
    pub w_up: &'a [u8],
    /// FFN down projection: [intermediate_size × embed_dim] Q4_0
    pub w_down: &'a [u8],
}

/// Yield frequency configuration (ULTRA 3, 6)
#[derive(Clone, Copy)]
pub struct YieldConfig {
    /// Yield every N rows during GEMM (0 = never)
    pub gemm_yield: usize,
}

impl YieldConfig {
    /// Foreground inference: less yielding, faster tokens
    pub fn foreground() -> Self {
        Self { gemm_yield: 128 }
    }

    /// Background inference: frequent yielding, responsive GUI
    pub fn background() -> Self {
        Self { gemm_yield: 16 }
    }
}

/// Run one transformer forward pass for a single token position.
///
/// # Arguments
/// - `token_id`: input token index
/// - `pos`: current sequence position (for RoPE)
/// - `config`: model configuration
/// - `weights`: model weight tensors
/// - `kv_cache`: KV-cache manager (updated in-place)
/// - `arena`: bump arena for intermediates (reset by CALLER after use)
/// - `yield_cfg`: cooperative yield configuration
///
/// # Returns
/// Logits slice of [vocab_size] f32 values allocated from arena.
/// Caller must read logits before arena.reset().
pub fn forward<'a>(
    token_id: u32,
    pos: usize,
    config: &ModelConfig,
    weights: &ModelWeights,
    kv_cache: &mut KvCacheManager,
    arena: &'a BumpArena,
    yield_cfg: &YieldConfig,
) -> Option<&'a [f32]> {
    let dim = config.embed_dim;
    let head_dim = config.head_dim;
    let n_heads = config.n_heads;
    let n_kv_heads = config.n_kv_heads;
    let kv_dim = n_kv_heads * head_dim;

    // Allocate working buffers from arena
    let x = arena.alloc_f32(dim)?;           // current hidden state
    let xb = arena.alloc_f32(dim)?;          // after RMSNorm
    let q = arena.alloc_f32(n_heads * head_dim)?;    // queries
    let k = arena.alloc_f32(kv_dim)?;                // keys
    let v = arena.alloc_f32(kv_dim)?;                // values
    let attn_out = arena.alloc_f32(dim)?;             // attention output
    let ffn_buf1 = arena.alloc_f32(config.intermediate_size)?; // gate
    let ffn_buf2 = arena.alloc_f32(config.intermediate_size)?; // up
    let xb2 = arena.alloc_f32(dim)?;         // after FFN norm
    let logits = arena.alloc_f32(config.vocab_size)?; // output logits

    // Attention scores buffer (per-head, reused across heads)
    let max_ctx = kv_cache.layer(0).active_length() + 1;
    let att = arena.alloc_f32(max_ctx)?;

    // === Token embedding lookup ===
    // Dequantize the embedding row for token_id from Q4_0
    embed_token(token_id as usize, config.vocab_size, dim, weights.token_embed, x);

    // DEBUG: Check embedding for NaN
    {
        let mut nan = 0u32;
        let mut zero = 0u32;
        for i in 0..dim {
            if x[i].is_nan() { nan += 1; }
            if x[i] == 0.0 { zero += 1; }
        }
        if nan > 0 || pos == 0 {
            libfolk::println!("[FWD] tok={} pos={} embed: NaN={} Zero={}/{} x[0]={:.6} x[1]={:.6}",
                token_id, pos, nan, zero, dim, x[0], x[1]);
        }
    }

    // === Transformer layers ===
    for layer in 0..config.n_layers {
        let lw = &weights.layers[layer];

        // --- Attention sublayer ---
        // 1. RMSNorm
        ops::rmsnorm_into(x, lw.attn_norm, xb, config.rms_norm_eps);

        // DEBUG: Check RMSNorm output for layer 0, token 0
        if pos == 0 && layer == 0 {
            let mut nan = 0u32;
            let mut sum_sq = 0.0f32;
            for i in 0..dim {
                if xb[i].is_nan() { nan += 1; }
                sum_sq += xb[i] * xb[i];
            }
            let rms = ops::fast_sqrt(sum_sq / dim as f32);
            libfolk::println!("[FWD] L0 RMSNorm: NaN={} rms={:.6} xb[0]={:.6} xb[1]={:.6}", nan, rms, xb[0], xb[1]);
        }

        // 2. Q/K/V projections (f32 activations × Q4_0 weights)
        gemm::gemm_f32_x_q4(q, xb, lw.wq, 1, dim, n_heads * head_dim,
            FuseOp::None, yield_cfg.gemm_yield, arena);

        // DEBUG: Check Q projection output for layer 0, token 0
        if pos == 0 && layer == 0 {
            let mut nan = 0u32;
            for i in 0..n_heads*head_dim { if q[i].is_nan() { nan += 1; } }
            libfolk::println!("[FWD] L0 Q-proj: NaN={} q[0]={:.6} q[1]={:.6} q[63]={:.6}",
                nan, q[0], q[1], q[63]);
        }
        gemm::gemm_f32_x_q4(k, xb, lw.wk, 1, dim, kv_dim,
            FuseOp::None, yield_cfg.gemm_yield, arena);
        gemm::gemm_f32_x_q4(v, xb, lw.wv, 1, dim, kv_dim,
            FuseOp::None, yield_cfg.gemm_yield, arena);

        // 3. RoPE on Q and K
        ops::rope_inplace(q, head_dim, pos, config.rope_base);
        ops::rope_inplace(k, head_dim, pos, config.rope_base);

        // 4. Store K,V in cache
        kv_cache.layer_mut(layer).store(k, v);

        // 5. Multi-head attention with KV-cache
        let seq_len = kv_cache.layer(layer).active_length();
        let kv_group_size = n_heads / n_kv_heads; // for GQA

        // Zero attention output
        for i in 0..dim { attn_out[i] = 0.0; }

        for h in 0..n_heads {
            let kv_h = h / kv_group_size; // KV head for this Q head
            let q_offset = h * head_dim;

            // Compute attention scores: Q · K^T / sqrt(head_dim)
            let scale = crate::ops::fast_rsqrt(head_dim as f32);
            for t in 0..seq_len {
                let k_vec = kv_cache.layer(layer).get_key(t, kv_h);
                let mut score = 0.0f32;
                for d in 0..head_dim {
                    score += q[q_offset + d] * k_vec[d];
                }
                att[t] = score * scale;
            }

            // Softmax over attention scores
            ops::softmax(&mut att[..seq_len]);

            // Weighted sum of values
            let out_offset = h * head_dim;
            for t in 0..seq_len {
                let v_vec = kv_cache.layer(layer).get_value(t, kv_h);
                let w = att[t];
                for d in 0..head_dim {
                    attn_out[out_offset + d] += w * v_vec[d];
                }
            }
        }

        // 6. Output projection (attn_out × Wo → xb)
        for i in 0..dim { xb[i] = 0.0; }
        gemm::gemm_f32_x_q4(xb, attn_out, lw.wo, 1, dim, dim,
            FuseOp::None, yield_cfg.gemm_yield, arena);

        // 7. Residual connection
        for i in 0..dim { x[i] += xb[i]; }

        // Yield between attention and FFN
        libfolk::sys::yield_cpu();

        // --- FFN sublayer ---
        // 1. RMSNorm
        ops::rmsnorm_into(x, lw.ffn_norm, xb2, config.rms_norm_eps);

        // 2. Gate + Up projections
        for i in 0..config.intermediate_size { ffn_buf1[i] = 0.0; }
        for i in 0..config.intermediate_size { ffn_buf2[i] = 0.0; }

        // gate = xb2 × W_gate (with fused SiLU)
        gemm::gemm_f32_x_q4(ffn_buf1, xb2, lw.w_gate, 1, dim, config.intermediate_size,
            FuseOp::SiLU, yield_cfg.gemm_yield, arena);

        // up = xb2 × W_up
        gemm::gemm_f32_x_q4(ffn_buf2, xb2, lw.w_up, 1, dim, config.intermediate_size,
            FuseOp::None, yield_cfg.gemm_yield, arena);

        // 3. Element-wise gate * up
        for i in 0..config.intermediate_size {
            ffn_buf1[i] *= ffn_buf2[i];
        }

        // 4. Down projection
        for i in 0..dim { xb[i] = 0.0; }
        gemm::gemm_f32_x_q4(xb, ffn_buf1, lw.w_down, 1, config.intermediate_size, dim,
            FuseOp::None, yield_cfg.gemm_yield, arena);

        // 5. Residual connection
        for i in 0..dim { x[i] += xb[i]; }

        // DEBUG: Check for NaN after each layer (only for first token)
        if pos == 0 && layer < 2 {
            let mut nan = 0u32;
            for i in 0..dim {
                if x[i].is_nan() { nan += 1; }
            }
            libfolk::println!("[FWD] layer {} done: NaN={}/{} x[0]={:.6} x[1]={:.6}",
                layer, nan, dim, x[0], x[1]);
        }
    }

    // === Final norm + LM head ===
    ops::rmsnorm(x, weights.final_norm, config.rms_norm_eps);

    // Output projection: x × W_output → logits
    for i in 0..config.vocab_size { logits[i] = 0.0; }
    if weights.output_is_q8 {
        gemm::gemm_f32_x_q8(logits, x, weights.output_weight, 1, dim, config.vocab_size,
            FuseOp::None, yield_cfg.gemm_yield, arena);
    } else {
        gemm::gemm_f32_x_q4(logits, x, weights.output_weight, 1, dim, config.vocab_size,
            FuseOp::None, yield_cfg.gemm_yield, arena);
    }

    Some(logits)
}

/// Dequantize a token embedding from quantized weight table.
///
/// Supports both Q4_0 and Q8_0 formats. The block size determines format:
/// - Q4_0: 18 bytes/block (2 scale + 16 data) → 32 values
/// - Q8_0: 36 bytes/block (4 scale + 32 data) → 32 values
fn embed_token(token_id: usize, vocab_size: usize, dim: usize, embed_data: &[u8], output: &mut [f32]) {
    debug_assert!(token_id < vocab_size);

    let blocks_per_row = dim / 32;

    // Detect format: check total data size to determine block size
    let expected_q4 = vocab_size * blocks_per_row * quantize::Q4_0_BLOCK_SIZE;
    let expected_q8 = vocab_size * blocks_per_row * quantize::Q8_0_BLOCK_SIZE;

    let (block_size, is_q8) = if embed_data.len() >= expected_q8 {
        (quantize::Q8_0_BLOCK_SIZE, true)
    } else {
        (quantize::Q4_0_BLOCK_SIZE, false)
    };

    let row_bytes = blocks_per_row * block_size;
    let row_start = token_id * row_bytes;

    let mut out_idx = 0;
    for blk in 0..blocks_per_row {
        let block_start = row_start + blk * block_size;
        let block = &embed_data[block_start..block_start + block_size];
        if is_q8 {
            quantize::dequantize_q8_0_block(block, &mut output[out_idx..out_idx + 32]);
        } else {
            quantize::dequantize_q4_0_block(block, &mut output[out_idx..out_idx + 32]);
        }
        out_idx += 32;
    }
}

/// Argmax over a logits slice — returns the token ID with highest logit.
#[inline]
pub fn argmax(logits: &[f32]) -> u32 {
    let mut max_val = f32::NEG_INFINITY;
    let mut max_idx = 0u32;
    for (i, &v) in logits.iter().enumerate() {
        if v > max_val {
            max_val = v;
            max_idx = i as u32;
        }
    }
    max_idx
}

/// Top-K sampling: select from the K highest-probability tokens.
///
/// Uses `att_buf` as scratch space for the top-K candidates.
/// Returns sampled token ID.
pub fn sample_top_k(logits: &mut [f32], k: usize, temperature: f32, random_u32: u32) -> u32 {
    let vocab_size = logits.len();
    let k = k.min(vocab_size);

    if temperature <= 0.0 || k == 1 {
        return argmax(logits);
    }

    // Apply temperature
    ops::softmax_temperature(logits, temperature);

    // Find top-K indices using partial sort
    // Simple O(V*K) selection — fine for V=49152, K=40
    let mut top_indices = [0u32; 64]; // max K=64
    let mut top_probs = [0.0f32; 64];
    let actual_k = k.min(64);

    for i in 0..actual_k {
        let mut best_idx = 0usize;
        let mut best_val = f32::NEG_INFINITY;
        for j in 0..vocab_size {
            if logits[j] > best_val {
                // Check it's not already in top list
                let mut already = false;
                for prev in 0..i {
                    if top_indices[prev] == j as u32 {
                        already = true;
                        break;
                    }
                }
                if !already {
                    best_val = logits[j];
                    best_idx = j;
                }
            }
        }
        top_indices[i] = best_idx as u32;
        top_probs[i] = best_val;
    }

    // Renormalize top-K probabilities
    let mut sum = 0.0f32;
    for i in 0..actual_k {
        sum += top_probs[i];
    }
    if sum > 0.0 {
        for i in 0..actual_k {
            top_probs[i] /= sum;
        }
    }

    // Sample from top-K using random_u32
    let r = (random_u32 as f32) / (u32::MAX as f32);
    let mut cumsum = 0.0f32;
    for i in 0..actual_k {
        cumsum += top_probs[i];
        if r < cumsum {
            return top_indices[i];
        }
    }

    // Fallback
    top_indices[0]
}
