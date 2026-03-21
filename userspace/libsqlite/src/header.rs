//! SQLite database header parsing (first 100 bytes)

use crate::Error;

/// SQLite magic string
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Parsed SQLite database header
#[derive(Debug, Clone, Copy)]
pub struct DbHeader {
    /// Page size in bytes (power of 2, 512-65536)
    pub page_size: u32,
    /// File format write version (1 = legacy, 2 = WAL)
    pub write_version: u8,
    /// File format read version
    pub read_version: u8,
    /// Reserved bytes at end of each page
    pub reserved_bytes: u8,
    /// Maximum embedded payload fraction (must be 64)
    pub max_payload_frac: u8,
    /// Minimum embedded payload fraction (must be 32)
    pub min_payload_frac: u8,
    /// Leaf payload fraction (must be 32)
    pub leaf_payload_frac: u8,
    /// File change counter
    pub change_counter: u32,
    /// Database size in pages (0 if file grew and not yet updated)
    pub db_size_pages: u32,
    /// First freelist trunk page
    pub first_freelist_trunk: u32,
    /// Total freelist pages
    pub total_freelist_pages: u32,
    /// Schema cookie
    pub schema_cookie: u32,
    /// Schema format number (1-4)
    pub schema_format: u32,
    /// Default page cache size
    pub default_cache_size: u32,
    /// Largest root b-tree page (for auto-vacuum)
    pub largest_root_btree: u32,
    /// Text encoding (1=UTF-8, 2=UTF-16LE, 3=UTF-16BE)
    pub text_encoding: u32,
    /// User version
    pub user_version: u32,
    /// Incremental vacuum mode
    pub incremental_vacuum: u32,
    /// Application ID
    pub application_id: u32,
    /// Version valid for number
    pub version_valid_for: u32,
    /// SQLite version number
    pub sqlite_version: u32,
}

impl DbHeader {
    /// Parse header from database bytes
    pub fn parse(data: &[u8]) -> Result<Self, Error> {
        if data.len() < 100 {
            return Err(Error::TooSmall);
        }

        // Check magic
        if &data[0..16] != SQLITE_MAGIC {
            return Err(Error::InvalidMagic);
        }

        // Page size is at offset 16-17 (big-endian)
        // Special case: 1 means 65536
        let raw_page_size = u16::from_be_bytes([data[16], data[17]]);
        let page_size: u32 = if raw_page_size == 1 { 65536 } else { raw_page_size as u32 };

        // Validate page size is power of 2 and in valid range
        if page_size < 512 || (page_size & (page_size - 1)) != 0 {
            return Err(Error::InvalidPageSize);
        }

        // Database size in pages at offset 28-31
        let db_size_pages = u32::from_be_bytes([data[28], data[29], data[30], data[31]]);

        // If db_size_pages is 0, calculate from file size
        let db_size_pages = if db_size_pages == 0 {
            (data.len() / page_size as usize) as u32
        } else {
            db_size_pages
        };

        Ok(Self {
            page_size,
            write_version: data[18],
            read_version: data[19],
            reserved_bytes: data[20],
            max_payload_frac: data[21],
            min_payload_frac: data[22],
            leaf_payload_frac: data[23],
            change_counter: u32::from_be_bytes([data[24], data[25], data[26], data[27]]),
            db_size_pages,
            first_freelist_trunk: u32::from_be_bytes([data[32], data[33], data[34], data[35]]),
            total_freelist_pages: u32::from_be_bytes([data[36], data[37], data[38], data[39]]),
            schema_cookie: u32::from_be_bytes([data[40], data[41], data[42], data[43]]),
            schema_format: u32::from_be_bytes([data[44], data[45], data[46], data[47]]),
            default_cache_size: u32::from_be_bytes([data[48], data[49], data[50], data[51]]),
            largest_root_btree: u32::from_be_bytes([data[52], data[53], data[54], data[55]]),
            text_encoding: u32::from_be_bytes([data[56], data[57], data[58], data[59]]),
            user_version: u32::from_be_bytes([data[60], data[61], data[62], data[63]]),
            incremental_vacuum: u32::from_be_bytes([data[64], data[65], data[66], data[67]]),
            application_id: u32::from_be_bytes([data[68], data[69], data[70], data[71]]),
            // Bytes 72-91 are reserved
            version_valid_for: u32::from_be_bytes([data[92], data[93], data[94], data[95]]),
            sqlite_version: u32::from_be_bytes([data[96], data[97], data[98], data[99]]),
        })
    }
}
