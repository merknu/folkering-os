//! Single-pass DSML parser.
//!
//! Grammar (intentionally minimal — the agent has to produce this from
//! a model, so every feature we add is a new place to hallucinate):
//!
//! ```text
//! document   ::= ws* element ws*
//! element    ::= '<' name (ws+ attribute)* ws* ('/>' | '>' content '</' name '>')
//! attribute  ::= name '=' '"' value '"'
//! content    ::= (element | text)*
//! name       ::= [A-Za-z_][A-Za-z0-9_-]*
//! ```
//!
//! Things we deliberately don't support (yet):
//! - Single-quoted attributes
//! - HTML-style unquoted attributes
//! - Comments (`<!-- -->`)
//! - CDATA sections
//! - XML namespaces (`xmlns:foo`)
//! - Self-closing tags without preceding whitespace (`<br/>` works,
//!   `<br />` works; nothing else)
//!
//! Errors are intentionally specific so we can include them in agent
//! feedback ("you wrote `<Button onclick`, did you mean `on_click`?").

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::dom::{AttrMap, Node, NodeKind, Tree};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// EOF before we saw a complete document.
    UnexpectedEof,
    /// Saw `<` but no name followed.
    ExpectedTagName { offset: usize },
    /// Closing tag name doesn't match opening tag.
    MismatchedClose { expected: String, found: String, offset: usize },
    /// Attribute syntax broken. Includes raw byte for diagnostics.
    BadAttribute { offset: usize, byte: u8 },
    /// Document had stuff after the root element closed.
    TrailingContent { offset: usize },
}

/// Parse a DSML string into a `Tree`. Returns `Err` on the first issue
/// encountered — there's no recovery; the agent has to fix the markup.
pub fn parse(src: &str) -> Result<Tree, ParseError> {
    let bytes = src.as_bytes();
    let mut p = Parser { src: bytes, pos: 0, tree: Tree::new() };
    p.skip_whitespace();
    let root = p.parse_element()?;
    debug_assert_eq!(root, 0); // first element must be the root
    p.skip_whitespace();
    if p.pos < bytes.len() {
        return Err(ParseError::TrailingContent { offset: p.pos });
    }
    Ok(p.tree)
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    tree: Tree,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_whitespace(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn read_name(&mut self) -> Option<String> {
        let start = self.pos;
        if let Some(b) = self.peek() {
            if !is_name_start(b) { return None; }
        } else {
            return None;
        }
        while let Some(b) = self.peek() {
            if is_name_cont(b) { self.pos += 1; } else { break; }
        }
        // SAFETY: We only advanced past ASCII name bytes, so the slice
        // is valid UTF-8 by construction.
        let s = unsafe { core::str::from_utf8_unchecked(&self.src[start..self.pos]) };
        Some(String::from(s))
    }

    /// Read a `key="value"` attribute. Returns `(key, value)`.
    fn read_attribute(&mut self) -> Result<(String, String), ParseError> {
        let key = self.read_name().ok_or_else(|| ParseError::BadAttribute {
            offset: self.pos,
            byte: self.peek().unwrap_or(0),
        })?;
        if self.bump() != Some(b'=') {
            return Err(ParseError::BadAttribute { offset: self.pos, byte: self.peek().unwrap_or(0) });
        }
        if self.bump() != Some(b'"') {
            return Err(ParseError::BadAttribute { offset: self.pos, byte: self.peek().unwrap_or(0) });
        }
        let val_start = self.pos;
        while let Some(b) = self.peek() {
            if b == b'"' { break; }
            self.pos += 1;
        }
        // SAFETY: attribute values are bounded by `"`; UTF-8 sanity is
        // preserved because we only advance over ASCII boundaries (we
        // don't decode multi-byte sequences). The `from_utf8` check at
        // creation handles validation for non-ASCII bytes.
        let val = match core::str::from_utf8(&self.src[val_start..self.pos]) {
            Ok(s) => String::from(s),
            Err(_) => return Err(ParseError::BadAttribute { offset: val_start, byte: self.src[val_start] }),
        };
        if self.bump() != Some(b'"') {
            return Err(ParseError::UnexpectedEof);
        }
        Ok((key, val))
    }

    /// Parse one element starting at `<`. Returns the index of the
    /// element node in `self.tree.nodes`.
    fn parse_element(&mut self) -> Result<u32, ParseError> {
        if self.bump() != Some(b'<') {
            return Err(ParseError::ExpectedTagName { offset: self.pos });
        }
        let tag = self.read_name().ok_or(ParseError::ExpectedTagName { offset: self.pos })?;

        // Pre-allocate the node so children can reference our index;
        // we backfill children at the end.
        let elem_idx = self.tree.push(Node {
            kind: NodeKind::Element,
            name: tag.clone(),
            attrs: AttrMap::new(),
            children: Vec::new(),
            bounds: Default::default(),
        });

        // Attributes
        let mut attrs = AttrMap::new();
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'/') => {
                    // self-closing
                    self.bump();
                    if self.bump() != Some(b'>') {
                        return Err(ParseError::BadAttribute { offset: self.pos, byte: self.peek().unwrap_or(0) });
                    }
                    self.tree.nodes[elem_idx as usize].attrs = attrs;
                    return Ok(elem_idx);
                }
                Some(b'>') => {
                    self.bump();
                    break;
                }
                Some(b) if is_name_start(b) => {
                    let (k, v) = self.read_attribute()?;
                    attrs.insert(k, v);
                }
                Some(b) => return Err(ParseError::BadAttribute { offset: self.pos, byte: b }),
                None => return Err(ParseError::UnexpectedEof),
            }
        }

        // Children: text or nested elements until the matching </tag>.
        let mut children: Vec<u32> = Vec::new();
        loop {
            // Peek for "</" without consuming.
            if self.starts_with(b"</") {
                self.pos += 2;
                let close_name = self.read_name().ok_or(ParseError::UnexpectedEof)?;
                if close_name != tag {
                    return Err(ParseError::MismatchedClose {
                        expected: tag,
                        found: close_name,
                        offset: self.pos,
                    });
                }
                self.skip_whitespace();
                if self.bump() != Some(b'>') {
                    return Err(ParseError::UnexpectedEof);
                }
                break;
            }
            if self.peek() == Some(b'<') {
                let child = self.parse_element()?;
                children.push(child);
                continue;
            }
            // Otherwise it's text content. Read until next '<'.
            let text_start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'<' { break; }
                self.pos += 1;
            }
            if text_start == self.pos {
                return Err(ParseError::UnexpectedEof);
            }
            let raw = &self.src[text_start..self.pos];
            let trimmed_str = match core::str::from_utf8(raw) {
                Ok(s) => s.trim(),
                Err(_) => return Err(ParseError::BadAttribute { offset: text_start, byte: raw[0] }),
            };
            if !trimmed_str.is_empty() {
                let text_idx = self.tree.push(Node {
                    kind: NodeKind::Text,
                    name: String::from(trimmed_str),
                    attrs: AttrMap::new(),
                    children: Vec::new(),
                    bounds: Default::default(),
                });
                children.push(text_idx);
            }
        }

        // Stitch attributes + children onto the pre-allocated element.
        let node = &mut self.tree.nodes[elem_idx as usize];
        node.attrs = attrs;
        node.children = children;
        Ok(elem_idx)
    }

    fn starts_with(&self, pat: &[u8]) -> bool {
        self.src.get(self.pos..self.pos + pat.len()) == Some(pat)
    }
}

#[inline]
fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

#[inline]
fn is_name_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn parses_self_closing() {
        let t = parse(r#"<Foo bar="baz"/>"#).unwrap();
        assert_eq!(t.nodes.len(), 1);
        assert_eq!(t.nodes[0].name, "Foo");
        assert_eq!(t.nodes[0].attrs.get("bar"), Some("baz"));
    }

    #[test]
    fn parses_nested() {
        let t = parse(r#"<Outer><Inner x="1"/></Outer>"#).unwrap();
        assert_eq!(t.nodes.len(), 2);
        assert_eq!(t.nodes[0].children, alloc::vec![1u32]);
        assert_eq!(t.nodes[1].name, "Inner");
        assert_eq!(t.nodes[1].attrs.get("x"), Some("1"));
    }

    #[test]
    fn parses_text_content() {
        let t = parse(r#"<Button>Click me</Button>"#).unwrap();
        assert_eq!(t.nodes.len(), 2);
        assert_eq!(t.nodes[0].name, "Button");
        assert_eq!(t.nodes[1].kind, NodeKind::Text);
        assert_eq!(t.nodes[1].name, "Click me");
    }

    #[test]
    fn rejects_mismatched_close() {
        let err = parse(r#"<a></b>"#).unwrap_err();
        match err {
            ParseError::MismatchedClose { expected, found, .. } => {
                assert_eq!(expected, "a".to_string());
                assert_eq!(found, "b".to_string());
            }
            _ => panic!("expected MismatchedClose, got {:?}", err),
        }
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(matches!(
            parse(r#"<a/>extra"#).unwrap_err(),
            ParseError::TrailingContent { .. }
        ));
    }

    #[test]
    fn handles_whitespace_around_root() {
        let t = parse("\n\t<Foo/>\n  ").unwrap();
        assert_eq!(t.nodes.len(), 1);
    }

    #[test]
    fn parses_rapport_example() {
        // Full sample from the rapport's Del 4 — this is the format the
        // agent is expected to produce.
        let src = r##"
            <Window width="800" height="600" bg_color="#1E1E1E">
                <VBox padding="20" spacing="10" align="center">
                    <Text font_size="24" color="#FFFFFF" bind_text="status_message"/>
                    <ProgressBar value="0.5"/>
                    <Button id="btn_restart" bg_color="#3A3A3A" on_click="trigger_restart">
                        Restart Neural Core
                    </Button>
                </VBox>
            </Window>"##;
        let t = parse(src).expect("rapport sample must parse");
        assert_eq!(t.nodes[0].name, "Window");
        assert_eq!(t.nodes[0].attrs.get("width"), Some("800"));
        // Walk to the <Button> text and verify it survived trim().
        let vbox = t.nodes[0].children[0];
        let button = t.nodes[vbox as usize].children[2];
        let button_text = t.nodes[button as usize].children[0];
        assert_eq!(t.nodes[button_text as usize].name, "Restart Neural Core");
    }
}
