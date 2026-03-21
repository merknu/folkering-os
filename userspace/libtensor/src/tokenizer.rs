//! BPE Tokenizer — Greedy Longest Prefix Match (ULTRA 27, 29)
//!
//! Zero-alloc tokenizer that operates directly on mmap'd GGUF vocabulary data.
//! Uses greedy longest prefix match against the full vocabulary for encoding.
//!
//! Key design decisions:
//! - Scans vocab strings directly in GGUF mmap'd data (zero-copy)
//! - Builds offset table in arena for O(1) token→string lookup
//! - Uses GPT-2 byte encoding: raw bytes → Unicode chars for matching
//! - Falls back to byte tokens <0xHH> for unmatched bytes

use crate::arena::BumpArena;

// ============================================================================
// GPT-2 Byte Encoding
// ============================================================================
//
// SmolLM2 (and GPT-2 family) tokenizers use a byte-to-unicode mapping:
// - Printable ASCII (33-126): unchanged (1-byte UTF-8)
// - Latin-1 supplement (161-172, 174-255): unchanged (2-byte UTF-8)
// - All other bytes (0-32, 127-160, 173): mapped to U+0100..U+0143
//
// Key examples:
//   Space (0x20) → Ġ (U+0120, \xC4\xA0)
//   Newline (0x0A) → Ċ (U+010A, \xC4\x8A)
//   Tab (0x09) → ĉ (U+0109, \xC4\x89)

/// Check if a byte is in the GPT-2 "printable" set (maps to itself).
#[inline]
fn is_gpt2_printable(b: u8) -> bool {
    (b >= 33 && b <= 126) || (b >= 161 && b <= 172) || (b >= 174)
}

/// Encode a single raw byte to its GPT-2 UTF-8 representation.
/// Returns (utf8_bytes, length) where length is 1 or 2.
#[inline]
fn gpt2_encode_byte(b: u8) -> ([u8; 2], usize) {
    if is_gpt2_printable(b) {
        if b < 128 {
            // Printable ASCII: 1-byte UTF-8
            return ([b, 0], 1);
        } else {
            // Latin-1 supplement: 2-byte UTF-8 encoding of codepoint = b
            return ([0xC0 | (b >> 6), 0x80 | (b & 0x3F)], 2);
        }
    }
    // Non-printable: compute index among non-printable bytes
    let n: u16 = if b <= 32 {
        b as u16
    } else if b == 127 {
        33
    } else if b <= 160 {
        34 + (b as u16 - 128)
    } else {
        // b == 173
        34 + 33
    };
    let codepoint = 256 + n;
    // 2-byte UTF-8 encoding: codepoints 256-323 → \xC4\x80..\xC5\x03
    let utf8 = [0xC0 | ((codepoint >> 6) as u8), 0x80 | ((codepoint & 0x3F) as u8)];
    (utf8, 2)
}

/// Decode a GPT-2 UTF-8 character back to a raw byte.
/// Returns (raw_byte, bytes_consumed).
#[inline]
fn gpt2_decode_char(utf8: &[u8]) -> (u8, usize) {
    if utf8.is_empty() {
        return (0, 0);
    }
    if utf8[0] < 128 {
        // ASCII: in GPT-2 printable set, maps to itself
        return (utf8[0], 1);
    }
    if utf8.len() >= 2 && (utf8[0] & 0xE0) == 0xC0 {
        let codepoint = ((utf8[0] as u16 & 0x1F) << 6) | (utf8[1] as u16 & 0x3F);
        if codepoint >= 256 {
            // Non-printable byte, reverse the mapping
            let n = codepoint - 256;
            let byte = if n <= 32 {
                n as u8
            } else if n == 33 {
                127
            } else if n <= 66 {
                (128 + n - 34) as u8
            } else {
                173
            };
            return (byte, 2);
        } else {
            // Latin-1 supplement: codepoint = byte value
            return (codepoint as u8, 2);
        }
    }
    // 3+ byte UTF-8 or invalid: pass through first byte
    (utf8[0], 1)
}

/// BPE Tokenizer operating on mmap'd GGUF vocabulary data.
///
/// The tokenizer builds an offset table at init time for fast token→string lookups.
/// The offset table maps token ID → byte offset within the GGUF data where that
/// token's string begins (after the length prefix).
pub struct BpeTokenizer<'a> {
    /// Raw GGUF file data (mmap'd)
    data: &'a [u8],
    /// Offset within data where the tokens string array starts
    /// (past the array header, at the first string element)
    vocab_offset: usize,
    /// Number of tokens in vocabulary
    vocab_size: usize,
    /// BOS token ID
    pub bos_id: u32,
    /// EOS token ID
    pub eos_id: u32,
    /// Offset table: vocab_size entries, each u32 is byte offset from data start
    /// to the length-prefix of that token string. Allocated in arena.
    offsets: &'a [u32],
    /// Lengths table: vocab_size entries, each u16 is string length
    lengths: &'a [u16],
}

impl<'a> BpeTokenizer<'a> {
    /// Initialize tokenizer from GGUF data and metadata.
    ///
    /// Builds an offset+length table by scanning through the vocab string array once.
    /// Arena usage: vocab_size * 6 bytes (4 for offset + 2 for length).
    /// For SmolLM vocab_size=49152: ~288KB.
    pub fn new(
        data: &'a [u8],
        vocab_offset: usize,
        vocab_size: usize,
        bos_id: u32,
        eos_id: u32,
        arena: &'a BumpArena,
    ) -> Option<Self> {
        if vocab_offset == 0 || vocab_size == 0 {
            return None;
        }

        // Allocate offset and length tables
        let offsets = arena.alloc_slice::<u32>(vocab_size)?;
        let lengths = arena.alloc_slice::<u16>(vocab_size)?;

        // Build tables by scanning through the GGUF string array
        let mut pos = vocab_offset;
        for i in 0..vocab_size {
            if pos + 8 > data.len() {
                return None; // truncated
            }
            // GGUF strings: u64 length prefix, then bytes
            let str_len = u64::from_le_bytes([
                data[pos], data[pos + 1], data[pos + 2], data[pos + 3],
                data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7],
            ]) as usize;
            pos += 8;

            if pos + str_len > data.len() {
                return None; // truncated
            }

            offsets[i] = pos as u32;
            lengths[i] = str_len.min(u16::MAX as usize) as u16;
            pos += str_len;
        }

        Some(Self {
            data,
            vocab_offset,
            vocab_size,
            bos_id,
            eos_id,
            offsets,
            lengths,
        })
    }

    /// Get the string bytes for a token ID.
    #[inline]
    pub fn token_bytes(&self, id: u32) -> &[u8] {
        if (id as usize) >= self.vocab_size {
            return b"";
        }
        let offset = self.offsets[id as usize] as usize;
        let len = self.lengths[id as usize] as usize;
        &self.data[offset..offset + len]
    }

    /// Get token as UTF-8 string (may be invalid for byte tokens).
    pub fn token_str(&self, id: u32) -> &str {
        core::str::from_utf8(self.token_bytes(id)).unwrap_or("")
    }

    /// Encode text into token IDs using Greedy Longest Prefix Match.
    ///
    /// ULTRA 27: For each position, find the longest vocabulary token that matches.
    /// Uses GPT-2 byte encoding: each raw input byte is converted to its GPT-2
    /// Unicode representation before matching against vocabulary tokens.
    ///
    /// Returns the number of tokens written to `output`.
    pub fn encode(&self, text: &[u8], output: &mut [u32]) -> usize {
        if text.is_empty() || output.is_empty() {
            return 0;
        }

        let mut n_tokens = 0usize;
        let mut pos = 0usize;

        while pos < text.len() && n_tokens < output.len() {
            let mut best_id: u32 = u32::MAX;
            let mut best_len: usize = 0;

            for tok_id in 0..self.vocab_size {
                let tok_bytes = self.token_bytes(tok_id as u32);
                if tok_bytes.is_empty() {
                    continue;
                }

                // Try to match this token against text[pos..] using GPT-2 encoding
                let matched_len = self.try_match_gpt2(text, pos, tok_bytes);
                if matched_len > best_len {
                    best_len = matched_len;
                    best_id = tok_id as u32;
                }
            }

            if best_id != u32::MAX && best_len > 0 {
                output[n_tokens] = best_id;
                n_tokens += 1;
                pos += best_len;
            } else {
                // Should not happen with GPT-2 encoding (every byte has a token),
                // but fallback to byte token just in case
                let byte_tok = self.find_byte_token(text[pos]);
                output[n_tokens] = byte_tok;
                n_tokens += 1;
                pos += 1;
            }
        }

        n_tokens
    }

    /// Try to match a vocab token against text at position `pos`.
    ///
    /// Uses GPT-2 byte encoding: each raw input byte is converted to its
    /// GPT-2 Unicode representation (1-2 UTF-8 bytes) and compared against
    /// the token's bytes. This correctly handles space→Ġ, newline→Ċ, etc.
    ///
    /// Returns the number of raw input bytes consumed, or 0 if no match.
    #[inline]
    fn try_match_gpt2(&self, text: &[u8], pos: usize, tok: &[u8]) -> usize {
        let mut text_pos = pos;
        let mut tok_pos = 0;

        while tok_pos < tok.len() && text_pos < text.len() {
            let (encoded, enc_len) = gpt2_encode_byte(text[text_pos]);
            let remaining_tok = tok.len() - tok_pos;
            if remaining_tok < enc_len {
                return 0;
            }
            if tok[tok_pos..tok_pos + enc_len] != encoded[..enc_len] {
                return 0;
            }
            tok_pos += enc_len;
            text_pos += 1;
        }

        if tok_pos == tok.len() {
            text_pos - pos // number of raw input bytes consumed
        } else {
            0 // token not fully matched
        }
    }

    /// Find the byte fallback token for a given byte value.
    /// Searches for tokens of form "<0xHH>" in the vocabulary.
    fn find_byte_token(&self, byte: u8) -> u32 {
        // Build the expected byte token string: <0xHH>
        let hex_chars = b"0123456789ABCDEF";
        let expected: [u8; 6] = [
            b'<', b'0', b'x',
            hex_chars[(byte >> 4) as usize],
            hex_chars[(byte & 0x0F) as usize],
            b'>',
        ];

        for tok_id in 0..self.vocab_size {
            let tok = self.token_bytes(tok_id as u32);
            if tok == expected {
                return tok_id as u32;
            }
        }

        // Also try lowercase hex
        let expected_lower: [u8; 6] = [
            b'<', b'0', b'x',
            b"0123456789abcdef"[(byte >> 4) as usize],
            b"0123456789abcdef"[(byte & 0x0F) as usize],
            b'>',
        ];
        for tok_id in 0..self.vocab_size {
            let tok = self.token_bytes(tok_id as u32);
            if tok == expected_lower {
                return tok_id as u32;
            }
        }

        // Ultimate fallback: token 0 (usually <unk>)
        0
    }

    /// Decode token IDs back to UTF-8 text.
    ///
    /// Converts GPT-2 Unicode chars back to raw bytes.
    /// Returns number of bytes written.
    pub fn decode(&self, ids: &[u32], output: &mut [u8]) -> usize {
        let mut out_pos = 0;

        for &id in ids {
            let tok = self.token_bytes(id);

            // Decode GPT-2 encoded bytes back to raw bytes
            let mut i = 0;
            while i < tok.len() && out_pos < output.len() {
                let (byte, consumed) = gpt2_decode_char(&tok[i..]);
                if consumed == 0 { break; }
                output[out_pos] = byte;
                out_pos += 1;
                i += consumed;
            }
        }

        out_pos
    }

    /// Decode a single token ID to output buffer.
    /// Returns number of bytes written.
    pub fn decode_token(&self, id: u32, output: &mut [u8]) -> usize {
        self.decode(&[id], output)
    }

    /// Get vocabulary size
    pub fn vocab_size(&self) -> usize {
        self.vocab_size
    }
}
