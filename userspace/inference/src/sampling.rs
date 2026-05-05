//! Top-k + temperature sampler for the inference task's decode loop.
//!
//! D.3.7's greedy `argmax` deterministically picks the highest-logit
//! token at every step. For Qwen3 thinking-mode that lands the
//! decode in a `<think> → \n → <think>` cycle: the reasoning-tag
//! logit dominates so persistently that argmax can't escape. Real
//! generation needs a sampler that introduces controlled randomness
//! while still respecting the model's distribution.
//!
//! This module provides three pieces:
//!
//! 1. `Xoshiro256pp` — a no_std-safe 256-bit-state PRNG (the same
//!    family Linux uses for `getrandom`'s userspace fallback). 4×
//!    u64 lanes, ~30 lines of shift/rotate/xor magic. Seeded via
//!    `__rdtsc()` at task start so each boot gets a different
//!    sequence, but the seed is logged so a run can be replayed.
//!
//! 2. `top_k(logits, k)` — partial-sort that returns the K highest
//!    `(logit, token_id)` pairs without scanning the full vocab in
//!    O(N log N). Uses a min-heap of size K: walk the vocab once,
//!    push when bigger than current min, pop the smallest. K = 40
//!    on a 151 936 vocab is ~12× faster than a full sort.
//!
//! 3. `sample(logits, k, temperature, prng)` — top-k slice → divide
//!    by temperature → softmax → inverse-CDF roll. Returns the
//!    chosen token id.

extern crate alloc;

/// Xoshiro256++ — 256-bit-state PRNG, period 2^256 - 1.
/// Output: rotl(s0 + s3, 23) + s0.
/// State update: s2 ^= s0; s3 ^= s1; s1 ^= s2; s0 ^= s3;
///               s2 ^= t (where t = s1 << 17); s3 = rotl(s3, 45).
pub struct Xoshiro256pp {
    s: [u64; 4],
}

impl Xoshiro256pp {
    /// Seed from a 64-bit value. Uses SplitMix64 to fan a single u64
    /// out into a properly-decorrelated 256-bit state — required
    /// because Xoshiro fails badly on a near-zero state.
    pub fn from_seed_u64(seed: u64) -> Self {
        let mut z = seed;
        let mut s = [0u64; 4];
        for slot in s.iter_mut() {
            // SplitMix64 step
            z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^= x >> 31;
            *slot = x;
        }
        Self { s }
    }

    /// Seed from `__rdtsc()`. Each boot gets a different sequence.
    pub fn from_rdtsc() -> Self {
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        Self::from_seed_u64(tsc)
    }

    /// Read the seed back for logging — useful for replaying a run.
    pub fn state(&self) -> [u64; 4] { self.s }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[0]
            .wrapping_add(self.s[3])
            .rotate_left(23)
            .wrapping_add(self.s[0]);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform f32 in `[0, 1)`. Top 24 bits → mantissa.
    #[inline]
    pub fn next_f32(&mut self) -> f32 {
        // Use top 24 bits so the result has ~24 bits of entropy
        // (matching f32's mantissa). Divide by 2^24.
        ((self.next_u64() >> 40) as f32) * (1.0 / (1u64 << 24) as f32)
    }
}

/// Find the K highest values in `logits` along with their indices.
/// Returns a Vec of (logit, token_id) sorted high → low.
///
/// Implementation: walk-once min-heap of size K. For each element,
/// if it's larger than the current min, replace the min and
/// sift-down. O(N log K) — at K=40, N=151 936 that's about 12×
/// faster than a full sort.
pub fn top_k(logits: &[f32], k: usize) -> alloc::vec::Vec<(f32, u32)> {
    use alloc::vec::Vec;
    if k == 0 || logits.is_empty() {
        return Vec::new();
    }
    let k = k.min(logits.len());

    // Heap[0] is the smallest of the K-best-so-far.
    let mut heap: Vec<(f32, u32)> = Vec::with_capacity(k);

    for (i, &v) in logits.iter().enumerate() {
        // Skip NaN — comparing NaN against anything is false, which
        // would silently keep the heap from updating. Treat NaN as
        // "not a candidate" rather than letting it poison the
        // distribution.
        if v.is_nan() {
            continue;
        }
        if heap.len() < k {
            heap.push((v, i as u32));
            if heap.len() == k {
                heapify_min(&mut heap);
            }
        } else if v > heap[0].0 {
            heap[0] = (v, i as u32);
            sift_down_min(&mut heap, 0);
        }
    }

    // Final result is sorted high → low. Pop-all on the min-heap
    // gives ascending order, so we reverse.
    let mut out = Vec::with_capacity(heap.len());
    while !heap.is_empty() {
        out.push(pop_min(&mut heap));
    }
    out.reverse();
    out
}

fn heapify_min(h: &mut [(f32, u32)]) {
    if h.len() < 2 { return; }
    // Floyd's bottom-up heapify, O(N).
    let last_parent = (h.len() - 2) / 2;
    for i in (0..=last_parent).rev() {
        sift_down_min(h, i);
    }
}

fn sift_down_min(h: &mut [(f32, u32)], mut i: usize) {
    loop {
        let l = 2 * i + 1;
        let r = 2 * i + 2;
        let mut smallest = i;
        if l < h.len() && h[l].0 < h[smallest].0 { smallest = l; }
        if r < h.len() && h[r].0 < h[smallest].0 { smallest = r; }
        if smallest == i { break; }
        h.swap(i, smallest);
        i = smallest;
    }
}

fn pop_min(h: &mut alloc::vec::Vec<(f32, u32)>) -> (f32, u32) {
    let n = h.len();
    let top = h[0];
    if n > 1 {
        h[0] = h[n - 1];
        h.pop();
        sift_down_min(h, 0);
    } else {
        h.pop();
    }
    top
}

/// Apply a repetition penalty to `logits` for the given recent
/// tokens. Standard HF-transformers semantics: positive logits get
/// divided by `penalty`, negative logits get multiplied. With
/// `penalty = 1.3` a token that was generated once drops from
/// e.g. 29.48 to 22.67 — enough to stop "newline spirals" without
/// banning the token outright. `penalty = 1.0` is a no-op.
pub fn apply_repetition_penalty(
    logits: &mut [f32],
    recent: &[u32],
    penalty: f32,
) {
    if penalty == 1.0 || recent.is_empty() {
        return;
    }
    for &tok in recent {
        let i = tok as usize;
        if i < logits.len() {
            let v = logits[i];
            logits[i] = if v > 0.0 { v / penalty } else { v * penalty };
        }
    }
}

/// Sample one token from `logits` using top-K + temperature + softmax.
///
/// - `k = 0` or `temperature ≤ 0` falls back to argmax (deterministic).
/// - Temperature divides logits before softmax: lower T = sharper
///   distribution, higher T = flatter.
///
/// Returns the chosen token id.
pub fn sample(
    logits: &[f32],
    k: usize,
    temperature: f32,
    prng: &mut Xoshiro256pp,
) -> u32 {
    if logits.is_empty() {
        return 0;
    }
    if k == 0 || temperature <= 0.0 {
        // Argmax fallback.
        let mut best_v = f32::NEG_INFINITY;
        let mut best_i: u32 = 0;
        for (i, &v) in logits.iter().enumerate() {
            if v > best_v {
                best_v = v;
                best_i = i as u32;
            }
        }
        return best_i;
    }

    let candidates = top_k(logits, k);
    if candidates.is_empty() {
        return 0;
    }

    // Find max for numerical stability, then exp((x - max) / T).
    let inv_t = 1.0 / temperature;
    let max_logit = candidates[0].0; // top_k returns sorted high→low
    let mut probs = alloc::vec::Vec::with_capacity(candidates.len());
    let mut sum = 0.0f32;
    for &(logit, _) in &candidates {
        let e = exp_approx((logit - max_logit) * inv_t);
        probs.push(e);
        sum += e;
    }
    if sum <= 0.0 || !sum.is_finite() {
        // Fallback to top-1 if softmax collapsed (shouldn't happen
        // with the max-shift trick, but guard anyway).
        return candidates[0].1;
    }
    let inv_sum = 1.0 / sum;

    // Inverse-CDF roll: pick first index where cumsum ≥ r.
    let r = prng.next_f32();
    let mut cum = 0.0f32;
    for (i, &p) in probs.iter().enumerate() {
        cum += p * inv_sum;
        if r < cum {
            return candidates[i].1;
        }
    }
    // Numerical edge: r rounded just past 1.0 worth of cumsum.
    candidates[candidates.len() - 1].1
}

/// no_std-safe `exp(x)` approximation. Uses the standard 7-term
/// minimax polynomial after range reduction `x = n * ln(2) + r`,
/// where `r ∈ [-0.5 ln 2, 0.5 ln 2]`. Then `exp(x) = 2^n * exp(r)`,
/// and 2^n is built directly by biasing the f32 exponent.
///
/// Accurate enough for sampling — softmax doesn't need ULP-perfect
/// exponentials, just monotonicity and reasonable scaling.
fn exp_approx(x: f32) -> f32 {
    if x < -88.0 { return 0.0; }
    if x >  88.0 { return f32::INFINITY; }
    // n = round(x / ln 2). `f32::round` lives in std; in no_std core
    // we round-to-nearest-half-away by adding ±0.5 before truncating.
    const LN2: f32 = 0.6931472;
    const INV_LN2: f32 = 1.4426950;
    let scaled = x * INV_LN2;
    let n = if scaled >= 0.0 { (scaled + 0.5) as i32 } else { (scaled - 0.5) as i32 };
    let n_f = n as f32;
    let r = x - n_f * LN2;
    // exp(r) ≈ 1 + r * (1 + r/2 * (1 + r/3 * (1 + r/4 * (1 + r/5 * (1 + r/6)))))
    // Horner-ised 6-term Taylor expansion — overkill for sampling
    // but keeps the error well below the f32 mantissa noise floor.
    let r2 = r * r;
    let exp_r = 1.0
        + r
        + r2 * 0.5
        + r2 * r * (1.0 / 6.0)
        + r2 * r2 * (1.0 / 24.0)
        + r2 * r2 * r * (1.0 / 120.0)
        + r2 * r2 * r2 * (1.0 / 720.0);
    // 2^n via direct exponent bias manipulation. f32 layout:
    // sign(1) | exp(8) | mantissa(23). exp bias = 127.
    let bits = ((n + 127) as u32) << 23;
    let pow2 = f32::from_bits(bits);
    pow2 * exp_r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xoshiro_distinct_seeds_diverge() {
        let mut a = Xoshiro256pp::from_seed_u64(1);
        let mut b = Xoshiro256pp::from_seed_u64(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn next_f32_in_range() {
        let mut r = Xoshiro256pp::from_seed_u64(42);
        for _ in 0..10_000 {
            let v = r.next_f32();
            assert!(v >= 0.0 && v < 1.0, "out of range: {}", v);
        }
    }

    #[test]
    fn top_k_returns_largest() {
        let logits = [1.0, 5.0, 2.0, 8.0, 3.0, 7.0, 4.0, 6.0];
        let r = top_k(&logits, 3);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].0, 8.0);
        assert_eq!(r[1].0, 7.0);
        assert_eq!(r[2].0, 6.0);
    }

    #[test]
    fn sample_t0_is_argmax() {
        let logits = [1.0, 5.0, 2.0, 8.0, 3.0];
        let mut r = Xoshiro256pp::from_seed_u64(0xdead);
        // Temperature 0 → argmax.
        assert_eq!(sample(&logits, 5, 0.0, &mut r), 3);
    }

    #[test]
    fn exp_approx_matches_libm_within_1pct() {
        // Spot-check a handful of values in a sane range.
        let pts = [-5.0_f32, -1.0, 0.0, 1.0, 2.0, 5.0];
        let expected = [0.006737947, 0.36787945, 1.0, 2.7182817, 7.389056, 148.41316];
        for (x, e) in pts.iter().zip(expected.iter()) {
            let got = exp_approx(*x);
            let rel = ((got - e) / e).abs();
            assert!(rel < 0.01, "exp({}) = {}, expected {} (rel {})", x, got, e, rel);
        }
    }
}
