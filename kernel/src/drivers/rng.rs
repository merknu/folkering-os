//! Hardware RNG Driver — RDRAND/RDSEED + RDTSC fallback
//!
//! Provides cryptographic-quality random bytes for TLS sessions.
//! Uses RDRAND (Intel Ivy Bridge+) when available, falls back to RDTSC mixing.

use core::sync::atomic::{AtomicBool, Ordering};

static HAS_RDRAND: AtomicBool = AtomicBool::new(false);
static HAS_RDSEED: AtomicBool = AtomicBool::new(false);

/// Detect RDRAND/RDSEED via CPUID and log result.
pub fn init() {
    let rdrand = detect_rdrand();
    let rdseed = detect_rdseed();
    HAS_RDRAND.store(rdrand, Ordering::Relaxed);
    HAS_RDSEED.store(rdseed, Ordering::Relaxed);

    if rdrand {
        crate::serial_str!("[RNG] RDRAND available");
        if rdseed {
            crate::serial_strln!(", RDSEED available");
        } else {
            crate::serial_strln!("");
        }
        // Self-test
        if let Some(val) = rdrand_u64() {
            crate::serial_str!("[RNG] Self-test: 0x");
            crate::drivers::serial::write_hex(val);
            crate::drivers::serial::write_newline();
        } else {
            crate::serial_strln!("[RNG] Self-test failed, falling back to RDTSC");
            HAS_RDRAND.store(false, Ordering::Relaxed);
        }
    } else {
        crate::serial_strln!("[RNG] RDRAND not available, using RDTSC entropy mixing");
        // Log a test value from RDTSC fallback
        let test = mix(rdtsc());
        crate::serial_str!("[RNG] RDTSC test: 0x");
        crate::drivers::serial::write_hex(test);
        crate::drivers::serial::write_newline();
    }
}

// ── CPUID Detection ─────────────────────────────────────────────────────────

fn detect_rdrand() -> bool {
    let ecx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "mov eax, 1",
            "cpuid",
            "pop rbx",
            out("ecx") ecx,
            out("eax") _,
            out("edx") _,
        );
    }
    (ecx & (1 << 30)) != 0
}

fn detect_rdseed() -> bool {
    let ebx: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "mov eax, 7",
            "xor ecx, ecx",
            "cpuid",
            "mov {out_ebx:e}, ebx",
            "pop rbx",
            out_ebx = out(reg) ebx,
            out("eax") _,
            out("ecx") _,
            out("edx") _,
        );
    }
    (ebx & (1 << 18)) != 0
}

// ── RDRAND/RDSEED Instructions ──────────────────────────────────────────────

/// Generate a 64-bit random value via RDRAND. Returns None on failure.
pub fn rdrand_u64() -> Option<u64> {
    let value: u64;
    let ok: u8;
    unsafe {
        core::arch::asm!(
            "rdrand {val}",
            "setc {ok}",
            val = out(reg) value,
            ok = out(reg_byte) ok,
        );
    }
    if ok != 0 { Some(value) } else { None }
}

/// Generate a 64-bit random seed via RDSEED. Returns None on failure.
pub fn rdseed_u64() -> Option<u64> {
    if !HAS_RDSEED.load(Ordering::Relaxed) {
        return None;
    }
    let value: u64;
    let ok: u8;
    unsafe {
        core::arch::asm!(
            "rdseed {val}",
            "setc {ok}",
            val = out(reg) value,
            ok = out(reg_byte) ok,
        );
    }
    if ok != 0 { Some(value) } else { None }
}

// ── RDTSC Fallback ──────────────────────────────────────────────────────────

/// Read Time Stamp Counter
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi);
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Simple mixing function for RDTSC-based entropy
fn mix(state: u64) -> u64 {
    // SplitMix64
    let mut z = state.wrapping_add(0x9e3779b97f4a7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Get a single random u64. Uses RDRAND if available, RDTSC fallback otherwise.
pub fn random_u64() -> u64 {
    if HAS_RDRAND.load(Ordering::Relaxed) {
        // Retry up to 10 times
        for _ in 0..10 {
            if let Some(v) = rdrand_u64() {
                return v;
            }
        }
    }
    // Fallback: mix RDTSC
    mix(rdtsc())
}

/// Fill a buffer with random bytes. Cryptographic quality if RDRAND available.
pub fn fill_bytes(buf: &mut [u8]) {
    let use_rdrand = HAS_RDRAND.load(Ordering::Relaxed);
    let mut offset = 0;

    while offset + 8 <= buf.len() {
        let val = if use_rdrand {
            rdrand_u64().unwrap_or_else(|| mix(rdtsc()))
        } else {
            mix(rdtsc().wrapping_add(offset as u64))
        };
        buf[offset..offset + 8].copy_from_slice(&val.to_le_bytes());
        offset += 8;
    }

    // Remaining bytes
    if offset < buf.len() {
        let val = if use_rdrand {
            rdrand_u64().unwrap_or_else(|| mix(rdtsc()))
        } else {
            mix(rdtsc().wrapping_add(offset as u64))
        };
        let bytes = val.to_le_bytes();
        for i in 0..(buf.len() - offset) {
            buf[offset + i] = bytes[i];
        }
    }
}

/// Check if hardware RNG (RDRAND) is available
pub fn has_rdrand() -> bool {
    HAS_RDRAND.load(Ordering::Relaxed)
}
