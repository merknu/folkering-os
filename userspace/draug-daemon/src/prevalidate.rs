//! Local syntactic pre-validation for LLM-emitted Rust code.
//!
//! The proxy's `cargo check` round-trip costs seconds per attempt;
//! when the LLM hits its token cap mid-expression and emits half a
//! function (or duplicates a `}`), we'd rather catch it here than
//! waste a Proxmox round-trip and a cargo build slot.
//!
//! This is *not* a compiler. It's a bracket-balance + literal-state
//! check that knows enough Rust lexical rules to:
//!   * skip `//` and `/* */` (with nesting) comments
//!   * skip `"…"` strings (with `\"` escapes)
//!   * skip `r"…"` / `r#"…"#` / `r##"…"##` raw strings
//!   * skip `'x'`, `'\n'`, `'\u{1F600}'` char literals
//!   * skip lifetime tokens `'a` (which look like an unterminated
//!     char literal otherwise)
//!
//! Inside code (i.e. outside the above), we count `{`, `(`, `[` on
//! the way down and pop on `}`, `)`, `]`. Any imbalance — or any
//! unterminated literal at end-of-input — is a pre-validation
//! failure.
//!
//! We deliberately don't try to validate keywords, semicolons, or
//! anything semantic. The goal is "this looks like complete Rust",
//! not "this Rust compiles". The retry-with-feedback loop in
//! `draug_async::process_patch_result` handles real compilation
//! errors; pre-validation just catches the obvious truncations and
//! mismatched-brace cases that don't need a full cargo cycle to
//! diagnose.

extern crate alloc;

use alloc::string::String;

/// Outcome of a pre-validation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrevalidateOutcome {
    /// All literals closed, all brackets balanced. Safe to ship to
    /// the proxy.
    Ok,
    /// Bracket imbalance — `expected` is what we wanted to see next
    /// (e.g. "}"), `found` is what we actually got (or "(none)" at
    /// end-of-input).
    BracketMismatch { expected: char, found: char, byte_offset: usize },
    /// Reached end of input while still inside a string / char / raw
    /// string / block comment.
    Unterminated { kind: &'static str, byte_offset: usize },
    /// Closing bracket with no matching open.
    StrayClose { found: char, byte_offset: usize },
}

impl PrevalidateOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, PrevalidateOutcome::Ok)
    }

    /// Format a single-line human-readable diagnostic. Good enough
    /// to feed straight into the retry-with-feedback prompt.
    pub fn diagnostic(&self) -> String {
        let mut s = String::with_capacity(96);
        match self {
            PrevalidateOutcome::Ok => s.push_str("ok"),
            PrevalidateOutcome::BracketMismatch { expected, found, byte_offset } => {
                s.push_str("[prevalidate] bracket mismatch at byte ");
                push_dec(&mut s, *byte_offset);
                s.push_str(": expected '");
                s.push(*expected);
                s.push_str("', found '");
                s.push(*found);
                s.push('\'');
            }
            PrevalidateOutcome::Unterminated { kind, byte_offset } => {
                s.push_str("[prevalidate] unterminated ");
                s.push_str(kind);
                s.push_str(" starting at byte ");
                push_dec(&mut s, *byte_offset);
            }
            PrevalidateOutcome::StrayClose { found, byte_offset } => {
                s.push_str("[prevalidate] stray closing '");
                s.push(*found);
                s.push_str("' at byte ");
                push_dec(&mut s, *byte_offset);
                s.push_str(" with no matching open");
            }
        }
        s
    }
}

/// Bracket-balance + literal-state check. See module docs.
pub fn check(src: &str) -> PrevalidateOutcome {
    // Bounded stack: code with > 256 nested brackets is already
    // pathological — the LLM isn't producing that on accident.
    const MAX_DEPTH: usize = 256;
    let mut stack: [(u8, usize); MAX_DEPTH] = [(0u8, 0usize); MAX_DEPTH];
    let mut depth: usize = 0;

    let bytes = src.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            // Line comment
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            // Block comment (with nesting, like rustc)
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                let start = i;
                i += 2;
                let mut nest: usize = 1;
                while i + 1 < bytes.len() && nest > 0 {
                    if bytes[i] == b'/' && bytes[i + 1] == b'*' {
                        nest += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        nest -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if nest > 0 {
                    return PrevalidateOutcome::Unterminated {
                        kind: "block comment", byte_offset: start,
                    };
                }
            }
            // Raw string: r"…" or r#"…"# or r##"…"## …
            b'r' if i + 1 < bytes.len()
                && (bytes[i + 1] == b'"' || bytes[i + 1] == b'#') =>
            {
                let start = i;
                i += 1;
                let mut hashes: usize = 0;
                while i < bytes.len() && bytes[i] == b'#' {
                    hashes += 1;
                    i += 1;
                }
                if i >= bytes.len() || bytes[i] != b'"' {
                    // Not actually a raw string, just an `r` ident
                    // followed by `#` for some other reason. Rewind
                    // and treat as identifier byte.
                    i = start + 1;
                    continue;
                }
                i += 1; // past opening "
                // Look for closing `"` followed by exactly `hashes` `#`s
                let mut closed = false;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        let mut k = 0usize;
                        while k < hashes
                            && i + 1 + k < bytes.len()
                            && bytes[i + 1 + k] == b'#'
                        {
                            k += 1;
                        }
                        if k == hashes {
                            i += 1 + hashes;
                            closed = true;
                            break;
                        }
                    }
                    i += 1;
                }
                if !closed {
                    return PrevalidateOutcome::Unterminated {
                        kind: "raw string literal", byte_offset: start,
                    };
                }
            }
            // String literal
            b'"' => {
                let start = i;
                i += 1;
                let mut closed = false;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if c == b'"' {
                        i += 1;
                        closed = true;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    return PrevalidateOutcome::Unterminated {
                        kind: "string literal", byte_offset: start,
                    };
                }
            }
            // Char literal OR lifetime. Disambiguate by looking ahead:
            // a lifetime is `'` followed by an XID-start char and not
            // immediately followed by `'`. A char literal always has
            // a closing `'` within ~10 bytes (the longest is
            // `'\u{10FFFF}'` = 10 bytes including quotes).
            b'\'' => {
                let start = i;
                // Try to parse as char literal first.
                let lookahead = (bytes.len() - i).min(12);
                let mut j = i + 1;
                let end = i + lookahead;
                let mut is_char = false;
                if j < end {
                    if bytes[j] == b'\\' {
                        // Escape — scan until '\''
                        j += 1;
                        while j < end {
                            if bytes[j] == b'\'' {
                                is_char = true;
                                break;
                            }
                            j += 1;
                        }
                    } else {
                        // Single byte / multi-byte UTF-8 codepoint
                        // followed by `'`. Walk past one UTF-8 char
                        // (1..=4 bytes), then check.
                        let lead = bytes[j];
                        let utf8_len = if lead < 0x80 { 1 }
                            else if lead < 0xC0 { 1 }   // invalid; treat as 1
                            else if lead < 0xE0 { 2 }
                            else if lead < 0xF0 { 3 }
                            else { 4 };
                        j += utf8_len;
                        if j < bytes.len() && bytes[j] == b'\'' {
                            is_char = true;
                        }
                    }
                }
                if is_char {
                    i = j + 1;
                } else {
                    // Treat as lifetime — single tick, advance one
                    // byte and let the rest of the loop see the
                    // identifier characters as ordinary code (they
                    // don't affect bracket balance).
                    let _ = start;
                    i += 1;
                }
            }
            b'{' | b'(' | b'[' => {
                if depth >= MAX_DEPTH {
                    // Pathological — bail out as imbalance.
                    return PrevalidateOutcome::BracketMismatch {
                        expected: ')', found: b as char, byte_offset: i,
                    };
                }
                stack[depth] = (b, i);
                depth += 1;
                i += 1;
            }
            b'}' | b')' | b']' => {
                if depth == 0 {
                    return PrevalidateOutcome::StrayClose {
                        found: b as char, byte_offset: i,
                    };
                }
                let (open, _) = stack[depth - 1];
                let want_close = match open {
                    b'{' => b'}', b'(' => b')', b'[' => b']', _ => 0,
                };
                if b != want_close {
                    return PrevalidateOutcome::BracketMismatch {
                        expected: want_close as char,
                        found: b as char,
                        byte_offset: i,
                    };
                }
                depth -= 1;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    if depth > 0 {
        let (open, off) = stack[depth - 1];
        let want_close = match open {
            b'{' => '}', b'(' => ')', b'[' => ']', _ => '?',
        };
        return PrevalidateOutcome::BracketMismatch {
            expected: want_close, found: '\0', byte_offset: off,
        };
    }

    PrevalidateOutcome::Ok
}

fn push_dec(out: &mut String, mut v: usize) {
    if v == 0 {
        out.push('0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut n = 0;
    while v > 0 {
        buf[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    while n > 0 {
        n -= 1;
        out.push(buf[n] as char);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_simple() {
        assert!(check("fn main() { let x = 1; }").is_ok());
    }

    #[test]
    fn ok_strings_and_comments_with_brackets() {
        let src = "
            // {
            /* { ( [ */
            let s = \"{ ( [\";
            let c = '{';
            let raw = r\"{ ( [\";
            fn lifetime<'a>(_: &'a str) {}
        ";
        assert!(check(src).is_ok(), "got {:?}", check(src));
    }

    #[test]
    fn ok_raw_string_with_hashes() {
        let src = "let raw = r#\"contains \" quote\"#;";
        assert!(check(src).is_ok(), "got {:?}", check(src));
    }

    #[test]
    fn detects_truncated_function() {
        // LLM ran out of tokens mid-block
        let src = "fn main() {\n    let x = vec![\n        1, 2,";
        let r = check(src);
        assert!(!r.is_ok(), "should detect truncation, got {:?}", r);
    }

    #[test]
    fn detects_stray_close() {
        let r = check("fn main() { } }");
        assert!(matches!(r, PrevalidateOutcome::StrayClose { .. }), "got {:?}", r);
    }

    #[test]
    fn detects_mismatched() {
        let r = check("fn main() { let x = vec![1, 2; }");
        assert!(matches!(r, PrevalidateOutcome::BracketMismatch { .. }), "got {:?}", r);
    }

    #[test]
    fn detects_unterminated_string() {
        let r = check("fn main() { let s = \"hello; }");
        assert!(matches!(r, PrevalidateOutcome::Unterminated { kind, .. } if kind == "string literal"), "got {:?}", r);
    }

    #[test]
    fn nested_block_comment_ok() {
        assert!(check("fn x() { /* outer /* inner */ still in outer */ let y = 1; }").is_ok());
    }
}
