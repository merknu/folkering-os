//! SQLite record format parsing
//!
//! Record format:
//! [header_size: varint][col_types: varint*][values: bytes*]
//!
//! Type codes:
//! - 0: NULL
//! - 1: 8-bit signed int
//! - 2: 16-bit signed int (big-endian)
//! - 3: 24-bit signed int (big-endian)
//! - 4: 32-bit signed int (big-endian)
//! - 5: 48-bit signed int (big-endian)
//! - 6: 64-bit signed int (big-endian)
//! - 7: 64-bit IEEE float (big-endian)
//! - 8: Integer constant 0
//! - 9: Integer constant 1
//! - N >= 12, even: BLOB of (N-12)/2 bytes
//! - N >= 13, odd: TEXT of (N-13)/2 bytes

use crate::varint::decode_varint;
use crate::Error;

/// A value from a SQLite record
#[derive(Debug, Clone)]
pub enum Value<'a> {
    /// NULL value
    Null,
    /// Integer value
    Integer(i64),
    /// Float value
    Float(f64),
    /// Binary blob
    Blob(&'a [u8]),
    /// Text string (UTF-8)
    Text(&'a str),
}

impl<'a> Value<'a> {
    /// Get as integer if this is an Integer value
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Integer(v) => Some(*v),
            _ => None,
        }
    }

    /// Get as text if this is a Text value
    pub fn as_text(&self) -> Option<&'a str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Get as blob if this is a Blob value
    pub fn as_blob(&self) -> Option<&'a [u8]> {
        match self {
            Value::Blob(b) => Some(b),
            _ => None,
        }
    }
}

/// A parsed SQLite record
#[derive(Debug)]
pub struct Record<'a> {
    /// The rowid (for table leaf cells)
    pub rowid: i64,
    /// Column values
    values: &'a [u8],
    /// Column type codes
    types: &'a [u8],
    /// Number of columns
    column_count: usize,
}

impl<'a> Record<'a> {
    /// Parse a record from raw cell payload
    ///
    /// `rowid` is the rowid from the cell (0 if not applicable)
    pub fn parse(rowid: i64, payload: &'a [u8]) -> Result<Self, Error> {
        if payload.is_empty() {
            return Err(Error::InvalidRecord);
        }

        // First varint is header size (includes the size varint itself)
        let (header_size, header_size_len) = decode_varint(payload)?;
        let header_size = header_size as usize;

        if header_size > payload.len() || header_size < header_size_len {
            return Err(Error::InvalidRecord);
        }

        let types = &payload[header_size_len..header_size];
        let values = &payload[header_size..];

        // Count columns by walking through type codes
        let mut pos = 0;
        let mut column_count = 0;
        while pos < types.len() {
            let (_, len) = decode_varint(&types[pos..])?;
            pos += len;
            column_count += 1;
        }

        Ok(Self {
            rowid,
            values,
            types,
            column_count,
        })
    }

    /// Get the number of columns
    pub fn column_count(&self) -> usize {
        self.column_count
    }

    /// Get a column value by index
    pub fn get(&self, index: usize) -> Option<Value<'a>> {
        if index >= self.column_count {
            return None;
        }

        // Walk through types to find the column's type and value offset
        let mut type_pos = 0;
        let mut value_pos = 0;

        for i in 0..=index {
            let (type_code, type_len) = decode_varint(&self.types[type_pos..]).ok()?;
            let type_code = type_code as u64;

            if i == index {
                // Found the column - decode its value
                return self.decode_value(type_code, value_pos);
            }

            // Advance to next column
            type_pos += type_len;
            value_pos += value_size(type_code);
        }

        None
    }

    /// Decode a value given its type code and position in values
    fn decode_value(&self, type_code: u64, pos: usize) -> Option<Value<'a>> {
        let data = &self.values[pos..];

        match type_code {
            0 => Some(Value::Null),
            1 => {
                if data.is_empty() { return None; }
                Some(Value::Integer(data[0] as i8 as i64))
            }
            2 => {
                if data.len() < 2 { return None; }
                let val = i16::from_be_bytes([data[0], data[1]]);
                Some(Value::Integer(val as i64))
            }
            3 => {
                if data.len() < 3 { return None; }
                let val = ((data[0] as i32) << 16) | ((data[1] as i32) << 8) | (data[2] as i32);
                // Sign extend from 24 bits
                let val = if val & 0x800000 != 0 { val | !0xFFFFFF } else { val };
                Some(Value::Integer(val as i64))
            }
            4 => {
                if data.len() < 4 { return None; }
                let val = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                Some(Value::Integer(val as i64))
            }
            5 => {
                if data.len() < 6 { return None; }
                let val = ((data[0] as i64) << 40)
                    | ((data[1] as i64) << 32)
                    | ((data[2] as i64) << 24)
                    | ((data[3] as i64) << 16)
                    | ((data[4] as i64) << 8)
                    | (data[5] as i64);
                // Sign extend from 48 bits
                let val = if val & 0x800000000000 != 0 { val | !0xFFFFFFFFFFFF } else { val };
                Some(Value::Integer(val))
            }
            6 => {
                if data.len() < 8 { return None; }
                let val = i64::from_be_bytes([
                    data[0], data[1], data[2], data[3],
                    data[4], data[5], data[6], data[7],
                ]);
                Some(Value::Integer(val))
            }
            7 => {
                if data.len() < 8 { return None; }
                let bits = u64::from_be_bytes([
                    data[0], data[1], data[2], data[3],
                    data[4], data[5], data[6], data[7],
                ]);
                Some(Value::Float(f64::from_bits(bits)))
            }
            8 => Some(Value::Integer(0)),
            9 => Some(Value::Integer(1)),
            n if n >= 12 && n % 2 == 0 => {
                // BLOB
                let len = ((n - 12) / 2) as usize;
                if data.len() < len { return None; }
                Some(Value::Blob(&data[..len]))
            }
            n if n >= 13 && n % 2 == 1 => {
                // TEXT
                let len = ((n - 13) / 2) as usize;
                if data.len() < len { return None; }
                let text = core::str::from_utf8(&data[..len]).ok()?;
                Some(Value::Text(text))
            }
            _ => None,
        }
    }
}

/// Calculate the size of a value given its type code
fn value_size(type_code: u64) -> usize {
    match type_code {
        0 => 0,      // NULL
        1 => 1,      // 8-bit int
        2 => 2,      // 16-bit int
        3 => 3,      // 24-bit int
        4 => 4,      // 32-bit int
        5 => 6,      // 48-bit int
        6 => 8,      // 64-bit int
        7 => 8,      // 64-bit float
        8 => 0,      // Integer 0
        9 => 0,      // Integer 1
        n if n >= 12 && n % 2 == 0 => ((n - 12) / 2) as usize, // BLOB
        n if n >= 13 && n % 2 == 1 => ((n - 13) / 2) as usize, // TEXT
        _ => 0,
    }
}
