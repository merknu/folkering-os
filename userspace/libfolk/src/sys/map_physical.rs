//! Physical Memory Mapping
//!
//! Provides syscall wrappers for mapping physical device memory into
//! the task's virtual address space.
//!
//! # Usage
//!
//! ```ignore
//! use libfolk::sys::map_physical::{map_physical, MapFlags};
//!
//! // Map framebuffer with write-combining
//! let result = map_physical(
//!     fb_phys_addr,
//!     fb_virt_addr,
//!     fb_size,
//!     MapFlags::READ | MapFlags::WRITE | MapFlags::CACHE_WC,
//! );
//! ```
//!
//! # Security
//!
//! This syscall requires a capability that covers the requested physical
//! address range. For framebuffer access, the kernel grants a Framebuffer
//! capability to the compositor at boot.

use crate::syscall::syscall5;

/// Syscall number for map_physical (must match kernel)
const SYS_MAP_PHYSICAL: u64 = 0x24;

/// Mapping flags for map_physical syscall
pub mod flags {
    /// Allow reading from mapped memory
    pub const MAP_READ: u64 = 0x01;
    /// Allow writing to mapped memory
    pub const MAP_WRITE: u64 = 0x02;
    /// Allow executing from mapped memory
    pub const MAP_EXEC: u64 = 0x04;
    /// Use Write-Combining caching (for framebuffer)
    pub const MAP_CACHE_WC: u64 = 0x10;
    /// Use Uncached mode (for MMIO devices)
    pub const MAP_CACHE_UC: u64 = 0x20;
}

/// Convenient flag type
pub struct MapFlags(u64);

impl MapFlags {
    pub const READ: MapFlags = MapFlags(flags::MAP_READ);
    pub const WRITE: MapFlags = MapFlags(flags::MAP_WRITE);
    pub const EXEC: MapFlags = MapFlags(flags::MAP_EXEC);
    pub const CACHE_WC: MapFlags = MapFlags(flags::MAP_CACHE_WC);
    pub const CACHE_UC: MapFlags = MapFlags(flags::MAP_CACHE_UC);

    /// Create empty flags
    pub const fn empty() -> Self {
        MapFlags(0)
    }

    /// Get raw value
    pub const fn bits(&self) -> u64 {
        self.0
    }
}

impl core::ops::BitOr for MapFlags {
    type Output = MapFlags;

    fn bitor(self, rhs: Self) -> Self::Output {
        MapFlags(self.0 | rhs.0)
    }
}

/// Error type for map_physical
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// Permission denied (no capability)
    PermissionDenied,
    /// Invalid address (not aligned, in kernel space, etc.)
    InvalidAddress,
    /// Invalid size
    InvalidSize,
    /// Page table operation failed
    MappingFailed,
    /// Unknown error
    Unknown,
}

/// Map physical memory into this task's address space.
///
/// # Arguments
///
/// * `phys_addr` - Physical address to map (must be page-aligned)
/// * `virt_addr` - Virtual address to map to (must be page-aligned)
/// * `size` - Size in bytes to map (rounded up to page boundary)
/// * `flags` - Mapping flags
///
/// # Returns
///
/// * `Ok(())` on success
/// * `Err(MapError)` on failure
///
/// # Example
///
/// ```ignore
/// // Map framebuffer at 4GB virtual address with write-combining
/// map_physical(
///     fb_config.physical_address,
///     0x1_0000_0000,  // 4GB
///     fb_config.size_bytes() as u64,
///     MapFlags::READ | MapFlags::WRITE | MapFlags::CACHE_WC,
/// )?;
/// ```
pub fn map_physical(
    phys_addr: u64,
    virt_addr: u64,
    size: u64,
    flags: MapFlags,
) -> Result<(), MapError> {
    let result = unsafe {
        syscall5(SYS_MAP_PHYSICAL, phys_addr, virt_addr, size, flags.bits(), 0)
    };

    if result == 0 {
        Ok(())
    } else {
        Err(MapError::Unknown)
    }
}

/// Map physical memory with default read/write permissions.
///
/// Convenience wrapper for common case of mapping device memory.
pub fn map_physical_rw(phys_addr: u64, virt_addr: u64, size: u64) -> Result<(), MapError> {
    map_physical(
        phys_addr,
        virt_addr,
        size,
        MapFlags::READ | MapFlags::WRITE,
    )
}

/// Map framebuffer with write-combining.
///
/// Convenience wrapper specifically for framebuffer mapping.
/// Uses write-combining for optimal write performance.
pub fn map_framebuffer(phys_addr: u64, virt_addr: u64, size: u64) -> Result<(), MapError> {
    map_physical(
        phys_addr,
        virt_addr,
        size,
        MapFlags::READ | MapFlags::WRITE | MapFlags::CACHE_WC,
    )
}
