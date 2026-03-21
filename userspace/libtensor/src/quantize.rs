//! Quantization formats: Q4_0 and Q8_0
//!
//! Q4_0: 32 values per block = 2 bytes f16 scale + 16 bytes data = 18 bytes
//! Q8_0: 32 values per block = 4 bytes f32 scale + 32 bytes i8 data = 36 bytes
//!
//! These match the GGML/GGUF specification exactly.

/// Q4_0 block: 32 4-bit values with f16 scale
/// Layout: [f16 scale (2 bytes)][16 bytes of nibble pairs]
pub const Q4_0_BLOCK_SIZE: usize = 18; // 2 + 16
pub const Q4_0_BLOCK_VALUES: usize = 32;

/// Q8_0 block: 32 i8 values with f16 scale
/// Layout: [f16 scale (2 bytes)][32 bytes of i8 values]
/// Matches GGML spec: `struct block_q8_0 { ggml_half d; int8_t qs[32]; }`
pub const Q8_0_BLOCK_SIZE: usize = 34; // 2 + 32
pub const Q8_0_BLOCK_VALUES: usize = 32;

/// Dequantize a single Q4_0 block (32 values) into f32 output.
///
/// Q4_0 format per block:
/// - bytes [0..2]: f16 scale factor (IEEE 754 half-precision)
/// - bytes [2..18]: 16 bytes, each containing two 4-bit values
///   - low nibble = first value, high nibble = second value
///   - values are unsigned 0-15, centered by subtracting 8
///
/// Dequantized value = (nibble - 8) * scale
#[inline]
pub fn dequantize_q4_0_block(block: &[u8], out: &mut [f32]) {
    debug_assert!(block.len() >= Q4_0_BLOCK_SIZE);
    debug_assert!(out.len() >= Q4_0_BLOCK_VALUES);

    let scale = f16_to_f32(u16::from_le_bytes([block[0], block[1]]));

    // GGML Q4_0 layout: lo nibbles → first half (0-15), hi nibbles → second half (16-31)
    for i in 0..16 {
        let byte = block[2 + i];
        let lo = (byte & 0x0F) as i8 - 8;
        let hi = ((byte >> 4) & 0x0F) as i8 - 8;
        out[i] = lo as f32 * scale;       // lo → position i (first half)
        out[16 + i] = hi as f32 * scale;  // hi → position i+16 (second half)
    }
}

/// Dequantize a single Q8_0 block (32 values) into f32 output.
///
/// Q8_0 format per block (GGML spec):
/// - bytes [0..2]: f16 scale factor (IEEE 754 half-precision)
/// - bytes [2..34]: 32 signed i8 values
///
/// Dequantized value = i8_val * scale
#[inline]
pub fn dequantize_q8_0_block(block: &[u8], out: &mut [f32]) {
    debug_assert!(block.len() >= Q8_0_BLOCK_SIZE);
    debug_assert!(out.len() >= Q8_0_BLOCK_VALUES);

    let scale = f16_to_f32(u16::from_le_bytes([block[0], block[1]]));

    for i in 0..32 {
        out[i] = (block[2 + i] as i8) as f32 * scale;
    }
}

/// Quantize f32 values to Q8_0 format.
///
/// Finds max absolute value in each block of 32, computes scale,
/// then rounds each value to nearest i8.
pub fn quantize_f32_to_q8_0(input: &[f32], output: &mut [u8]) {
    let n_blocks = (input.len() + 31) / 32;
    debug_assert!(output.len() >= n_blocks * Q8_0_BLOCK_SIZE);

    for block_idx in 0..n_blocks {
        let start = block_idx * 32;
        let end = (start + 32).min(input.len());
        let out_offset = block_idx * Q8_0_BLOCK_SIZE;

        // Find max absolute value
        let mut amax = 0.0f32;
        for i in start..end {
            let abs = if input[i] < 0.0 { -input[i] } else { input[i] };
            if abs > amax {
                amax = abs;
            }
        }

        let scale = if amax > 0.0 { amax / 127.0 } else { 0.0 };
        let inv_scale = if scale > 0.0 { 1.0 / scale } else { 0.0 };

        // Write scale as f16 (GGML spec)
        let scale_f16 = f32_to_f16(scale);
        let scale_bytes = scale_f16.to_le_bytes();
        output[out_offset..out_offset + 2].copy_from_slice(&scale_bytes);

        // Quantize values (round to nearest, matching llama.cpp's roundf())
        for i in 0..32 {
            let src_idx = start + i;
            if src_idx < end {
                let v = input[src_idx] * inv_scale;
                // Round to nearest integer (not truncate!)
                let rounded = if v >= 0.0 { (v + 0.5) as i32 } else { (v - 0.5) as i32 };
                // Clamp to i8 range
                let q = if rounded > 127 {
                    127i8
                } else if rounded < -128 {
                    -128i8
                } else {
                    rounded as i8
                };
                output[out_offset + 2 + i] = q as u8;
            } else {
                output[out_offset + 2 + i] = 0;
            }
        }
    }
}

/// Get scale factor from a Q4_0 block.
#[inline]
pub fn q4_0_block_scale(block: &[u8]) -> f32 {
    f16_to_f32(u16::from_le_bytes([block[0], block[1]]))
}

/// Get scale factor from a Q8_0 block.
#[inline]
pub fn q8_0_block_scale(block: &[u8]) -> f32 {
    f16_to_f32(u16::from_le_bytes([block[0], block[1]]))
}

/// Get i8 values from a Q8_0 block (zero-copy).
#[inline]
pub fn q8_0_block_values(block: &[u8]) -> &[i8] {
    unsafe { core::slice::from_raw_parts(block[2..34].as_ptr() as *const i8, 32) }
}

/// Convert Q4_0 block values to u8 array (for integer GEMM).
/// GGML layout: lo nibbles → first half (0-15), hi nibbles → second half (16-31).
/// The subtraction of 8 (zero-point) is handled during accumulation.
#[inline]
pub fn q4_0_to_u8_block(block: &[u8], out: &mut [u8; 32]) {
    for i in 0..16 {
        let byte = block[2 + i];
        out[i] = byte & 0x0F;           // lo → first half
        out[16 + i] = (byte >> 4) & 0x0F; // hi → second half
    }
}

/// Convert IEEE 754 half-precision (f16) to single-precision (f32).
#[inline]
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1F) as u32;
    let mant = (h & 0x3FF) as u32;

    if exp == 0 {
        if mant == 0 {
            // Zero
            return f32::from_bits(sign << 31);
        }
        // Subnormal: normalize
        let mut e = exp;
        let mut m = mant;
        while (m & 0x400) == 0 {
            m <<= 1;
            e = e.wrapping_sub(1);
        }
        e = e.wrapping_add(1);
        m &= !0x400;
        let f32_bits = (sign << 31) | ((e + 127 - 15) << 23) | (m << 13);
        return f32::from_bits(f32_bits);
    }

    if exp == 31 {
        // Inf or NaN
        let f32_bits = (sign << 31) | (0xFF << 23) | (mant << 13);
        return f32::from_bits(f32_bits);
    }

    // Normal
    let f32_bits = (sign << 31) | ((exp + 127 - 15) << 23) | (mant << 13);
    f32::from_bits(f32_bits)
}

/// Convert f32 to f16 (IEEE 754 half-precision).
#[inline]
pub fn f32_to_f16(f: f32) -> u16 {
    let bits = f.to_bits();
    let sign = (bits >> 31) & 1;
    let exp = ((bits >> 23) & 0xFF) as i32;
    let mant = bits & 0x7F_FFFF;

    if exp == 0xFF {
        // Inf/NaN
        return ((sign << 15) | (0x1F << 10) | (mant >> 13)) as u16;
    }

    let new_exp = exp - 127 + 15;
    if new_exp >= 31 {
        // Overflow → Inf
        return ((sign << 15) | (0x1F << 10)) as u16;
    }
    if new_exp <= 0 {
        // Underflow → zero (simplified)
        return (sign << 15) as u16;
    }

    ((sign << 15) | ((new_exp as u32) << 10) | (mant >> 13)) as u16
}

/// Dot product of Q4_0 weights × Q8_0 activations for one block of 32 values.
///
/// Integer-only: extracts Q4_0 nibbles as u8, Q8_0 values as i8,
/// multiplies, accumulates in i32, then scales at the end.
///
/// Zero-point correction: Q4_0 values are 0-15 with implicit offset of -8.
/// We compute: sum(q4_unsigned * q8_val) then subtract 8 * sum(q8_val).
#[inline]
pub fn dot_q4_0_q8_0_block(q4_block: &[u8], q8_block: &[u8]) -> f32 {
    let q4_scale = q4_0_block_scale(q4_block);
    let q8_scale = q8_0_block_scale(q8_block);

    let mut sum_prod = 0i32;
    let mut sum_q8 = 0i32;

    // GGML Q4_0 layout: lo nibbles → first half (0-15), hi nibbles → second half (16-31)
    for i in 0..16 {
        let byte = q4_block[2 + i];
        let q4_lo = (byte & 0x0F) as i32;      // → position i (first half)
        let q4_hi = ((byte >> 4) & 0x0F) as i32; // → position i+16 (second half)

        let q8_first = (q8_block[2 + i] as i8) as i32;      // activation at position i
        let q8_second = (q8_block[2 + 16 + i] as i8) as i32; // activation at position i+16

        sum_prod += q4_lo * q8_first + q4_hi * q8_second;
        sum_q8 += q8_first + q8_second;
    }

    // Apply zero-point correction and scales
    let corrected = sum_prod - 8 * sum_q8;
    corrected as f32 * q4_scale * q8_scale
}
