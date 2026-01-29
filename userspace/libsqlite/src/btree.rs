//! SQLite B-tree traversal

use crate::page::{BTreePageHeader, get_cell_pointers};
use crate::record::Record;
use crate::varint::decode_varint;
use crate::{Error, SqliteDb};

/// Maximum traversal depth to prevent stack overflow
const MAX_DEPTH: usize = 64;

/// Scanner for iterating over table rows
pub struct TableScanner<'a, 'db> {
    db: &'db SqliteDb<'a>,
    /// Stack of (page_num, cell_index) for traversal
    stack: [(u32, u16); MAX_DEPTH],
    /// Current stack depth
    depth: usize,
    /// Whether iteration has finished
    finished: bool,
}

impl<'a, 'db> TableScanner<'a, 'db> {
    /// Create a new scanner starting at the given root page
    pub fn new(db: &'db SqliteDb<'a>, root_page: u32) -> Result<Self, Error> {
        let mut scanner = Self {
            db,
            stack: [(0, 0); MAX_DEPTH],
            depth: 0,
            finished: false,
        };

        // Descend to leftmost leaf
        scanner.descend_to_leaf(root_page)?;

        Ok(scanner)
    }

    /// Descend from a page to its leftmost leaf
    fn descend_to_leaf(&mut self, mut page_num: u32) -> Result<(), Error> {
        loop {
            if self.depth >= MAX_DEPTH {
                return Err(Error::PageOutOfBounds);
            }

            let page = self.db.page(page_num)?;
            let header_offset = if page_num == 1 { 100 } else { 0 };
            let header = BTreePageHeader::parse(page, header_offset)?;

            // Push this page onto the stack
            self.stack[self.depth] = (page_num, 0);
            self.depth += 1;

            if header.page_type.is_leaf() {
                // Reached a leaf - done descending
                break;
            }

            // Interior page - descend to first child
            let child_page = self.get_first_child(page, &header, header_offset)?;
            page_num = child_page;
        }

        Ok(())
    }

    /// Get the first child page number from an interior page
    fn get_first_child(&self, page: &[u8], header: &BTreePageHeader, header_offset: usize) -> Result<u32, Error> {
        if header.cell_count == 0 {
            // No cells - right pointer is the only child
            header.right_pointer.ok_or(Error::InvalidPageType)
        } else {
            // Get first cell's left child pointer
            let first_cell_offset = get_cell_pointers(page, header, header_offset)
                .next()
                .ok_or(Error::InvalidRecord)?;

            // Interior table cell format: [left_child: u32][rowid: varint][payload_size: varint][payload]
            // We just need the left child pointer (first 4 bytes)
            let cell = &page[first_cell_offset as usize..];
            if cell.len() < 4 {
                return Err(Error::InvalidRecord);
            }
            Ok(u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]))
        }
    }

    /// Get nth cell offset from page (0-indexed)
    fn get_cell_offset(&self, page: &[u8], header: &BTreePageHeader, header_offset: usize, n: u16) -> Option<u16> {
        get_cell_pointers(page, header, header_offset).nth(n as usize)
    }

    /// Parse a leaf table cell and return the record
    fn parse_leaf_cell(&self, page: &[u8], cell_offset: u16) -> Result<Record<'a>, Error> {
        let cell = &page[cell_offset as usize..];

        // Leaf table cell format: [payload_size: varint][rowid: varint][payload]
        let (payload_size, ps_len) = decode_varint(cell)?;
        let (rowid, rowid_len) = decode_varint(&cell[ps_len..])?;

        let payload_start = ps_len + rowid_len;
        let payload_size = payload_size as usize;

        // For now, we only handle inline payloads (no overflow pages)
        // TODO: Handle overflow pages for large records
        if payload_start + payload_size > cell.len() {
            // This could be an overflow situation - for now, take what we can
            let available = cell.len() - payload_start;
            if available == 0 {
                return Err(Error::InvalidRecord);
            }
            let payload = &cell[payload_start..];

            // Convert lifetime - safe because page data comes from db.data
            let payload: &'a [u8] = unsafe {
                core::slice::from_raw_parts(payload.as_ptr(), payload.len())
            };

            return Record::parse(rowid, payload);
        }

        let payload = &cell[payload_start..payload_start + payload_size];

        // Convert lifetime - safe because page data comes from db.data
        let payload: &'a [u8] = unsafe {
            core::slice::from_raw_parts(payload.as_ptr(), payload.len())
        };

        Record::parse(rowid, payload)
    }

    /// Advance to the next cell, potentially moving to sibling/parent pages
    fn advance(&mut self) -> Result<bool, Error> {
        while self.depth > 0 {
            let (page_num, cell_idx) = self.stack[self.depth - 1];
            let page = self.db.page(page_num)?;
            let header_offset = if page_num == 1 { 100 } else { 0 };
            let header = BTreePageHeader::parse(page, header_offset)?;

            let next_cell = cell_idx + 1;

            if header.page_type.is_leaf() {
                if next_cell < header.cell_count {
                    // More cells in this leaf
                    self.stack[self.depth - 1].1 = next_cell;
                    return Ok(true);
                } else {
                    // Leaf exhausted - go back up
                    self.depth -= 1;
                    continue;
                }
            } else {
                // Interior page
                if next_cell < header.cell_count {
                    // Move to next cell and descend
                    self.stack[self.depth - 1].1 = next_cell;

                    // Get the child pointer from this cell
                    let cell_offset = self.get_cell_offset(page, &header, header_offset, next_cell)
                        .ok_or(Error::InvalidRecord)?;
                    let cell = &page[cell_offset as usize..];

                    // Interior table cell: [left_child: u32][rowid: varint]...
                    // We want the left child of the NEXT cell, which is stored in this cell
                    if cell.len() < 4 {
                        return Err(Error::InvalidRecord);
                    }
                    let child = u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]);

                    self.descend_to_leaf(child)?;
                    return Ok(true);
                } else if let Some(right_ptr) = header.right_pointer {
                    // No more cells, but there's a right pointer
                    self.stack[self.depth - 1].1 = next_cell;
                    self.descend_to_leaf(right_ptr)?;
                    return Ok(true);
                } else {
                    // Interior page exhausted
                    self.depth -= 1;
                    continue;
                }
            }
        }

        // Stack empty - done
        Ok(false)
    }
}

impl<'a, 'db> Iterator for TableScanner<'a, 'db> {
    type Item = Result<Record<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        if self.depth == 0 {
            self.finished = true;
            return None;
        }

        // Get current position
        let (page_num, cell_idx) = self.stack[self.depth - 1];

        // Get the page and header
        let page = match self.db.page(page_num) {
            Ok(p) => p,
            Err(e) => {
                self.finished = true;
                return Some(Err(e));
            }
        };

        let header_offset = if page_num == 1 { 100 } else { 0 };
        let header = match BTreePageHeader::parse(page, header_offset) {
            Ok(h) => h,
            Err(e) => {
                self.finished = true;
                return Some(Err(e));
            }
        };

        // Verify we're at a leaf
        if !header.page_type.is_leaf() || !header.page_type.is_table() {
            self.finished = true;
            return Some(Err(Error::InvalidPageType));
        }

        // Get cell offset
        let cell_offset = match self.get_cell_offset(page, &header, header_offset, cell_idx) {
            Some(offset) => offset,
            None => {
                self.finished = true;
                return None;
            }
        };

        // Parse the cell
        let record = match self.parse_leaf_cell(page, cell_offset) {
            Ok(r) => r,
            Err(e) => {
                self.finished = true;
                return Some(Err(e));
            }
        };

        // Advance for next iteration
        match self.advance() {
            Ok(false) => self.finished = true,
            Err(e) => {
                self.finished = true;
                return Some(Err(e));
            }
            _ => {}
        }

        Some(Ok(record))
    }
}
