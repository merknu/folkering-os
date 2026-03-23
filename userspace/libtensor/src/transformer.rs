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

/// Attention dump buffer for capturing post-softmax attention weights.
///
/// During prefill, the caller allocates a buffer of `n_heads * max_seq * max_seq` f32
/// and passes it to forward(). After softmax, each head's attention weights are copied
/// into the buffer at `[head * max_seq * max_seq + pos * max_seq]`.
///
/// The resulting buffer contains the full causal attention matrix for one layer,
/// ready for writing to the disk mailbox and visualization via MCP attention_heatmap.
pub struct AttnDump<'a> {
    pub buffer: &'a mut [f32],
    pub dump_layer: usize,
    pub n_heads: usize,
    pub max_seq: usize,
}

impl<'a> AttnDump<'a> {
    /// Store post-softmax attention weights for one head at one position.
    #[inline]
    pub fn store(&mut self, layer: usize, head: usize, pos: usize, att: &[f32], seq_len: usize) {
        if layer != self.dump_layer {
            return;
        }
        // Layout: [head, query_pos, key_pos] in row-major
        let offset = head * self.max_seq * self.max_seq + pos * self.max_seq;
        let end = offset + seq_len;
        if end <= self.buffer.len() {
            self.buffer[offset..end].copy_from_slice(&att[..seq_len]);
        }
    }
}

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

    /// Quantization format of output_weight
    pub output_quant: OutputQuant,
}

/// Quantization format for the output projection weight
#[derive(Clone, Copy, PartialEq)]
pub enum OutputQuant {
    Q4_0,
    Q8_0,
    Q6_K,
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
    mut attn_dump: Option<&mut AttnDump>,
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

    // Q8_0 scratch buffer for quantized activations (used by gemm_q4_q8)
    // Max size needed: intermediate_size (1536) → 48 blocks × 34 bytes = 1632
    let q8_max_blocks = config.intermediate_size / 32;
    let q8_buf = arena.alloc_slice::<u8>(q8_max_blocks * quantize::Q8_0_BLOCK_SIZE)?;

    // Scratch buffers for dequantizing Q8_0 KV-cache entries (one head at a time)
    let k_dequant = arena.alloc_f32(head_dim)?;
    let v_dequant = arena.alloc_f32(head_dim)?;

    // Attention scores buffer (per-head, reused across heads)
    let max_ctx = kv_cache.layer(0).active_length() + 1;
    let att = arena.alloc_f32(max_ctx)?;

    // === Token embedding lookup ===
    // Dequantize the embedding row for token_id from Q4_0
    embed_token(token_id as usize, config.vocab_size, dim, weights.token_embed, x);

    // === Transformer layers ===
    for layer in 0..config.n_layers {
        let lw = &weights.layers[layer];

        // --- Attention sublayer ---
        // 1. RMSNorm
        ops::rmsnorm_into(x, lw.attn_norm, xb, config.rms_norm_eps);

        // 2. Q/K/V projections: quantize xb to Q8_0, then use integer dot product
        // This matches llama.cpp's approach and avoids f32 accumulation drift.
        for i in 0..n_heads * head_dim { q[i] = 0.0; }
        for i in 0..kv_dim { k[i] = 0.0; v[i] = 0.0; }
        quantize::quantize_f32_to_q8_0(xb, q8_buf);
        gemm::gemm_q4_q8(q, q8_buf, lw.wq, 1, dim, n_heads * head_dim,
            FuseOp::None, yield_cfg.gemm_yield);
        gemm::gemm_q4_q8(k, q8_buf, lw.wk, 1, dim, kv_dim,
            FuseOp::None, yield_cfg.gemm_yield);
        gemm::gemm_q4_q8(v, q8_buf, lw.wv, 1, dim, kv_dim,
            FuseOp::None, yield_cfg.gemm_yield);

        // 3. RoPE on Q and K
        ops::rope_inplace(q, head_dim, pos, config.rope_base);
        ops::rope_inplace(k, head_dim, pos, config.rope_base);

        // 4. Store K,V in cache with absolute position (for RoPE alignment)
        kv_cache.layer_mut(layer).store(k, v, pos);

        // 5. Multi-head attention with KV-cache
        let seq_len = kv_cache.layer(layer).active_length();
        let kv_group_size = n_heads / n_kv_heads; // for GQA

        // Zero attention output
        for i in 0..dim { attn_out[i] = 0.0; }

        for h in 0..n_heads {
            let kv_h = h / kv_group_size; // KV head for this Q head
            let q_offset = h * head_dim;

            // Compute attention scores: Q · K^T / sqrt(head_dim)
            // AVX2-accelerated dot product for 4-8x speedup over scalar
            let scale = crate::ops::fast_rsqrt(head_dim as f32);
            let q_slice = &q[q_offset..q_offset + head_dim];
            for t in 0..seq_len {
                kv_cache.layer(layer).get_key(t, kv_h, k_dequant);
                let score = if crate::simd::has_avx2() {
                    unsafe { crate::simd::avx2::dot_f32_avx2(q_slice, k_dequant, head_dim) }
                } else {
                    crate::simd::dot_f32_scalar(q_slice, k_dequant, head_dim)
                };
                att[t] = score * scale;
            }

            // Softmax over attention scores
            ops::softmax(&mut att[..seq_len]);

            // Capture post-softmax attention for visualization
            if let Some(ref mut dump) = attn_dump {
                dump.store(layer, h, pos, att, seq_len);
            }

            // Weighted sum of values (AVX2 AXPY: out += w * V)
            let out_offset = h * head_dim;
            for t in 0..seq_len {
                kv_cache.layer(layer).get_value(t, kv_h, v_dequant);
                let w = att[t];
                if crate::simd::has_avx2() {
                    unsafe {
                        crate::simd::axpy_f32_avx2(
                            w, v_dequant,
                            &mut attn_out[out_offset..out_offset + head_dim],
                            head_dim,
                        );
                    }
                } else {
                    crate::simd::axpy_f32_scalar(
                        w, v_dequant,
                        &mut attn_out[out_offset..out_offset + head_dim],
                        head_dim,
                    );
                }
            }
        }

        // 6. Output projection (attn_out × Wo → xb)
        for i in 0..dim { xb[i] = 0.0; }
        quantize::quantize_f32_to_q8_0(attn_out, q8_buf);
        gemm::gemm_q4_q8(xb, q8_buf, lw.wo, 1, dim, dim,
            FuseOp::None, yield_cfg.gemm_yield);

        // 7. Residual connection
        for i in 0..dim { x[i] += xb[i]; }

        // Yield between attention and FFN
        libfolk::sys::yield_cpu();

        // --- FFN sublayer ---
        // 1. RMSNorm
        ops::rmsnorm_into(x, lw.ffn_norm, xb2, config.rms_norm_eps);

        // 2. Gate + Up projections (quantize xb2 once, reuse for both)
        for i in 0..config.intermediate_size { ffn_buf1[i] = 0.0; }
        for i in 0..config.intermediate_size { ffn_buf2[i] = 0.0; }
        quantize::quantize_f32_to_q8_0(xb2, q8_buf);

        // gate = xb2 × W_gate (with fused SiLU)
        gemm::gemm_q4_q8(ffn_buf1, q8_buf, lw.w_gate, 1, dim, config.intermediate_size,
            FuseOp::SiLU, yield_cfg.gemm_yield);

        // up = xb2 × W_up
        gemm::gemm_q4_q8(ffn_buf2, q8_buf, lw.w_up, 1, dim, config.intermediate_size,
            FuseOp::None, yield_cfg.gemm_yield);

        // 3. Element-wise gate * up
        for i in 0..config.intermediate_size {
            ffn_buf1[i] *= ffn_buf2[i];
        }

        // 4. Down projection (quantize gate*up, then GEMM)
        for i in 0..dim { xb[i] = 0.0; }
        quantize::quantize_f32_to_q8_0(ffn_buf1, q8_buf);
        gemm::gemm_q4_q8(xb, q8_buf, lw.w_down, 1, config.intermediate_size, dim,
            FuseOp::None, yield_cfg.gemm_yield);

        // 5. Residual connection
        for i in 0..dim { x[i] += xb[i]; }

    }

    // === Final norm + LM head ===
    ops::rmsnorm(x, weights.final_norm, config.rms_norm_eps);

    // Output projection: x × W_output → logits
    for i in 0..config.vocab_size { logits[i] = 0.0; }
    match weights.output_quant {
        OutputQuant::Q8_0 => {
            gemm::gemm_f32_x_q8(logits, x, weights.output_weight, 1, dim, config.vocab_size,
                FuseOp::None, yield_cfg.gemm_yield, arena);
        }
        OutputQuant::Q6_K => {
            gemm::gemm_f32_x_q6k(logits, x, weights.output_weight, 1, dim, config.vocab_size,
                yield_cfg.gemm_yield);
        }
        OutputQuant::Q4_0 => {
            gemm::gemm_f32_x_q4(logits, x, weights.output_weight, 1, dim, config.vocab_size,
                FuseOp::None, yield_cfg.gemm_yield, arena);
        }
    }

    Some(logits)
}

/// Batched prefill forward pass — 3-Phase Architecture.
///
/// Processes a batch of prompt tokens with improved L2 cache utilization:
/// - Phase A (Batched): RMSNorm + Q/K/V GEMMs — weight matrices stay in L2
/// - Phase B (Sequential): Per-token RoPE, KV-store, Attention, Output Proj
/// - Phase C (Batched): FFN GEMMs — weight matrices stay in L2
///
/// Only processes transformer layers. Does NOT compute final logits (caller
/// should use single-token `forward()` for the last prompt token to get logits).
pub fn forward_prefill_batch(
    tokens: &[u32],
    start_pos: usize,
    config: &ModelConfig,
    weights: &ModelWeights,
    kv_cache: &mut KvCacheManager,
    arena: &BumpArena,
    yield_cfg: &YieldConfig,
) -> Option<()> {
    let batch = tokens.len();
    if batch == 0 { return Some(()); }

    let dim = config.embed_dim;
    let head_dim = config.head_dim;
    let n_heads = config.n_heads;
    let n_kv_heads = config.n_kv_heads;
    let kv_dim = n_kv_heads * head_dim;
    let qk_dim = n_heads * head_dim;
    let intermediate = config.intermediate_size;

    // Q8_0 row size for dim-length vectors
    let q8_blocks = dim / 32;
    let q8_row_bytes = q8_blocks * quantize::Q8_0_BLOCK_SIZE;
    // Q8_0 row size for intermediate-length vectors (FFN down proj)
    let q8_inter_blocks = intermediate / 32;
    let q8_inter_row_bytes = q8_inter_blocks * quantize::Q8_0_BLOCK_SIZE;

    // === Allocate ALL batched buffers (batch × per_token_size) ===
    let x_batch   = arena.alloc_f32(batch * dim)?;
    let xb_batch  = arena.alloc_f32(batch * dim)?;
    let q_batch   = arena.alloc_f32(batch * qk_dim)?;
    let k_batch   = arena.alloc_f32(batch * kv_dim)?;
    let v_batch   = arena.alloc_f32(batch * kv_dim)?;
    let attn_out  = arena.alloc_f32(dim)?;             // per-token (Phase B reuse)
    let xb_single = arena.alloc_f32(dim)?;             // per-token scratch
    let q8_batch  = arena.alloc_slice::<u8>(batch * q8_row_bytes)?;

    // Phase C buffers
    let xb2_batch  = arena.alloc_f32(batch * dim)?;
    let ffn1_batch = arena.alloc_f32(batch * intermediate)?;
    let ffn2_batch = arena.alloc_f32(batch * intermediate)?;
    let q8_inter   = arena.alloc_slice::<u8>(q8_inter_row_bytes)?; // per-token for down proj

    // Attention scratch (per-token, reused in Phase B)
    let max_ctx = kv_cache.layer(0).active_length() + batch;
    let att = arena.alloc_f32(max_ctx)?;

    // KV dequant scratch (per-head, reused in Phase B)
    let k_dequant = arena.alloc_f32(head_dim)?;
    let v_dequant = arena.alloc_f32(head_dim)?;

    // === Step 1: Batched Embedding ===
    for i in 0..batch {
        embed_token(
            tokens[i] as usize, config.vocab_size, dim,
            weights.token_embed,
            &mut x_batch[i * dim..(i + 1) * dim],
        );
    }

    // === Step 2: For each layer ===
    for layer in 0..config.n_layers {
        let lw = &weights.layers[layer];

        // ============================================================
        // PHASE A (Batched): RMSNorm + Q/K/V GEMMs
        // Weight matrices wq/wk/wv stay hot in L2 across batch iterations
        // ============================================================

        for i in 0..batch {
            let x_row = &x_batch[i * dim..(i + 1) * dim];
            let xb_row = &mut xb_batch[i * dim..(i + 1) * dim];
            ops::rmsnorm_into(x_row, lw.attn_norm, xb_row, config.rms_norm_eps);

            // Quantize per-token (NEVER across token boundaries)
            let q8_row = &mut q8_batch[i * q8_row_bytes..(i + 1) * q8_row_bytes];
            quantize::quantize_f32_to_q8_0(xb_row, q8_row);

            // Q/K/V GEMMs — wq/wk/wv stay in L2 from previous iteration
            let q_row = &mut q_batch[i * qk_dim..(i + 1) * qk_dim];
            let k_row = &mut k_batch[i * kv_dim..(i + 1) * kv_dim];
            let v_row = &mut v_batch[i * kv_dim..(i + 1) * kv_dim];
            for j in 0..qk_dim { q_row[j] = 0.0; }
            for j in 0..kv_dim { k_row[j] = 0.0; v_row[j] = 0.0; }

            gemm::gemm_q4_q8(q_row, q8_row, lw.wq, 1, dim, qk_dim,
                FuseOp::None, yield_cfg.gemm_yield);
            gemm::gemm_q4_q8(k_row, q8_row, lw.wk, 1, dim, kv_dim,
                FuseOp::None, yield_cfg.gemm_yield);
            gemm::gemm_q4_q8(v_row, q8_row, lw.wv, 1, dim, kv_dim,
                FuseOp::None, yield_cfg.gemm_yield);
        }

        // Yield between Phase A and B
        libfolk::sys::yield_cpu();

        // ============================================================
        // PHASE B (Sequential): Per-token Attention
        // Must be sequential due to causal dependency in KV-cache
        // ============================================================

        let kv_group_size = n_heads / n_kv_heads;

        for i in 0..batch {
            let pos = start_pos + i;

            // RoPE on Q and K (must be done before immutable borrow for store)
            ops::rope_inplace(
                &mut q_batch[i * qk_dim..(i + 1) * qk_dim],
                head_dim, pos, config.rope_base,
            );
            ops::rope_inplace(
                &mut k_batch[i * kv_dim..(i + 1) * kv_dim],
                head_dim, pos, config.rope_base,
            );

            // Store K,V in cache (immutable borrows after RoPE mutation)
            let k_row = &k_batch[i * kv_dim..(i + 1) * kv_dim];
            let v_row = &v_batch[i * kv_dim..(i + 1) * kv_dim];
            kv_cache.layer_mut(layer).store(k_row, v_row, pos);

            // Multi-head attention
            let seq_len = kv_cache.layer(layer).active_length();
            for j in 0..dim { attn_out[j] = 0.0; }

            for h in 0..n_heads {
                let kv_h = h / kv_group_size;
                let q_offset = i * qk_dim + h * head_dim;
                let q_slice = &q_batch[q_offset..q_offset + head_dim];

                let scale = crate::ops::fast_rsqrt(head_dim as f32);
                for t in 0..seq_len {
                    kv_cache.layer(layer).get_key(t, kv_h, k_dequant);
                    let score = if crate::simd::has_avx2() {
                        unsafe { crate::simd::avx2::dot_f32_avx2(q_slice, k_dequant, head_dim) }
                    } else {
                        crate::simd::dot_f32_scalar(q_slice, k_dequant, head_dim)
                    };
                    att[t] = score * scale;
                }

                ops::softmax(&mut att[..seq_len]);

                let out_offset = h * head_dim;
                for t in 0..seq_len {
                    kv_cache.layer(layer).get_value(t, kv_h, v_dequant);
                    let w = att[t];
                    if crate::simd::has_avx2() {
                        unsafe {
                            crate::simd::axpy_f32_avx2(
                                w, v_dequant,
                                &mut attn_out[out_offset..out_offset + head_dim],
                                head_dim,
                            );
                        }
                    } else {
                        crate::simd::axpy_f32_scalar(
                            w, v_dequant,
                            &mut attn_out[out_offset..out_offset + head_dim],
                            head_dim,
                        );
                    }
                }
            }

            // Output projection: attn_out × Wo → xb_single
            for j in 0..dim { xb_single[j] = 0.0; }
            let q8_row = &mut q8_batch[i * q8_row_bytes..(i + 1) * q8_row_bytes];
            quantize::quantize_f32_to_q8_0(attn_out, q8_row);
            gemm::gemm_q4_q8(xb_single, q8_row, lw.wo, 1, dim, dim,
                FuseOp::None, yield_cfg.gemm_yield);

            // Attention residual
            let x_row = &mut x_batch[i * dim..(i + 1) * dim];
            for j in 0..dim { x_row[j] += xb_single[j]; }
        }

        // Yield between Phase B and C
        libfolk::sys::yield_cpu();

        // ============================================================
        // PHASE C (Batched): FFN GEMMs
        // Weight matrices w_gate/w_up/w_down stay hot in L2
        // ============================================================

        for i in 0..batch {
            let x_row = &x_batch[i * dim..(i + 1) * dim];
            let xb2_row = &mut xb2_batch[i * dim..(i + 1) * dim];
            ops::rmsnorm_into(x_row, lw.ffn_norm, xb2_row, config.rms_norm_eps);

            // Quantize per-token
            let q8_row = &mut q8_batch[i * q8_row_bytes..(i + 1) * q8_row_bytes];
            quantize::quantize_f32_to_q8_0(xb2_row, q8_row);

            // Gate projection (with fused SiLU)
            let ffn1_row = &mut ffn1_batch[i * intermediate..(i + 1) * intermediate];
            for j in 0..intermediate { ffn1_row[j] = 0.0; }
            gemm::gemm_q4_q8(ffn1_row, q8_row, lw.w_gate, 1, dim, intermediate,
                FuseOp::SiLU, yield_cfg.gemm_yield);

            // Up projection
            let ffn2_row = &mut ffn2_batch[i * intermediate..(i + 1) * intermediate];
            for j in 0..intermediate { ffn2_row[j] = 0.0; }
            gemm::gemm_q4_q8(ffn2_row, q8_row, lw.w_up, 1, dim, intermediate,
                FuseOp::None, yield_cfg.gemm_yield);

            // Element-wise gate * up
            for j in 0..intermediate { ffn1_row[j] *= ffn2_row[j]; }

            // Down projection
            let xb_row = &mut xb_batch[i * dim..(i + 1) * dim];
            for j in 0..dim { xb_row[j] = 0.0; }
            quantize::quantize_f32_to_q8_0(ffn1_row, q8_inter);
            gemm::gemm_q4_q8(xb_row, q8_inter, lw.w_down, 1, intermediate, dim,
                FuseOp::None, yield_cfg.gemm_yield);

            // FFN residual
            let x_row = &mut x_batch[i * dim..(i + 1) * dim];
            for j in 0..dim { x_row[j] += xb_row[j]; }
        }
    }

    Some(())
}

/// Dequantize a token embedding from quantized weight table.
///
/// Supports both Q4_0 and Q8_0 formats. The block size determines format:
/// - Q4_0: 18 bytes/block (2 scale + 16 data) → 32 values
/// - Q8_0: 36 bytes/block (4 scale + 32 data) → 32 values
fn embed_token(token_id: usize, vocab_size: usize, dim: usize, embed_data: &[u8], output: &mut [f32]) {
    debug_assert!(token_id < vocab_size);

    let blocks_per_row = dim / 32; // for Q4_0/Q8_0

    // Detect format from total data size
    // Debug: log on first call only
    static EMBED_LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if !EMBED_LOGGED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        let blocks_q6k = dim / quantize::Q6_K_BLOCK_VALUES;
        let expected_q6k = vocab_size * blocks_q6k * quantize::Q6_K_BLOCK_SIZE;
        libfolk::println!("[EMBED] data_len={} expected_q6k={} blocks_q6k={} dim={} vocab={}",
            embed_data.len(), expected_q6k, blocks_q6k, dim, vocab_size);
    }
    // Q6_K: 256 values/block, 210 bytes/block
    // Q8_0: 32 values/block, 34 bytes/block
    // Q4_0: 32 values/block, 18 bytes/block
    let blocks_q6k = dim / quantize::Q6_K_BLOCK_VALUES; // 256 values per block
    let expected_q6k = vocab_size * blocks_q6k * quantize::Q6_K_BLOCK_SIZE;

    if embed_data.len() >= expected_q6k && blocks_q6k > 0 {
        // Q6_K format (Qwen3 embeddings)
        let row_bytes = blocks_q6k * quantize::Q6_K_BLOCK_SIZE;
        let row_start = token_id * row_bytes;
        let mut out_idx = 0;
        for blk in 0..blocks_q6k {
            let block_start = row_start + blk * quantize::Q6_K_BLOCK_SIZE;
            let block = &embed_data[block_start..block_start + quantize::Q6_K_BLOCK_SIZE];
            let vals = quantize::Q6_K_BLOCK_VALUES.min(dim - out_idx);
            quantize::dequantize_q6_k_block(block, &mut output[out_idx..out_idx + vals]);
            out_idx += vals;
        }
    } else {
        // Q4_0 or Q8_0 format (SmolLM2 embeddings)
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
