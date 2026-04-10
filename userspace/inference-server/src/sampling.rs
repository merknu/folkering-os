//! Token sampling: NaN-sanitization, repetition penalty, temperature, softmax,
//! Top-K + Top-P (nucleus) sampling.
//!
//! Phase B3: now takes a pre-allocated `logits_buf` (from
//! `InferenceEngine.logits_buf`) instead of bumping the BumpArena per token.

use libfolk::sys::random::random_u32;
use libtensor::transformer::argmax;

use crate::config::SamplingConfig;

/// Sample next token with repetition penalty and Top-P sampling (ULTRA 33, 31).
///
/// `logits_buf` must have `len() >= logits.len()` — only the leading
/// `vocab_size` slots are touched. Pre-allocated in `InferenceEngine.logits_buf`.
pub fn sample_with_penalties(
    logits: &[f32],
    recent_tokens: &[u32],
    logits_buf: &mut [f32],
    cfg: &SamplingConfig,
) -> u32 {
    let vocab_size = logits.len();

    // Phase B3: reuse pre-allocated buffer instead of arena.alloc_f32()
    if logits_buf.len() < vocab_size {
        return argmax(logits);
    }
    let logits_copy = &mut logits_buf[..vocab_size];
    logits_copy.copy_from_slice(logits);

    // ULTRA 31: Sanitize logits — clamp and NaN check
    for v in logits_copy.iter_mut() {
        if v.is_nan() || v.is_infinite() {
            *v = -100.0;
        } else if *v > 100.0 {
            *v = 100.0;
        } else if *v < -100.0 {
            *v = -100.0;
        }
    }

    // Repetition penalty
    let penalty_window = recent_tokens.len().min(cfg.rep_window);
    if penalty_window > 0 && cfg.rep_penalty != 1.0 {
        let start = recent_tokens.len().saturating_sub(cfg.rep_window);
        for &tok in &recent_tokens[start..] {
            if (tok as usize) < vocab_size {
                if logits_copy[tok as usize] > 0.0 {
                    logits_copy[tok as usize] /= cfg.rep_penalty;
                } else {
                    logits_copy[tok as usize] *= cfg.rep_penalty;
                }
            }
        }
    }

    // Apply temperature
    if cfg.temperature > 0.0 && cfg.temperature != 1.0 {
        let inv_t = 1.0 / cfg.temperature;
        for v in logits_copy.iter_mut() {
            *v *= inv_t;
        }
    }

    // ULTRA 33: Top-P (nucleus) sampling
    // 1. Softmax
    libtensor::ops::softmax(logits_copy);

    // 2. Top-K + Top-P nucleus sampling
    // NOTE: This is the naive O(vocab_size × max_nucleus) selection — heap-based
    // top-k is deferred to a future phase (see breezy-spinning-adleman plan B3).
    let mut cumsum = 0.0f32;
    let mut nucleus_count = 0usize;
    let mut nucleus_ids = [0u32; 128];
    let mut nucleus_probs = [0.0f32; 128];
    // Top-K limits the nucleus size (0 = no limit, use all 128 slots)
    let max_nucleus = if cfg.top_k > 0 {
        (cfg.top_k as usize).min(128)
    } else {
        128
    };

    for n in 0..max_nucleus {
        // Find max remaining prob
        let mut best_idx = 0usize;
        let mut best_prob = -1.0f32;
        for j in 0..vocab_size {
            if logits_copy[j] > best_prob {
                best_prob = logits_copy[j];
                best_idx = j;
            }
        }

        if best_prob <= 0.0 {
            break;
        }

        nucleus_ids[n] = best_idx as u32;
        nucleus_probs[n] = best_prob;
        nucleus_count = n + 1;
        logits_copy[best_idx] = -1.0; // mark as used

        cumsum += best_prob;
        if cumsum >= cfg.top_p {
            break;
        }
    }

    if nucleus_count == 0 {
        return 0; // fallback
    }

    // Renormalize nucleus probabilities
    let mut sum = 0.0f32;
    for i in 0..nucleus_count {
        sum += nucleus_probs[i];
    }
    if sum > 0.0 {
        for i in 0..nucleus_count {
            nucleus_probs[i] /= sum;
        }
    }

    // Sample from nucleus using kernel RNG
    let r = (random_u32() as f32) / (u32::MAX as f32);
    let mut cum = 0.0f32;
    for i in 0..nucleus_count {
        cum += nucleus_probs[i];
        if r < cum {
            return nucleus_ids[i];
        }
    }

    nucleus_ids[0] // fallback
}
