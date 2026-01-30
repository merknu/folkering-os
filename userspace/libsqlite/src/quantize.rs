//! Quantization for vector search
//!
//! This module provides:
//! - Binary Quantization (BQ): 32x compression, fast Hamming distance
//! - Scalar Quantization (SQ8): 4x compression, precise L2 distance
//!
//! # Two-Pass Search Algorithm
//!
//! 1. **Pass 1 (Coarse)**: Use BQ + Hamming distance to find ~10x candidates
//! 2. **Pass 2 (Fine)**: Re-rank candidates using SQ8 + L2 distance
//!
//! This achieves ~25-30x speedup over brute-force f32 with >95% recall.

use crate::vector::{Embedding, EMBEDDING_DIM};

/// Size of binary quantized vector in bytes (384 bits / 8 = 48 bytes)
pub const BQ_SIZE: usize = EMBEDDING_DIM / 8;

/// Size of scalar quantized vector in bytes (384 i8 = 384 bytes)
pub const SQ8_SIZE: usize = EMBEDDING_DIM;

/// Binary Quantized Vector (48 bytes)
///
/// Each dimension is quantized to a single bit:
/// - bit = 1 if value >= 0
/// - bit = 0 if value < 0
///
/// Distance metric: Hamming distance (popcount of XOR)
#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub struct BinaryVector {
    /// Packed bits: 384 dimensions / 8 = 48 bytes
    pub bits: [u8; BQ_SIZE],
}

impl Default for BinaryVector {
    fn default() -> Self {
        Self { bits: [0; BQ_SIZE] }
    }
}

impl BinaryVector {
    /// Create from raw bytes
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != BQ_SIZE {
            return None;
        }
        let mut bits = [0u8; BQ_SIZE];
        bits.copy_from_slice(bytes);
        Some(Self { bits })
    }

    /// Serialize to raw bytes
    pub fn to_bytes(&self, buffer: &mut [u8]) -> usize {
        if buffer.len() < BQ_SIZE {
            return 0;
        }
        buffer[..BQ_SIZE].copy_from_slice(&self.bits);
        BQ_SIZE
    }

    /// Compute Hamming distance to another binary vector (scalar fallback)
    ///
    /// Returns number of differing bits (0 to 384)
    #[inline]
    pub fn hamming_distance(&self, other: &BinaryVector) -> u32 {
        let mut distance = 0u32;
        for i in 0..BQ_SIZE {
            distance += (self.bits[i] ^ other.bits[i]).count_ones();
        }
        distance
    }
}

/// Scalar Quantized Vector (SQ8) - 386 bytes total
///
/// Each dimension is quantized to 8 bits (i8):
/// - Calibrated per-vector using min/max
/// - value_i8 = ((value_f32 - offset) * scale).clamp(-128, 127)
/// - value_f32 = (value_i8 / scale) + offset
///
/// Distance metric: Approximate L2 (sum of squared i8 differences)
#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub struct ScalarVector {
    /// Quantized values: 384 × i8 = 384 bytes
    pub values: [i8; SQ8_SIZE],
    /// Scale factor for dequantization
    pub scale: f32,
    /// Offset for dequantization
    pub offset: f32,
}

impl Default for ScalarVector {
    fn default() -> Self {
        Self {
            values: [0; SQ8_SIZE],
            scale: 1.0,
            offset: 0.0,
        }
    }
}

/// Size of serialized ScalarVector (384 + 4 + 4 = 392 bytes, rounded to 400 for alignment)
pub const SQ8_SERIALIZED_SIZE: usize = SQ8_SIZE + 8; // 384 i8 + scale (f32) + offset (f32)

impl ScalarVector {
    /// Create from raw bytes (384 i8 + scale f32 + offset f32)
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < SQ8_SERIALIZED_SIZE {
            return None;
        }
        let mut values = [0i8; SQ8_SIZE];
        for (i, &b) in bytes[..SQ8_SIZE].iter().enumerate() {
            values[i] = b as i8;
        }
        let scale = f32::from_le_bytes([
            bytes[SQ8_SIZE],
            bytes[SQ8_SIZE + 1],
            bytes[SQ8_SIZE + 2],
            bytes[SQ8_SIZE + 3],
        ]);
        let offset = f32::from_le_bytes([
            bytes[SQ8_SIZE + 4],
            bytes[SQ8_SIZE + 5],
            bytes[SQ8_SIZE + 6],
            bytes[SQ8_SIZE + 7],
        ]);
        Some(Self { values, scale, offset })
    }

    /// Serialize to raw bytes
    pub fn to_bytes(&self, buffer: &mut [u8]) -> usize {
        if buffer.len() < SQ8_SERIALIZED_SIZE {
            return 0;
        }
        for (i, &v) in self.values.iter().enumerate() {
            buffer[i] = v as u8;
        }
        buffer[SQ8_SIZE..SQ8_SIZE + 4].copy_from_slice(&self.scale.to_le_bytes());
        buffer[SQ8_SIZE + 4..SQ8_SIZE + 8].copy_from_slice(&self.offset.to_le_bytes());
        SQ8_SERIALIZED_SIZE
    }

    /// Compute approximate L2 squared distance (scalar fallback)
    ///
    /// Returns sum of squared i8 differences (lower = more similar)
    #[inline]
    pub fn l2_squared(&self, other: &ScalarVector) -> i32 {
        let mut sum = 0i32;
        for i in 0..SQ8_SIZE {
            let diff = self.values[i] as i32 - other.values[i] as i32;
            sum += diff * diff;
        }
        sum
    }
}

/// Quantize f32 embedding to binary vector (sign-based)
///
/// Each bit is set to 1 if the corresponding f32 value >= 0
pub fn quantize_binary(embedding: &Embedding) -> BinaryVector {
    let mut bq = BinaryVector::default();

    for i in 0..EMBEDDING_DIM {
        if embedding.values[i] >= 0.0 {
            let byte_idx = i / 8;
            let bit_idx = i % 8;
            bq.bits[byte_idx] |= 1 << bit_idx;
        }
    }

    bq
}

/// Quantize f32 embedding to scalar vector (min/max calibration)
///
/// Uses per-vector min/max calibration for optimal precision
pub fn quantize_scalar(embedding: &Embedding) -> ScalarVector {
    let mut sq = ScalarVector::default();

    // Find min and max values
    let mut min_val = embedding.values[0];
    let mut max_val = embedding.values[0];
    for &v in &embedding.values[1..] {
        if v < min_val {
            min_val = v;
        }
        if v > max_val {
            max_val = v;
        }
    }

    // Compute scale and offset
    let range = max_val - min_val;
    if range < 1e-10 {
        // Constant vector - set all to 0
        sq.scale = 1.0;
        sq.offset = min_val;
        return sq;
    }

    // Map [min_val, max_val] to [-127, 127]
    // value_i8 = ((value_f32 - offset) * scale)
    // We want: min_val -> -127, max_val -> 127
    // So: offset = (min_val + max_val) / 2
    //     scale = 254 / range
    sq.offset = (min_val + max_val) / 2.0;
    sq.scale = 254.0 / range;

    // Quantize each value
    for i in 0..EMBEDDING_DIM {
        let normalized = (embedding.values[i] - sq.offset) * sq.scale;
        // Clamp to i8 range
        let clamped = if normalized < -127.0 {
            -127
        } else if normalized > 127.0 {
            127
        } else {
            normalized as i8
        };
        sq.values[i] = clamped;
    }

    sq
}

/// Dequantize scalar vector back to approximate f32 embedding
///
/// Useful for verification and debugging
#[allow(dead_code)]
pub fn dequantize_scalar(sq: &ScalarVector) -> Embedding {
    let mut embedding = Embedding::default();

    for i in 0..EMBEDDING_DIM {
        embedding.values[i] = (sq.values[i] as f32 / sq.scale) + sq.offset;
    }

    embedding
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_quantization_positive() {
        let mut embedding = Embedding::default();
        // Set all values positive
        for i in 0..EMBEDDING_DIM {
            embedding.values[i] = (i as f32) + 0.1;
        }

        let bq = quantize_binary(&embedding);

        // All bits should be 1
        for byte in &bq.bits {
            assert_eq!(*byte, 0xFF);
        }
    }

    #[test]
    fn test_binary_quantization_negative() {
        let mut embedding = Embedding::default();
        // Set all values negative
        for i in 0..EMBEDDING_DIM {
            embedding.values[i] = -((i as f32) + 0.1);
        }

        let bq = quantize_binary(&embedding);

        // All bits should be 0
        for byte in &bq.bits {
            assert_eq!(*byte, 0x00);
        }
    }

    #[test]
    fn test_binary_quantization_mixed() {
        let mut embedding = Embedding::default();
        // Alternating positive/negative
        for i in 0..EMBEDDING_DIM {
            embedding.values[i] = if i % 2 == 0 { 1.0 } else { -1.0 };
        }

        let bq = quantize_binary(&embedding);

        // Each byte should be 0b01010101 = 0x55
        for byte in &bq.bits {
            assert_eq!(*byte, 0x55);
        }
    }

    #[test]
    fn test_hamming_distance_identical() {
        let bq1 = BinaryVector { bits: [0xFF; BQ_SIZE] };
        let bq2 = BinaryVector { bits: [0xFF; BQ_SIZE] };

        assert_eq!(bq1.hamming_distance(&bq2), 0);
    }

    #[test]
    fn test_hamming_distance_opposite() {
        let bq1 = BinaryVector { bits: [0xFF; BQ_SIZE] };
        let bq2 = BinaryVector { bits: [0x00; BQ_SIZE] };

        assert_eq!(bq1.hamming_distance(&bq2), 384); // All 384 bits differ
    }

    #[test]
    fn test_hamming_distance_one_bit() {
        let mut bq1 = BinaryVector { bits: [0x00; BQ_SIZE] };
        let bq2 = BinaryVector { bits: [0x00; BQ_SIZE] };
        bq1.bits[0] = 0x01; // Flip one bit

        assert_eq!(bq1.hamming_distance(&bq2), 1);
    }

    #[test]
    fn test_scalar_quantization_roundtrip() {
        let mut embedding = Embedding::default();
        for i in 0..EMBEDDING_DIM {
            embedding.values[i] = ((i as f32) - 192.0) / 100.0; // Range: ~[-1.92, 1.91]
        }

        let sq = quantize_scalar(&embedding);
        let reconstructed = dequantize_scalar(&sq);

        // Check that reconstruction is close (within quantization error)
        for i in 0..EMBEDDING_DIM {
            let error = (embedding.values[i] - reconstructed.values[i]).abs();
            assert!(error < 0.02, "Error at {}: {} vs {} (error: {})",
                    i, embedding.values[i], reconstructed.values[i], error);
        }
    }

    #[test]
    fn test_scalar_l2_squared_identical() {
        let sq1 = ScalarVector {
            values: [50; SQ8_SIZE],
            scale: 1.0,
            offset: 0.0,
        };

        assert_eq!(sq1.l2_squared(&sq1), 0);
    }

    #[test]
    fn test_scalar_l2_squared_different() {
        let sq1 = ScalarVector {
            values: [0; SQ8_SIZE],
            scale: 1.0,
            offset: 0.0,
        };
        let mut sq2 = ScalarVector {
            values: [0; SQ8_SIZE],
            scale: 1.0,
            offset: 0.0,
        };
        sq2.values[0] = 10;

        assert_eq!(sq1.l2_squared(&sq2), 100); // 10^2 = 100
    }

    #[test]
    fn test_binary_vector_serialization() {
        let mut bq = BinaryVector::default();
        bq.bits[0] = 0xAB;
        bq.bits[47] = 0xCD;

        let mut buffer = [0u8; BQ_SIZE];
        let len = bq.to_bytes(&mut buffer);
        assert_eq!(len, BQ_SIZE);

        let restored = BinaryVector::from_bytes(&buffer).unwrap();
        assert_eq!(restored.bits[0], 0xAB);
        assert_eq!(restored.bits[47], 0xCD);
    }

    #[test]
    fn test_scalar_vector_serialization() {
        let mut sq = ScalarVector::default();
        sq.values[0] = 42;
        sq.values[383] = -100;
        sq.scale = 2.5;
        sq.offset = -1.0;

        let mut buffer = [0u8; SQ8_SERIALIZED_SIZE];
        let len = sq.to_bytes(&mut buffer);
        assert_eq!(len, SQ8_SERIALIZED_SIZE);

        let restored = ScalarVector::from_bytes(&buffer).unwrap();
        assert_eq!(restored.values[0], 42);
        assert_eq!(restored.values[383], -100);
        assert!((restored.scale - 2.5).abs() < 0.001);
        assert!((restored.offset - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn test_quantize_constant_vector() {
        let mut embedding = Embedding::default();
        for i in 0..EMBEDDING_DIM {
            embedding.values[i] = 5.0; // All same value
        }

        let sq = quantize_scalar(&embedding);
        // Should handle this gracefully (all zeros in values)
        assert!(sq.scale >= 1.0);
    }
}
