//! Gemini API Client — HTTPS POST to Google's Generative Language API
//!
//! Sends a prompt to Gemini 2.5 Flash and returns the generated text.
//! Uses the existing TLS 1.3 stack (embedded-tls over smoltcp).

extern crate alloc;

use alloc::vec::Vec;

use super::json;
use super::tls;

/// Gemini API key — replace with your actual key
const API_KEY: &str = "***REDACTED***";

/// Gemini API host
const GEMINI_HOST: &str = "generativelanguage.googleapis.com";

/// Maximum response size (128KB — Gemini can return large code blocks)
const MAX_RESPONSE: usize = 131072;

/// Ask Gemini a question. Returns the generated text or an error string.
///
/// This function:
/// 1. Resolves the API hostname via DNS (with hardcoded fallback)
/// 2. Builds a JSON request with proper escaping
/// 3. Makes an HTTPS POST with TLS 1.3
/// 4. Parses the response JSON to extract the generated text
/// 5. Returns unescaped text ready for display
pub fn ask_gemini(prompt: &str) -> Result<Vec<u8>, &'static str> {
    crate::serial_str!("[GEMINI] Sending prompt (");
    crate::drivers::serial::write_dec(prompt.len() as u32);
    crate::serial_str!(" bytes)...\n");

    // 1. Resolve IP via DNS (with fallback)
    let ip = resolve_gemini_ip()?;

    crate::serial_str!("[GEMINI] Resolved IP: ");
    crate::drivers::serial::write_dec(ip[0] as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[1] as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[2] as u32);
    crate::serial_str!(".");
    crate::drivers::serial::write_dec(ip[3] as u32);
    crate::drivers::serial::write_newline();

    // 2. Build JSON body with escaped prompt
    let body = build_request_json(prompt);

    // 3. Build full HTTP POST request
    let request = build_https_post(&body);

    crate::serial_str!("[GEMINI] POST request: ");
    crate::drivers::serial::write_dec(request.len() as u32);
    crate::serial_str!(" bytes\n");

    // 4. Send via TLS
    let response = tls::https_get_raw(ip, GEMINI_HOST, &request)?;

    crate::serial_str!("[GEMINI] Response: ");
    crate::drivers::serial::write_dec(response.len() as u32);
    crate::serial_str!(" bytes\n");

    // 5. Parse HTTP status
    let status = parse_http_status(&response);
    if status != 200 {
        crate::serial_str!("[GEMINI] HTTP error: ");
        crate::drivers::serial::write_dec(status as u32);
        crate::drivers::serial::write_newline();

        // Log first 200 bytes of response for debugging
        let preview_len = response.len().min(200);
        if let Ok(preview) = core::str::from_utf8(&response[..preview_len]) {
            crate::drivers::serial::write_str(preview);
            crate::drivers::serial::write_newline();
        }

        return match status {
            400 => Err("Gemini: bad request (JSON error)"),
            401 | 403 => Err("Gemini: invalid API key"),
            429 => Err("Gemini: rate limited"),
            500..=599 => Err("Gemini: server error"),
            _ => Err("Gemini: unexpected HTTP status"),
        };
    }

    // 6. Extract body (after \r\n\r\n)
    let body_start = find_body_start(&response).ok_or("no HTTP body")?;
    let body_bytes = &response[body_start..];

    // 7. Extract generated text from JSON
    // Response format: {"candidates":[{"content":{"parts":[{"text":"..."}]}}]}
    let text = extract_gemini_text(body_bytes)?;

    crate::serial_str!("[GEMINI] Generated ");
    crate::drivers::serial::write_dec(text.len() as u32);
    crate::serial_str!(" bytes of text\n");

    Ok(text)
}

/// Resolve Gemini API IP via DNS with hardcoded fallback
fn resolve_gemini_ip() -> Result<[u8; 4], &'static str> {
    let packed = super::dns_lookup(GEMINI_HOST);
    if packed != 0 {
        let a = ((packed >> 24) & 0xFF) as u8;
        let b = ((packed >> 16) & 0xFF) as u8;
        let c = ((packed >> 8) & 0xFF) as u8;
        let d = (packed & 0xFF) as u8;
        Ok([a, b, c, d])
    } else {
        // Fallback: generativelanguage.googleapis.com often resolves to
        // a Google front-end IP. This may need updating.
        crate::serial_str!("[GEMINI] DNS failed, using fallback IP\n");
        Ok([142, 250, 74, 106]) // One of Google's IPs
    }
}

/// Build the JSON request body with proper escaping
fn build_request_json(prompt: &str) -> Vec<u8> {
    let mut json = Vec::with_capacity(prompt.len() + 128);
    json.extend_from_slice(b"{\"contents\":[{\"parts\":[{\"text\":\"");
    // Escape the prompt for JSON
    json_escape_into(prompt, &mut json);
    json.extend_from_slice(b"\"}]}]}");
    json
}

/// Escape a string for JSON: handles \, ", \n, \r, \t
fn json_escape_into(s: &str, out: &mut Vec<u8>) {
    for &b in s.as_bytes() {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'"' => out.extend_from_slice(b"\\\""),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b if b < 0x20 => {
                // Control character — skip
            }
            _ => out.push(b),
        }
    }
}

/// Build the full HTTP POST request
fn build_https_post(body: &[u8]) -> Vec<u8> {
    let mut request = Vec::with_capacity(512 + body.len());

    // Request line with API key
    request.extend_from_slice(b"POST /v1beta/models/gemini-2.5-flash:generateContent?key=");
    request.extend_from_slice(API_KEY.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\n");

    // Headers
    request.extend_from_slice(b"Host: ");
    request.extend_from_slice(GEMINI_HOST.as_bytes());
    request.extend_from_slice(b"\r\n");
    request.extend_from_slice(b"Content-Type: application/json\r\n");

    // Content-Length (decimal)
    request.extend_from_slice(b"Content-Length: ");
    let mut len_buf = [0u8; 10];
    let len_str = format_decimal(body.len(), &mut len_buf);
    request.extend_from_slice(len_str);
    request.extend_from_slice(b"\r\n");

    request.extend_from_slice(b"Connection: close\r\n");
    request.extend_from_slice(b"\r\n");

    // Body
    request.extend_from_slice(body);

    request
}

/// Format a usize as decimal ASCII bytes
fn format_decimal(mut n: usize, buf: &mut [u8; 10]) -> &[u8] {
    if n == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = 10;
    while n > 0 && i > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    &buf[i..]
}

/// Parse HTTP status code from response
fn parse_http_status(response: &[u8]) -> u16 {
    // "HTTP/1.1 200 OK\r\n" — status at bytes 9..12
    if response.len() < 12 || !response.starts_with(b"HTTP/") {
        return 0;
    }
    let status_bytes = &response[9..12];
    let mut status = 0u16;
    for &b in status_bytes {
        if b.is_ascii_digit() {
            status = status * 10 + (b - b'0') as u16;
        }
    }
    status
}

/// Find the start of HTTP body (after \r\n\r\n)
fn find_body_start(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}

/// Extract the generated text from Gemini JSON response.
/// Looks for the "text" field in the nested structure.
/// Also unescapes JSON string escapes (\n, \", \\, \t).
fn extract_gemini_text(body: &[u8]) -> Result<Vec<u8>, &'static str> {
    // Find "text":" pattern (the actual generated content)
    let pattern = b"\"text\":\"";
    let body_str = body;

    let start = find_pattern(body_str, pattern).ok_or("no text field in response")?;
    let text_start = start + pattern.len();

    // Find the closing quote (handling escapes)
    let mut end = text_start;
    while end < body_str.len() {
        if body_str[end] == b'\\' {
            end += 2; // Skip escaped character
            continue;
        }
        if body_str[end] == b'"' {
            break;
        }
        end += 1;
    }

    if end >= body_str.len() {
        return Err("unterminated text field");
    }

    // Unescape the JSON string
    let escaped = &body_str[text_start..end];
    let mut result = Vec::with_capacity(escaped.len());
    let mut i = 0;
    while i < escaped.len() {
        if escaped[i] == b'\\' && i + 1 < escaped.len() {
            match escaped[i + 1] {
                b'n' => result.push(b'\n'),
                b'r' => result.push(b'\r'),
                b't' => result.push(b'\t'),
                b'"' => result.push(b'"'),
                b'\\' => result.push(b'\\'),
                b'/' => result.push(b'/'),
                other => {
                    result.push(b'\\');
                    result.push(other);
                }
            }
            i += 2;
        } else {
            result.push(escaped[i]);
            i += 1;
        }
    }

    Ok(result)
}

/// Find a byte pattern in a slice
fn find_pattern(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    for i in 0..=(haystack.len() - needle.len()) {
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}
