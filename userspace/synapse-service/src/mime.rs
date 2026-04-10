//! MIME type auto-detection from filename extension and content magic bytes.

/// Auto-detect MIME type from filename extension and content magic bytes.
pub fn auto_detect_mime<'a>(name: &str, content: &[u8]) -> &'a str {
    // Check extension
    if name.ends_with(".wasm") { return "application/wasm"; }
    if name.ends_with(".txt")  { return "text/plain"; }
    if name.ends_with(".json") { return "application/json"; }
    if name.ends_with(".csv")  { return "text/csv"; }
    if name.ends_with(".html") { return "text/html"; }
    if name.ends_with(".rs")   { return "text/x-rust"; }

    // Check content magic bytes
    if content.len() >= 4 {
        // WASM magic: \0asm
        if content[0] == 0x00 && content[1] == b'a' && content[2] == b's' && content[3] == b'm' {
            return "application/wasm";
        }
        // ELF magic: \x7fELF
        if content[0] == 0x7f && content[1] == b'E' && content[2] == b'L' && content[3] == b'F' {
            return "application/x-elf";
        }
        // JSON start
        if content[0] == b'{' || content[0] == b'[' {
            return "application/json";
        }
    }

    // Check if content is valid UTF-8 text
    if content.len() > 0 && core::str::from_utf8(content).is_ok() {
        return "text/plain";
    }

    "application/octet-stream"
}
