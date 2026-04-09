//! Minimal JSON field extraction for no_std environments.
//! Zero-alloc: returns borrowed string slices from the input JSON.

/// Extract a string value for a given key from JSON.
/// Returns a slice into the original `json` string (zero-copy).
///
/// Limitations: does not handle escaped characters in the RETURNED value.
/// For values with escapes, use `extract_owned` (requires alloc feature).
pub fn extract<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    // Find "key" pattern
    let mut search_buf = [0u8; 130]; // "key" with max 128-char key
    if key.len() + 2 > search_buf.len() { return None; }
    search_buf[0] = b'"';
    search_buf[1..1 + key.len()].copy_from_slice(key.as_bytes());
    search_buf[1 + key.len()] = b'"';
    let search = core::str::from_utf8(&search_buf[..key.len() + 2]).ok()?;

    let key_pos = json.find(search)?;
    let after_key = &json[key_pos + search.len()..];

    let after_colon = after_key.trim_start();
    if !after_colon.starts_with(':') { return None; }
    let value_start = after_colon[1..].trim_start();
    if !value_start.starts_with('"') { return None; }

    // Find closing quote (skip escaped quotes)
    let content = &value_start[1..];
    let mut end = 0;
    let bytes = content.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'\\' { end += 2; continue; }
        if bytes[end] == b'"' { break; }
        end += 1;
    }
    if end >= bytes.len() { return None; }
    Some(&content[..end])
}
