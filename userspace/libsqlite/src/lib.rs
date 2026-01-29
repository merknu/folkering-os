//! Minimal no_std SQLite B-tree reader for Folkering OS
//!
//! This library provides read-only access to SQLite databases,
//! supporting table scans and key lookups via B-tree traversal.

#![no_std]

mod header;
mod varint;
mod page;
mod btree;
mod record;

pub use header::DbHeader;
pub use record::{Record, Value};
pub use btree::TableScanner;

/// Errors that can occur when reading SQLite databases
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Database too small to contain valid header
    TooSmall,
    /// Invalid magic bytes (not a SQLite database)
    InvalidMagic,
    /// Unsupported page size
    InvalidPageSize,
    /// Page index out of bounds
    PageOutOfBounds,
    /// Invalid B-tree page type
    InvalidPageType,
    /// Invalid varint encoding
    InvalidVarint,
    /// Record parsing failed
    InvalidRecord,
    /// Table not found in schema
    TableNotFound,
    /// Column not found in record
    ColumnNotFound,
}

/// SQLite database reader
pub struct SqliteDb<'a> {
    data: &'a [u8],
    header: DbHeader,
}

impl<'a> SqliteDb<'a> {
    /// Open a SQLite database from raw bytes
    pub fn open(data: &'a [u8]) -> Result<Self, Error> {
        let header = DbHeader::parse(data)?;
        Ok(Self { data, header })
    }

    /// Get the database header
    pub fn header(&self) -> &DbHeader {
        &self.header
    }

    /// Get page size in bytes
    pub fn page_size(&self) -> u32 {
        self.header.page_size
    }

    /// Get a page by 1-based page number
    pub fn page(&self, page_num: u32) -> Result<&'a [u8], Error> {
        if page_num == 0 || page_num > self.header.db_size_pages {
            return Err(Error::PageOutOfBounds);
        }
        let page_size = self.page_size() as usize;
        let start = (page_num as usize - 1) * page_size;
        let end = start + page_size;
        if end > self.data.len() {
            return Err(Error::PageOutOfBounds);
        }
        Ok(&self.data[start..end])
    }

    /// Get the raw database bytes
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Scan the sqlite_schema table to find a table's root page
    pub fn find_table_root(&self, table_name: &str) -> Result<u32, Error> {
        // sqlite_schema is always on page 1
        let scanner = TableScanner::new(self, 1)?;

        for result in scanner {
            let record = result?;
            // sqlite_schema columns: type, name, tbl_name, rootpage, sql
            // We want type='table' and name=table_name
            if record.column_count() >= 4 {
                if let (Some(Value::Text(type_val)), Some(Value::Text(name)), Some(Value::Integer(root))) =
                    (record.get(0), record.get(1), record.get(3))
                {
                    if type_val == "table" && name == table_name {
                        return Ok(root as u32);
                    }
                }
            }
        }
        Err(Error::TableNotFound)
    }

    /// Scan all rows in a table
    pub fn table_scan(&self, table_name: &str) -> Result<TableScanner<'a, '_>, Error> {
        let root_page = self.find_table_root(table_name)?;
        TableScanner::new(self, root_page)
    }

    /// Query a table by name column (simple WHERE name = ? lookup)
    pub fn query_by_name(&self, table_name: &str, name: &str) -> Result<Option<Record<'a>>, Error> {
        let scanner = self.table_scan(table_name)?;

        for result in scanner {
            let record = result?;
            // Assume column 1 is 'name' (after rowid column 0)
            if let Some(Value::Text(record_name)) = record.get(1) {
                if record_name == name {
                    return Ok(Some(record));
                }
            }
        }
        Ok(None)
    }
}
