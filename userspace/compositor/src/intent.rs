//! Intent Engine — Parses AI directives into OS actions
//!
//! Receives structured commands from the Gemini proxy and routes them
//! to Window Manager, Synapse filesystem, or Shell.
//!
//! Uses lightweight manual JSON parsing (no serde dependency) to avoid
//! heap fragmentation in no_std. Operates on a pre-allocated arena buffer.

extern crate alloc;
use alloc::string::String;

/// Maximum arena size for JSON transaction processing
const ARENA_SIZE: usize = 65536; // 64KB

/// Parsed AI intent — deterministic actions the LLM can invoke
#[derive(Debug)]
pub enum AgentIntent {
    /// Move a window to new coordinates
    MoveWindow { window_id: u32, x: u32, y: u32 },
    /// Resize a window
    ResizeWindow { window_id: u32, w: u32, h: u32 },
    /// Close a window
    CloseWindow { window_id: u32 },
    /// Read a file from Synapse VFS
    ReadFile { path: String },
    /// Write content to a file
    WriteFile { path: String, content: String },
    /// Generate a WASM tool — triggers second call to proxy
    GenerateTool { prompt: String },
    /// WASM tool compiled and ready — binary is base64-encoded
    ToolReady { binary_base64: String },
    /// Plain text response (no structured action)
    TextResponse { text: String },
    /// Error from the AI
    Error { message: String },
}

/// Parse a JSON-like response from the Gemini proxy into an AgentIntent.
/// Supports both structured JSON-RPC and plain text responses.
///
/// Expected JSON format:
/// ```json
/// {"action": "move_window", "window_id": 2, "x": 100, "y": 200}
/// ```
///
/// Falls back to TextResponse for unstructured text.
pub fn parse_intent(response: &str) -> AgentIntent {
    let trimmed = response.trim();

    // Strip <think>...</think> blocks — extract content after closing tag
    // DeepSeek-R1 wraps reasoning in <think> tags before the JSON action
    let effective = if let Some(think_start) = trimmed.find("<think>") {
        if let Some(think_end) = trimmed.find("</think>") {
            trimmed[think_end + 8..].trim()
        } else {
            trimmed // unclosed tag — use full text
        }
    } else {
        trimmed
    };

    // Debug: log think tag extraction
    if trimmed.contains("<think>") {
        use libfolk::sys::io::write_str;
        write_str("[INTENT] Found <think> in response, effective starts with: ");
        write_str(&effective[..effective.len().min(30)]);
        write_str("\n");
    }

    // Check if response is JSON (starts with '{')
    if !effective.starts_with('{') {
        return AgentIntent::TextResponse { text: String::from(trimmed) };
    }

    // Extract "action" field
    let action = match extract_str(effective, "action") {
        Some(a) => a,
        None => return AgentIntent::TextResponse { text: String::from(trimmed) },
    };

    match action.as_str() {
        "move_window" => {
            let wid = extract_num(effective, "window_id").unwrap_or(0);
            let x = extract_num(effective, "x").unwrap_or(0);
            let y = extract_num(effective, "y").unwrap_or(0);
            AgentIntent::MoveWindow { window_id: wid, x, y }
        }
        "resize_window" => {
            let wid = extract_num(effective, "window_id").unwrap_or(0);
            let w = extract_num(effective, "w").unwrap_or(400);
            let h = extract_num(effective, "h").unwrap_or(300);
            AgentIntent::ResizeWindow { window_id: wid, w, h }
        }
        "close_window" => {
            let wid = extract_num(effective, "window_id").unwrap_or(0);
            AgentIntent::CloseWindow { window_id: wid }
        }
        "read_file" => {
            let path = extract_str(effective, "path").unwrap_or_default();
            AgentIntent::ReadFile { path }
        }
        "write_file" => {
            let path = extract_str(effective, "path").unwrap_or_default();
            let content = extract_str(effective, "content").unwrap_or_default();
            AgentIntent::WriteFile { path, content }
        }
        "generate_tool" => {
            let prompt = extract_str(effective, "prompt").unwrap_or_default();
            AgentIntent::GenerateTool { prompt }
        }
        "tool_ready" => {
            let binary = extract_str(effective, "binary").unwrap_or_default();
            AgentIntent::ToolReady { binary_base64: binary }
        }
        "error" => {
            let msg = extract_str(effective, "message").unwrap_or_default();
            AgentIntent::Error { message: msg }
        }
        _ => AgentIntent::TextResponse { text: String::from(trimmed) },
    }
}

/// Extract a string value from JSON: "key": "value"
fn extract_str(json: &str, key: &str) -> Option<String> {
    let pattern = alloc::format!("\"{}\":", key);
    let start = json.find(&pattern)? + pattern.len();
    let rest = json[start..].trim_start();

    if !rest.starts_with('"') {
        return None;
    }

    let inner = &rest[1..];
    let mut end = 0;
    let bytes = inner.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'\\' {
            end += 2; // Skip escaped char
            continue;
        }
        if bytes[end] == b'"' {
            break;
        }
        end += 1;
    }

    Some(String::from(&inner[..end]))
}

/// Extract a numeric value from JSON: "key": 123
fn extract_num(json: &str, key: &str) -> Option<u32> {
    let pattern = alloc::format!("\"{}\":", key);
    let start = json.find(&pattern)? + pattern.len();
    let rest = json[start..].trim_start();

    let mut end = 0;
    let bytes = rest.as_bytes();
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }

    if end == 0 { return None; }
    rest[..end].parse().ok()
}

// ── Base64 Decoder ──────────────────────────────────────────────────────

use alloc::vec::Vec;

/// Decode base64 string to bytes. Standard alphabet (A-Z, a-z, 0-9, +, /).
/// Handles = and == padding. Returns None on invalid input.
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    #[inline]
    fn decode_char(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes = input.as_bytes();
    // Filter out whitespace/newlines
    let clean: Vec<u8> = bytes.iter().copied().filter(|&b| b != b'\n' && b != b'\r' && b != b' ').collect();
    let len = clean.len();
    if len == 0 { return Some(Vec::new()); }
    if len % 4 != 0 { return None; }

    let mut out = Vec::with_capacity(len / 4 * 3);

    for chunk in clean.chunks_exact(4) {
        let a = decode_char(chunk[0])?;
        let b = decode_char(chunk[1])?;

        // Third and fourth may be padding
        let c_pad = chunk[2] == b'=';
        let d_pad = chunk[3] == b'=';

        let c = if c_pad { 0 } else { decode_char(chunk[2])? };
        let d = if d_pad { 0 } else { decode_char(chunk[3])? };

        let triple = ((a as u32) << 18) | ((b as u32) << 12) | ((c as u32) << 6) | (d as u32);

        out.push((triple >> 16) as u8);
        if !c_pad { out.push((triple >> 8) as u8); }
        if !d_pad { out.push(triple as u8); }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_intent tests ──

    #[test]
    fn parse_text_response() {
        let r = parse_intent("Hello, how can I help?");
        match r {
            AgentIntent::TextResponse { text } => assert_eq!(text, "Hello, how can I help?"),
            _ => panic!("Expected TextResponse"),
        }
    }

    #[test]
    fn parse_move_window() {
        let r = parse_intent(r#"{"action": "move_window", "window_id": 3, "x": 100, "y": 200}"#);
        match r {
            AgentIntent::MoveWindow { window_id, x, y } => {
                assert_eq!(window_id, 3);
                assert_eq!(x, 100);
                assert_eq!(y, 200);
            }
            _ => panic!("Expected MoveWindow"),
        }
    }

    #[test]
    fn parse_close_window() {
        let r = parse_intent(r#"{"action": "close_window", "window_id": 5}"#);
        match r {
            AgentIntent::CloseWindow { window_id } => assert_eq!(window_id, 5),
            _ => panic!("Expected CloseWindow"),
        }
    }

    #[test]
    fn parse_generate_tool() {
        let r = parse_intent(r#"{"action": "generate_tool", "prompt": "draw a circle"}"#);
        match r {
            AgentIntent::GenerateTool { prompt } => assert_eq!(prompt, "draw a circle"),
            _ => panic!("Expected GenerateTool"),
        }
    }

    #[test]
    fn parse_think_tags_stripped_for_json() {
        // DeepSeek-R1 response: <think>...</think> followed by JSON
        let input = "<think>\nI should move window 2\n</think>\n{\"action\": \"move_window\", \"window_id\": 2, \"x\": 50, \"y\": 50}";
        let r = parse_intent(input);
        match r {
            AgentIntent::MoveWindow { window_id, x, y } => {
                assert_eq!(window_id, 2);
                assert_eq!(x, 50);
                assert_eq!(y, 50);
            }
            _ => panic!("Expected MoveWindow after think stripping, got {:?}",
                        match r { AgentIntent::TextResponse { text } => text, _ => String::from("other") }),
        }
    }

    #[test]
    fn parse_think_tags_unclosed_falls_through() {
        let input = "<think>\nstill thinking...";
        let r = parse_intent(input);
        match r {
            AgentIntent::TextResponse { .. } => {} // expected
            _ => panic!("Unclosed think should fallback to TextResponse"),
        }
    }

    // ── extract_str tests ──

    #[test]
    fn extract_str_basic() {
        let json = r#"{"name": "hello", "value": 42}"#;
        assert_eq!(extract_str(json, "name"), Some(String::from("hello")));
    }

    #[test]
    fn extract_str_with_spaces() {
        let json = r#"{"action":  "move_window"}"#;
        assert_eq!(extract_str(json, "action"), Some(String::from("move_window")));
    }

    #[test]
    fn extract_str_missing() {
        let json = r#"{"action": "test"}"#;
        assert_eq!(extract_str(json, "missing"), None);
    }

    #[test]
    fn extract_str_escaped_quote() {
        let json = r#"{"msg": "say \"hello\""}"#;
        assert_eq!(extract_str(json, "msg"), Some(String::from(r#"say \"hello\""#)));
    }

    // ── extract_num tests ──

    #[test]
    fn extract_num_basic() {
        let json = r#"{"x": 42, "y": 100}"#;
        assert_eq!(extract_num(json, "x"), Some(42));
        assert_eq!(extract_num(json, "y"), Some(100));
    }

    #[test]
    fn extract_num_zero() {
        assert_eq!(extract_num(r#"{"val": 0}"#, "val"), Some(0));
    }

    #[test]
    fn extract_num_missing() {
        assert_eq!(extract_num(r#"{"x": 1}"#, "z"), None);
    }

    // ── base64_decode tests ──

    #[test]
    fn base64_empty() {
        assert_eq!(base64_decode(""), Some(vec![]));
    }

    #[test]
    fn base64_hello() {
        // "Hello" = SGVsbG8=
        assert_eq!(base64_decode("SGVsbG8="), Some(b"Hello".to_vec()));
    }

    #[test]
    fn base64_padding_two() {
        // "Hi" = SGk=
        assert_eq!(base64_decode("SGk="), Some(b"Hi".to_vec()));
    }

    #[test]
    fn base64_no_padding() {
        // "Hey!" = SGV5IQ==
        assert_eq!(base64_decode("SGV5IQ=="), Some(b"Hey!".to_vec()));
    }

    #[test]
    fn base64_invalid_length() {
        assert_eq!(base64_decode("ABC"), None); // not multiple of 4
    }

    #[test]
    fn base64_with_whitespace() {
        assert_eq!(base64_decode("SGVs\nbG8="), Some(b"Hello".to_vec()));
    }
}
