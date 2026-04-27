//! Apply an LLM-produced refactor patch to a target file.
//!
//! Two strategies, auto-detected from the patch contents:
//!
//!   * `Replace` — the patch contains `fn <target_fn>` at item position.
//!     Splice it into the byte range the original fn occupies (computed
//!     by `source_extract`), preserving everything outside that range.
//!
//!   * `Append`  — the patch defines other names (typically a *new*
//!     helper fn alongside the original, like the alloc_pages_with_layout
//!     case). Append above the last closing brace of the file's outermost
//!     `impl` if the original fn lives inside one, otherwise at end of
//!     file. Either way the original fn stays intact — the LLM is adding,
//!     not rewriting.
//!
//! No semantic check here: we don't try to make the patch compile, only
//! to splice it into the source so the cargo-check phase can give a
//! verdict. That separation lets the same applier serve both
//! "human edited the prompt and saved code by hand" and "LLM generated
//! it" workflows.

use std::path::Path;

use crate::source_extract::{self, ExtractError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Replace,
    Append,
}

#[derive(Debug)]
pub enum ApplyError {
    Io(std::io::Error),
    Extract(ExtractError),
    PatchEmpty,
    /// The patch claims to replace `fn <name>` but `<name>` doesn't
    /// match the target — caught here so we don't silently splice an
    /// unrelated fn into the wrong byte range.
    NameMismatch { expected: String, found: String },
    /// Append-mode but couldn't find a sane insertion point.
    NoInsertionPoint,
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Io(e) => write!(f, "io: {e}"),
            ApplyError::Extract(e) => write!(f, "extract: {e}"),
            ApplyError::PatchEmpty => write!(f, "patch is empty"),
            ApplyError::NameMismatch { expected, found } =>
                write!(f, "patch defines fn '{found}' but target is '{expected}'"),
            ApplyError::NoInsertionPoint =>
                write!(f, "couldn't find a sane append insertion point"),
        }
    }
}

impl std::error::Error for ApplyError {}

#[derive(Debug)]
pub struct ApplyResult {
    pub strategy: Strategy,
    /// The full patched file content. Caller writes this to disk in
    /// the sandbox (or unit tests assert on it).
    pub patched: String,
    /// 1-indexed line numbers of the splice/append region in the
    /// PATCHED file. Useful for diff display.
    pub start_line: usize,
    pub end_line: usize,
}

/// Decide strategy by inspecting the patch text. Replace if a top-level
/// `fn <target_fn>` token appears; otherwise append.
pub fn detect_strategy(patch: &str, target_fn: &str) -> Strategy {
    if contains_fn_def(patch, target_fn) {
        Strategy::Replace
    } else {
        Strategy::Append
    }
}

fn contains_fn_def(patch: &str, name: &str) -> bool {
    // Very small needle-search. The prompt instructs the model to emit
    // a single fenced block, so false positives (e.g. `// fn name`) are
    // rare. We tolerate them — `cargo check` is the actual filter.
    let needle = format!("fn {name}");
    let bytes = patch.as_bytes();
    let mut i = 0;
    while let Some(rel) = patch[i..].find(&needle) {
        let pos = i + rel;
        let after = pos + needle.len();
        let before_ok = pos == 0 || !is_ident_byte(bytes[pos - 1]);
        let after_ok = bytes.get(after).is_none_or(|c| !is_ident_byte(*c));
        if before_ok && after_ok { return true; }
        i = pos + 1;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Apply `patch` to the file at `target_file`, replacing or appending
/// according to the detected strategy. Returns the patched content (not
/// written to disk — caller decides where it lands).
pub fn apply(
    target_file: &Path,
    target_fn: &str,
    patch: &str,
) -> Result<ApplyResult, ApplyError> {
    let trimmed = patch.trim();
    if trimmed.is_empty() {
        return Err(ApplyError::PatchEmpty);
    }
    let strategy = detect_strategy(trimmed, target_fn);

    let raw = std::fs::read_to_string(target_file).map_err(ApplyError::Io)?;

    match strategy {
        Strategy::Replace => apply_replace(&raw, target_file, target_fn, trimmed),
        Strategy::Append => apply_append(&raw, target_file, target_fn, trimmed),
    }
}

fn apply_replace(
    raw: &str,
    target_file: &Path,
    target_fn: &str,
    patch: &str,
) -> Result<ApplyResult, ApplyError> {
    let extracted = source_extract::extract_fn(target_file, target_fn)
        .map_err(ApplyError::Extract)?;
    // Find the byte range of `extracted.source` inside `raw` so we can
    // splice. `extract_fn` already validated uniqueness; `find` is OK.
    let start_byte = raw.find(&extracted.source).ok_or_else(|| {
        ApplyError::Extract(ExtractError::NotFound(target_fn.to_string()))
    })?;
    let end_byte = start_byte + extracted.source.len();

    // Indent the patch to match the original's leading whitespace, so
    // the splice doesn't disturb impl-block alignment. We pull the
    // indent from the first line of the original slice.
    let indent = leading_whitespace(&extracted.source);
    let indented_patch = reindent(patch, indent);

    let mut patched = String::with_capacity(raw.len() + patch.len());
    patched.push_str(&raw[..start_byte]);
    patched.push_str(&indented_patch);
    if !indented_patch.ends_with('\n') { patched.push('\n'); }
    patched.push_str(&raw[end_byte..]);

    let start_line = raw[..start_byte].matches('\n').count() + 1;
    let end_line = start_line + indented_patch.matches('\n').count();

    Ok(ApplyResult {
        strategy: Strategy::Replace,
        patched,
        start_line,
        end_line,
    })
}

fn apply_append(
    raw: &str,
    target_file: &Path,
    target_fn: &str,
    patch: &str,
) -> Result<ApplyResult, ApplyError> {
    // Find where the original fn lives so we know whether to append
    // inside its impl block or at end of file.
    let extracted = source_extract::extract_fn(target_file, target_fn)
        .map_err(ApplyError::Extract)?;
    let start_byte = raw.find(&extracted.source).ok_or_else(|| {
        ApplyError::Extract(ExtractError::NotFound(target_fn.to_string()))
    })?;
    let end_byte = start_byte + extracted.source.len();

    // Heuristic: if the byte range falls inside an `impl ... { ... }`,
    // insert just after the original fn, before the next sibling. That
    // keeps the new fn beside its predecessor instead of orphaning it
    // at the end of the file.
    let inside_impl = is_in_impl_block(raw, start_byte);

    let (insert_at, indent) = if inside_impl {
        let after = end_byte;
        // Skip any trailing blank line that already exists.
        let mut p = after;
        let bytes = raw.as_bytes();
        while p < bytes.len() && (bytes[p] == b'\n' || bytes[p] == b' ' || bytes[p] == b'\t') {
            if bytes[p] == b'\n' { p += 1; break; }
            p += 1;
        }
        let indent = leading_whitespace(&extracted.source);
        (p, indent)
    } else {
        // Append at end of file (or just before trailing whitespace).
        let trim_end = raw.trim_end_matches(|c: char| c.is_whitespace()).len();
        (trim_end, "")
    };

    let indented_patch = reindent(patch, indent);
    let mut patched = String::with_capacity(raw.len() + patch.len() + 4);
    patched.push_str(&raw[..insert_at]);
    if !patched.ends_with('\n') { patched.push('\n'); }
    patched.push('\n');
    patched.push_str(&indented_patch);
    if !indented_patch.ends_with('\n') { patched.push('\n'); }
    patched.push_str(&raw[insert_at..]);

    let start_line = raw[..insert_at].matches('\n').count() + 1;
    let end_line = start_line + indented_patch.matches('\n').count();

    Ok(ApplyResult {
        strategy: Strategy::Append,
        patched,
        start_line,
        end_line,
    })
}

fn leading_whitespace(s: &str) -> &str {
    let line = s.lines().next().unwrap_or("");
    let trimmed = line.trim_start();
    &line[..line.len() - trimmed.len()]
}

fn reindent(patch: &str, indent: &str) -> String {
    if indent.is_empty() { return patch.to_string(); }
    let mut out = String::with_capacity(patch.len() + indent.len() * 8);
    for (i, line) in patch.lines().enumerate() {
        if i > 0 { out.push('\n'); }
        if !line.is_empty() {
            out.push_str(indent);
        }
        out.push_str(line);
    }
    if patch.ends_with('\n') { out.push('\n'); }
    out
}

/// Cheap "are we inside an impl block at byte `pos`?" check. Walks
/// from start of file tracking impl-block depth.
fn is_in_impl_block(src: &str, pos: usize) -> bool {
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut depth: i32 = 0;
    while i < pos.min(bytes.len()) {
        let b = bytes[i];
        if b == b'"' {
            // Skip string literal.
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' { i += 2; continue; }
                if bytes[i] == b'"' { i += 1; break; }
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') { i += 1; }
            i += 2;
            continue;
        }
        if (kw_matches(src, i, "impl") || kw_matches(src, i, "trait")) && depth == 0 {
            // Walk to next `{` or `;`.
            let mut j = i;
            while j < bytes.len() && bytes[j] != b'{' && bytes[j] != b';' { j += 1; }
            if j < bytes.len() && bytes[j] == b'{' {
                depth = 1;
                i = j + 1;
                continue;
            }
            i = j + 1;
            continue;
        }
        if depth > 0 {
            if b == b'{' { depth += 1; }
            else if b == b'}' { depth -= 1; }
        }
        i += 1;
    }
    depth > 0
}

fn kw_matches(src: &str, i: usize, kw: &str) -> bool {
    let bytes = src.as_bytes();
    if src.as_bytes().get(i..i + kw.len()) != Some(kw.as_bytes()) { return false; }
    let after = i + kw.len();
    let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
    let after_ok = bytes.get(after).is_none_or(|c| !is_ident_byte(*c));
    before_ok && after_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("draug-apply-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Unique filename per test so parallel runs don't clobber.
        let path = dir.join(format!("test_{}.rs", rand_suffix()));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn rand_suffix() -> u64 {
        // No rand crate — use the system clock.
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
    }

    #[test]
    fn detect_strategy_picks_replace_when_fn_named() {
        assert_eq!(
            detect_strategy("fn foo() { 42 }", "foo"),
            Strategy::Replace
        );
        assert_eq!(
            detect_strategy("pub fn foo(x: i32) -> i32 { x }", "foo"),
            Strategy::Replace
        );
    }

    #[test]
    fn detect_strategy_picks_append_when_other_name() {
        assert_eq!(
            detect_strategy("fn helper() {}\nfn other_thing() {}", "foo"),
            Strategy::Append,
        );
    }

    #[test]
    fn replace_swaps_target_fn_byte_range() {
        let original = "\
fn unrelated() { 1 }

fn foo() {
    panic!()
}

fn after() { 2 }
";
        let target = write_temp(original);
        let patch = "fn foo() -> i32 { 42 }";
        let result = apply(&target, "foo", patch).expect("apply");
        assert_eq!(result.strategy, Strategy::Replace);
        assert!(result.patched.contains("fn foo() -> i32 { 42 }"));
        assert!(!result.patched.contains("panic!()"));
        // Surrounding fns intact.
        assert!(result.patched.contains("fn unrelated()"));
        assert!(result.patched.contains("fn after()"));
        let _ = std::fs::remove_file(&target);
    }

    #[test]
    fn append_mode_keeps_target_intact() {
        let original = "\
fn unrelated() { 1 }

fn foo() { 1 }

fn after() { 2 }
";
        let target = write_temp(original);
        let patch = "fn foo_with_layout(size: usize) -> i32 { 42 }";
        let result = apply(&target, "foo", patch).expect("apply");
        assert_eq!(result.strategy, Strategy::Append);
        assert!(result.patched.contains("fn foo() { 1 }"),
            "original foo must survive — got {:?}", result.patched);
        assert!(result.patched.contains("fn foo_with_layout"));
        let _ = std::fs::remove_file(&target);
    }

    #[test]
    fn replace_inside_impl_preserves_indent() {
        let original = "\
struct S;
impl S {
    fn foo(&self) -> i32 { 1 }
}
";
        let target = write_temp(original);
        let patch = "fn foo(&self) -> i32 { 42 }";
        let result = apply(&target, "foo", patch).expect("apply");
        assert_eq!(result.strategy, Strategy::Replace);
        // Indent must be preserved (4 spaces).
        assert!(
            result.patched.contains("    fn foo(&self) -> i32 { 42 }"),
            "patched file must keep impl-block indent; got\n{}", result.patched,
        );
        let _ = std::fs::remove_file(&target);
    }

    #[test]
    fn append_inside_impl_lands_in_block_not_eof() {
        let original = "\
struct S;
impl S {
    fn foo(&self) -> i32 { 1 }
}

fn at_end() {}
";
        let target = write_temp(original);
        let patch = "fn foo_v2(&self) -> i32 { 2 }";
        let result = apply(&target, "foo", patch).expect("apply");
        assert_eq!(result.strategy, Strategy::Append);
        // The new fn should appear before the `}` that closes impl S,
        // not after `fn at_end`.
        let foo_v2_pos = result.patched.find("foo_v2").expect("present");
        let at_end_pos = result.patched.find("fn at_end").expect("present");
        assert!(foo_v2_pos < at_end_pos,
            "foo_v2 should land in impl block (before fn at_end); got\n{}", result.patched);
        let _ = std::fs::remove_file(&target);
    }
}
