//! Page Attribute Table (PAT) Configuration
//!
//! Configures the PAT MSR to enable Write-Combining (WC) memory type
//! at PAT index 4, which is used for framebuffer mappings.
//!
//! # PAT Index Layout (after init)
//!
//! | Index | Memory Type | Flags (PAT, PCD, PWT) |
//! |-------|-------------|------------------------|
//! | 0     | WB          | 0, 0, 0               |
//! | 1     | WT          | 0, 0, 1               |
//! | 2     | UC-         | 0, 1, 0               |
//! | 3     | UC          | 0, 1, 1               |
//! | 4     | WC          | 1, 0, 0 (custom)      |
//! | 5     | WT          | 1, 0, 1               |
//! | 6     | UC-         | 1, 1, 0               |
//! | 7     | UC          | 1, 1, 1               |
//!
//! # Usage
//!
//! For Write-Combining (PAT index 4), set page table entry flags:
//! - 4KiB pages: PAT bit = bit 7
//! - 2MiB pages: PAT bit = bit 12
//! - PCD = 0, PWT = 0

/// PAT MSR index
const PAT_MSR_INDEX: u32 = 0x277;

/// Memory types for PAT entries
#[repr(u8)]
#[allow(dead_code)]
pub enum PatMemoryType {
    /// Uncacheable (UC) - Strong ordering, no caching
    Uncacheable = 0x00,
    /// Write-Combining (WC) - Weak ordering, combines writes
    WriteCombining = 0x01,
    /// Write-Through (WT) - Reads can be cached, writes go through
    WriteThrough = 0x04,
    /// Write-Protected (WP) - Reads cached, writes cause fault
    WriteProtected = 0x05,
    /// Write-Back (WB) - Full caching (default for RAM)
    WriteBack = 0x06,
    /// Uncacheable Minus (UC-) - Like UC but can be overridden by MTRR
    UncacheableMinus = 0x07,
}

/// Default PAT value from Intel manuals:
/// Index 0-3: WB(0x06), WT(0x04), UC-(0x07), UC(0x00)
/// Index 4-7: WB(0x06), WT(0x04), UC-(0x07), UC(0x00)
/// = 0x00_07_04_06_00_07_04_06
#[allow(dead_code)]
const PAT_DEFAULT: u64 = 0x0007040600070406;

/// Modified PAT value with Write-Combining at index 4:
/// Index 0-3: WB(0x06), WT(0x04), UC-(0x07), UC(0x00) - unchanged
/// Index 4-7: WC(0x01), WT(0x04), UC-(0x07), UC(0x00) - index 4 changed
/// = 0x00_07_04_01_00_07_04_06
const PAT_WITH_WC_AT_4: u64 = 0x0007040100070406;

/// Initialize PAT with Write-Combining at index 4.
///
/// This must be called early in boot, after GDT setup but before
/// any framebuffer or MMIO mappings that need Write-Combining.
///
/// # Safety
///
/// Must be called with interrupts disabled and before any other
/// CPU uses these PAT settings.
pub unsafe fn init() {
    let low = PAT_WITH_WC_AT_4 as u32;
    let high = (PAT_WITH_WC_AT_4 >> 32) as u32;

    core::arch::asm!(
        "wrmsr",
        in("ecx") PAT_MSR_INDEX,
        in("eax") low,
        in("edx") high,
        options(nostack, preserves_flags)
    );

    crate::serial_strln!("[PAT] Write-Combining enabled at PAT index 4");
}

/// Read current PAT MSR value (for debugging).
#[allow(dead_code)]
pub fn read() -> u64 {
    let low: u32;
    let high: u32;

    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") PAT_MSR_INDEX,
            out("eax") low,
            out("edx") high,
            options(nostack, preserves_flags)
        );
    }

    ((high as u64) << 32) | (low as u64)
}

/// Check if PAT is correctly configured with WC at index 4.
#[allow(dead_code)]
pub fn verify() -> bool {
    let current = read();
    let index_4_type = ((current >> 32) & 0xFF) as u8;
    index_4_type == PatMemoryType::WriteCombining as u8
}

/// Page table flag helpers for PAT index selection.
///
/// PAT index is encoded in 3 bits across PWT, PCD, and PAT flags:
/// - PWT = bit 0 of index
/// - PCD = bit 1 of index
/// - PAT = bit 2 of index (bit 7 for 4K pages, bit 12 for 2M pages)
pub mod flags {
    use x86_64::structures::paging::PageTableFlags as PTF;

    /// Flags for PAT index 0 (Write-Back, default)
    pub const PAT_INDEX_0: PTF = PTF::empty();

    /// Flags for PAT index 1 (Write-Through)
    pub const PAT_INDEX_1: PTF = PTF::WRITE_THROUGH;

    /// Flags for PAT index 2 (Uncacheable-)
    pub const PAT_INDEX_2: PTF = PTF::NO_CACHE;

    /// Flags for PAT index 3 (Uncacheable)
    pub const PAT_INDEX_3: PTF = PTF::WRITE_THROUGH.union(PTF::NO_CACHE);

    /// Flags for PAT index 4 (Write-Combining after init)
    /// Uses the PAT bit (bit 7 for 4K pages) without PCD or PWT
    /// Note: We use from_bits_truncate since PAT bit isn't directly exposed
    pub const PAT_INDEX_4: PTF = PTF::from_bits_truncate(0x80);

    /// Flags for framebuffer mapping (Write-Combining)
    /// Combines PAT index 4 with standard present+writable flags
    pub const FRAMEBUFFER: PTF = PTF::PRESENT
        .union(PTF::WRITABLE)
        .union(PTF::USER_ACCESSIBLE)
        .union(PTF::NO_EXECUTE)
        .union(PTF::from_bits_truncate(0x80));  // PAT bit (bit 7) for index 4

    /// Flags for uncached MMIO (like APIC)
    pub const MMIO_UNCACHED: PTF = PTF::PRESENT
        .union(PTF::WRITABLE)
        .union(PTF::NO_CACHE)
        .union(PTF::WRITE_THROUGH)
        .union(PTF::NO_EXECUTE);
}
