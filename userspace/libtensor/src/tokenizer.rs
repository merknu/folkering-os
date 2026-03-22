//! BPE Tokenizer — Proper BPE with merge priorities
//!
//! Implements Byte-Pair Encoding with merge-priority ranking from GGUF metadata.
//! Uses GPT-2 byte encoding and merge rules from `tokenizer.ggml.merges`.
//!
//! Architecture:
//! - At init: builds FNV-1a hash table (temporary, 512KB) for vocab string->ID lookup
//! - Parses merge rules into sorted (left_id, right_id) table for binary search
//! - Builds byte->token mapping (1KB) for initial character tokenization
//! - At encode time: converts bytes to char tokens, then iteratively applies merges
//! - Hash table memory is reclaimed after init via arena.reset_to()

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
//   Space (0x20) -> U+0120 (Ġ, \xC4\xA0)
//   Newline (0x0A) -> U+010A (Ċ, \xC4\x8A)
//   Tab (0x09) -> U+0109 (ĉ, \xC4\x89)

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
            return ([b, 0], 1);
        } else {
            return ([0xC0 | (b >> 6), 0x80 | (b & 0x3F)], 2);
        }
    }
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
        return (utf8[0], 1);
    }
    if utf8.len() >= 2 && (utf8[0] & 0xE0) == 0xC0 {
        let codepoint = ((utf8[0] as u16 & 0x1F) << 6) | (utf8[1] as u16 & 0x3F);
        if codepoint >= 256 {
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
            return (codepoint as u8, 2);
        }
    }
    (utf8[0], 1)
}

// ============================================================================
// FNV-1a Hash (for vocab string -> token ID lookup during init)
// ============================================================================

const FNV_OFFSET: u32 = 2166136261;
const FNV_PRIME: u32 = 16777619;

#[inline]
fn fnv1a(data: &[u8]) -> u32 {
    let mut hash = FNV_OFFSET;
    for &b in data {
        hash ^= b as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// ============================================================================
// GPT-2 Pre-tokenizer
// ============================================================================
//
// Splits text into segments at word boundaries before BPE. Merges do not cross
// segment boundaries. Matches HuggingFace/llama-cpp behavior:
// - Words carry their leading space: "hello world" → ["hello", " world"]
// - Contractions split: "don't" → ["don", "'t"]
// - Digits grouped 1-3: "12345" → ["123", "45"]
// - Whitespace before words leaves 1 space for the word prefix

#[inline]
fn pt_is_letter(b: u8) -> bool {
    b.is_ascii_alphabetic() || b >= 128 // non-ASCII treated as letter
}

#[inline]
fn pt_is_digit(b: u8) -> bool {
    b.is_ascii_digit()
}

#[inline]
fn pt_is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

#[inline]
fn pt_is_newline(b: u8) -> bool {
    b == b'\n' || b == b'\r'
}

/// Control whitespace: \t, \x0b, \x0c — whitespace that is NOT space and NOT newline.
/// GPT-2 regex treats these as a separate alternation from space/newline whitespace.
#[inline]
fn pt_is_ctrl_ws(b: u8) -> bool {
    matches!(b, b'\t' | 0x0b | 0x0c)
}

/// Try to match a contraction ('s, 't, 're, 've, 'm, 'll, 'd) at pos.
/// LOWERCASE ONLY — llama-cpp/HuggingFace do NOT match uppercase contractions
/// (e.g., "DON'T" is NOT split as contraction, but "don't" IS).
fn pt_try_contraction(text: &[u8], pos: usize) -> usize {
    if pos >= text.len() || text[pos] != b'\'' {
        return 0;
    }
    let rem = text.len() - pos;
    if rem >= 3 {
        let c1 = text[pos + 1];
        let c2 = text[pos + 2];
        if (c1 == b'r' && c2 == b'e')
            || (c1 == b'v' && c2 == b'e')
            || (c1 == b'l' && c2 == b'l')
        {
            return 3;
        }
    }
    if rem >= 2 {
        let c1 = text[pos + 1];
        if matches!(c1, b's' | b't' | b'm' | b'd') {
            return 2;
        }
    }
    0
}

/// Count consecutive letters starting at pos.
#[inline]
fn pt_letters(text: &[u8], pos: usize) -> usize {
    let mut i = pos;
    while i < text.len() && pt_is_letter(text[i]) {
        i += 1;
    }
    i - pos
}

/// Count consecutive digits (max 3) starting at pos.
#[inline]
fn pt_digits(text: &[u8], pos: usize) -> usize {
    let mut i = pos;
    while i < text.len() && pt_is_digit(text[i]) && (i - pos) < 3 {
        i += 1;
    }
    i - pos
}

/// Find end of whitespace run starting at pos.
#[inline]
fn pt_ws_end(text: &[u8], pos: usize) -> usize {
    let mut i = pos;
    while i < text.len() && pt_is_ws(text[i]) {
        i += 1;
    }
    i
}

/// Count consecutive punctuation (non-letter, non-digit, non-whitespace).
fn pt_punct(text: &[u8], pos: usize) -> usize {
    let mut i = pos;
    while i < text.len() && !pt_is_letter(text[i]) && !pt_is_digit(text[i]) && !pt_is_ws(text[i]) {
        i += 1;
    }
    // Include trailing newlines (GPT-2 regex rule 4)
    while i < text.len() && pt_is_newline(text[i]) {
        i += 1;
    }
    (i - pos).max(1)
}

/// Get the length of the next pre-token segment starting at pos.
///
/// Implements GPT-2 pre-tokenizer rules:
/// 1. Contractions ('s, 't, etc.) are separate segments
/// 2. Letters with optional leading space/punct prefix
/// 3. 1-3 digits
/// 4. Punctuation groups
/// 5. Whitespace: leaves 1 space for next word's prefix
fn pt_next(text: &[u8], pos: usize) -> usize {
    let b = text[pos];

    // Contraction: 's 't 're 've 'm 'll 'd
    if b == b'\'' {
        let clen = pt_try_contraction(text, pos);
        if clen > 0 {
            return clen;
        }
    }

    // Letter: consume consecutive letters
    if pt_is_letter(b) {
        return pt_letters(text, pos);
    }

    // Digit: consume 1-3 digits
    if pt_is_digit(b) {
        return pt_digits(text, pos);
    }

    // Control whitespace (\t, \x0b, \x0c): separate segment from space/newline.
    // GPT-2 regex alt 5: [\s&&[^\S\r\n]]+(?:\r\n|\r|\n)?
    // These get their own segment, optionally followed by one newline.
    if pt_is_ctrl_ws(b) {
        let mut i = pos;
        while i < text.len() && pt_is_ctrl_ws(text[i]) {
            i += 1;
        }
        // Optional trailing newline (\r\n, \r, or \n)
        if i < text.len() && text[i] == b'\r' {
            i += 1;
            if i < text.len() && text[i] == b'\n' {
                i += 1;
            }
        } else if i < text.len() && text[i] == b'\n' {
            i += 1;
        }
        return i - pos;
    }

    // Space or newline whitespace (\n, \r, space)
    // Key rules:
    // - Multiple ws before a letter: consume all but last, last space becomes word prefix
    // - Single newline directly before letter: keep separate (NOT a word prefix)
    // - Single space before letter: include as word prefix
    // - Space(s) before newline: group together (GPT-2 alt 5)
    if b == b' ' || pt_is_newline(b) {
        let ws_end = pt_ws_end(text, pos);
        if ws_end < text.len() {
            // Followed by non-whitespace
            if ws_end - pos > 1 {
                // Multiple ws chars: consume all but last (last becomes word prefix)
                return ws_end - pos - 1;
            }
            // Single whitespace char before non-whitespace
            let next = text[pos + 1];
            // Newlines are NEVER word prefixes — keep them separate
            if pt_is_newline(b) {
                return 1;
            }
            // Space (0x20): include as prefix with following segment
            if pt_is_letter(next) {
                return 1 + pt_letters(text, pos + 1);
            }
            if pt_is_digit(next) {
                return 1 + pt_digits(text, pos + 1);
            }
            // Space + punctuation group (GPT-2 regex rule 4)
            return 1 + pt_punct(text, pos + 1);
        }
        // Whitespace at end of text: consume all
        return ws_end - pos;
    }

    // Punctuation: non-letter, non-digit, non-whitespace
    // If followed by a letter, this char is the word's optional prefix
    if pos + 1 < text.len() && pt_is_letter(text[pos + 1]) {
        return 1 + pt_letters(text, pos + 1);
    }

    // Consume punctuation group
    pt_punct(text, pos)
}

// ============================================================================
// Hash Table
// ============================================================================

/// Hash table size (power of 2, ~75% load for 49152 vocab entries)
const HASH_SIZE: usize = 65536;
const HASH_MASK: usize = HASH_SIZE - 1;
/// Sentinel for empty hash table slot
const HASH_EMPTY: u32 = u32::MAX;

/// Look up a byte string in the vocab hash table.
/// Each entry is [fnv1a_hash, token_id]. Empty slots have token_id = HASH_EMPTY.
fn hash_find(
    table: &[[u32; 2]],
    data: &[u8],
    offsets: &[u32],
    lengths: &[u16],
    needle: &[u8],
) -> Option<u32> {
    let h = fnv1a(needle);
    let mut slot = (h as usize) & HASH_MASK;
    for _ in 0..HASH_SIZE {
        if table[slot][1] == HASH_EMPTY {
            return None;
        }
        if table[slot][0] == h {
            let tid = table[slot][1] as usize;
            let off = offsets[tid] as usize;
            let len = lengths[tid] as usize;
            if len == needle.len() && &data[off..off + len] == needle {
                return Some(table[slot][1]);
            }
        }
        slot = (slot + 1) & HASH_MASK;
    }
    None
}

// ============================================================================
// BPE Merge Entry + Sorting
// ============================================================================

/// A BPE merge rule: (left, right) -> merged at given rank.
/// Sorted by (left, right) for binary search during encoding.
#[derive(Clone, Copy)]
#[repr(C)]
struct MergeEntry {
    left: u32,
    right: u32,
    merged: u32,
    rank: u32,
}

/// Heapsort merge entries by (left, right) for binary search.
fn heapsort_merges(arr: &mut [MergeEntry]) {
    let n = arr.len();
    if n <= 1 {
        return;
    }
    // Build max-heap
    let mut i = n / 2;
    while i > 0 {
        i -= 1;
        sift_down(arr, i, n);
    }
    // Extract elements
    let mut end = n;
    while end > 1 {
        end -= 1;
        arr.swap(0, end);
        sift_down(arr, 0, end);
    }
}

fn sift_down(arr: &mut [MergeEntry], mut root: usize, end: usize) {
    loop {
        let left = 2 * root + 1;
        if left >= end {
            break;
        }
        let right = left + 1;
        let mut max = root;
        if merge_lt(&arr[max], &arr[left]) {
            max = left;
        }
        if right < end && merge_lt(&arr[max], &arr[right]) {
            max = right;
        }
        if max == root {
            break;
        }
        arr.swap(root, max);
        root = max;
    }
}

/// Compare merge entries by (left, right) — true if a < b.
#[inline]
fn merge_lt(a: &MergeEntry, b: &MergeEntry) -> bool {
    a.left < b.left || (a.left == b.left && a.right < b.right)
}

// ============================================================================
// Constants
// ============================================================================

/// Maximum input bytes for BPE working buffer (stack-allocated)
const MAX_BPE_WORK: usize = 2048;

// ============================================================================
// BPE Tokenizer
// ============================================================================

/// BPE Tokenizer operating on mmap'd GGUF vocabulary and merge data.
///
/// Init-time arena usage:
/// - offsets[vocab_size] + lengths[vocab_size]: ~288KB (persistent)
/// - byte_tokens[256]: 1KB (persistent)
/// - merges[merges_count]: ~782KB (persistent)
/// - hash_table[65536]: 512KB (temporary, freed after init)
pub struct BpeTokenizer<'a> {
    /// Raw GGUF file data (mmap'd)
    data: &'a [u8],
    /// Offset within data where the tokens string array starts
    vocab_offset: usize,
    /// Number of tokens in vocabulary
    vocab_size: usize,
    /// BOS token ID
    pub bos_id: u32,
    /// EOS token ID
    pub eos_id: u32,
    /// Token ID -> byte offset in data to token string
    offsets: &'a [u32],
    /// Token ID -> string length in bytes
    lengths: &'a [u16],
    /// BPE merge table sorted by (left, right) for binary search
    merges: &'a [MergeEntry],
    /// Number of valid merge entries (may be < merges.len())
    n_merges: usize,
    /// Raw byte value (0-255) -> initial character token ID
    byte_tokens: &'a [u32],
}

impl<'a> BpeTokenizer<'a> {
    /// Initialize tokenizer from GGUF data with BPE merge support.
    ///
    /// Builds vocab offset/length tables, byte-token mapping, and merge table.
    /// Uses a temporary hash table (512KB) for init that is freed via arena.reset_to().
    ///
    /// Arena usage: ~1.6MB persistent + 512KB temporary.
    pub fn new(
        data: &'a [u8],
        vocab_offset: usize,
        vocab_size: usize,
        bos_id: u32,
        eos_id: u32,
        merges_offset: usize,
        merges_count: usize,
        arena: &'a BumpArena,
    ) -> Option<Self> {
        if vocab_offset == 0 || vocab_size == 0 {
            return None;
        }

        // === Persistent allocations (survive arena reset) ===

        let offsets = arena.alloc_slice::<u32>(vocab_size)?;
        let lengths = arena.alloc_slice::<u16>(vocab_size)?;

        // Build offset/length tables by scanning the GGUF string array
        let mut pos = vocab_offset;
        for i in 0..vocab_size {
            if pos + 8 > data.len() {
                return None;
            }
            let str_len = u64::from_le_bytes([
                data[pos],
                data[pos + 1],
                data[pos + 2],
                data[pos + 3],
                data[pos + 4],
                data[pos + 5],
                data[pos + 6],
                data[pos + 7],
            ]) as usize;
            pos += 8;
            if pos + str_len > data.len() {
                return None;
            }
            offsets[i] = pos as u32;
            lengths[i] = str_len.min(u16::MAX as usize) as u16;
            pos += str_len;
        }

        let byte_tokens = arena.alloc_slice::<u32>(256)?;
        for v in byte_tokens.iter_mut() {
            *v = 0;
        }

        // Allocate merge table (may be empty if no merges in GGUF)
        let merges_buf = if merges_count > 0 {
            arena.alloc_slice::<MergeEntry>(merges_count)?
        } else {
            // No merges — build byte_tokens via brute force, return with empty merges
            for byte_val in 0..256u16 {
                let (gpt2, gpt2_len) = gpt2_encode_byte(byte_val as u8);
                byte_tokens[byte_val as usize] =
                    brute_find_token(data, offsets, lengths, vocab_size, &gpt2[..gpt2_len]);
            }
            return Some(Self {
                data,
                vocab_offset,
                vocab_size,
                bos_id,
                eos_id,
                offsets,
                lengths,
                merges: &[],
                n_merges: 0,
                byte_tokens,
            });
        };

        // Save arena position — persistent allocations are below this mark
        let arena_mark = arena.used();

        // === Temporary hash table (freed after init) ===

        let hash_table = arena.alloc_slice::<[u32; 2]>(HASH_SIZE)?;
        for e in hash_table.iter_mut() {
            *e = [0, HASH_EMPTY];
        }

        // Insert all vocab tokens into hash table
        for id in 0..vocab_size {
            let off = offsets[id] as usize;
            let len = lengths[id] as usize;
            let h = fnv1a(&data[off..off + len]);
            let mut slot = (h as usize) & HASH_MASK;
            loop {
                if hash_table[slot][1] == HASH_EMPTY {
                    hash_table[slot] = [h, id as u32];
                    break;
                }
                slot = (slot + 1) & HASH_MASK;
            }
        }

        // Build byte_tokens[256]: each raw byte -> its initial character token ID
        for byte_val in 0..256u16 {
            let (gpt2, gpt2_len) = gpt2_encode_byte(byte_val as u8);
            byte_tokens[byte_val as usize] =
                hash_find(hash_table, data, offsets, lengths, &gpt2[..gpt2_len]).unwrap_or(0);
        }

        // Parse merge strings from GGUF and build merge table
        let mut merge_pos = merges_offset;
        let mut n_valid = 0usize;
        let mut concat_buf = [0u8; 128];

        for rank in 0..merges_count {
            if merge_pos + 8 > data.len() {
                break;
            }
            let str_len = u64::from_le_bytes([
                data[merge_pos],
                data[merge_pos + 1],
                data[merge_pos + 2],
                data[merge_pos + 3],
                data[merge_pos + 4],
                data[merge_pos + 5],
                data[merge_pos + 6],
                data[merge_pos + 7],
            ]) as usize;
            merge_pos += 8;
            if merge_pos + str_len > data.len() {
                break;
            }
            let merge_str = &data[merge_pos..merge_pos + str_len];
            merge_pos += str_len;

            // Split on first space (0x20) — delimiter between left and right tokens
            // GPT-2 tokens never contain literal 0x20 (space is encoded as Ġ)
            let space = match merge_str.iter().position(|&b| b == 0x20) {
                Some(p) => p,
                None => continue,
            };
            let left_str = &merge_str[..space];
            let right_str = &merge_str[space + 1..];
            if left_str.is_empty() || right_str.is_empty() {
                continue;
            }

            // Look up left and right token IDs
            let left_id = match hash_find(hash_table, data, offsets, lengths, left_str) {
                Some(id) => id,
                None => continue,
            };
            let right_id = match hash_find(hash_table, data, offsets, lengths, right_str) {
                Some(id) => id,
                None => continue,
            };

            // Concatenate left+right and look up the merged token ID
            let clen = left_str.len() + right_str.len();
            if clen > 128 {
                continue;
            }
            concat_buf[..left_str.len()].copy_from_slice(left_str);
            concat_buf[left_str.len()..clen].copy_from_slice(right_str);

            let merged_id =
                match hash_find(hash_table, data, offsets, lengths, &concat_buf[..clen]) {
                    Some(id) => id,
                    None => continue,
                };

            merges_buf[n_valid] = MergeEntry {
                left: left_id,
                right: right_id,
                merged: merged_id,
                rank: rank as u32,
            };
            n_valid += 1;
        }

        // Sort merge table by (left, right) for binary search
        heapsort_merges(&mut merges_buf[..n_valid]);

        // Free hash table memory — persistent data is below arena_mark
        arena.reset_to(arena_mark);

        // Coerce &mut to & for the struct field
        let merges: &'a [MergeEntry] = merges_buf;

        Some(Self {
            data,
            vocab_offset,
            vocab_size,
            bos_id,
            eos_id,
            offsets,
            lengths,
            merges,
            n_merges: n_valid,
            byte_tokens,
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

    /// Binary search for a merge entry by (left, right) token IDs.
    /// Returns the entry if found (contains merged token ID and rank).
    #[inline]
    fn find_merge(&self, left: u32, right: u32) -> Option<&MergeEntry> {
        let merges = &self.merges[..self.n_merges];
        let mut lo = 0usize;
        let mut hi = merges.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let e = &merges[mid];
            if e.left < left || (e.left == left && e.right < right) {
                lo = mid + 1;
            } else if e.left == left && e.right == right {
                return Some(e);
            } else {
                hi = mid;
            }
        }
        None
    }

    /// Encode text into token IDs using proper BPE with merge priorities.
    ///
    /// Special tokens (`<|im_start|>`, `<|im_end|>`, `<|endoftext|>`) are detected
    /// in the input text and emitted as their token IDs directly. BPE is applied
    /// only to the text segments between special tokens.
    pub fn encode(&self, text: &[u8], output: &mut [u32]) -> usize {
        if text.is_empty() || output.is_empty() {
            return 0;
        }

        if self.n_merges == 0 {
            return self.encode_greedy(text, output);
        }

        // Special token patterns and their IDs
        // These are emitted directly without BPE decomposition
        const SPECIAL_TOKENS: &[(&[u8], u32)] = &[
            (b"<|im_start|>", 1), // BOS / im_start
            (b"<|im_end|>", 2),   // EOS / im_end
            (b"<|endoftext|>", 0), // endoftext
        ];

        let mut n_tokens = 0usize;
        let mut pos = 0usize;

        while pos < text.len() && n_tokens < output.len() {
            // Check for special token at current position
            let mut found = false;
            for &(pattern, token_id) in SPECIAL_TOKENS {
                if pos + pattern.len() <= text.len()
                    && &text[pos..pos + pattern.len()] == pattern
                {
                    output[n_tokens] = token_id;
                    n_tokens += 1;
                    pos += pattern.len();
                    found = true;
                    break;
                }
            }
            if found {
                continue;
            }

            // Find the next special token (or end of text) to delimit the BPE segment
            let mut seg_end = text.len();
            for sp in (pos + 1)..text.len() {
                let mut hit = false;
                for &(pattern, _) in SPECIAL_TOKENS {
                    if sp + pattern.len() <= text.len()
                        && &text[sp..sp + pattern.len()] == pattern
                    {
                        hit = true;
                        break;
                    }
                }
                if hit {
                    seg_end = sp;
                    break;
                }
            }

            // Pre-tokenize segment, then BPE each pre-token independently
            let segment = &text[pos..seg_end];
            let added = self.encode_pretokenized(segment, &mut output[n_tokens..]);
            n_tokens += added;
            pos = seg_end;
        }

        n_tokens
    }

    /// Pre-tokenize text segment, then BPE-encode each pre-token independently.
    /// This prevents BPE merges from crossing word boundaries.
    fn encode_pretokenized(&self, text: &[u8], output: &mut [u32]) -> usize {
        if text.is_empty() || output.is_empty() {
            return 0;
        }
        let mut n_tokens = 0;
        let mut pos = 0;
        while pos < text.len() && n_tokens < output.len() {
            let seg_len = pt_next(text, pos);
            let added = self.encode_bpe_segment(&text[pos..pos + seg_len], &mut output[n_tokens..]);
            n_tokens += added;
            pos += seg_len;
        }
        n_tokens
    }

    /// Apply BPE encoding to a text segment (no special tokens).
    fn encode_bpe_segment(&self, text: &[u8], output: &mut [u32]) -> usize {
        if text.is_empty() || output.is_empty() {
            return 0;
        }

        // Step 1: Convert each byte to its initial character token
        let n = text.len().min(MAX_BPE_WORK);
        let mut work = [0u32; MAX_BPE_WORK];
        for i in 0..n {
            work[i] = self.byte_tokens[text[i] as usize];
        }
        let mut len = n;

        // Step 2: Iteratively apply BPE merges
        loop {
            // Find the adjacent pair with lowest rank (highest priority)
            let mut best_rank = u32::MAX;
            let mut best_left = 0u32;
            let mut best_right = 0u32;
            let mut best_merged = 0u32;

            for i in 0..len.saturating_sub(1) {
                if let Some(entry) = self.find_merge(work[i], work[i + 1]) {
                    if entry.rank < best_rank {
                        best_rank = entry.rank;
                        best_left = entry.left;
                        best_right = entry.right;
                        best_merged = entry.merged;
                    }
                }
            }

            if best_rank == u32::MAX {
                break;
            }

            // Merge all occurrences of (best_left, best_right) -> best_merged
            let mut new_len = 0;
            let mut i = 0;
            while i < len {
                if i + 1 < len && work[i] == best_left && work[i + 1] == best_right {
                    work[new_len] = best_merged;
                    new_len += 1;
                    i += 2;
                } else {
                    work[new_len] = work[i];
                    new_len += 1;
                    i += 1;
                }
            }
            len = new_len;
        }

        // Copy to output
        let out_n = len.min(output.len());
        for i in 0..out_n {
            output[i] = work[i];
        }
        out_n
    }

    /// Fallback: greedy longest prefix match (used when no merge data available).
    fn encode_greedy(&self, text: &[u8], output: &mut [u32]) -> usize {
        let mut n_tokens = 0usize;
        let mut pos = 0usize;

        while pos < text.len() && n_tokens < output.len() {
            let mut best_id: u32 = u32::MAX;
            let mut best_len: usize = 0;

            for tok_id in 4..self.vocab_size {
                let tok_bytes = self.token_bytes(tok_id as u32);
                if tok_bytes.is_empty() {
                    continue;
                }
                let matched_len = try_match_gpt2(text, pos, tok_bytes);
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
                // Byte fallback
                output[n_tokens] = self.byte_tokens[text[pos] as usize];
                n_tokens += 1;
                pos += 1;
            }
        }

        n_tokens
    }

    /// Decode token IDs back to UTF-8 text.
    ///
    /// Converts GPT-2 Unicode chars back to raw bytes.
    /// Returns number of bytes written.
    pub fn decode(&self, ids: &[u32], output: &mut [u8]) -> usize {
        let mut out_pos = 0;

        for &id in ids {
            let tok = self.token_bytes(id);
            let mut i = 0;
            while i < tok.len() && out_pos < output.len() {
                let (byte, consumed) = gpt2_decode_char(&tok[i..]);
                if consumed == 0 {
                    break;
                }
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

// ============================================================================
// Standalone helpers
// ============================================================================

/// Brute-force search for a token by its byte string. O(vocab_size).
/// Used as fallback when hash table is not available.
fn brute_find_token(
    data: &[u8],
    offsets: &[u32],
    lengths: &[u16],
    vocab_size: usize,
    needle: &[u8],
) -> u32 {
    for id in 0..vocab_size {
        let off = offsets[id] as usize;
        let len = lengths[id] as usize;
        if len == needle.len() && &data[off..off + len] == needle {
            return id as u32;
        }
    }
    0 // fallback to UNK
}

/// Try to match a vocab token against text at position `pos` using GPT-2 encoding.
/// Returns the number of raw input bytes consumed, or 0 if no match.
fn try_match_gpt2(text: &[u8], pos: usize, tok: &[u8]) -> usize {
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
        text_pos - pos
    } else {
        0
    }
}
