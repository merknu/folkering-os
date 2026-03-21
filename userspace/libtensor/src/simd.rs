//! SIMD detection and AVX2 intrinsics wrappers (ULTRA 1)
//!
//! CPUID-based runtime detection with dual codepath:
//! AVX2 integer GEMM (fast) + scalar fallback (portable).

use core::sync::atomic::{AtomicBool, Ordering};

static HAS_AVX2: AtomicBool = AtomicBool::new(false);
static CPUID_CHECKED: AtomicBool = AtomicBool::new(false);

/// Check CPU features via CPUID and store results.
/// Must be called once at init before any SIMD operations.
pub fn detect_cpu_features() {
    if CPUID_CHECKED.load(Ordering::Acquire) {
        return;
    }

    let has_avx2 = unsafe { check_avx2() };
    HAS_AVX2.store(has_avx2, Ordering::Release);
    CPUID_CHECKED.store(true, Ordering::Release);
}

/// Returns true if AVX2 is available.
#[inline(always)]
pub fn has_avx2() -> bool {
    HAS_AVX2.load(Ordering::Relaxed)
}

/// Check AVX2 support:
/// 1. CPUID.1:ECX bit 27 (OSXSAVE) — OS must have enabled XSAVE
/// 2. CPUID.7:EBX bit 5 (AVX2) — CPU must support AVX2
/// 3. XCR0 bits 1+2 (SSE+AVX state) — OS must have enabled AVX state saving
///
/// All three must be true for AVX2 instructions to be safe.
unsafe fn check_avx2() -> bool {
    // Step 1: Check OSXSAVE (CPUID.1:ECX bit 27)
    let ecx1: u32;
    core::arch::asm!(
        "mov {tmp_rbx}, rbx",
        "mov eax, 1",
        "cpuid",
        "mov {out:e}, ecx",
        "mov rbx, {tmp_rbx}",
        out = out(reg) ecx1,
        tmp_rbx = out(reg) _,
        out("eax") _,
        out("ecx") _,
        out("edx") _,
        options(nostack),
    );
    let osxsave = (ecx1 & (1 << 27)) != 0;
    if !osxsave {
        return false; // OS hasn't enabled XSAVE — AVX will #GP
    }

    // Step 2: Check AVX2 hardware support (CPUID.7:EBX bit 5)
    let ebx7: u32;
    core::arch::asm!(
        "mov {tmp_rbx}, rbx",
        "mov eax, 7",
        "xor ecx, ecx",
        "cpuid",
        "mov {out:e}, ebx",
        "mov rbx, {tmp_rbx}",
        out = out(reg) ebx7,
        tmp_rbx = out(reg) _,
        out("eax") _,
        out("ecx") _,
        out("edx") _,
        options(nostack),
    );
    let avx2_hw = (ebx7 & (1 << 5)) != 0;
    if !avx2_hw {
        return false;
    }

    // Step 3: Check XCR0 (SSE state bit 1 + AVX state bit 2)
    let xcr0_lo: u32;
    core::arch::asm!(
        "xor ecx, ecx",    // XCR0
        "xgetbv",
        "mov {out:e}, eax",
        out = out(reg) xcr0_lo,
        out("eax") _,
        out("ecx") _,
        out("edx") _,
        options(nostack),
    );
    let sse_avx_enabled = (xcr0_lo & 0x6) == 0x6; // bits 1+2

    avx2_hw && sse_avx_enabled
}

// =============================================================================
// AVX2 intrinsic wrappers
// =============================================================================
// These use `core::arch::x86_64` intrinsics which are available in no_std.
// They are gated by `#[target_feature(enable = "avx2")]` so the caller must
// ensure AVX2 is available (checked via `has_avx2()`).

#[cfg(target_arch = "x86_64")]
pub mod avx2 {
    #[allow(unused_imports)]
    use core::arch::x86_64::*;

    /// Horizontally sum 8 f32 values in a __m256 register.
    ///
    /// # Safety
    /// Caller must ensure AVX2 is available.
    #[inline]
    #[target_feature(enable = "avx2", enable = "fma")]
    pub unsafe fn hsum_f32x8(v: __m256) -> f32 {
        // [a0+a4, a1+a5, a2+a6, a3+a7] (128-bit halves added)
        let hi = _mm256_extractf128_ps(v, 1);
        let lo = _mm256_castps256_ps128(v);
        let sum128 = _mm_add_ps(lo, hi);
        // [s0+s2, s1+s3, ...]
        let shuf = _mm_movehdup_ps(sum128);
        let sum64 = _mm_add_ps(sum128, shuf);
        // [s01+s23, ...]
        let shuf2 = _mm_movehl_ps(sum64, sum64);
        let sum32 = _mm_add_ss(sum64, shuf2);
        _mm_cvtss_f32(sum32)
    }

    /// Dot product of two f32 slices using AVX2 FMA.
    /// Processes 8 elements at a time.
    ///
    /// # Safety
    /// Caller must ensure AVX2+FMA is available and slices are valid.
    #[inline]
    #[target_feature(enable = "avx2", enable = "fma")]
    pub unsafe fn dot_f32_avx2(a: &[f32], b: &[f32], len: usize) -> f32 {
        let mut acc = _mm256_setzero_ps();
        let mut i = 0;

        // Process 8 elements at a time
        while i + 8 <= len {
            let va = _mm256_loadu_ps(a.as_ptr().add(i));
            let vb = _mm256_loadu_ps(b.as_ptr().add(i));
            acc = _mm256_fmadd_ps(va, vb, acc);
            i += 8;
        }

        let mut sum = hsum_f32x8(acc);

        // Scalar tail
        while i < len {
            sum += a[i] * b[i];
            i += 1;
        }

        sum
    }

    /// Integer dot product: u8 × i8 → i32, accumulated.
    /// Uses _mm256_maddubs_epi16 for the core multiply.
    /// `a` values are treated as unsigned (u8), `b` as signed (i8).
    ///
    /// Processes 32 elements at a time.
    ///
    /// # Safety
    /// Caller must ensure AVX2 is available.
    #[inline]
    #[target_feature(enable = "avx2")]
    pub unsafe fn dot_i8_avx2(a_u8: &[u8], b_i8: &[i8], len: usize) -> i32 {
        let mut acc = _mm256_setzero_si256();
        let ones_16 = _mm256_set1_epi16(1);
        let mut i = 0;

        while i + 32 <= len {
            let va = _mm256_loadu_si256(a_u8.as_ptr().add(i) as *const __m256i);
            let vb = _mm256_loadu_si256(b_i8.as_ptr().add(i) as *const __m256i);

            // u8 × i8 → i16 pairs, adjacent pairs added
            let prod16 = _mm256_maddubs_epi16(va, vb);
            // i16 → i32 by multiplying with 1s and horizontal add
            let prod32 = _mm256_madd_epi16(prod16, ones_16);
            acc = _mm256_add_epi32(acc, prod32);

            i += 32;
        }

        // Horizontal sum of 8 i32 lanes
        let hi128 = _mm256_extracti128_si256(acc, 1);
        let lo128 = _mm256_castsi256_si128(acc);
        let sum128 = _mm_add_epi32(lo128, hi128);
        let shuf = _mm_shuffle_epi32(sum128, 0b_01_00_11_10);
        let sum64 = _mm_add_epi32(sum128, shuf);
        let shuf2 = _mm_shuffle_epi32(sum64, 0b_00_00_00_01);
        let sum32 = _mm_add_epi32(sum64, shuf2);
        let mut result = _mm_cvtsi128_si32(sum32);

        // Scalar tail
        while i < len {
            result += (a_u8[i] as i32) * (b_i8[i] as i32);
            i += 1;
        }

        result
    }
}

// =============================================================================
// Scalar fallbacks
// =============================================================================

/// Scalar dot product of f32 slices.
#[inline]
pub fn dot_f32_scalar(a: &[f32], b: &[f32], len: usize) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..len {
        sum += a[i] * b[i];
    }
    sum
}

/// Scalar integer dot product: u8 × i8 → i32.
#[inline]
pub fn dot_i8_scalar(a_u8: &[u8], b_i8: &[i8], len: usize) -> i32 {
    let mut sum = 0i32;
    for i in 0..len {
        sum += (a_u8[i] as i32) * (b_i8[i] as i32);
    }
    sum
}
