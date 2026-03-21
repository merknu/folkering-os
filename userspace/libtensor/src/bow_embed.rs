//! Bag-of-Words Embedding — Runtime 384-dim vectors (M36)
//!
//! For runtime queries that don't go through the pre-computed embedding pipeline.
//! Produces 384-dim vectors compatible with Synapse's existing vector search
//! (all-MiniLM-L6-v2 format, though lower quality than the real model).
//!
//! Approach: learned projection matrix (~100KB) maps word-frequency vectors
//! to the same embedding space. For cold-start, we use random projections
//! with a fixed seed for deterministic results.
//!
//! Zero-allocation: all computation uses caller-provided buffers.

use crate::arena::BumpArena;

/// Output embedding dimension (matches Synapse's all-MiniLM-L6-v2)
pub const EMBED_DIM: usize = 384;

/// Maximum vocabulary for BoW (most frequent English words + code tokens)
pub const BOW_VOCAB_SIZE: usize = 4096;

/// Hash a token (word) to a vocabulary index using FNV-1a.
/// Returns index in [0, BOW_VOCAB_SIZE).
#[inline]
fn hash_token(token: &[u8]) -> usize {
    let mut h: u32 = 0x811c9dc5; // FNV offset basis
    for &b in token {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193); // FNV prime
    }
    (h as usize) % BOW_VOCAB_SIZE
}

/// Deterministic pseudo-random f32 in [-1, 1] from seed.
/// Used for random projection matrix generation.
#[inline]
fn prng_f32(seed: u32) -> f32 {
    // Xorshift32
    let mut x = seed;
    if x == 0 { x = 1; }
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    // Map to [-1, 1]
    (x as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// Compute a 384-dim embedding for the given text using Bag-of-Words
/// with random projection.
///
/// # Arguments
/// - `text`: input text (UTF-8)
/// - `output`: output buffer, must be at least EMBED_DIM f32s
/// - `arena`: bump arena for temporary BoW vector
///
/// # Algorithm
/// 1. Tokenize text into whitespace-separated words
/// 2. Hash each word to a BOW_VOCAB_SIZE-dim sparse vector (frequency counts)
/// 3. Project sparse vector through random projection matrix → 384-dim
/// 4. L2-normalize the result
///
/// The projection matrix is generated on-the-fly from a fixed seed,
/// so no static data is needed. Each "column" of the projection matrix
/// is computed only for non-zero BoW entries (sparse multiply).
pub fn bow_embed(text: &[u8], output: &mut [f32], arena: &BumpArena) {
    debug_assert!(output.len() >= EMBED_DIM);

    // Step 1: Build sparse BoW vector in arena
    // We use a compact representation: (index, count) pairs
    // Since we process sequentially, we use a dense temporary vector
    let bow = match arena.alloc_f32(BOW_VOCAB_SIZE) {
        Some(v) => v,
        None => {
            // Arena exhausted — return zero vector
            for i in 0..EMBED_DIM {
                output[i] = 0.0;
            }
            return;
        }
    };

    // Tokenize and count (simple whitespace + lowercase)
    let mut word_start = 0usize;
    let mut in_word = false;
    let mut total_words = 0u32;

    for i in 0..text.len() + 1 {
        let is_sep = if i < text.len() {
            let b = text[i];
            b == b' ' || b == b'\n' || b == b'\t' || b == b'\r'
                || b == b',' || b == b'.' || b == b';' || b == b':'
                || b == b'(' || b == b')' || b == b'[' || b == b']'
                || b == b'{' || b == b'}'
        } else {
            true // end of text
        };

        if is_sep {
            if in_word && i > word_start {
                let word = &text[word_start..i];
                if word.len() >= 2 {
                    // Lowercase hash
                    let idx = hash_token_lowercase(word);
                    bow[idx] += 1.0;
                    total_words += 1;
                }
                in_word = false;
            }
        } else if !in_word {
            word_start = i;
            in_word = true;
        }
    }

    // Normalize BoW by total words (TF normalization)
    if total_words > 0 {
        let inv_total = 1.0 / total_words as f32;
        for i in 0..BOW_VOCAB_SIZE {
            bow[i] *= inv_total;
        }
    }

    // Step 2: Random projection — sparse multiply
    // output[d] = sum over non-zero bow[v] of bow[v] * proj[v][d]
    // where proj[v][d] = prng_f32(v * EMBED_DIM + d)
    for d in 0..EMBED_DIM {
        output[d] = 0.0;
    }

    for v in 0..BOW_VOCAB_SIZE {
        if bow[v] == 0.0 {
            continue; // Skip zero entries (sparse)
        }
        let weight = bow[v];
        let base_seed = (v * EMBED_DIM) as u32;
        for d in 0..EMBED_DIM {
            output[d] += weight * prng_f32(base_seed + d as u32);
        }
    }

    // Step 3: L2-normalize
    let mut norm_sq = 0.0f32;
    for d in 0..EMBED_DIM {
        norm_sq += output[d] * output[d];
    }
    if norm_sq > 0.0 {
        let inv_norm = crate::ops::fast_rsqrt(norm_sq);
        for d in 0..EMBED_DIM {
            output[d] *= inv_norm;
        }
    }
}

/// Hash a token with implicit lowercasing.
#[inline]
fn hash_token_lowercase(token: &[u8]) -> usize {
    let mut h: u32 = 0x811c9dc5;
    for &b in token {
        let lower = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
        h ^= lower as u32;
        h = h.wrapping_mul(0x01000193);
    }
    (h as usize) % BOW_VOCAB_SIZE
}

/// Compute cosine similarity between two 384-dim vectors.
#[inline]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert!(a.len() >= EMBED_DIM && b.len() >= EMBED_DIM);

    let dot = if crate::simd::has_avx2() {
        #[cfg(target_arch = "x86_64")]
        unsafe { crate::simd::avx2::dot_f32_avx2(a, b, EMBED_DIM) }
        #[cfg(not(target_arch = "x86_64"))]
        crate::simd::dot_f32_scalar(a, b, EMBED_DIM)
    } else {
        crate::simd::dot_f32_scalar(a, b, EMBED_DIM)
    };

    // Vectors should already be L2-normalized, but clamp just in case
    if dot > 1.0 { 1.0 } else if dot < -1.0 { -1.0 } else { dot }
}
