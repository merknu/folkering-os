//! Boot Information Access
//!
//! Provides access to boot-time configuration passed from the kernel.
//! The boot info page is mapped at a fixed virtual address by the kernel.
//!
//! # Usage
//!
//! ```ignore
//! use libfolk::sys::boot_info::{get_boot_info, FramebufferConfig};
//!
//! let info = get_boot_info();
//! if let Some(fb) = info.framebuffer() {
//!     let phys_addr = fb.physical_address;
//!     let width = fb.width;
//!     let height = fb.height;
//! }
//! ```

/// Fixed virtual address where boot info page is mapped
/// Note: Must not conflict with ELF load addresses (typically 0x200000+)
pub const BOOT_INFO_VADDR: usize = 0x0000_0000_0010_0000; // 1MB mark

/// Magic value to verify boot info integrity
pub const BOOT_INFO_MAGIC: u64 = 0x464F4C4B424F4F54; // "FOLKBOOT"

/// Current ABI version
pub const BOOT_INFO_VERSION: u64 = 1;

/// Boot info flags
pub mod flags {
    /// Framebuffer is available
    pub const HAS_FRAMEBUFFER: u64 = 1 << 0;
    /// RSDP is available
    pub const HAS_RSDP: u64 = 1 << 1;
}

/// Framebuffer configuration from boot info.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FramebufferConfig {
    /// Physical address of the framebuffer
    pub physical_address: u64,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Bytes per scanline (may differ from width * bpp/8)
    pub pitch: u32,
    /// Bits per pixel (usually 32)
    pub bpp: u16,
    /// Memory model (1 = RGB)
    pub memory_model: u8,
    /// Red mask size in bits
    pub red_mask_size: u8,
    /// Red mask shift (bit position)
    pub red_mask_shift: u8,
    /// Green mask size in bits
    pub green_mask_size: u8,
    /// Green mask shift (bit position)
    pub green_mask_shift: u8,
    /// Blue mask size in bits
    pub blue_mask_size: u8,
    /// Blue mask shift (bit position)
    pub blue_mask_shift: u8,
    /// Reserved for alignment
    pub _reserved: [u8; 3],
}

impl FramebufferConfig {
    /// Calculate framebuffer size in bytes
    pub fn size_bytes(&self) -> usize {
        self.pitch as usize * self.height as usize
    }

    /// Calculate bytes per pixel
    pub fn bytes_per_pixel(&self) -> usize {
        (self.bpp as usize + 7) / 8
    }
}

/// Boot information structure.
///
/// This structure is placed at BOOT_INFO_VADDR by the kernel and provides
/// userspace services with essential boot-time data.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FolkeringBootInfo {
    /// Magic number for verification (BOOT_INFO_MAGIC)
    pub magic: u64,
    /// ABI version (BOOT_INFO_VERSION)
    pub version: u64,
    /// Feature flags
    pub flags: u64,
    /// Framebuffer configuration
    pub framebuffer: FramebufferConfig,
    /// ACPI RSDP physical address (0 if not available)
    pub rsdp_address: u64,
    /// Physical address of the boot info page itself
    pub self_phys_addr: u64,
    /// Reserved for future use
    pub _reserved: [u64; 8],
}

impl FolkeringBootInfo {
    /// Check if boot info is valid
    pub fn is_valid(&self) -> bool {
        self.magic == BOOT_INFO_MAGIC && self.version == BOOT_INFO_VERSION
    }

    /// Check if framebuffer is available
    pub fn has_framebuffer(&self) -> bool {
        self.flags & flags::HAS_FRAMEBUFFER != 0
    }

    /// Get framebuffer config if available
    pub fn framebuffer(&self) -> Option<&FramebufferConfig> {
        if self.has_framebuffer() {
            Some(&self.framebuffer)
        } else {
            None
        }
    }

    /// Check if RSDP is available
    pub fn has_rsdp(&self) -> bool {
        self.flags & flags::HAS_RSDP != 0
    }
}

/// Get reference to boot info structure.
///
/// # Safety
///
/// This is safe because the kernel guarantees the boot info page is mapped
/// at BOOT_INFO_VADDR before any userspace task starts.
///
/// # Returns
///
/// Reference to the boot info, or None if not mapped or invalid.
pub fn get_boot_info() -> Option<&'static FolkeringBootInfo> {
    let info = unsafe { &*(BOOT_INFO_VADDR as *const FolkeringBootInfo) };

    if info.is_valid() {
        Some(info)
    } else {
        None
    }
}

/// Get boot info, panicking if not available.
///
/// Use this in code that requires boot info to function.
pub fn boot_info() -> &'static FolkeringBootInfo {
    get_boot_info().expect("Boot info not available")
}
