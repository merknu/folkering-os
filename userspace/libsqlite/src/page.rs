//! SQLite B-tree page structures
//!
//! Page types:
//! - 0x02: Interior index b-tree page
//! - 0x05: Interior table b-tree page
//! - 0x0a: Leaf index b-tree page
//! - 0x0d: Leaf table b-tree page

use crate::Error;

/// B-tree page type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    /// Interior index b-tree page (0x02)
    InteriorIndex,
    /// Interior table b-tree page (0x05)
    InteriorTable,
    /// Leaf index b-tree page (0x0a)
    LeafIndex,
    /// Leaf table b-tree page (0x0d)
    LeafTable,
}

impl PageType {
    /// Parse page type from byte
    pub fn from_byte(byte: u8) -> Result<Self, Error> {
        match byte {
            0x02 => Ok(PageType::InteriorIndex),
            0x05 => Ok(PageType::InteriorTable),
            0x0a => Ok(PageType::LeafIndex),
            0x0d => Ok(PageType::LeafTable),
            _ => Err(Error::InvalidPageType),
        }
    }

    /// Whether this is a leaf page
    pub fn is_leaf(self) -> bool {
        matches!(self, PageType::LeafIndex | PageType::LeafTable)
    }

    /// Whether this is a table (vs index) page
    pub fn is_table(self) -> bool {
        matches!(self, PageType::InteriorTable | PageType::LeafTable)
    }
}

/// Parsed B-tree page header
#[derive(Debug, Clone, Copy)]
pub struct BTreePageHeader {
    /// Page type
    pub page_type: PageType,
    /// First freeblock offset (0 if none)
    pub first_freeblock: u16,
    /// Number of cells on this page
    pub cell_count: u16,
    /// Start of cell content area (0 means 65536)
    pub cell_content_start: u16,
    /// Number of fragmented free bytes
    pub fragmented_bytes: u8,
    /// Right-most pointer (interior pages only)
    pub right_pointer: Option<u32>,
}

impl BTreePageHeader {
    /// Parse header from page bytes
    ///
    /// `header_offset` is 0 for normal pages, 100 for page 1 (after DB header)
    pub fn parse(page: &[u8], header_offset: usize) -> Result<Self, Error> {
        if page.len() < header_offset + 8 {
            return Err(Error::TooSmall);
        }

        let data = &page[header_offset..];
        let page_type = PageType::from_byte(data[0])?;

        let first_freeblock = u16::from_be_bytes([data[1], data[2]]);
        let cell_count = u16::from_be_bytes([data[3], data[4]]);
        let cell_content_start = u16::from_be_bytes([data[5], data[6]]);
        let fragmented_bytes = data[7];

        // Interior pages have a 4-byte right pointer after the header
        let right_pointer = if !page_type.is_leaf() {
            if data.len() < 12 {
                return Err(Error::TooSmall);
            }
            Some(u32::from_be_bytes([data[8], data[9], data[10], data[11]]))
        } else {
            None
        };

        Ok(Self {
            page_type,
            first_freeblock,
            cell_count,
            cell_content_start,
            fragmented_bytes,
            right_pointer,
        })
    }

    /// Get the size of this header in bytes
    pub fn header_size(&self) -> usize {
        if self.page_type.is_leaf() { 8 } else { 12 }
    }
}

/// Get cell pointer array from a page
///
/// Returns iterator over cell offsets
pub fn get_cell_pointers<'a>(page: &'a [u8], header: &BTreePageHeader, header_offset: usize) -> impl Iterator<Item = u16> + 'a {
    let array_start = header_offset + header.header_size();
    let count = header.cell_count as usize;

    (0..count).map(move |i| {
        let offset = array_start + i * 2;
        u16::from_be_bytes([page[offset], page[offset + 1]])
    })
}
