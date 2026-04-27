//! Locate and extract a function's source text from a `.rs` file.
//!
//! The extractor is the part of the refactor flow that gives Draug
//! something concrete to refactor. We want the *original text* (not a
//! pretty-printed AST round-trip) so layout, comments, and blank lines
//! survive into the prompt.
//!
//! Strategy:
//! 1. Parse the file with `syn` to confirm a fn with that name exists.
//!    Fails fast if the task fixture points at a typo or moved fn.
//! 2. Text-scan for `fn <name>` with word boundaries, then walk back
//!    over `pub`/`pub(...)` visibility and `#[...]` attributes, then
//!    walk forward through the signature `(...)` + optional return
//!    type and into the body `{ ... }` with brace counting that skips
//!    string and char literals + comments.
//! 3. Return the resulting slice `[start..end]` as `String`.
//!
//! Brace counting is intentionally simple — it handles the conventions
//! actually used in folkering (no raw-string `r#"…"#` with `{` inside,
//! no nested raw-strings). If a future fn breaks it, the test for that
//! specific function will catch it and we add cases.

use std::path::Path;
use syn::visit::Visit;

#[derive(Debug)]
pub enum ExtractError {
    Io(std::io::Error),
    Parse(String),
    NotFound(String),
    Ambiguous { name: String, count: usize },
    UnbalancedBraces,
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractError::Io(e) => write!(f, "io: {e}"),
            ExtractError::Parse(e) => write!(f, "parse: {e}"),
            ExtractError::NotFound(n) => write!(f, "fn '{n}' not found in source"),
            ExtractError::Ambiguous { name, count } => {
                write!(f, "fn '{name}' appears {count} times in source — disambiguate")
            }
            ExtractError::UnbalancedBraces => write!(f, "fn body has unbalanced braces"),
        }
    }
}

impl std::error::Error for ExtractError {}

/// Result of extracting a fn from a source file.
#[derive(Debug)]
pub struct Extracted {
    /// The full text of the fn, including attributes and visibility.
    pub source: String,
    /// 1-indexed line number where the extracted slice starts.
    pub start_line: usize,
    /// 1-indexed line number where the extracted slice ends (inclusive).
    pub end_line: usize,
}

pub fn extract_fn(path: &Path, fn_name: &str) -> Result<Extracted, ExtractError> {
    let raw = std::fs::read_to_string(path).map_err(ExtractError::Io)?;

    // (1) Validate the fn actually exists. We use syn for this rather
    //     than trusting raw text-search, because text-search would happily
    //     match a `// fn foo` comment or a `fn foo` substring inside a
    //     macro_rules! body.
    let parsed = syn::parse_file(&raw)
        .map_err(|e| ExtractError::Parse(e.to_string()))?;
    let mut counter = NameCounter { target: fn_name, free: 0, method: 0 };
    counter.visit_file(&parsed);
    let total = counter.free + counter.method;
    if total == 0 {
        return Err(ExtractError::NotFound(fn_name.to_string()));
    }
    // Disambiguation rule: prefer a free fn when one exists alongside
    // impl methods of the same name (free fns are typically the public
    // API; the impl method is usually a private helper that happens to
    // share a name). Only fail Ambiguous when there are multiple frees
    // or multiple impl methods with no free.
    let prefer_free = counter.free >= 1;
    if prefer_free && counter.free > 1 {
        return Err(ExtractError::Ambiguous { name: fn_name.to_string(), count: counter.free });
    }
    if !prefer_free && counter.method > 1 {
        return Err(ExtractError::Ambiguous { name: fn_name.to_string(), count: counter.method });
    }

    // (2) Locate the `fn <name>` token in the raw text. With prefer_free
    //     we want the match at impl-depth 0; otherwise pick the first.
    let fn_pos = find_fn_keyword(&raw, fn_name, prefer_free)
        .ok_or_else(|| ExtractError::NotFound(fn_name.to_string()))?;

    // (3) Walk back from fn_pos over visibility / attributes to find the
    //     real start of the item.
    let start = walk_back_to_item_start(&raw, fn_pos);

    // (4) Walk forward to find the `{` that opens the body, skipping the
    //     `(args)` parens and any return type / where clause.
    let body_open = match find_body_open(&raw, fn_pos) {
        Some(p) => p,
        None => {
            // Trait fn / extern fn with no body — return up to the trailing `;`.
            // For our 5 tasks this never fires; keep the path for completeness.
            let semi = raw[fn_pos..].find(';')
                .ok_or(ExtractError::UnbalancedBraces)?;
            let end = fn_pos + semi + 1;
            return Ok(slice(&raw, start, end));
        }
    };

    // (5) Brace-count from body_open until balanced.
    let body_close = match_brace(&raw, body_open)
        .ok_or(ExtractError::UnbalancedBraces)?;

    Ok(slice(&raw, start, body_close + 1))
}

// ── Internal helpers ────────────────────────────────────────────────

struct NameCounter<'a> {
    target: &'a str,
    free: usize,    // free `fn` items
    method: usize,  // `impl X { fn ... }` and trait methods
}

impl<'ast, 'a> Visit<'ast> for NameCounter<'a> {
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if f.sig.ident == self.target { self.free += 1; }
        syn::visit::visit_item_fn(self, f);
    }
    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        if f.sig.ident == self.target { self.method += 1; }
        syn::visit::visit_impl_item_fn(self, f);
    }
    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        if f.sig.ident == self.target { self.method += 1; }
        syn::visit::visit_trait_item_fn(self, f);
    }
}

/// Find the `fn` keyword that introduces the named function. Returns the
/// byte offset of the `f` in `fn`. When `prefer_free` is true and the file
/// has both a free fn and impl methods of the same name, the free one
/// (top-level, brace-depth 0 outside any `impl/trait` block) wins.
fn find_fn_keyword(src: &str, name: &str, prefer_free: bool) -> Option<usize> {
    let needle = format!("fn {name}");
    let bytes = src.as_bytes();

    let mut matches: Vec<(usize, bool)> = Vec::new(); // (pos, is_free)
    let mut i = 0;
    let mut item_depth: i32 = 0; // braces opened by `impl`/`trait` blocks

    while i < bytes.len() {
        let b = bytes[i];

        // Skip strings + comments so the depth counter doesn't see them.
        if b == b'"' {
            if let Some(end) = skip_string(src, i) { i = end; continue; }
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }

        // Track `impl ... {` and `trait ... {` blocks. Anything inside
        // those is a method. Anything outside is a free fn.
        if (starts_keyword(src, i, "impl") || starts_keyword(src, i, "trait"))
            && item_depth == 0
        {
            // Consume up to the next `{` or `;` (forward decl).
            let mut j = i;
            while j < bytes.len() && bytes[j] != b'{' && bytes[j] != b';' { j += 1; }
            if j < bytes.len() && bytes[j] == b'{' {
                item_depth = 1;
                i = j + 1;
                continue;
            }
            i = j + 1;
            continue;
        }

        // Track brace depth INSIDE an impl/trait block.
        if item_depth > 0 {
            if b == b'{' { item_depth += 1; }
            else if b == b'}' { item_depth -= 1; }
        }

        // Detect `fn <name>` matches with proper word boundaries.
        if b == b'f' && src.as_bytes().get(i..i + needle.len()) == Some(needle.as_bytes()) {
            let after = i + needle.len();
            let after_ok = bytes.get(after).is_none_or(|c| !is_ident_byte(*c));
            let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            if after_ok && before_ok {
                matches.push((i, item_depth == 0));
            }
        }
        i += 1;
    }

    if matches.is_empty() { return None; }
    if prefer_free {
        if let Some((p, _)) = matches.iter().find(|(_, free)| *free) {
            return Some(*p);
        }
    }
    Some(matches[0].0)
}

fn starts_keyword(src: &str, i: usize, kw: &str) -> bool {
    let bytes = src.as_bytes();
    if src.as_bytes().get(i..i + kw.len()) != Some(kw.as_bytes()) { return false; }
    let after = i + kw.len();
    let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
    let after_ok = bytes.get(after).is_none_or(|c| !is_ident_byte(*c));
    before_ok && after_ok
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Walk backward from `fn_pos` to capture preceding visibility (`pub`,
/// `pub(crate)`, `pub(super)`, `pub(in path)`) and any `#[...]` attribute
/// blocks attached to this item. Stops at the first blank line or
/// previous item.
fn walk_back_to_item_start(src: &str, fn_pos: usize) -> usize {
    let bytes = src.as_bytes();
    // Find start of the line containing `fn`.
    let mut line_start = fn_pos;
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }

    // From line_start, walk back over preceding lines as long as they
    // are attribute lines (`#[...]`), doc comments (`///`/`//!`), or
    // blank/whitespace continuation. Stop at any other content.
    let mut start = line_start;
    while start > 0 {
        // Find start of previous line.
        let mut prev_end = start - 1; // newline
        let mut prev_start = prev_end;
        while prev_start > 0 && bytes[prev_start - 1] != b'\n' {
            prev_start -= 1;
        }
        let line = &src[prev_start..prev_end];
        let trimmed = line.trim_start();
        let attached =
            trimmed.starts_with("#[")
            || trimmed.starts_with("///")
            || trimmed.starts_with("//!")
            || trimmed.is_empty();
        if !attached { break; }
        if trimmed.is_empty() {
            // Blank line — break the chain. The current item starts after
            // this blank, not before.
            break;
        }
        start = prev_start;
    }
    start
}

/// From `fn_pos` (pointing at `f` in `fn name`), walk forward through the
/// signature and find the byte offset of the `{` that opens the body.
fn find_body_open(src: &str, fn_pos: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = fn_pos;
    // Walk to the first `(` (start of arg list). syn confirmed there is
    // a fn here, so this is well-defined.
    while i < bytes.len() && bytes[i] != b'(' { i += 1; }
    // Match the paren pair.
    let close_paren = match_paren(src, i)?;
    // After the args, find the next `{` that's not inside a `<...>` type
    // arg, string, or comment. For our 5 tasks the next `{` after the
    // arg-list close is the body — keep the simple form.
    let mut j = close_paren + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'{' => return Some(j),
            b';' => return None, // fn declaration only, no body
            _ => {}
        }
        j += 1;
    }
    None
}

/// Match `(` at `open` to its closing `)` with depth tracking that skips
/// string and char literals + line/block comments.
fn match_paren(src: &str, open: usize) -> Option<usize> {
    match_bracketed(src, open, b'(', b')')
}

fn match_brace(src: &str, open: usize) -> Option<usize> {
    match_bracketed(src, open, b'{', b'}')
}

fn match_bracketed(src: &str, open: usize, op: u8, cl: u8) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut depth: i32 = 0;
    let mut i = open;
    while i < bytes.len() {
        let b = bytes[i];

        // Skip string literal "...".
        if b == b'"' {
            i = skip_string(src, i)?;
            continue;
        }
        // Skip char literal '...'.
        if b == b'\'' {
            if let Some(end) = skip_char_literal(src, i) {
                i = end;
                continue;
            }
            // Otherwise it's a lifetime like 'a — let it fall through.
        }
        // Skip line comment to end of line.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        // Skip block comment /* ... */.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }

        if b == op { depth += 1; }
        else if b == cl {
            depth -= 1;
            if depth == 0 { return Some(i); }
        }
        i += 1;
    }
    None
}

/// Given `i` pointing at the opening `"`, return the byte offset just
/// past the closing `"`. Handles `\"` escapes.
fn skip_string(src: &str, i: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2, // skip escaped char (including \")
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// Char literal `'x'` or `'\n'` etc. Returns None if it looks like a
/// lifetime (e.g. `'a` not followed by `'`).
fn skip_char_literal(src: &str, i: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut j = i + 1;
    if j >= bytes.len() { return None; }
    if bytes[j] == b'\\' {
        j += 2;
    } else {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b'\'' {
        return Some(j + 1);
    }
    None
}

/// Quick sanity check that a position isn't inside a string or comment.
/// Linear scan from start of file; OK for the usage pattern (1 call per
/// fn extraction). If extract_fn ever becomes hot, swap for a one-pass
/// tokenizer.
fn is_inside_skip_zone(src: &str, pos: usize) -> bool {
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < pos {
        let b = bytes[i];
        if b == b'"' {
            if let Some(end) = skip_string(src, i) {
                if pos < end { return true; }
                i = end;
                continue;
            }
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            // Line comment
            let line_end = src[i..].find('\n').map(|d| i + d + 1).unwrap_or(bytes.len());
            if pos < line_end { return true; }
            i = line_end;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let mut j = i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                j += 1;
            }
            let block_end = (j + 2).min(bytes.len());
            if pos < block_end { return true; }
            i = block_end;
            continue;
        }
        i += 1;
    }
    false
}

fn slice(src: &str, start: usize, end: usize) -> Extracted {
    let source = src[start..end].to_string();
    let start_line = src[..start].matches('\n').count() + 1;
    let end_line = start_line + source.matches('\n').count();
    Extracted { source, start_line, end_line }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn folkering_root() -> PathBuf {
        // tools/draug-eval-runner → folkering-os
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().parent().unwrap().to_path_buf()
    }

    #[test]
    fn extracts_pop_i32_slot_from_real_source() {
        let path = folkering_root()
            .join("tools/a64-encoder/src/wasm_lower/stack.rs");
        let ex = extract_fn(&path, "pop_i32_slot").expect("extract");
        assert!(ex.source.contains("fn pop_i32_slot"),
            "extracted slice must contain the fn signature");
        assert!(ex.source.contains("StackUnderflow"),
            "extracted slice must contain the body using StackUnderflow");
        assert!(ex.source.trim_end().ends_with('}'),
            "must end with closing brace; got tail = {:?}",
            &ex.source[ex.source.len().saturating_sub(40)..]);
    }

    #[test]
    fn extracts_alloc_pages_kernel_fn() {
        let path = folkering_root()
            .join("kernel/src/memory/physical.rs");
        let ex = extract_fn(&path, "alloc_pages").expect("extract");
        assert!(ex.source.contains("fn alloc_pages"));
        assert!(ex.source.trim_end().ends_with('}'));
    }

    #[test]
    fn returns_not_found_for_typo() {
        let path = folkering_root()
            .join("kernel/src/memory/physical.rs");
        let err = extract_fn(&path, "alocate_pages").unwrap_err(); // typo
        assert!(matches!(err, ExtractError::NotFound(_)),
            "expected NotFound, got {err:?}");
    }
}
