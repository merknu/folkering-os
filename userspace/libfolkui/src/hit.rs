//! Point-to-node hit testing.
//!
//! Apps that received a `MouseClick` from the input pipeline call
//! `hit_test(tree, x, y)` to find the deepest node whose laid-out
//! bounds contain `(x, y)`. The convenience wrapper `hit_test_id`
//! returns that node's `id` attribute, which is what apps usually
//! match against in their click handler:
//!
//! ```ignore
//! match hit_test_id(&tree, ev.x, ev.y) {
//!     Some("btn_add") => state.set("display", "...add..."),
//!     Some("btn_clear") => state.clear(),
//!     _ => {}
//! }
//! ```
//!
//! Hit testing assumes `layout::layout` has already populated
//! `Node::bounds`. Calling it on a tree before layout returns
//! `None` (every node has zero-bounds, which can never contain a
//! valid point).
//!
//! ## Algorithm
//!
//! Depth-first preorder. For each node whose bounds contain
//! `(x, y)`, descend into its children; if any child also matches,
//! that child wins (since children paint on top in source order).
//! Tie-breaking among siblings: later children win, matching the
//! compiler's source-order z-order (the second of two overlapping
//! siblings paints on top).

use crate::dom::Tree;

/// Index of a node in `Tree::nodes`. Same numbering layout::layout
/// uses; stable for the lifetime of the tree.
pub type NodeId = u32;

/// Find the deepest node whose laid-out bounds contain `(x, y)`.
/// Returns `None` if no node matches (point is outside the root)
/// or if layout hasn't been run.
pub fn hit_test(tree: &Tree, x: i32, y: i32) -> Option<NodeId> {
    let root = tree.root()?;
    walk(tree, root, x, y)
}

/// Convenience: hit-test, then look up the matched node's `id`
/// attribute. Returns `None` if no node matched, or the matched
/// node has no `id` attribute set.
pub fn hit_test_id<'a>(tree: &'a Tree, x: i32, y: i32) -> Option<&'a str> {
    let id = hit_test(tree, x, y)?;
    tree.nodes.get(id as usize)?.attrs.get("id")
}

fn walk(tree: &Tree, idx: NodeId, x: i32, y: i32) -> Option<NodeId> {
    let node = tree.nodes.get(idx as usize)?;
    if !contains(node.bounds.x, node.bounds.y, node.bounds.w, node.bounds.h, x, y) {
        return None;
    }
    // Children paint in source order, so the LAST child wins ties.
    // Walk the child list in reverse so the deepest match for the
    // top-most overlapping subtree is found first.
    for &child in node.children.iter().rev() {
        if let Some(deeper) = walk(tree, child, x, y) {
            return Some(deeper);
        }
    }
    Some(idx)
}

#[inline]
fn contains(bx: i32, by: i32, bw: u32, bh: u32, x: i32, y: i32) -> bool {
    if bw == 0 || bh == 0 { return false; }
    x >= bx
        && (x as i64) < bx as i64 + bw as i64
        && y >= by
        && (y as i64) < by as i64 + bh as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{layout, LayoutConstraint};
    use crate::parser::parse;

    fn lay(src: &str, w: u32, h: u32) -> crate::dom::Tree {
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: w, max_h: h });
        t
    }

    #[test]
    fn hits_root_when_inside_and_no_children() {
        let t = lay(r#"<Window width="100" height="100"/>"#, 100, 100);
        assert_eq!(hit_test(&t, 50, 50), Some(0));
        assert_eq!(hit_test(&t, 0, 0), Some(0));
    }

    #[test]
    fn misses_when_outside() {
        let t = lay(r#"<Window width="100" height="100"/>"#, 200, 200);
        assert_eq!(hit_test(&t, 150, 150), None);
        assert_eq!(hit_test(&t, -1, 50), None);
    }

    #[test]
    fn descends_into_children() {
        // VBox with two stacked text labels.
        let src = r#"<VBox padding="0" spacing="0">
                       <Text>A</Text>
                       <Text>B</Text>
                     </VBox>"#;
        let t = lay(src, 200, 200);
        // Top half is child 0 (Text "A"); bottom half is child 1.
        let top = hit_test(&t, 4, 4).unwrap();
        let bottom = hit_test(&t, 4, 150).unwrap();
        assert_ne!(top, bottom);
        // Both should be deeper than the VBox root.
        assert_ne!(top, 0);
        assert_ne!(bottom, 0);
    }

    #[test]
    fn id_lookup_returns_button_id() {
        let src = r##"<HBox padding="0" spacing="0">
                       <Button id="btn_a" width="50" height="50">A</Button>
                       <Button id="btn_b" width="50" height="50">B</Button>
                     </HBox>"##;
        let t = lay(src, 100, 50);
        // Click left button.
        let left = hit_test_id(&t, 25, 25);
        assert_eq!(left, Some("btn_a"));
        // Click right button.
        let right = hit_test_id(&t, 75, 25);
        assert_eq!(right, Some("btn_b"));
    }

    #[test]
    fn returns_none_when_layout_not_run() {
        // Manually parse without layout — bounds stay at default
        // (zero w/h). Hit test should return None for any point.
        let t = parse(r#"<Window width="100" height="100"/>"#).unwrap();
        assert_eq!(hit_test(&t, 50, 50), None);
    }

    #[test]
    fn nested_button_in_vbox_wins_over_outer() {
        let src = r##"<VBox padding="10" spacing="0">
                       <Button id="b" width="50" height="50">X</Button>
                     </VBox>"##;
        let t = lay(src, 200, 200);
        // Click into the button's bounds — should resolve to the
        // Button (deeper match), not the VBox.
        let id = hit_test_id(&t, 30, 30);
        assert_eq!(id, Some("b"));
    }
}
