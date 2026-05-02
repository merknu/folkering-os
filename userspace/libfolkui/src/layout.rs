//! Minimal layout engine — VBox/HBox stacks with `padding` + `spacing`.
//!
//! Two passes:
//! 1. **Top-down constraint**: parent gives each child a "max width /
//!    height". Today we just propagate the parent's content rect, minus
//!    padding, divided uniformly along the stacking axis. (A real
//!    flexbox would distribute remaining space according to `flex-grow`
//!    weights; we pick equal shares.)
//! 2. **Bottom-up size**: leaves report their intrinsic size (text uses
//!    `font_size` × character count, rects use the explicit `width` /
//!    `height` attrs, default 0). Parents aggregate.
//!
//! For Window-level the parent gives the screen rect. Children of a
//! `<Window>` without VBox/HBox wrapping fall back to overlapping at
//! `(0, 0)` — explicit positions via `x`/`y` attributes override.
//!
//! Outputs land on `Node::bounds`. The compiler reads them.

extern crate alloc;
use alloc::vec::Vec;

use crate::dom::{Bounds, NodeKind, Tree};

#[derive(Debug, Clone, Copy)]
pub struct LayoutConstraint {
    pub x: i32,
    pub y: i32,
    pub max_w: u32,
    pub max_h: u32,
}

/// Run layout on `tree` starting at the root, given an outer
/// constraint (typically the screen rect). Mutates `Node::bounds`
/// for every node in the tree.
pub fn layout(tree: &mut Tree, outer: LayoutConstraint) {
    let Some(root) = tree.root() else { return; };
    layout_node(tree, root, outer);
}

fn layout_node(tree: &mut Tree, idx: u32, outer: LayoutConstraint) {
    let (kind, padding, spacing, explicit_w, explicit_h, explicit_x, explicit_y, name) = {
        let n = &tree.nodes[idx as usize];
        let kind = n.kind.clone();
        let padding = n.attrs.get_u32("padding").unwrap_or(0);
        let spacing = n.attrs.get_u32("spacing").unwrap_or(0);
        let explicit_w = n.attrs.get_u32("width");
        let explicit_h = n.attrs.get_u32("height");
        let explicit_x = n.attrs.get_i32("x");
        let explicit_y = n.attrs.get_i32("y");
        let name = n.name.clone();
        (kind, padding, spacing, explicit_w, explicit_h, explicit_x, explicit_y, name)
    };

    // Determine our outer bounds. Explicit attrs override the
    // constraint; otherwise we fill the constraint.
    let bx = explicit_x.unwrap_or(outer.x);
    let by = explicit_y.unwrap_or(outer.y);
    let bw = explicit_w.unwrap_or(outer.max_w);
    let bh = explicit_h.unwrap_or(outer.max_h);

    // Text leaves: intrinsic size based on font_size + content length.
    if matches!(kind, NodeKind::Text) {
        let approx_glyph_w = 8u32; // matches the existing 8x16 font in compositor
        let approx_glyph_h = 16u32;
        let chars = name.chars().count() as u32;
        tree.nodes[idx as usize].bounds = Bounds {
            x: bx,
            y: by,
            w: chars.saturating_mul(approx_glyph_w).min(bw),
            h: approx_glyph_h.min(bh),
        };
        return;
    }

    tree.nodes[idx as usize].bounds = Bounds { x: bx, y: by, w: bw, h: bh };

    // Stack children for VBox / HBox; otherwise children inherit the
    // content area and stack along Y by default (the simplest sane
    // default — the agent can always wrap in an explicit VBox).
    let stack_axis = match name.as_str() {
        "HBox" => Axis::X,
        _      => Axis::Y, // VBox + everything else
    };

    let inner_x = bx + padding as i32;
    let inner_y = by + padding as i32;
    let inner_w = bw.saturating_sub(padding * 2);
    let inner_h = bh.saturating_sub(padding * 2);

    let children = tree.nodes[idx as usize].children.clone();
    if children.is_empty() {
        return;
    }

    // Equal-share distribution of inner space along the stack axis.
    // Account for spacing between children: total_spacing = (n - 1) × spacing.
    let n = children.len() as u32;
    let total_spacing = if n > 1 { spacing * (n - 1) } else { 0 };

    match stack_axis {
        Axis::Y => {
            let avail = inner_h.saturating_sub(total_spacing);
            let per = avail / n.max(1);
            let mut cursor_y = inner_y;
            for &child in &children {
                let constraint = LayoutConstraint {
                    x: inner_x,
                    y: cursor_y,
                    max_w: inner_w,
                    max_h: per,
                };
                layout_node(tree, child, constraint);
                let used_h = tree.nodes[child as usize].bounds.h;
                cursor_y += core::cmp::max(per, used_h) as i32 + spacing as i32;
            }
        }
        Axis::X => {
            let avail = inner_w.saturating_sub(total_spacing);
            let per = avail / n.max(1);
            let mut cursor_x = inner_x;
            for &child in &children {
                let constraint = LayoutConstraint {
                    x: cursor_x,
                    y: inner_y,
                    max_w: per,
                    max_h: inner_h,
                };
                layout_node(tree, child, constraint);
                let used_w = tree.nodes[child as usize].bounds.w;
                cursor_x += core::cmp::max(per, used_w) as i32 + spacing as i32;
            }
        }
    }

    let _ = (Vec::<u32>::new,); // keep `Vec` import alive if we shrink further
}

#[derive(Clone, Copy)]
enum Axis { X, Y }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[test]
    fn explicit_window_size_propagates() {
        let mut t = parse(r#"<Window width="800" height="600"/>"#).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 1024, max_h: 768 });
        assert_eq!(t.nodes[0].bounds, Bounds { x: 0, y: 0, w: 800, h: 600 });
    }

    #[test]
    fn vbox_stacks_children_vertically() {
        let src = r#"<VBox spacing="10" padding="0">
                       <Text>A</Text>
                       <Text>B</Text>
                     </VBox>"#;
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 200, max_h: 200 });
        // VBox occupies the full constraint; children stack along Y.
        assert_eq!(t.nodes[0].bounds, Bounds { x: 0, y: 0, w: 200, h: 200 });
        // Two children, spacing=10, so each gets (200-10)/2 = 95 max_h.
        let a = &t.nodes[1];
        let b = &t.nodes[2];
        assert!(b.bounds.y > a.bounds.y, "second child should be below first");
    }

    #[test]
    fn hbox_stacks_children_horizontally() {
        let src = r#"<HBox spacing="0" padding="0">
                       <Text>A</Text>
                       <Text>B</Text>
                     </HBox>"#;
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 200, max_h: 100 });
        let a = &t.nodes[1];
        let b = &t.nodes[2];
        assert!(b.bounds.x > a.bounds.x, "second child should be to the right");
        assert_eq!(a.bounds.y, b.bounds.y);
    }

    #[test]
    fn padding_shrinks_child_area() {
        let src = r#"<VBox padding="20"><Text>A</Text></VBox>"#;
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 200, max_h: 200 });
        let child = &t.nodes[1];
        assert_eq!(child.bounds.x, 20);
        assert_eq!(child.bounds.y, 20);
    }
}
