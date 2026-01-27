//! Folk-Pack (FPK) binary format definitions
//!
//! Shared format for the initrd image used by both the host-side
//! folk-pack tool and the kernel ramdisk driver.
//!
//! Layout:
//! - Header (64 bytes)
//! - Entry table (64 bytes × N)
//! - Data section (page-aligned blobs)

/// Magic bytes identifying a Folk-Pack image
pub const FPK_MAGIC: [u8; 4] = *b"FOLK";

/// Current format version
pub const FPK_VERSION: u16 = 1;

/// Page alignment for data blobs (4 KiB)
pub const FPK_PAGE_SIZE: usize = 4096;

/// Maximum name length (null-padded)
pub const FPK_NAME_LEN: usize = 32;

/// Entry type: ELF executable
pub const ENTRY_TYPE_ELF: u16 = 0;

/// Entry type: raw data blob
pub const ENTRY_TYPE_DATA: u16 = 1;

/// Folk-Pack image header (64 bytes, fixed size)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct FpkHeader {
    /// Magic identifier: b"FOLK"
    pub magic: [u8; 4],
    /// Format version (currently 1)
    pub version: u16,
    /// Number of entries in the entry table
    pub entry_count: u16,
    /// Total size of the entire image in bytes
    pub total_size: u64,
    /// Reserved for future use
    pub reserved: [u8; 48],
}

const _: () = assert!(core::mem::size_of::<FpkHeader>() == 64);

impl FpkHeader {
    /// Validate that this header has correct magic and version
    pub fn is_valid(&self) -> bool {
        self.magic == FPK_MAGIC && self.version == FPK_VERSION
    }
}

/// Folk-Pack entry descriptor (64 bytes, fixed size)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct FpkEntry {
    /// Unique entry ID (0-based)
    pub id: u16,
    /// Entry type (0 = ELF, 1 = DATA)
    pub entry_type: u16,
    /// Null-padded name (max 32 bytes including null)
    pub name: [u8; FPK_NAME_LEN],
    /// Byte offset from start of file to entry data
    pub offset: u64,
    /// Size of entry data in bytes
    pub size: u64,
    /// First 8 bytes of SHA-256 hash (integrity check)
    pub hash: [u8; 8],
}

const _: () = assert!(core::mem::size_of::<FpkEntry>() == 64);

/// Directory entry for userspace (subset of FpkEntry, no offset/hash)
///
/// Shared between kernel and userspace via the FS_READ_DIR syscall.
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct DirEntry {
    /// Unique entry ID (0-based)
    pub id: u16,
    /// Entry type (0 = ELF, 1 = DATA)
    pub entry_type: u16,
    /// Null-padded name (max 32 bytes)
    pub name: [u8; FPK_NAME_LEN],
    /// File size in bytes
    pub size: u64,
}

// 2 + 2 + 32 + 8 = 44 bytes (packed, no padding)
const _: () = assert!(core::mem::size_of::<DirEntry>() == 44);

impl FpkEntry {
    /// Get the entry name as a &str (up to first null byte)
    pub fn name_str(&self) -> &str {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(FPK_NAME_LEN);
        // Safety: names are ASCII, written by folk-pack tool
        core::str::from_utf8(&self.name[..len]).unwrap_or("<invalid>")
    }

    /// Check if this entry is an ELF executable
    pub fn is_elf(&self) -> bool {
        self.entry_type == ENTRY_TYPE_ELF
    }
}
