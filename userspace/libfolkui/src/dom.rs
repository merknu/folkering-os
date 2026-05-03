//! DOM types — flat `Vec<Node>` with index-based parent/child links.
//!
//! Why flat instead of a heap-allocated tree of `Box<Node>`: an arena
//! representation means we can `Vec::clear()` between frames without
//! triggering thousands of individual deallocations. The agent rebuilds
//! the markup; we rebuild the tree; both share the same allocator
//! capacity.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// `<Window>`, `<VBox>`, `<HBox>`, `<Button>`, `<ProgressBar>`, …
    /// We keep tag names case-sensitive (camelCase or PascalCase) so the
    /// agent gets a hard error on typos rather than silent fallbacks.
    Element,
    /// Plain text inside an element (e.g. the "Restart Neural Core"
    /// label inside a `<Button>`).
    Text,
}

/// One DOM node. Variable-length fields go into the parent `Tree`'s
/// per-node `attrs` and `children` vectors so the node itself stays
/// small and `Copy`-able.
#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    /// Tag name (`Element`) or text content (`Text`).
    pub name: String,
    /// Index into `Tree::attrs`. Empty if no attributes.
    pub attrs: AttrMap,
    /// Indices of child nodes inside the parent `Tree::nodes`.
    pub children: Vec<u32>,
    /// Computed bounds after `layout::layout`. `(0,0,0,0)` means
    /// "layout hasn't been run yet" and the compiler will skip drawing.
    pub bounds: Bounds,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Tiny string-keyed map. We use a `Vec<(String, String)>` rather than
/// a hashmap because (a) attribute counts are tiny (~3-5) so linear scan
/// wins, (b) BTreeMap pulls in alloc::collections which we'd rather
/// keep optional, and (c) ordering is preserved for diagnostics.
#[derive(Debug, Clone, Default)]
pub struct AttrMap(pub Vec<(String, String)>);

impl AttrMap {
    pub fn new() -> Self { Self(Vec::new()) }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find_map(|(k, v)| if k == key { Some(v.as_str()) } else { None })
    }

    /// Parse a `#RRGGBB` or `#RRGGBBAA` color attribute. Returns the
    /// 0xRRGGBBAA u32 expected by `DrawRectCmd::color_rgba` — alpha
    /// defaults to 0xFF when omitted. Returns `None` on malformed
    /// input rather than panicking; the compiler treats that as
    /// "fall back to default color" so a typo doesn't crash the app.
    pub fn get_color(&self, key: &str) -> Option<u32> {
        let v = self.get(key)?;
        let v = v.strip_prefix('#')?;
        match v.len() {
            6 => {
                let r = u8::from_str_radix(&v[0..2], 16).ok()?;
                let g = u8::from_str_radix(&v[2..4], 16).ok()?;
                let b = u8::from_str_radix(&v[4..6], 16).ok()?;
                Some(((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | 0xFF)
            }
            8 => {
                let r = u8::from_str_radix(&v[0..2], 16).ok()?;
                let g = u8::from_str_radix(&v[2..4], 16).ok()?;
                let b = u8::from_str_radix(&v[4..6], 16).ok()?;
                let a = u8::from_str_radix(&v[6..8], 16).ok()?;
                Some(((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | a as u32)
            }
            _ => None,
        }
    }

    pub fn get_u32(&self, key: &str) -> Option<u32> {
        self.get(key).and_then(|s| s.parse().ok())
    }

    pub fn get_i32(&self, key: &str) -> Option<i32> {
        self.get(key).and_then(|s| s.parse().ok())
    }

    pub fn insert(&mut self, key: String, value: String) {
        // Last write wins, matching XML semantics for repeated attrs.
        if let Some(slot) = self.0.iter_mut().find(|(k, _)| k == &key) {
            slot.1 = value;
        } else {
            self.0.push((key, value));
        }
    }
}

/// The whole document. `nodes[0]` is always the root.
#[derive(Debug, Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
}

impl Tree {
    pub fn new() -> Self { Self { nodes: Vec::new() } }

    /// Get the root node index. `parser::parse` guarantees there is
    /// always at least one node when it returns `Ok`.
    pub fn root(&self) -> Option<u32> {
        if self.nodes.is_empty() { None } else { Some(0) }
    }

    pub fn node(&self, idx: u32) -> Option<&Node> {
        self.nodes.get(idx as usize)
    }

    pub fn node_mut(&mut self, idx: u32) -> Option<&mut Node> {
        self.nodes.get_mut(idx as usize)
    }

    /// Push a node and return its index. Caller wires up the parent's
    /// `children` separately.
    pub fn push(&mut self, n: Node) -> u32 {
        let idx = self.nodes.len() as u32;
        self.nodes.push(n);
        idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn attr_color_six_digit() {
        let mut a = AttrMap::new();
        a.insert("c".to_string(), "#1A2B3C".to_string());
        assert_eq!(a.get_color("c"), Some(0x1A_2B_3C_FFu32));
    }

    #[test]
    fn attr_color_eight_digit_includes_alpha() {
        let mut a = AttrMap::new();
        a.insert("c".to_string(), "#12345678".to_string());
        assert_eq!(a.get_color("c"), Some(0x12_34_56_78u32));
    }

    #[test]
    fn attr_color_malformed_returns_none() {
        let mut a = AttrMap::new();
        a.insert("c".to_string(), "rebeccapurple".to_string());
        assert!(a.get_color("c").is_none());
    }

    #[test]
    fn attr_insert_overwrite() {
        let mut a = AttrMap::new();
        a.insert("k".to_string(), "1".to_string());
        a.insert("k".to_string(), "2".to_string());
        assert_eq!(a.get("k"), Some("2"));
        assert_eq!(a.0.len(), 1);
    }
}
