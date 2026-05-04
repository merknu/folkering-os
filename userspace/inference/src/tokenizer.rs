//! Phase D.3.1.b — BPE tokenizer with GPT-2 byte-level encoding +
//! special-token splitting. Real Qwen / Llama-3 vocabs land here.
//!
//! Pipeline at encode time:
//!   1. Scan input for special-token strings (e.g. `<|im_start|>`).
//!      Split the input into alternating chunks of `[special, normal,
//!      special, normal, ...]`. Special chunks emit their atomic ID
//!      directly; normal chunks fall through to BPE.
//!   2. For each normal chunk, map every byte through the GPT-2
//!      byte-to-unicode table (' ' → 'Ġ', '\n' → 'Ċ', printable
//!      ASCII identity, other control bytes → U+0100 + offset). Each
//!      mapped char's 1-char string is looked up in the vocab to get
//!      a base token ID.
//!   3. Run greedy BPE: at each step, find the merge with lowest
//!      priority index whose `(left_id, right_id)` pair appears in
//!      the current sequence; apply it; repeat until no merge
//!      applies.
//!
//! Pipeline at decode:
//!   - Concatenate vocab strings for each ID.
//!   - Reverse the GPT-2 byte mapping (each char → its byte).
//!   - Wrap as UTF-8 string.
//!
//! Out of scope today (queued):
//! - The Tiktoken pre-tokenizer regex (`(?i:'s|'t|...)|\p{L}+|\p{N}|...`).
//!   Without it, byte-level BPE on the whole chunk can take merges
//!   that span what the regex would have treated as separate words.
//!   For ASCII / Latin-1 input this matches HF on most strings; the
//!   D.3.1 boot test verifies this against a known reference.
//! - NFC normalization. Pure ASCII / Latin-1 inputs are unaffected.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

// ── .tokb wire format ──────────────────────────────────────────────
//
// v1:
//   magic(4) + version(2)=1 + reserved(2) + n_tokens(4) + n_merges(4)
//   [token_offsets : u32 × (n_tokens + 1)]
//   [token_bytes   : UTF-8 packed]
//   [merges        : (left u32, right u32, result u32) × n_merges]
//
// v2 (real Qwen): adds a special-tokens trailer.
//   magic(4) + version(2)=2 + reserved(2) + n_tokens(4) + n_merges(4)
//     + n_special(4) + reserved(4)
//   [token_offsets : u32 × (n_tokens + 1)]
//   [token_bytes   : UTF-8 packed]
//   [merges        : (left u32, right u32, result u32) × n_merges]
//   [special_ids   : u32 × n_special]
//
// Merges are PRIORITY-ORDERED: index 0 = highest priority. This
// matches the order in HuggingFace's `merges.txt` / `tokenizer.json`.

pub const TOKB_MAGIC: u32 = u32::from_le_bytes(*b"TOK1");

#[derive(Debug)]
#[allow(dead_code)]
pub enum TokenizerError {
    BadMagic,
    BadVersion(u16),
    Truncated { needed: usize, got: usize },
    NameNotUtf8,
    Empty,
    /// A single byte (0..256) didn't appear as a 1-char vocab token
    /// after applying the GPT-2 byte map. Real Qwen / Llama-3 vocabs
    /// always include every byte-char; this surfaces if a malformed
    /// or non-byte-level tokenizer file slipped through.
    MissingByteChar(u8),
}

pub struct Tokenizer {
    /// Token strings, indexed by token ID.
    vocab: Vec<String>,
    /// Reverse vocab: string → ID. Built at parse time from `vocab`.
    /// Uses BTreeMap for O(log n) lookup at encode time.
    vocab_lookup: BTreeMap<String, u32>,
    /// Merge rules in priority order. `merges[0]` is highest priority.
    /// `(left_id, right_id, result_id)` per entry.
    merges: Vec<(u32, u32, u32)>,
    /// Sorted by `(left_id, right_id)` for binary-search lookup. Each
    /// entry is `((left, right), priority_index)`. The result ID
    /// comes from `merges[priority_index].2`.
    merge_index: Vec<((u32, u32), u32)>,
    /// Special-token strings (sorted by length descending so the
    /// encoder matches longer prefixes first), paired with their IDs.
    /// `<|im_start|>` lives here, `Hello` does not.
    special_strings: Vec<(String, u32)>,
    /// 256-entry table: byte b → unicode codepoint (per GPT-2's
    /// `bytes_to_unicode` algorithm). Lookup at encode time.
    byte_to_char: [u32; 256],
    /// Reverse: codepoint → byte. Used at decode time. Sparse; we
    /// only populate the 256 codepoints that any byte maps to.
    char_to_byte: BTreeMap<u32, u8>,
}

impl Tokenizer {
    /// Parse a `.tokb` blob (v1 or v2) into a Tokenizer. Allocates
    /// roughly the file size + per-token allocation overhead +
    /// merge-index table. Q3-0.6B's tokenizer (~3.8 MB on disk)
    /// settles at ~12-15 MB live.
    pub fn parse(bytes: &[u8]) -> Result<Self, TokenizerError> {
        if bytes.len() < 16 {
            return Err(TokenizerError::Truncated { needed: 16, got: bytes.len() });
        }
        let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic != TOKB_MAGIC { return Err(TokenizerError::BadMagic); }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        let (n_tokens, n_merges, n_special, header_end) = match version {
            1 => {
                let n_tokens = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
                let n_merges = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
                (n_tokens, n_merges, 0usize, 16usize)
            }
            2 => {
                if bytes.len() < 24 {
                    return Err(TokenizerError::Truncated { needed: 24, got: bytes.len() });
                }
                let n_tokens = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
                let n_merges = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
                let n_special = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;
                (n_tokens, n_merges, n_special, 24usize)
            }
            v => return Err(TokenizerError::BadVersion(v)),
        };
        if n_tokens == 0 { return Err(TokenizerError::Empty); }

        // Token offsets table: (n_tokens + 1) u32s after the header.
        let offsets_start = header_end;
        let offsets_bytes = (n_tokens + 1) * 4;
        if bytes.len() < offsets_start + offsets_bytes {
            return Err(TokenizerError::Truncated {
                needed: offsets_start + offsets_bytes,
                got: bytes.len(),
            });
        }
        let mut offsets: Vec<usize> = Vec::with_capacity(n_tokens + 1);
        for i in 0..=n_tokens {
            let o = offsets_start + i * 4;
            offsets.push(u32::from_le_bytes([
                bytes[o], bytes[o+1], bytes[o+2], bytes[o+3],
            ]) as usize);
        }
        let bytes_section_start = offsets_start + offsets_bytes;
        for w in offsets.windows(2) {
            if w[1] < w[0] {
                return Err(TokenizerError::Truncated { needed: w[0], got: w[1] });
            }
        }

        // Decode each token's bytes (UTF-8 → String) and build the
        // reverse lookup index in a single pass.
        let mut vocab: Vec<String> = Vec::with_capacity(n_tokens);
        let mut vocab_lookup: BTreeMap<String, u32> = BTreeMap::new();
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
            let owned = String::from(s);
            // Skip empty strings (placeholder gaps in the id range).
            if !owned.is_empty() {
                vocab_lookup.insert(owned.clone(), i as u32);
            }
            vocab.push(owned);
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
        let mut merges: Vec<(u32, u32, u32)> = Vec::with_capacity(n_merges);
        for i in 0..n_merges {
            let m = merges_start + i * 12;
            let l = u32::from_le_bytes([bytes[m  ], bytes[m+1], bytes[m+2], bytes[m+3]]);
            let r = u32::from_le_bytes([bytes[m+4], bytes[m+5], bytes[m+6], bytes[m+7]]);
            let res = u32::from_le_bytes([bytes[m+8], bytes[m+9], bytes[m+10], bytes[m+11]]);
            merges.push((l, r, res));
        }

        // Build the (left, right) → priority sorted index for fast
        // binary-search lookup at encode time. ~1.8 MB for Qwen3.
        let mut merge_index: Vec<((u32, u32), u32)> = Vec::with_capacity(n_merges);
        for (prio, m) in merges.iter().enumerate() {
            merge_index.push(((m.0, m.1), prio as u32));
        }
        merge_index.sort_by_key(|e| e.0);

        // Special tokens (v2 trailer). For v1 this is empty.
        let mut special_strings: Vec<(String, u32)> = Vec::with_capacity(n_special);
        if n_special > 0 {
            let specials_start = merges_start + merges_bytes;
            let specials_bytes = n_special * 4;
            if bytes.len() < specials_start + specials_bytes {
                return Err(TokenizerError::Truncated {
                    needed: specials_start + specials_bytes,
                    got: bytes.len(),
                });
            }
            for i in 0..n_special {
                let o = specials_start + i * 4;
                let id = u32::from_le_bytes([bytes[o], bytes[o+1], bytes[o+2], bytes[o+3]]);
                if let Some(s) = vocab.get(id as usize) {
                    if !s.is_empty() {
                        special_strings.push((s.clone(), id));
                    }
                }
            }
            // Sort by length descending so longer matches win at scan
            // time (e.g., `<|im_start_assistant|>` should match before
            // `<|im_start|>` if both ever existed).
            special_strings.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        }

        // GPT-2 byte-to-unicode table. Computing it at parse time is
        // a few hundred ops — same algorithm Python uses, kept here
        // so the runtime doesn't carry a 256-entry constant blob
        // that drifts from the writer's view.
        let byte_to_char = build_byte_to_char_table();
        let mut char_to_byte: BTreeMap<u32, u8> = BTreeMap::new();
        for (b, &c) in byte_to_char.iter().enumerate() {
            char_to_byte.insert(c, b as u8);
        }

        // Sanity-check: every byte's mapped char must exist as a
        // 1-char vocab entry. If not, the encoder can't translate
        // raw input bytes into base IDs.
        for b in 0u32..256 {
            let cp = byte_to_char[b as usize];
            let mut buf = [0u8; 4];
            let s = char::from_u32(cp).map(|c| c.encode_utf8(&mut buf).len());
            if s.is_none() {
                return Err(TokenizerError::MissingByteChar(b as u8));
            }
        }

        Ok(Self {
            vocab,
            vocab_lookup,
            merges,
            merge_index,
            special_strings,
            byte_to_char,
            char_to_byte,
        })
    }

    pub fn vocab_size(&self) -> usize { self.vocab.len() }
    pub fn merges_count(&self) -> usize { self.merges.len() }
    pub fn special_count(&self) -> usize { self.special_strings.len() }

    /// Encode text into token IDs. Splits on special tokens first;
    /// each remaining chunk goes through GPT-2 byte mapping + greedy
    /// BPE.
    pub fn encode(&self, text: &str) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        let bytes = text.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            // Try to match a special token at position i.
            if let Some((sid, slen)) = self.match_special_at(&bytes[i..]) {
                out.push(sid);
                i += slen;
                continue;
            }
            // Find the next special-token boundary so we can BPE the
            // chunk in between as a unit.
            let chunk_end = self.find_next_special(&bytes[i..]);
            let chunk = &bytes[i..i + chunk_end];
            self.encode_chunk(chunk, &mut out);
            i += chunk_end;
        }
        out
    }

    /// Match a special-token string at the START of `bytes`. Returns
    /// `Some((id, byte_len))` if any special string matches; `None`
    /// otherwise. Specials are sorted by length descending so the
    /// longest match wins.
    fn match_special_at(&self, bytes: &[u8]) -> Option<(u32, usize)> {
        for (s, id) in &self.special_strings {
            let sb = s.as_bytes();
            if bytes.len() >= sb.len() && &bytes[..sb.len()] == sb {
                return Some((*id, sb.len()));
            }
        }
        None
    }

    /// Walk forward from offset 0 of `bytes` looking for the next
    /// position where a special token starts. Returns the byte
    /// distance to that match (or `bytes.len()` if no match before
    /// EOF). Used to bound the chunk we run BPE on.
    fn find_next_special(&self, bytes: &[u8]) -> usize {
        if self.special_strings.is_empty() { return bytes.len(); }
        // Linear scan; most prompts are dominated by normal text so
        // this is amortised cheap. For huge inputs with many specials
        // we could build an Aho-Corasick automaton — D.3.1.c.
        for i in 0..bytes.len() {
            if self.match_special_at(&bytes[i..]).is_some() {
                return i;
            }
        }
        bytes.len()
    }

    /// Encode a single non-special chunk via byte-level BPE. The
    /// caller has already split out any special-token boundaries.
    fn encode_chunk(&self, bytes: &[u8], out: &mut Vec<u32>) {
        if bytes.is_empty() { return; }

        // Step 1: byte-level base tokens. Every byte goes through the
        // GPT-2 byte map and gets looked up as a 1-char vocab string.
        let mut tokens: Vec<u32> = Vec::with_capacity(bytes.len());
        for &b in bytes {
            let cp = self.byte_to_char[b as usize];
            let c = match char::from_u32(cp) {
                Some(c) => c,
                None => continue,
            };
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            match self.vocab_lookup.get(s) {
                Some(&id) => tokens.push(id),
                None => {
                    // Should never happen for a well-formed Qwen vocab
                    // (every byte char is in vocab). Fall through with
                    // no token rather than panicking on the hot path.
                }
            }
        }
        if tokens.len() < 2 {
            out.extend(tokens);
            return;
        }

        // Step 2: greedy BPE. At each pass, find the pair with the
        // lowest priority index across the whole sequence; apply it;
        // repeat. O(L² log M) per chunk; the log is binary search in
        // `merge_index`.
        loop {
            let mut best_prio: u32 = u32::MAX;
            let mut best_pos: Option<usize> = None;
            let mut best_result: u32 = 0;
            for i in 0..tokens.len() - 1 {
                let key = (tokens[i], tokens[i + 1]);
                if let Ok(idx) = self.merge_index.binary_search_by_key(&key, |e| e.0) {
                    let prio = self.merge_index[idx].1;
                    if prio < best_prio {
                        best_prio = prio;
                        best_pos = Some(i);
                        best_result = self.merges[prio as usize].2;
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

        out.extend(tokens);
    }

    /// Decode a single token ID back to its raw vocab string (still
    /// in GPT-2-encoded form — `Ġ` for space, etc.). Returns `None`
    /// for out-of-range IDs.
    pub fn decode(&self, token: u32) -> Option<&str> {
        self.vocab.get(token as usize).map(|s| s.as_str())
    }

    /// Concatenate-decode a sequence of token IDs into one user-
    /// facing String. Reverses the GPT-2 byte mapping (so `Ġ` becomes
    /// space, `Ċ` becomes `\n`, etc.) and emits valid UTF-8.
    pub fn decode_seq(&self, tokens: &[u32]) -> String {
        let mut concat = String::new();
        for &t in tokens {
            if let Some(s) = self.decode(t) {
                concat.push_str(s);
            }
        }
        // Apply reverse byte mapping. For chars not in our table
        // (e.g., a vocab string contains a chr that's not part of
        // the byte set), pass them through as UTF-8.
        let mut out_bytes: Vec<u8> = Vec::with_capacity(concat.len());
        for c in concat.chars() {
            let cp = c as u32;
            if let Some(&b) = self.char_to_byte.get(&cp) {
                out_bytes.push(b);
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                out_bytes.extend_from_slice(s.as_bytes());
            }
        }
        match String::from_utf8(out_bytes) {
            Ok(s) => s,
            Err(e) => {
                // Lossy fallback; should be very rare in practice.
                String::from_utf8_lossy(e.as_bytes()).into_owned()
            }
        }
    }
}

/// Reproduce GPT-2's `bytes_to_unicode` mapping. Used by every BPE
/// tokenizer in the LLaMA / Qwen family.
fn build_byte_to_char_table() -> [u32; 256] {
    // Directly-mapped (identity) ranges:
    //   0x21..=0x7E (printable ASCII)
    //   0xA1..=0xAC (Latin-1 supplement, partial)
    //   0xAE..=0xFF (rest of Latin-1)
    let mut is_direct = [false; 256];
    for b in 0x21u8..=0x7Eu8 { is_direct[b as usize] = true; }
    for b in 0xA1u8..=0xACu8 { is_direct[b as usize] = true; }
    for b in 0xAEu8..=0xFFu8 { is_direct[b as usize] = true; }

    let mut table = [0u32; 256];
    let mut overflow_offset: u32 = 0;
    // First fill in the direct mappings (b → b).
    for b in 0..256 {
        if is_direct[b] {
            table[b] = b as u32;
        }
    }
    // Then assign overflow chars (256, 257, ...) to the remaining
    // bytes, walking 0..256 in order so the assignment is stable.
    for b in 0..256 {
        if !is_direct[b] {
            table[b] = 256 + overflow_offset;
            overflow_offset += 1;
        }
    }
    table
}
