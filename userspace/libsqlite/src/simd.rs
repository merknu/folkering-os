//! SIMD-accelerated distance computations
//!
//! This module provides:
//! - CPU feature detection (AVX2, POPCNT)
//! - Hamming distance (with POPCNT when available)
//! - L2 squared distance for i8 vectors
//! - Scalar fallbacks for all operations
//!
//! # Notes
//!
//! AVX2 intrinsics are not currently used due to inline asm restrictions
//! in no_std environments. The scalar implementations are still fast
//! enough for small datasets (<1000 vectors).

use crate::quantize::{BinaryVector, ScalarVector, BQ_SIZE};

/// CPU feature flags
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuFeatures {
    /// AVX2 support (256-bit integer operations)
    pub avx2: bool,
    /// POPCNT support (hardware popcount)
    pub popcnt: bool,
}

impl CpuFeatures {
    /// Check if AVX2 is available (for SIMD distance computations)
    #[inline]
    pub fn has_avx2(&self) -> bool {
        self.avx2
    }

    /// Check if POPCNT is available
    #[inline]
    pub fn has_popcnt(&self) -> bool {
        self.popcnt
    }
}

/// Detect CPU features at runtime
///
/// Uses CPUID instruction to check for AVX2 and POPCNT support.
#[cfg(target_arch = "x86_64")]
pub fn detect_cpu_features() -> CpuFeatures {
    let mut features = CpuFeatures::default();

    // CPUID leaf 1 for POPCNT (ECX bit 23)
    let ecx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "mov eax, 1",
            "cpuid",
            "mov {ecx:e}, ecx",
            "pop rbx",
            ecx = out(reg) ecx,
            out("eax") _,
            out("ecx") _,
            out("edx") _,
            options(nostack),
        );
    }
    features.popcnt = (ecx & (1 << 23)) != 0;

    // CPUID leaf 7, subleaf 0 for AVX2 (EBX bit 5)
    let ebx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "mov eax, 7",
            "xor ecx, ecx",
            "cpuid",
            "mov {ebx:e}, ebx",
            "pop rbx",
            ebx = out(reg) ebx,
            out("eax") _,
            out("ecx") _,
            out("edx") _,
            options(nostack),
        );
    }
    features.avx2 = (ebx & (1 << 5)) != 0;

    features
}

/// Detect CPU features (non-x86_64 fallback - no SIMD support)
#[cfg(not(target_arch = "x86_64"))]
pub fn detect_cpu_features() -> CpuFeatures {
    CpuFeatures::default()
}

// ============================================================================
// Hamming Distance
// ============================================================================

/// Compute Hamming distance using hardware POPCNT (scalar, 64-bit at a time)
///
/// # Safety
/// Requires POPCNT CPU feature
#[cfg(target_arch = "x86_64")]
#[inline]
pub unsafe fn hamming_distance_popcnt(a: &BinaryVector, b: &BinaryVector) -> u32 {
    let mut distance = 0u32;

    // Process 8 bytes at a time using 64-bit POPCNT
    let a_ptr = a.bits.as_ptr() as *const u64;
    let b_ptr = b.bits.as_ptr() as *const u64;

    for i in 0..(BQ_SIZE / 8) {
        let xor = *a_ptr.add(i) ^ *b_ptr.add(i);
        let count: u64;
        core::arch::asm!(
            "popcnt {out}, {in}",
            in = in(reg) xor,
            out = out(reg) count,
            options(nostack, nomem, preserves_flags),
        );
        distance += count as u32;
    }

    distance
}

/// Compute Hamming distance (scalar fallback)
///
/// Uses a lookup table for byte popcount.
#[inline]
pub fn hamming_distance_scalar(a: &BinaryVector, b: &BinaryVector) -> u32 {
    a.hamming_distance(b)
}

/// Compute Hamming distance using best available method
#[inline]
pub fn hamming_distance(a: &BinaryVector, b: &BinaryVector, features: &CpuFeatures) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if features.has_popcnt() {
            return unsafe { hamming_distance_popcnt(a, b) };
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    let _ = features;

    hamming_distance_scalar(a, b)
}

// ============================================================================
// L2 Squared Distance for SQ8
// ============================================================================

/// Compute L2 squared distance (scalar implementation)
///
/// For i8 vectors, computes sum of (a[i] - b[i])^2
#[inline]
pub fn l2_squared_scalar(a: &ScalarVector, b: &ScalarVector) -> i32 {
    a.l2_squared(b)
}

/// Compute L2 squared distance using best available method
#[inline]
pub fn l2_squared(a: &ScalarVector, b: &ScalarVector, _features: &CpuFeatures) -> i32 {
    // AVX2 implementation disabled due to inline asm restrictions
    // Scalar is still fast enough for our use case
    l2_squared_scalar(a, b)
}

// ============================================================================
// Batch Operations
// ============================================================================

/// Result of a batch distance computation
#[derive(Clone, Copy, Debug)]
pub struct DistanceResult {
    /// Index of the vector in the batch
    pub index: u32,
    /// Distance value
    pub distance: u32,
}

/// Batch compute Hamming distances from query to multiple vectors
///
/// Returns results (not sorted)
pub fn batch_hamming_distance(
    query: &BinaryVector,
    vectors: &[BinaryVector],
    results: &mut [DistanceResult],
    features: &CpuFeatures,
) -> usize {
    let count = vectors.len().min(results.len());

    for (i, vec) in vectors.iter().take(count).enumerate() {
        let distance = hamming_distance(query, vec, features);
        results[i] = DistanceResult {
            index: i as u32,
            distance,
        };
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quantize::quantize_binary;
    use crate::vector::Embedding;

    #[test]
    fn test_detect_cpu_features() {
        let features = detect_cpu_features();
        // Just make sure it doesn't crash
        println!("AVX2: {}, POPCNT: {}", features.avx2, features.popcnt);
    }

    #[test]
    fn test_hamming_scalar_consistency() {
        let bq1 = BinaryVector { bits: [0xAA; BQ_SIZE] };
        let bq2 = BinaryVector { bits: [0x55; BQ_SIZE] };

        let scalar_result = hamming_distance_scalar(&bq1, &bq2);

        // 0xAA ^ 0x55 = 0xFF, popcount(0xFF) = 8
        // 48 bytes * 8 bits = 384
        assert_eq!(scalar_result, 384);
    }

    #[test]
    fn test_l2_scalar_consistency() {
        let sq1 = ScalarVector {
            values: [10; 384],
            scale: 1.0,
            offset: 0.0,
        };
        let sq2 = ScalarVector {
            values: [0; 384],
            scale: 1.0,
            offset: 0.0,
        };

        let scalar_result = l2_squared_scalar(&sq1, &sq2);

        // (10 - 0)^2 * 384 = 100 * 384 = 38400
        assert_eq!(scalar_result, 38400);
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn test_simd_hamming_parity() {
        let features = detect_cpu_features();

        let mut embedding1 = Embedding::default();
        let mut embedding2 = Embedding::default();
        for i in 0..384 {
            embedding1.values[i] = ((i * 7) % 100) as f32 - 50.0;
            embedding2.values[i] = ((i * 11) % 100) as f32 - 50.0;
        }

        let bq1 = quantize_binary(&embedding1);
        let bq2 = quantize_binary(&embedding2);

        let scalar_result = hamming_distance_scalar(&bq1, &bq2);

        if features.has_popcnt() {
            let popcnt_result = unsafe { hamming_distance_popcnt(&bq1, &bq2) };
            assert_eq!(scalar_result, popcnt_result, "POPCNT result differs from scalar");
        }
    }

    #[test]
    fn test_batch_hamming() {
        let features = detect_cpu_features();

        let query = BinaryVector { bits: [0xFF; BQ_SIZE] };
        let vectors = [
            BinaryVector { bits: [0xFF; BQ_SIZE] }, // Distance 0
            BinaryVector { bits: [0x00; BQ_SIZE] }, // Distance 384
            BinaryVector { bits: [0xAA; BQ_SIZE] }, // Distance 192
        ];

        let mut results = [DistanceResult { index: 0, distance: 0 }; 3];
        let count = batch_hamming_distance(&query, &vectors, &mut results, &features);

        assert_eq!(count, 3);
        assert_eq!(results[0].distance, 0);
        assert_eq!(results[1].distance, 384);
        assert_eq!(results[2].distance, 192);
    }
}
