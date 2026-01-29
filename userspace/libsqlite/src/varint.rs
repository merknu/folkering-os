//! SQLite varint encoding/decoding
//!
//! SQLite uses a variable-length integer encoding where:
//! - Each byte uses 7 bits for data and 1 bit as continuation flag
//! - If high bit is set, more bytes follow
//! - Maximum 9 bytes (64-bit values)
//! - The 9th byte uses all 8 bits

use crate::Error;

/// Decode a varint from a byte slice
///
/// Returns (value, bytes_consumed) or error if invalid
pub fn decode_varint(bytes: &[u8]) -> Result<(i64, usize), Error> {
    if bytes.is_empty() {
        return Err(Error::InvalidVarint);
    }

    let mut value: u64 = 0;

    // First 8 bytes use 7 bits each
    for (i, &byte) in bytes.iter().take(8).enumerate() {
        if byte < 0x80 {
            // No continuation - this is the last byte
            value = (value << 7) | (byte as u64);
            return Ok((value as i64, i + 1));
        }
        value = (value << 7) | ((byte & 0x7F) as u64);
    }

    // 9th byte uses all 8 bits
    if bytes.len() >= 9 {
        value = (value << 8) | (bytes[8] as u64);
        Ok((value as i64, 9))
    } else {
        Err(Error::InvalidVarint)
    }
}

/// Get the size of a varint without fully decoding it
pub fn varint_size(bytes: &[u8]) -> Result<usize, Error> {
    for (i, &byte) in bytes.iter().take(9).enumerate() {
        if byte < 0x80 {
            return Ok(i + 1);
        }
    }
    if bytes.len() >= 9 {
        Ok(9)
    } else {
        Err(Error::InvalidVarint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_byte() {
        // Values 0-127 encode in single byte
        assert_eq!(decode_varint(&[0x00]).unwrap(), (0, 1));
        assert_eq!(decode_varint(&[0x01]).unwrap(), (1, 1));
        assert_eq!(decode_varint(&[0x7F]).unwrap(), (127, 1));
    }

    #[test]
    fn test_two_bytes() {
        // 128 = 0x80 0x00 -> (1 << 7) | 0 = 128
        assert_eq!(decode_varint(&[0x81, 0x00]).unwrap(), (128, 2));
        // 129 = 0x81 0x01
        assert_eq!(decode_varint(&[0x81, 0x01]).unwrap(), (129, 2));
        // 16383 = 0xFF 0x7F
        assert_eq!(decode_varint(&[0xFF, 0x7F]).unwrap(), (16383, 2));
    }

    #[test]
    fn test_three_bytes() {
        // 16384 = 0x81 0x80 0x00
        assert_eq!(decode_varint(&[0x81, 0x80, 0x00]).unwrap(), (16384, 3));
    }
}
