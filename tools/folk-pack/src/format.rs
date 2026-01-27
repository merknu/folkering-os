//! Folk-Pack (FPK) binary format definitions (host-side copy)
//!
//! These structs mirror kernel/src/fs/format.rs exactly.
//! Both must be kept in sync.

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
    pub magic: [u8; 4],
    pub version: u16,
    pub entry_count: u16,
    pub total_size: u64,
    pub reserved: [u8; 48],
}

/// Folk-Pack entry descriptor (64 bytes, fixed size)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct FpkEntry {
    pub id: u16,
    pub entry_type: u16,
    pub name: [u8; FPK_NAME_LEN],
    pub offset: u64,
    pub size: u64,
    pub hash: [u8; 8],
}

impl FpkHeader {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                self as *const Self as *const u8,
                core::mem::size_of::<Self>(),
            )
        }
    }
}

impl FpkEntry {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                self as *const Self as *const u8,
                core::mem::size_of::<Self>(),
            )
        }
    }
}
