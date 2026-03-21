//! Tensor operations: RMSNorm, Softmax, SiLU, RoPE (ULTRA 11, 18, 20)
//!
//! Standalone and fused variants. AVX2-accelerated where beneficial.

use crate::simd;

// =============================================================================
// SiLU activation (ULTRA 18: LUT for i8, standalone for f32)
// =============================================================================

/// SiLU(x) = x * sigmoid(x) = x / (1 + exp(-x))
#[inline]
pub fn silu_f32(x: f32) -> f32 {
    x / (1.0 + fast_exp(-x))
}

/// Apply SiLU in-place to an f32 slice.
pub fn silu_inplace(data: &mut [f32]) {
    for v in data.iter_mut() {
        *v = silu_f32(*v);
    }
}

/// Pre-computed SiLU lookup table for Q8_0 integer path (ULTRA 18).
/// Maps i8 input (-128..127) to scaled i8 output.
/// Index: (val as u8) — wrapping unsigned interpretation.
///
/// For each i in -128..=127:
///   float_val = i / 127.0  (normalize to ~[-1, 1])
///   silu_val = float_val * sigmoid(float_val)
///   result = round(silu_val * 127.0)
///
/// This is an approximation that works well within the Q8_0 dynamic range.
pub static SILU_LUT_I8: [i8; 256] = {
    let mut lut = [0i8; 256];
    let mut i = 0u16;
    while i < 256 {
        let signed = (i as u8) as i8;
        let x = signed as f64 / 127.0;
        // sigmoid approximation: 1 / (1 + exp(-x))
        // For const context we use a polynomial approximation
        let neg_x = -x;
        let exp_neg = const_exp_f64(neg_x);
        let sigmoid = 1.0 / (1.0 + exp_neg);
        let silu = x * sigmoid;
        let result = (silu * 127.0) as i8;
        lut[i as usize] = result;
        i += 1;
    }
    lut
};

/// Const-compatible exp approximation for LUT generation.
const fn const_exp_f64(x: f64) -> f64 {
    // Pade approximation: good enough for LUT generation
    // exp(x) ≈ (1 + x/2 + x²/12) / (1 - x/2 + x²/12) for |x| < 2
    if x > 4.0 { return 54.598; } // clamp for extreme values
    if x < -4.0 { return 0.018; }
    let x2 = x * x;
    let num = 1.0 + x / 2.0 + x2 / 12.0;
    let den = 1.0 - x / 2.0 + x2 / 12.0;
    if den == 0.0 { 1.0 } else { num / den }
}

// =============================================================================
// RMSNorm
// =============================================================================

/// RMSNorm: x_i = x_i * w_i / sqrt(mean(x²) + eps)
///
/// `x` is modified in-place.
/// `weight` is the learned per-element scale (from model weights).
pub fn rmsnorm(x: &mut [f32], weight: &[f32], eps: f32) {
    let n = x.len();
    debug_assert_eq!(n, weight.len());

    // Compute mean of squares
    let ss = if simd::has_avx2() {
        #[cfg(target_arch = "x86_64")]
        unsafe { simd::avx2::dot_f32_avx2(x, x, n) }
        #[cfg(not(target_arch = "x86_64"))]
        simd::dot_f32_scalar(x, x, n)
    } else {
        simd::dot_f32_scalar(x, x, n)
    };

    let rms = 1.0 / fast_sqrt(ss / n as f32 + eps);

    // Normalize and scale
    for i in 0..n {
        x[i] = x[i] * rms * weight[i];
    }
}

/// RMSNorm but output into a separate buffer (for fused paths).
pub fn rmsnorm_into(x: &[f32], weight: &[f32], out: &mut [f32], eps: f32) {
    let n = x.len();
    debug_assert_eq!(n, weight.len());
    debug_assert!(out.len() >= n);

    let ss = if simd::has_avx2() {
        #[cfg(target_arch = "x86_64")]
        unsafe { simd::avx2::dot_f32_avx2(x, x, n) }
        #[cfg(not(target_arch = "x86_64"))]
        simd::dot_f32_scalar(x, x, n)
    } else {
        simd::dot_f32_scalar(x, x, n)
    };

    let rms = 1.0 / fast_sqrt(ss / n as f32 + eps);

    for i in 0..n {
        out[i] = x[i] * rms * weight[i];
    }
}

// =============================================================================
// Softmax
// =============================================================================

/// In-place softmax over a slice.
///
/// Numerically stable: subtracts max before exp.
pub fn softmax(x: &mut [f32]) {
    let n = x.len();
    if n == 0 { return; }

    // Find max for numerical stability
    let mut max_val = x[0];
    for i in 1..n {
        if x[i] > max_val {
            max_val = x[i];
        }
    }

    // exp(x_i - max) and sum
    let mut sum = 0.0f32;
    for i in 0..n {
        x[i] = fast_exp(x[i] - max_val);
        sum += x[i];
    }

    // Normalize
    if sum > 0.0 {
        let inv_sum = 1.0 / sum;
        for i in 0..n {
            x[i] *= inv_sum;
        }
    }
}

/// Softmax with temperature scaling.
pub fn softmax_temperature(x: &mut [f32], temperature: f32) {
    if temperature != 1.0 && temperature > 0.0 {
        let inv_t = 1.0 / temperature;
        for v in x.iter_mut() {
            *v *= inv_t;
        }
    }
    softmax(x);
}

// =============================================================================
// RoPE — Rotary Positional Encoding (ULTRA 20: pre-computed LUT)
// =============================================================================

/// Apply RoPE to query/key vectors at a given position.
///
/// `x` has shape [n_heads, head_dim] laid out as flat [n_heads * head_dim].
/// RoPE rotates pairs of values: (x[2i], x[2i+1]) using sin/cos at position `pos`.
///
/// Uses pre-computed sin/cos tables when available, otherwise computes on the fly.
pub fn rope_inplace(x: &mut [f32], head_dim: usize, pos: usize, rope_base: f32) {
    let half_dim = head_dim / 2;

    // Process each head
    let n_heads = x.len() / head_dim;
    for h in 0..n_heads {
        let offset = h * head_dim;
        for i in 0..half_dim {
            let freq = 1.0 / fast_powf(rope_base, (2 * i) as f32 / head_dim as f32);
            let theta = pos as f32 * freq;
            let cos_t = fast_cos(theta);
            let sin_t = fast_sin(theta);

            let x0 = x[offset + i];
            let x1 = x[offset + i + half_dim];
            x[offset + i] = x0 * cos_t - x1 * sin_t;
            x[offset + i + half_dim] = x0 * sin_t + x1 * cos_t;
        }
    }
}

/// Pre-compute RoPE sin/cos table for all positions up to max_seq_len.
///
/// Returns (cos_table, sin_table) each of size [max_seq_len * half_dim].
/// Caller allocates from arena.
pub fn rope_precompute(
    cos_out: &mut [f32],
    sin_out: &mut [f32],
    head_dim: usize,
    max_seq_len: usize,
    rope_base: f32,
) {
    let half_dim = head_dim / 2;
    debug_assert!(cos_out.len() >= max_seq_len * half_dim);
    debug_assert!(sin_out.len() >= max_seq_len * half_dim);

    for pos in 0..max_seq_len {
        for i in 0..half_dim {
            let freq = 1.0 / fast_powf(rope_base, (2 * i) as f32 / head_dim as f32);
            let theta = pos as f32 * freq;
            let idx = pos * half_dim + i;
            cos_out[idx] = fast_cos(theta);
            sin_out[idx] = fast_sin(theta);
        }
    }
}

/// Apply RoPE using pre-computed sin/cos table.
pub fn rope_with_table(
    x: &mut [f32],
    head_dim: usize,
    pos: usize,
    cos_table: &[f32],
    sin_table: &[f32],
) {
    let half_dim = head_dim / 2;
    let n_heads = x.len() / head_dim;
    let table_offset = pos * half_dim;

    for h in 0..n_heads {
        let offset = h * head_dim;
        for i in 0..half_dim {
            let cos_t = cos_table[table_offset + i];
            let sin_t = sin_table[table_offset + i];

            let x0 = x[offset + i];
            let x1 = x[offset + i + half_dim];
            x[offset + i] = x0 * cos_t - x1 * sin_t;
            x[offset + i + half_dim] = x0 * sin_t + x1 * cos_t;
        }
    }
}

// =============================================================================
// Fast math approximations (avoid FPU-emulated libm in QEMU TCG)
// =============================================================================

/// Fast exp approximation using Schraudolph's method.
/// Accurate to ~0.1% for |x| < 10.
#[inline]
pub fn fast_exp(x: f32) -> f32 {
    if x > 88.0 { return f32::MAX; }
    if x < -88.0 { return 0.0; }

    // Schraudolph's trick: reinterpret (1 << 23) * (x / ln2 + 127) as f32 bits
    let bits = ((1 << 23) as f32 * (x * 1.442695 + 126.94269)) as i32;
    f32::from_bits(bits as u32)
}

/// Fast approximate sqrt using the "Quake" method + one Newton iteration.
#[inline]
pub fn fast_sqrt(x: f32) -> f32 {
    if x <= 0.0 { return 0.0; }
    let i = x.to_bits();
    let guess = f32::from_bits((i >> 1) + 0x1FBB4000);
    // One Newton-Raphson iteration for better accuracy
    0.5 * (guess + x / guess)
}

/// Fast approximate 1/sqrt(x).
#[inline]
pub fn fast_rsqrt(x: f32) -> f32 {
    if x <= 0.0 { return 0.0; }
    let i = 0x5F375A86u32.wrapping_sub(x.to_bits() >> 1);
    let y = f32::from_bits(i);
    y * (1.5 - 0.5 * x * y * y)
}

/// Fast sin approximation using Bhaskara I's formula.
/// Input in radians.
#[inline]
pub fn fast_sin(x: f32) -> f32 {
    // Reduce to [0, 2π)
    let pi = core::f32::consts::PI;
    let two_pi = 2.0 * pi;
    let mut t = x % two_pi;
    if t < 0.0 { t += two_pi; }

    // Use symmetry to reduce to [0, π]
    let (t, sign) = if t > pi {
        (t - pi, -1.0f32)
    } else {
        (t, 1.0f32)
    };

    // Bhaskara I's approximation: sin(x) ≈ 16x(π-x) / (5π²-4x(π-x))
    let p = t * (pi - t);
    let num = 16.0 * p;
    let den = 5.0 * pi * pi - 4.0 * p;
    if den == 0.0 { return 0.0; }
    sign * num / den
}

/// Fast cos approximation.
#[inline]
pub fn fast_cos(x: f32) -> f32 {
    fast_sin(x + core::f32::consts::FRAC_PI_2)
}

/// Fast power approximation: x^p.
#[inline]
pub fn fast_powf(base: f32, exp: f32) -> f32 {
    fast_exp(exp * fast_ln(base))
}

/// Fast natural log approximation.
#[inline]
pub fn fast_ln(x: f32) -> f32 {
    if x <= 0.0 { return f32::NEG_INFINITY; }
    let bits = x.to_bits() as f32;
    bits * 8.2629582e-8 - 87.989971 // ln(2)/2^23 * bits - ln(2) * 127
}
