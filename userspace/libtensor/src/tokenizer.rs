//! BPE Tokenizer — Greedy Longest Prefix Match (ULTRA 27, 29)
//!
//! Zero-alloc tokenizer that operates directly on mmap'd GGUF vocabulary data.
//! Uses greedy longest prefix match against the full vocabulary for encoding.
//!
//! Key design decisions:
//! - Scans vocab strings directly in GGUF mmap'd data (zero-copy)
//! - Builds offset table in arena for O(1) token→string lookup
//! - Converts ASCII space to ▁ (U+2581) before matching (ULTRA 29)
//! - Falls back to byte tokens <0xHH> for unmatched bytes

use crate::arena::BumpArena;

/// UTF-8 encoding of ▁ (U+2581, LOWER ONE EIGHTH BLOCK)
/// Used as space prefix in LLaMA/SmolLM BPE vocabularies
const SPIECE_UNDERLINE: [u8; 3] = [0xE2, 0x96, 0x81];

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
    /// ULTRA 29: ASCII spaces are converted to ▁ (U+2581) before matching.
    ///
    /// Returns the number of tokens written to `output`.
    pub fn encode(&self, text: &[u8], output: &mut [u32]) -> usize {
        if text.is_empty() || output.is_empty() {
            return 0;
        }

        // Pre-process: convert spaces to ▁ in a temporary buffer
        // We'll work with the original text and handle space→▁ inline during matching
        let mut n_tokens = 0usize;
        let mut pos = 0usize;

        while pos < text.len() && n_tokens < output.len() {
            let mut best_id: u32 = u32::MAX;
            let mut best_len: usize = 0;

            // Check if current position starts with a space
            let at_space = text[pos] == b' ';

            for tok_id in 0..self.vocab_size {
                let tok_bytes = self.token_bytes(tok_id as u32);
                if tok_bytes.is_empty() {
                    continue;
                }

                // Try to match this token against text[pos..]
                let matched_len = self.try_match(text, pos, tok_bytes, at_space);
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
                // Byte fallback: find <0xHH> token for this byte
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
    /// Handles ULTRA 29: if the token starts with ▁ and text[pos] is a space,
    /// match the ▁ against the space and continue matching the rest.
    ///
    /// Returns the number of bytes consumed from `text`, or 0 if no match.
    #[inline]
    fn try_match(&self, text: &[u8], pos: usize, tok: &[u8], at_space: bool) -> usize {
        let remaining = text.len() - pos;

        // Case 1: Token starts with ▁ (3 bytes) and we're at a space
        if at_space && tok.len() >= 3 && tok[0..3] == SPIECE_UNDERLINE {
            // Match space against ▁, then match rest of token against text[pos+1..]
            let tok_rest = &tok[3..];
            let text_after_space = pos + 1;
            if tok_rest.is_empty() {
                // Token is just ▁ → matches single space
                return 1;
            }
            if text_after_space + tok_rest.len() > text.len() {
                return 0;
            }
            if &text[text_after_space..text_after_space + tok_rest.len()] == tok_rest {
                return 1 + tok_rest.len(); // consumed: 1 space + rest
            }
            return 0;
        }

        // Case 2: Direct byte match
        if tok.len() > remaining {
            return 0;
        }
        if &text[pos..pos + tok.len()] == tok {
            return tok.len();
        }

        0
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
    /// Writes decoded bytes to `output`, replacing ▁ with space.
    /// Returns number of bytes written.
    pub fn decode(&self, ids: &[u32], output: &mut [u8]) -> usize {
        let mut out_pos = 0;

        for &id in ids {
            let tok = self.token_bytes(id);

            // Replace ▁ sequences with space
            let mut i = 0;
            while i < tok.len() && out_pos < output.len() {
                if i + 3 <= tok.len() && tok[i..i + 3] == SPIECE_UNDERLINE {
                    output[out_pos] = b' ';
                    out_pos += 1;
                    i += 3;
                } else {
                    output[out_pos] = tok[i];
                    out_pos += 1;
                    i += 1;
                }
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
