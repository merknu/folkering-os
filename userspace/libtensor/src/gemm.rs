//! GEMM — General Matrix Multiply (ULTRA 1, 9, 10, 11)
//!
//! Integer-only Q4_0×Q8_0 → f32 with AVX2 acceleration.
//! Batch-ready (N>1 for prompt processing / speculative decoding).
//! Fused activation (SiLU applied in-register before write-back).
//! Cooperative yield every `yield_every` rows.
//!
//! Core operation: C[M×N] += dequant(A_q8[M×K] × B_q4[K×N])
//! Where A = activations (quantized to Q8_0 on-the-fly)
//! and B = weights (stored as Q4_0 from GGUF).

use crate::arena::BumpArena;
use crate::quantize::{self, Q4_0_BLOCK_SIZE, Q8_0_BLOCK_SIZE};
use crate::simd;
use crate::FuseOp;
use crate::ops;

/// Matrix multiply: C[M×N] += A(Q8_0)[M×K] × B(Q4_0)[K×N]
///
/// # Arguments
/// - `c`: output buffer, M×N f32 values (must be pre-zeroed or will accumulate)
/// - `a_q8`: activation data in Q8_0 format (M×K values, quantized)
/// - `b_q4`: weight data in Q4_0 format (K×N values, from GGUF)
/// - `m`: number of rows (batch size or sequence length)
/// - `k`: inner dimension (model dimension)
/// - `n`: number of columns (output dimension)
/// - `fuse`: optional activation to apply after accumulation
/// - `yield_every`: yield CPU every N output rows (0 = never yield)
///
/// # Layout
/// - A is row-major: row i starts at block (i * K/32) in Q8_0 format
/// - B is row-major: element (k, n) is at block ((k * N + n) / 32) in Q4_0 format
///   BUT typically B is stored column-major for efficient dot products:
///   column n's K values are contiguous in Q4_0 blocks.
pub fn gemm_q4_q8(
    c: &mut [f32],
    a_q8: &[u8],
    b_q4: &[u8],
    m: usize,
    k: usize,
    n: usize,
    fuse: FuseOp,
    yield_every: usize,
) {
    debug_assert!(c.len() >= m * n);

    let k_blocks = k / 32; // number of Q blocks per vector

    for row in 0..m {
        // Cooperative yield
        if yield_every > 0 && row > 0 && row % yield_every == 0 {
            libfolk::sys::yield_cpu();
        }

        let a_row_offset = row * k_blocks * Q8_0_BLOCK_SIZE;

        for col in 0..n {
            let b_col_offset = col * k_blocks * Q4_0_BLOCK_SIZE;

            // Dot product over K/32 blocks
            let sum = gemm_dot_dispatch(
                &a_q8[a_row_offset..],
                &b_q4[b_col_offset..],
                k_blocks,
            );

            // Apply fused activation
            let result = match fuse {
                FuseOp::None => sum,
                FuseOp::SiLU => ops::silu_f32(sum),
            };

            c[row * n + col] += result;
        }
    }
}

/// Dispatch dot product to AVX2 or scalar path based on CPU features.
#[inline]
fn gemm_dot_dispatch(a_q8: &[u8], b_q4: &[u8], n_blocks: usize) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        if simd::has_avx2() {
            return unsafe { gemm_dot_avx2(a_q8, b_q4, n_blocks) };
        }
    }
    gemm_dot_scalar(a_q8, b_q4, n_blocks)
}

/// Scalar dot product over Q4_0 × Q8_0 blocks.
fn gemm_dot_scalar(a_q8: &[u8], b_q4: &[u8], n_blocks: usize) -> f32 {
    let mut sum = 0.0f32;
    for blk in 0..n_blocks {
        let a_off = blk * Q8_0_BLOCK_SIZE;
        let b_off = blk * Q4_0_BLOCK_SIZE;
        sum += quantize::dot_q4_0_q8_0_block(
            &b_q4[b_off..b_off + Q4_0_BLOCK_SIZE],
            &a_q8[a_off..a_off + Q8_0_BLOCK_SIZE],
        );
    }
    sum
}

/// AVX2-accelerated dot product over Q4_0 × Q8_0 blocks (ULTRA 10).
///
/// For each block pair:
/// 1. Load Q4_0 nibbles → expand to u8 (0-15)
/// 2. Load Q8_0 i8 values
/// 3. _mm256_maddubs_epi16(q4_u8, q8_i8) → i16 products
/// 4. _mm256_madd_epi16 → i32 accumulation
/// 5. Apply zero-point correction (subtract 8 * sum_q8)
/// 6. Scale by q4_scale * q8_scale
///
/// # Safety
/// Caller must ensure AVX2 is available.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn gemm_dot_avx2(a_q8: &[u8], b_q4: &[u8], n_blocks: usize) -> f32 {
    use core::arch::x86_64::*;

    let mut sum_f32 = 0.0f32;

    for blk in 0..n_blocks {
        let a_off = blk * Q8_0_BLOCK_SIZE;
        let b_off = blk * Q4_0_BLOCK_SIZE;

        // Get scales
        let q8_scale = quantize::q8_0_block_scale(&a_q8[a_off..]);
        let q4_scale = quantize::q4_0_block_scale(&b_q4[b_off..]);

        // Expand Q4_0 nibbles to 32 u8 values
        let mut q4_expanded = [0u8; 32];
        quantize::q4_0_to_u8_block(&b_q4[b_off..], &mut q4_expanded);

        // Load expanded Q4_0 as u8 and Q8_0 as i8 into AVX2 registers
        let va = _mm256_loadu_si256(q4_expanded.as_ptr() as *const __m256i);
        let vb = _mm256_loadu_si256(a_q8[a_off + 2..].as_ptr() as *const __m256i);

        // u8 × i8 → i16 pairs with adjacent addition
        let prod16 = _mm256_maddubs_epi16(va, vb);

        // i16 → i32 with horizontal pair addition
        let ones = _mm256_set1_epi16(1);
        let prod32 = _mm256_madd_epi16(prod16, ones);

        // Horizontal sum of 8 i32 values
        let hi128 = _mm256_extracti128_si256(prod32, 1);
        let lo128 = _mm256_castsi256_si128(prod32);
        let sum128 = _mm_add_epi32(lo128, hi128);
        let shuf = _mm_shuffle_epi32(sum128, 0b_01_00_11_10);
        let sum64 = _mm_add_epi32(sum128, shuf);
        let shuf2 = _mm_shuffle_epi32(sum64, 0b_00_00_00_01);
        let sum32 = _mm_add_epi32(sum64, shuf2);
        let dot_int = _mm_cvtsi128_si32(sum32);

        // Zero-point correction: subtract 8 * sum(q8_values)
        // Sum Q8_0 values
        let mut sum_q8 = 0i32;
        let q8_vals = &a_q8[a_off + 2..a_off + 34];
        for i in 0..32 {
            sum_q8 += (q8_vals[i] as i8) as i32;
        }

        let corrected = dot_int - 8 * sum_q8;
        sum_f32 += corrected as f32 * q4_scale * q8_scale;
    }

    sum_f32
}

/// Multiply f32 matrix A[M×K] by f32 matrix B[K×N], store in C[M×N].
/// Simple implementation for operations that need f32 (softmax output, etc).
pub fn gemm_f32(
    c: &mut [f32],
    a: &[f32],
    b: &[f32],
    m: usize,
    k: usize,
    n: usize,
    yield_every: usize,
) {
    debug_assert!(c.len() >= m * n);
    debug_assert!(a.len() >= m * k);
    debug_assert!(b.len() >= k * n);

    for row in 0..m {
        if yield_every > 0 && row > 0 && row % yield_every == 0 {
            libfolk::sys::yield_cpu();
        }

        for col in 0..n {
            let mut sum = 0.0f32;

            if simd::has_avx2() {
                #[cfg(target_arch = "x86_64")]
                {
                    // A row: a[row*k .. row*k+k], B col: b[col, col+n, col+2n, ...]
                    // B is row-major, so we need strided access. For now, use scalar.
                    // TODO: transpose B for vectorized access
                    for i in 0..k {
                        sum += a[row * k + i] * b[i * n + col];
                    }
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    for i in 0..k {
                        sum += a[row * k + i] * b[i * n + col];
                    }
                }
            } else {
                for i in 0..k {
                    sum += a[row * k + i] * b[i * n + col];
                }
            }

            c[row * n + col] += sum;
        }
    }
}

/// Quantize f32 activations to Q8_0 and multiply with Q4_0 weights.
///
/// Convenience function: quantizes `a_f32` on-the-fly, then calls `gemm_q4_q8`.
/// Uses arena for temporary Q8_0 buffer.
pub fn gemm_f32_x_q4(
    c: &mut [f32],
    a_f32: &[f32],
    b_q4: &[u8],
    m: usize,
    k: usize,
    n: usize,
    fuse: FuseOp,
    yield_every: usize,
    _arena: &BumpArena,
) {
    // Direct f32 × dequant(Q4_0) — no Q8_0 quantization of activations.
    let n_blocks = k / 32;
    let q4_row_bytes = n_blocks * Q4_0_BLOCK_SIZE;

    for row in 0..m {
        if yield_every > 0 && row > 0 && row % yield_every == 0 {
            libfolk::sys::yield_cpu();
        }
        let a_row = &a_f32[row * k..(row + 1) * k];

        for col in 0..n {
            let b_col_offset = col * q4_row_bytes;
            let mut acc = 0.0f32;

            for blk in 0..n_blocks {
                let blk_start = b_col_offset + blk * Q4_0_BLOCK_SIZE;
                let scale = quantize::q4_0_block_scale(&b_q4[blk_start..]);
                let a_base = blk * 32;

                for i in 0..16 {
                    let byte = b_q4[blk_start + 2 + i];
                    let lo = ((byte & 0x0F) as i8 - 8) as f32;
                    let hi = (((byte >> 4) & 0x0F) as i8 - 8) as f32;
                    acc += a_row[a_base + i * 2] * lo * scale;
                    acc += a_row[a_base + i * 2 + 1] * hi * scale;
                }
            }

            let idx = row * n + col;
            c[idx] += acc;

            match fuse {
                FuseOp::SiLU => { c[idx] = crate::ops::silu_f32(c[idx]); }
                FuseOp::None => {}
            }
        }
    }
}

/// GEMM: f32 activations × Q8_0 weights → f32 output
///
/// For models where some weights (embedding, output) are Q8_0 instead of Q4_0.
/// Direct scalar dot product: no quantization of activations needed.
pub fn gemm_f32_x_q8(
    c: &mut [f32],
    a_f32: &[f32],
    b_q8: &[u8],
    m: usize,
    k: usize,
    n: usize,
    fuse: FuseOp,
    yield_every: usize,
    _arena: &BumpArena,
) {
    let n_blocks = k / 32;
    let q8_row_bytes = n_blocks * Q8_0_BLOCK_SIZE;

    for row in 0..m {
        let a_row = &a_f32[row * k..(row + 1) * k];

        for col in 0..n {
            let b_col_offset = col * q8_row_bytes;
            let mut acc = 0.0f32;

            for blk in 0..n_blocks {
                let blk_offset = b_col_offset + blk * Q8_0_BLOCK_SIZE;
                let scale = quantize::q8_0_block_scale(&b_q8[blk_offset..]);

                let a_base = blk * 32;
                for i in 0..32 {
                    let q_val = b_q8[blk_offset + 2 + i] as i8;
                    acc += a_row[a_base + i] * (q_val as f32) * scale;
                }
            }

            let idx = row * n + col;
            c[idx] += acc;

            // Apply fused operation
            match fuse {
                FuseOp::SiLU => {
                    let x = c[idx];
                    c[idx] = x / (1.0 + crate::ops::fast_exp(-x));
                }
                FuseOp::None => {}
            }
        }

        if yield_every > 0 && row > 0 && row % yield_every == 0 {
            libfolk::sys::yield_cpu();
        }
    }
}
