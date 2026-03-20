//! Minimal JSON field extractor — no serde, no alloc needed for parsing.
//!
//! Extracts string values from flat JSON objects by key name.
//! Handles nested objects by skipping them. Not a full parser.

/// Find the string value for a given key in a JSON object.
/// Returns the value without quotes, or None if not found.
///
/// Example: `json_get_str(b'{"name":"hello","id":42}', "name")` → Some("hello")
pub fn json_get_str<'a>(json: &'a [u8], key: &str) -> Option<&'a str> {
    // Search for "key": "value"
    let key_pattern = build_key_pattern(key);
    let mut pos = 0;

    while pos + key_pattern.len() < json.len() {
        // Find the key pattern
        if let Some(found) = find_bytes(&json[pos..], key_pattern.as_bytes()) {
            let after_key = pos + found + key_pattern.len();

            // Skip whitespace
            let val_start = skip_whitespace(json, after_key);
            if val_start >= json.len() {
                return None;
            }

            // Check if value is a quoted string
            if json[val_start] == b'"' {
                let str_start = val_start + 1;
                if let Some(str_end) = find_char(json, str_start, b'"') {
                    let s = core::str::from_utf8(&json[str_start..str_end]).ok()?;
                    return Some(s);
                }
            }

            pos = val_start + 1;
        } else {
            break;
        }
    }

    None
}

/// Find a numeric value for a given key.
/// Example: `json_get_num(b'{"size":1234}', "size")` → Some(1234)
pub fn json_get_num(json: &[u8], key: &str) -> Option<u64> {
    let key_pattern = build_key_pattern(key);

    if let Some(found) = find_bytes(json, key_pattern.as_bytes()) {
        let after_key = found + key_pattern.len();
        let val_start = skip_whitespace(json, after_key);

        if val_start >= json.len() {
            return None;
        }

        // Parse digits
        let mut val: u64 = 0;
        let mut i = val_start;
        while i < json.len() && json[i] >= b'0' && json[i] <= b'9' {
            val = val * 10 + (json[i] - b'0') as u64;
            i += 1;
        }
        if i > val_start {
            return Some(val);
        }
    }

    None
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build the pattern `"key":` for searching
fn build_key_pattern(key: &str) -> heapless::String<64> {
    let mut s = heapless::String::new();
    let _ = s.push('"');
    for &b in key.as_bytes() {
        let _ = s.push(b as char);
    }
    let _ = s.push('"');
    let _ = s.push(':');
    s
}

/// Find a byte sequence in a slice, return offset from start
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    for i in 0..=(haystack.len() - needle.len()) {
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

/// Find a specific character starting at pos
fn find_char(data: &[u8], start: usize, ch: u8) -> Option<usize> {
    for i in start..data.len() {
        // Handle escaped quotes
        if data[i] == ch && (i == 0 || data[i - 1] != b'\\') {
            return Some(i);
        }
    }
    None
}

/// Skip whitespace characters
fn skip_whitespace(data: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < data.len() && (data[i] == b' ' || data[i] == b'\t' || data[i] == b'\n' || data[i] == b'\r') {
        i += 1;
    }
    i
}
