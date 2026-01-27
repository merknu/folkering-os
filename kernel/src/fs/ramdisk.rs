//! Ramdisk driver for Folk-Pack (FPK) initrd images
//!
//! Parses a Folk-Pack image loaded into memory by the bootloader
//! and provides access to individual entries (ELF binaries, data blobs).

use super::format::{FpkEntry, FpkHeader, FPK_NAME_LEN};

/// Errors from ramdisk operations
#[derive(Debug)]
pub enum RamdiskError {
    /// Image too small to contain a valid header
    TooSmall,
    /// Magic bytes don't match "FOLK"
    BadMagic,
    /// Unsupported format version
    BadVersion,
    /// Entry table extends beyond image bounds
    EntryTableOverflow,
    /// An entry's data region extends beyond image bounds
    EntryDataOverflow,
}

/// A parsed Folk-Pack ramdisk image in memory
pub struct Ramdisk {
    base: *const u8,
    size: usize,
}

impl Ramdisk {
    /// Parse a Folk-Pack image from a virtual memory address.
    ///
    /// The memory at `virt_addr` must remain valid for the lifetime of the Ramdisk
    /// (typically the entire boot, since Limine modules persist in physical memory).
    ///
    /// # Safety
    /// Caller must ensure `virt_addr` points to valid, readable memory of at least `size` bytes.
    pub unsafe fn from_memory(virt_addr: usize, size: usize) -> Result<Self, RamdiskError> {
        if size < core::mem::size_of::<FpkHeader>() {
            return Err(RamdiskError::TooSmall);
        }

        let base = virt_addr as *const u8;
        let header = &*(base as *const FpkHeader);

        if !header.is_valid() {
            if header.magic != super::format::FPK_MAGIC {
                return Err(RamdiskError::BadMagic);
            }
            return Err(RamdiskError::BadVersion);
        }

        // Verify entry table fits
        let entry_table_end = core::mem::size_of::<FpkHeader>()
            + (header.entry_count as usize) * core::mem::size_of::<FpkEntry>();
        if entry_table_end > size {
            return Err(RamdiskError::EntryTableOverflow);
        }

        // Verify each entry's data region is within bounds
        let entries = Self::entries_from_raw(base, header.entry_count as usize);
        for entry in entries {
            let end = entry.offset as usize + entry.size as usize;
            if end > size {
                return Err(RamdiskError::EntryDataOverflow);
            }
        }

        Ok(Ramdisk { base, size })
    }

    /// Get the image header
    fn header(&self) -> &FpkHeader {
        unsafe { &*(self.base as *const FpkHeader) }
    }

    /// Helper: get entries slice from raw pointer
    unsafe fn entries_from_raw(base: *const u8, count: usize) -> &'static [FpkEntry] {
        let entries_ptr = base.add(core::mem::size_of::<FpkHeader>()) as *const FpkEntry;
        core::slice::from_raw_parts(entries_ptr, count)
    }

    /// Get all entry descriptors
    pub fn entries(&self) -> &[FpkEntry] {
        unsafe { Self::entries_from_raw(self.base, self.header().entry_count as usize) }
    }

    /// Find an entry by name
    pub fn find(&self, name: &str) -> Option<&FpkEntry> {
        let name_bytes = name.as_bytes();
        if name_bytes.len() >= FPK_NAME_LEN {
            return None;
        }
        for e in self.entries() {
            let len = e.name.iter().position(|&b| b == 0).unwrap_or(FPK_NAME_LEN);
            if len == name_bytes.len() {
                let mut matched = true;
                for j in 0..len {
                    if e.name[j] != name_bytes[j] {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    return Some(e);
                }
            }
        }
        None
    }

    /// Get the raw bytes for an entry
    pub fn read(&self, entry: &FpkEntry) -> &[u8] {
        unsafe {
            let ptr = self.base.add(entry.offset as usize);
            core::slice::from_raw_parts(ptr, entry.size as usize)
        }
    }

    /// Get total number of entries
    pub fn entry_count(&self) -> usize {
        self.header().entry_count as usize
    }

    /// Get total image size
    pub fn image_size(&self) -> usize {
        self.size
    }
}
