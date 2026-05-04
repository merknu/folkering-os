//! Phase D.3.5 — multi-layer transformer forward pass.
//!
//! Stitches the building blocks (`embedding_lookup`, `rmsnorm`,
//! `attention_block`, `swiglu_ffn`, `linear`) into a real Qwen2.5 /
//! Llama-shaped forward pass and produces the next-token logits.
//!
//! Today's scope:
//!   - Tied embeddings only (`lm_head` shares the embed table).
//!   - n_kv_heads == n_heads (no grouped-query attention).
//!   - No attention biases (synthetic test fixture has zeros; real
//!     Qwen2.5 needs them — that's D.3.6).
//!   - No KV-cache (every call re-prefills the whole sequence — D.4).
//!
//! The function takes a parsed `.fbin` view rather than pre-loaded
//! tensors so the inference task doesn't need to buffer the entire
//! model in our 256 KiB bump heap. We slurp tensors layer-by-layer
//! and let the bump allocator reclaim them between layers (it can't,
//! actually — bump never frees — but the call shape leaves room for a
//! slab allocator swap when D.4 lands).
//!
//! Reference output is computed by `tools/fbin-gen/forward_ref.py`,
//! which mirrors this code in numpy with the same `fast_rsqrt` /
//! `fast_exp` approximations so the argmax is stable across both
//! implementations.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use crate::tensor_math;
use crate::weights::FbinView;

/// Topology + numerics knobs the runtime needs to drive the forward
/// pass. Mirrors the subset of `config.json` we actually consume.
pub struct ModelConfig {
    pub n_layers: usize,
    pub hidden_dim: usize,
    pub n_heads: usize,
    pub intermediate: usize,
    pub vocab: usize,
    /// Maximum positions the embedded RoPE tables cover. Forward pass
    /// asserts `seq_len <= max_pos`.
    pub max_pos: usize,
    /// RMSNorm epsilon. Qwen2.5 uses 1e-5.
    pub eps: f32,
}

/// Run the forward pass over `token_ids` and return the unnormalized
/// logits at the LAST position. Caller picks the next token via
/// `argmax` / sampler.
///
/// Returns `None` on any tensor lookup failure or shape mismatch —
/// proxying through `?` keeps the call sites readable; debug logging
/// happens at the boot-test layer above.
pub fn forward_pass(
    view: &FbinView,
    cfg: &ModelConfig,
    token_ids: &[u32],
) -> Option<Vec<f32>> {
    let seq_len = token_ids.len();
    if seq_len == 0 || seq_len > cfg.max_pos { return None; }
    if cfg.hidden_dim % cfg.n_heads != 0 { return None; }
    let head_dim = cfg.hidden_dim / cfg.n_heads;
    let pairs = head_dim / 2;

    let embed = view.find("embed").and_then(|t| view.read_f32(t))?;
    let final_norm = view.find("final_norm").and_then(|t| view.read_f32(t))?;
    let rope_cos_full = view.find("rope_cos").and_then(|t| view.read_f32(t))?;
    let rope_sin_full = view.find("rope_sin").and_then(|t| view.read_f32(t))?;

    // RoPE tables are stored at full max_pos; slice to seq_len.
    let need = seq_len * pairs;
    if rope_cos_full.len() < need || rope_sin_full.len() < need { return None; }
    let rope_cos = &rope_cos_full[..need];
    let rope_sin = &rope_sin_full[..need];

    // Step 1: embedding lookup → x : [seq_len, hidden]
    let mut x: Vec<f32> = Vec::with_capacity(seq_len * cfg.hidden_dim);
    for &id in token_ids {
        let row = tensor_math::embedding_lookup(
            &embed, cfg.vocab, cfg.hidden_dim, id,
        )?;
        x.extend(row);
    }

    // Step 2: per-layer attention + FFN with residuals.
    for li in 0..cfg.n_layers {
        let prefix = layer_prefix(li);
        let attn_norm = view.find(&join(&prefix, "attn_norm"))
            .and_then(|t| view.read_f32(t))?;
        let wq = view.find(&join(&prefix, "q")).and_then(|t| view.read_f32(t))?;
        let wk = view.find(&join(&prefix, "k")).and_then(|t| view.read_f32(t))?;
        let wv = view.find(&join(&prefix, "v")).and_then(|t| view.read_f32(t))?;
        let wo = view.find(&join(&prefix, "o")).and_then(|t| view.read_f32(t))?;
        let ffn_norm = view.find(&join(&prefix, "ffn_norm"))
            .and_then(|t| view.read_f32(t))?;
        let gate = view.find(&join(&prefix, "gate")).and_then(|t| view.read_f32(t))?;
        let up = view.find(&join(&prefix, "up")).and_then(|t| view.read_f32(t))?;
        let down = view.find(&join(&prefix, "down")).and_then(|t| view.read_f32(t))?;

        // 2a. Pre-attention RMSNorm (per-row).
        let mut x_normed = Vec::with_capacity(seq_len * cfg.hidden_dim);
        for s in 0..seq_len {
            let row = &x[s * cfg.hidden_dim..(s + 1) * cfg.hidden_dim];
            x_normed.extend(tensor_math::rmsnorm(row, &attn_norm, cfg.eps)?);
        }

        // 2b. Attention block (QKV → RoPE → SDPA → Wo).
        let attn = tensor_math::attention_block(
            &x_normed, &wq, &wk, &wv, &wo, rope_cos, rope_sin,
            seq_len, cfg.hidden_dim, cfg.n_heads,
        )?;

        // 2c. Residual.
        for i in 0..x.len() { x[i] += attn[i]; }

        // 2d. Pre-FFN RMSNorm (per-row).
        let mut x_normed2 = Vec::with_capacity(seq_len * cfg.hidden_dim);
        for s in 0..seq_len {
            let row = &x[s * cfg.hidden_dim..(s + 1) * cfg.hidden_dim];
            x_normed2.extend(tensor_math::rmsnorm(row, &ffn_norm, cfg.eps)?);
        }

        // 2e. SwiGLU FFN (per-row).
        let mut ffn_out = Vec::with_capacity(seq_len * cfg.hidden_dim);
        for s in 0..seq_len {
            let row = &x_normed2[s * cfg.hidden_dim..(s + 1) * cfg.hidden_dim];
            ffn_out.extend(tensor_math::swiglu_ffn(
                row, &gate, &up, &down, cfg.hidden_dim, cfg.intermediate,
            )?);
        }

        // 2f. Residual.
        for i in 0..x.len() { x[i] += ffn_out[i]; }
    }

    // Step 3: final norm on the last position only — that's the only
    // row we need to project to logits for greedy sampling. Skipping
    // normalization on the other rows costs nothing today but saves
    // (seq_len-1) RMSNorms + linears at scale.
    let last_off = (seq_len - 1) * cfg.hidden_dim;
    let last = &x[last_off..last_off + cfg.hidden_dim];
    let last_normed = tensor_math::rmsnorm(last, &final_norm, cfg.eps)?;

    // Step 4: lm_head (tied to embed) — logits = embed @ last_normed.
    // `embed` has shape [vocab, hidden], so it's already in the
    // [out_dim, in_dim] orientation `linear` wants.
    tensor_math::linear(&embed, cfg.hidden_dim, cfg.vocab, &last_normed)
}

/// Find the next token by greedy argmax. Ignores ties; first-wins.
pub fn argmax(logits: &[f32]) -> Option<u32> {
    if logits.is_empty() { return None; }
    let mut best_idx = 0u32;
    let mut best_val = logits[0];
    for (i, &v) in logits.iter().enumerate().skip(1) {
        if v > best_val {
            best_val = v;
            best_idx = i as u32;
        }
    }
    Some(best_idx)
}

// ── Helpers ────────────────────────────────────────────────────────

fn layer_prefix(li: usize) -> String {
    // Avoid `format!` to keep the bump heap usage predictable.
    let mut s = String::with_capacity(16);
    s.push_str("layer.");
    push_usize(&mut s, li);
    s
}

fn join(prefix: &str, name: &str) -> String {
    let mut s = String::with_capacity(prefix.len() + 1 + name.len());
    s.push_str(prefix);
    s.push('.');
    s.push_str(name);
    s
}

fn push_usize(s: &mut String, mut n: usize) {
    if n == 0 { s.push('0'); return; }
    // Stack-allocated digit buffer. usize on x86_64 maxes at 20 digits.
    let mut buf = [0u8; 20];
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        s.push(buf[i] as char);
    }
}
