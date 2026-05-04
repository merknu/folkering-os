//! Phase D.3.1 — BPE tokenizer.
//!
//! Byte-level greedy BPE: input bytes become single-byte tokens (IDs
//! 0..255), then we iteratively apply the highest-priority merge until
//! no more apply. Merges are loaded from a `.tokb` file (Synapse VFS
//! → `vfs_loader::read_file`).
//!
//! Out of scope today (D.3.1.b):
//! - GPT-2 byte-to-unicode mapping (the `Ġ` / `Ċ` shenanigans real
//!   Qwen2.5 needs for whitespace-prefixed tokens).
//! - Pre-tokenizer (word-boundary splitting before BPE — without it
//!   merges cross word boundaries, which real models don't allow).
//!
//! Both are easy lifts from `userspace/libtensor/src/tokenizer.rs` once
//! we plug in a real Qwen `tokenizer.json`. For the synthetic test
//! fixture (ASCII vocab, no whitespace tricks) the simple version below
//! is sufficient.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

// ── .tokb wire format ──────────────────────────────────────────────
//
// magic(4) + version(2) + reserved(2) + n_tokens(4) + n_merges(4)
// [token_offsets : u32 × (n_tokens + 1)]
// [token_bytes   : UTF-8 packed]
// [merges        : (left u32, right u32, result u32) × n_merges]
//
// Merges are PRIORITY-ORDERED: index 0 = highest priority. This
// matches the order in HuggingFace's `merges.txt`.

pub const TOKB_MAGIC: u32 = u32::from_le_bytes(*b"TOK1");
pub const TOKB_VERSION: u16 = 1;

#[derive(Debug)]
#[allow(dead_code)]
pub enum TokenizerError {
    BadMagic,
    BadVersion(u16),
    Truncated { needed: usize, got: usize },
    NameNotUtf8,
    Empty,
}

pub struct Tokenizer {
    /// Token strings, indexed by token ID.
    vocab: Vec<String>,
    /// Merge rules in priority order. `merges[0]` is highest priority.
    /// Each tuple is `(left_id, right_id, result_id)`.
    merges: Vec<(u32, u32, u32)>,
}

impl Tokenizer {
    /// Parse a `.tokb` blob into a Tokenizer. Allocates one `String`
    /// per token + one `Vec` for the merges; total memory is roughly
    /// the file size plus per-token allocation overhead.
    pub fn parse(bytes: &[u8]) -> Result<Self, TokenizerError> {
        if bytes.len() < 16 {
            return Err(TokenizerError::Truncated { needed: 16, got: bytes.len() });
        }
        let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic != TOKB_MAGIC { return Err(TokenizerError::BadMagic); }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != TOKB_VERSION { return Err(TokenizerError::BadVersion(version)); }
        let n_tokens = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        let n_merges = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
        if n_tokens == 0 { return Err(TokenizerError::Empty); }

        // Token offsets table: (n_tokens + 1) u32s after the header.
        let offsets_start = 16;
        let offsets_bytes = (n_tokens + 1) * 4;
        if bytes.len() < offsets_start + offsets_bytes {
            return Err(TokenizerError::Truncated {
                needed: offsets_start + offsets_bytes,
                got: bytes.len(),
            });
        }
        let mut offsets = Vec::with_capacity(n_tokens + 1);
        for i in 0..=n_tokens {
            let o = offsets_start + i * 4;
            offsets.push(u32::from_le_bytes([
                bytes[o], bytes[o+1], bytes[o+2], bytes[o+3],
            ]) as usize);
        }
        // Validate offsets are monotonic and stay within bounds.
        let bytes_section_start = offsets_start + offsets_bytes;
        for w in offsets.windows(2) {
            if w[1] < w[0] {
                return Err(TokenizerError::Truncated { needed: w[0], got: w[1] });
            }
        }

        // Decode each token's bytes (UTF-8 → String).
        let mut vocab = Vec::with_capacity(n_tokens);
        for i in 0..n_tokens {
            let start = bytes_section_start + offsets[i];
            let end = bytes_section_start + offsets[i + 1];
            if end > bytes.len() {
                return Err(TokenizerError::Truncated { needed: end, got: bytes.len() });
            }
            let s = match core::str::from_utf8(&bytes[start..end]) {
                Ok(s) => s,
                Err(_) => return Err(TokenizerError::NameNotUtf8),
            };
            vocab.push(String::from(s));
        }

        // Merges table follows the byte section. Each merge is 12 bytes.
        let merges_start = bytes_section_start
            + (offsets[n_tokens] - offsets[0]); // = bytes_section_start + total token bytes
        let merges_bytes = n_merges * 12;
        if bytes.len() < merges_start + merges_bytes {
            return Err(TokenizerError::Truncated {
                needed: merges_start + merges_bytes,
                got: bytes.len(),
            });
        }
        let mut merges = Vec::with_capacity(n_merges);
        for i in 0..n_merges {
            let m = merges_start + i * 12;
            let l = u32::from_le_bytes([bytes[m  ], bytes[m+1], bytes[m+2], bytes[m+3]]);
            let r = u32::from_le_bytes([bytes[m+4], bytes[m+5], bytes[m+6], bytes[m+7]]);
            let res = u32::from_le_bytes([bytes[m+8], bytes[m+9], bytes[m+10], bytes[m+11]]);
            merges.push((l, r, res));
        }

        Ok(Self { vocab, merges })
    }

    pub fn vocab_size(&self) -> usize { self.vocab.len() }
    pub fn merges_count(&self) -> usize { self.merges.len() }

    /// Encode text into token IDs using greedy byte-level BPE.
    ///
    /// Algorithm:
    ///   1. Each input byte becomes the single-byte token (IDs 0..255).
    ///   2. Loop: find the highest-priority merge whose `(left, right)`
    ///      pair appears anywhere in the current token sequence. Apply
    ///      it (replace pair with merged ID).
    ///   3. Stop when no merge applies.
    ///
    /// Worst case is O(n * merges * sequence_length) — fine for the
    /// short prompts inference touches today; D.3.1.b will bring in the
    /// linked-list-based merge queue from the legacy implementation
    /// when we hit real-prompt-length workloads.
    pub fn encode(&self, text: &str) -> Vec<u32> {
        let bytes = text.as_bytes();
        // Step 1: byte-level base tokens.
        let mut tokens: Vec<u32> = bytes.iter().map(|&b| b as u32).collect();
        if tokens.len() < 2 { return tokens; }

        // Step 2: iteratively apply highest-priority merge.
        loop {
            // Scan once to find the best merge in this iteration.
            // "Best" = lowest priority index in the merges table.
            let mut best_prio: usize = self.merges.len();
            let mut best_pos: Option<usize> = None;
            let mut best_result: u32 = 0;
            for i in 0..tokens.len() - 1 {
                let pair = (tokens[i], tokens[i + 1]);
                for (mp, m) in self.merges.iter().enumerate() {
                    if m.0 == pair.0 && m.1 == pair.1 {
                        if mp < best_prio {
                            best_prio = mp;
                            best_pos = Some(i);
                            best_result = m.2;
                        }
                        break; // first merges entry wins; merges are unique by (left,right)
                    }
                }
            }
            match best_pos {
                Some(pos) => {
                    tokens[pos] = best_result;
                    tokens.remove(pos + 1);
                    if tokens.len() < 2 { break; }
                }
                None => break,
            }
        }

        tokens
    }

    /// Decode a token ID back to its string. Returns `None` for
    /// out-of-range IDs.
    pub fn decode(&self, token: u32) -> Option<&str> {
        self.vocab.get(token as usize).map(|s| s.as_str())
    }

    /// Concatenate-decode a sequence of token IDs into one String.
    /// Used for round-trip tests and (eventually) printing model
    /// output back to the user.
    pub fn decode_seq(&self, tokens: &[u32]) -> String {
        let mut out = String::new();
        for &t in tokens {
            if let Some(s) = self.decode(t) {
                out.push_str(s);
            }
        }
        out
    }
}
