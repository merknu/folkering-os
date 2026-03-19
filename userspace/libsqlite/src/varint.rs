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

/// Encode a u64 value as a SQLite varint into buf.
/// Returns the number of bytes written. buf must be at least 9 bytes.
pub fn encode_varint(value: u64, buf: &mut [u8]) -> usize {
    if value <= 127 {
        buf[0] = value as u8;
        return 1;
    }

    // Count how many bytes we need
    let bytes_needed = if value <= 0x3FFF { 2 }
        else if value <= 0x1FFFFF { 3 }
        else if value <= 0xFFFFFFF { 4 }
        else if value <= 0x7_FFFFFFFF { 5 }
        else if value <= 0x3FF_FFFFFFFF { 6 }
        else if value <= 0x1FFFF_FFFFFFFF { 7 }
        else if value <= 0xFFFFFF_FFFFFFFF { 8 }
        else { 9 };

    if bytes_needed == 9 {
        // First 8 bytes use 7 bits each (with continuation), 9th uses all 8 bits
        let mut v = value;
        buf[8] = (v & 0xFF) as u8;
        v >>= 8;
        let mut i = 7;
        loop {
            buf[i] = 0x80 | ((v & 0x7F) as u8);
            v >>= 7;
            if i == 0 { break; }
            i -= 1;
        }
        return 9;
    }

    // For 2-8 bytes: all except last have continuation bit, 7 bits per byte, MSB first
    let mut v = value;
    let mut i = bytes_needed - 1;
    buf[i] = (v & 0x7F) as u8;
    v >>= 7;
    while i > 0 {
        i -= 1;
        buf[i] = 0x80 | ((v & 0x7F) as u8);
        v >>= 7;
    }
    bytes_needed
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

    #[test]
    fn test_encode_roundtrip() {
        let test_values: &[u64] = &[
            0, 1, 127, 128, 129, 16383, 16384, 0xFFFFFFF,
            0x10000000, 0x7_FFFFFFFF, 0xFFFFFF_FFFFFFFF, u64::MAX,
        ];
        let mut buf = [0u8; 9];
        for &val in test_values {
            let len = encode_varint(val, &mut buf);
            let (decoded, decoded_len) = decode_varint(&buf[..len]).unwrap();
            assert_eq!(decoded as u64, val, "roundtrip failed for {}", val);
            assert_eq!(decoded_len, len, "length mismatch for {}", val);
        }
    }
}
