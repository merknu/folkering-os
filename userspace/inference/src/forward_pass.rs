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
use crate::tensor_math::{KvCache, WeightView};
use crate::weights::{DType, FbinView, TensorMeta};

/// Owned-or-borrowed weight tensor. The fp32 path materialises a
/// fresh `Vec<f32>` (the `.fbin` data section is a borrowed slice
/// into the underlying VFS read; we copy it once here so the math
/// path can hold a `&[f32]`). The Q8_0 path borrows the raw bytes
/// directly — no dequantization at load time, the inner matvec
/// loop pays per-block.
enum LoadedWeight<'a> {
    F32(Vec<f32>),
    Q8(&'a [u8]),
}

impl<'a> LoadedWeight<'a> {
    fn view<'b>(&'b self) -> WeightView<'b> {
        match self {
            Self::F32(v) => WeightView::F32(v.as_slice()),
            Self::Q8(b) => WeightView::Q8(b),
        }
    }
}

/// Load a weight tensor by name, dispatching on its on-disk dtype.
/// Q8_0 stays as borrowed bytes (zero copy); fp32 is read into a
/// fresh `Vec<f32>`. Returns `None` if the tensor is missing or
/// has an unsupported dtype.
fn load_weight<'a>(view: &'a FbinView, name: &str) -> Option<LoadedWeight<'a>> {
    let meta: &TensorMeta = view.find(name)?;
    match meta.dtype {
        DType::F32 => view.read_f32(meta).map(LoadedWeight::F32),
        DType::Q8 => Some(LoadedWeight::Q8(view.data_for(meta))),
        DType::Q4 => None, // reserved; not implemented
    }
}

/// Topology + numerics knobs the runtime needs to drive the forward
/// pass. Mirrors the subset of `config.json` we actually consume.
pub struct ModelConfig {
    pub n_layers: usize,
    pub hidden_dim: usize,
    pub n_heads: usize,
    /// Grouped-query attention: number of distinct K/V heads. For
    /// non-GQA models pass `n_kv_heads = n_heads`. Real Qwen2.5-0.5B
    /// has `n_heads=14, n_kv_heads=2`; Qwen3-0.6B has 16/8.
    pub n_kv_heads: usize,
    /// Per-head dimension. Qwen3 sets this *independently* of
    /// `hidden_dim / n_heads` — Qwen3-0.6B has `hidden=1024,
    /// n_heads=16, head_dim=128`, so `n_heads*head_dim=2048` and the
    /// Wq/Wo projections operate on a 2048-dim attention space that
    /// shrinks back to 1024 across Wo. Qwen2.5 / Llama set head_dim
    /// = hidden_dim / n_heads, so this just becomes the same number.
    pub head_dim: usize,
    pub intermediate: usize,
    pub vocab: usize,
    /// Maximum positions the embedded RoPE tables cover. Forward
    /// pass asserts `cache.seq_len + new_token_ids.len() <= max_pos`.
    pub max_pos: usize,
    /// RMSNorm epsilon. Qwen2.5 uses 1e-5; Qwen3 uses 1e-6.
    pub eps: f32,
}

/// Run the forward pass over `new_token_ids`, advance `cache` to
/// reflect the new positions, and return unnormalized logits at the
/// LAST position. Caller picks the next token via `argmax` /
/// sampler, then can call again with that token (and the same
/// cache) to keep generating without re-prefilling history.
///
/// Two modes fall out of the same code path:
/// - **Prefill** — `cache.seq_len = 0`, `new_token_ids` carries the
///   whole prompt. Same arithmetic as the pre-D.4 single-shot
///   forward pass; caller observes argmax for the prompt's last
///   token.
/// - **Decode** — `cache.seq_len > 0`, `new_token_ids` is one (or
///   a small batch of) freshly sampled tokens. Per-call cost drops
///   from O(seq² * layers) to O(seq * layers) because K/V for past
///   positions come from the cache instead of being recomputed.
///
/// Returns `None` on any tensor lookup failure, shape mismatch, or
/// position overflow (`cache.seq_len + new_token_ids.len() >
/// cache.max_pos`).
pub fn forward_pass(
    view: &FbinView,
    cfg: &ModelConfig,
    cache: &mut KvCache,
    new_token_ids: &[u32],
) -> Option<Vec<f32>> {
    let new_seq = new_token_ids.len();
    if new_seq == 0 { return None; }
    let head_dim = cfg.head_dim;
    if head_dim == 0 || head_dim % 2 != 0 { return None; }
    let pairs = head_dim / 2;

    // Validate cache geometry matches model config.
    if cache.layers.len() != cfg.n_layers { return None; }
    if cache.n_kv_heads != cfg.n_kv_heads { return None; }
    if cache.head_dim != head_dim { return None; }
    if cache.max_pos > cfg.max_pos { return None; }

    let pos_offset = cache.seq_len;
    if pos_offset + new_seq > cache.max_pos { return None; }

    // Embed table: prefer fp32, fall back to Q8 if that's how the
    // .fbin stored it. Both paths return a Vec<f32> per token row.
    let embed_meta = view.find("embed")?;
    let embed_loaded = match embed_meta.dtype {
        DType::F32 => LoadedWeight::F32(view.read_f32(embed_meta)?),
        DType::Q8 => LoadedWeight::Q8(view.data_for(embed_meta)),
        DType::Q4 => return None,
    };

    let final_norm = view.find("final_norm").and_then(|t| view.read_f32(t))?;
    let rope_cos_full = view.find("rope_cos").and_then(|t| view.read_f32(t))?;
    let rope_sin_full = view.find("rope_sin").and_then(|t| view.read_f32(t))?;

    // Slice RoPE tables to the absolute positions covered by the
    // NEW tokens — `[pos_offset, pos_offset + new_seq)` — not the
    // [0, new_seq) prefix (which is what pre-cache code used).
    let rope_start = pos_offset * pairs;
    let rope_end = (pos_offset + new_seq) * pairs;
    if rope_cos_full.len() < rope_end || rope_sin_full.len() < rope_end {
        return None;
    }
    let rope_cos = &rope_cos_full[rope_start..rope_end];
    let rope_sin = &rope_sin_full[rope_start..rope_end];

    // Step 1: embedding lookup for the new tokens only. Dispatches
    // on the table's stored dtype so a Q8 embed (saves ~75% on the
    // table) drops in transparently.
    let mut x: Vec<f32> = Vec::with_capacity(new_seq * cfg.hidden_dim);
    for &id in new_token_ids {
        let row = match &embed_loaded {
            LoadedWeight::F32(t) => tensor_math::embedding_lookup(
                t, cfg.vocab, cfg.hidden_dim, id,
            )?,
            LoadedWeight::Q8(t) => tensor_math::embedding_lookup_q8(
                t, cfg.vocab, cfg.hidden_dim, id,
            )?,
        };
        x.extend(row);
    }

    // Step 2: per-layer attention + FFN with residuals.
    for li in 0..cfg.n_layers {
        let prefix = layer_prefix(li);
        // Norms and biases stay fp32 even with --quantize q8_0 on
        // (precision-sensitive + small). Projection matrices may be
        // either fp32 or Q8 depending on what `hf_to_fbin.py` emitted;
        // `LoadedWeight` hides the difference behind a uniform
        // `WeightView` for the matvec calls below.
        let attn_norm = view.find(&join(&prefix, "attn_norm"))
            .and_then(|t| view.read_f32(t))?;
        let wq = load_weight(view, &join(&prefix, "q"))?;
        let wk = load_weight(view, &join(&prefix, "k"))?;
        let wv = load_weight(view, &join(&prefix, "v"))?;
        let wo = load_weight(view, &join(&prefix, "o"))?;
        // Biases are Qwen2.5-only; Llama-3 and the original synthetic
        // omit them. Treat absence as "no bias to add" rather than an
        // error so a single forward_pass covers both architectures.
        let q_bias = view.find(&join(&prefix, "q_bias")).and_then(|t| view.read_f32(t));
        let k_bias = view.find(&join(&prefix, "k_bias")).and_then(|t| view.read_f32(t));
        let v_bias = view.find(&join(&prefix, "v_bias")).and_then(|t| view.read_f32(t));
        // Qwen3-only: per-head RMSNorm on Q and K after projection,
        // before RoPE. Llama-3 / Qwen2.5 don't ship these tensors;
        // their absence flows through as `None` and the runtime
        // skips the normalization step.
        let q_norm = view.find(&join(&prefix, "q_norm")).and_then(|t| view.read_f32(t));
        let k_norm = view.find(&join(&prefix, "k_norm")).and_then(|t| view.read_f32(t));
        let ffn_norm = view.find(&join(&prefix, "ffn_norm"))
            .and_then(|t| view.read_f32(t))?;
        let gate = load_weight(view, &join(&prefix, "gate"))?;
        let up = load_weight(view, &join(&prefix, "up"))?;
        let down = load_weight(view, &join(&prefix, "down"))?;

        // 2a. Pre-attention RMSNorm (per-row).
        let mut x_normed = Vec::with_capacity(new_seq * cfg.hidden_dim);
        for s in 0..new_seq {
            let row = &x[s * cfg.hidden_dim..(s + 1) * cfg.hidden_dim];
            x_normed.extend(tensor_math::rmsnorm(row, &attn_norm, cfg.eps)?);
        }

        // 2b. Attention block (QKV (+biases) → RoPE → write KV cache
        // → SDPA over [0, pos_offset + new_seq) → Wo). The cache for
        // this layer carries history of all prior decode steps.
        let attn = tensor_math::attention_block(
            &x_normed,
            wq.view(), wk.view(), wv.view(), wo.view(),
            q_bias.as_deref(), k_bias.as_deref(), v_bias.as_deref(),
            q_norm.as_deref(), k_norm.as_deref(), cfg.eps,
            rope_cos, rope_sin,
            new_seq, cfg.hidden_dim,
            head_dim, cfg.n_heads, cfg.n_kv_heads,
            &mut cache.layers[li],
            cache.max_pos,
            pos_offset,
        )?;

        // 2c. Residual.
        for i in 0..x.len() { x[i] += attn[i]; }

        // 2d. Pre-FFN RMSNorm (per-row).
        let mut x_normed2 = Vec::with_capacity(new_seq * cfg.hidden_dim);
        for s in 0..new_seq {
            let row = &x[s * cfg.hidden_dim..(s + 1) * cfg.hidden_dim];
            x_normed2.extend(tensor_math::rmsnorm(row, &ffn_norm, cfg.eps)?);
        }

        // 2e. SwiGLU FFN — batched across all `new_seq` tokens in
        //      one weight-matrix pass per projection (gate/up/down).
        let ffn_out = tensor_math::swiglu_ffn(
            &x_normed2,
            gate.view(), up.view(), down.view(),
            cfg.hidden_dim, cfg.intermediate,
            new_seq,
        )?;

        // 2f. Residual.
        for i in 0..x.len() { x[i] += ffn_out[i]; }
    }

    // Step 3: final norm on the last position only — that's the only
    // row we need to project to logits for greedy sampling. Skipping
    // normalization on the other rows costs nothing today but saves
    // (new_seq-1) RMSNorms + linears at scale.
    let last_off = (new_seq - 1) * cfg.hidden_dim;
    let last = &x[last_off..last_off + cfg.hidden_dim];
    let last_normed = tensor_math::rmsnorm(last, &final_norm, cfg.eps)?;

    // Step 4: lm_head (tied to embed) — logits = embed @ last_normed.
    // `embed` has shape [vocab, hidden], already in the [out_dim,
    // in_dim] orientation `linear` wants. Dispatch on the embed's
    // dtype so a Q8 table runs through linear_q8 (zero-copy on the
    // weight bytes; just dequantizes block-by-block during the
    // matvec).
    let logits = embed_loaded.view().matvec(cfg.hidden_dim, cfg.vocab, &last_normed)?;

    // Step 5: every layer succeeded — commit the new positions to
    // the cache so the next call lines up at the right offset.
    cache.seq_len += new_seq;

    Some(logits)
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
