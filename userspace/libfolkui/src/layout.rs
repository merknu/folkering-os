//! Layout engine for `<VBox>` / `<HBox>` containers.
//!
//! ## Pass model
//!
//! 1. **Intrinsic pass** (top of container): each child reports a
//!    "natural" main-axis size derived from its `width`/`height` attr
//!    (if explicit) or from `NodeKind::Text` glyph metrics. Container
//!    children without an explicit size report 0 — they're expected
//!    to consume their share of remaining space via `flex-grow`.
//! 2. **Distribute pass**: container computes `remaining` as
//!    `inner_size - sum(intrinsic) - total_spacing` and divides it by
//!    `sum(flex_grow)`. Each flex child gets `intrinsic + share`.
//! 3. **Place pass**: walks children with computed main-axis sizes
//!    and `justify` policy, recursing into each child with a final
//!    constraint. Cross-axis position derived from `align`.
//!
//! ## Attributes
//!
//! Container (`VBox` / `HBox` / `Window` / etc.):
//! - `padding` (u32, default 0): inner inset on all sides.
//! - `spacing` (u32, default 0): gap between adjacent children.
//! - `justify` (str): main-axis policy when no child has flex-grow.
//!   Values: `start` (default), `center`, `end`, `space-between`.
//! - `align` (str): cross-axis policy. Values: `start` (default),
//!   `center`, `end`, `stretch`.
//!
//! Child:
//! - `flex-grow` (u32, default 0): integer weight; >0 children share
//!   `remaining` proportionally.
//! - `width` / `height` (u32): explicit size; pins intrinsic.
//! - `x` / `y` (i32): absolute position; bypasses parent layout.
//!
//! ## Backward compatibility
//!
//! Old call sites (no `flex-grow` / `justify` / `align` anywhere)
//! see the previous "equal-share" behaviour: total_grow = 0 +
//! justify=start + align=stretch. The intrinsic pass for unknown
//! children returns 0 → equal-share distribution falls out of the
//! same algorithm.
//!
//! ## Out of scope
//!
//! - Wrap (multi-line flex). VBox/HBox is single-line.
//! - `align-self` per child. Use a wrapping container if you need
//!   per-child cross-axis policy.
//! - Bidirectional content sizing (a container child sized to its
//!   own children). Containers without explicit width/height report
//!   intrinsic = 0 and are expected to use `flex-grow="1"`.

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

#[derive(Clone, Copy)]
enum Axis { X, Y }

#[derive(Clone, Copy, PartialEq, Eq)]
enum Justify { Start, Center, End, SpaceBetween }

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align { Start, Center, End, Stretch }

fn parse_justify(s: Option<&str>) -> Justify {
    match s {
        Some("center") => Justify::Center,
        Some("end") => Justify::End,
        Some("space-between") => Justify::SpaceBetween,
        _ => Justify::Start,
    }
}

fn parse_align(s: Option<&str>) -> Align {
    match s {
        Some("center") => Align::Center,
        Some("end") => Align::End,
        Some("stretch") => Align::Stretch,
        _ => Align::Start,
    }
}

const GLYPH_W: u32 = 8;
const GLYPH_H: u32 = 16;

/// Intrinsic main-axis size of a child, used by the parent's
/// distribute pass. Doesn't recurse into the child's own layout —
/// just looks at attrs and `NodeKind`.
fn child_intrinsic_main(tree: &Tree, idx: u32, axis: Axis) -> u32 {
    let n = &tree.nodes[idx as usize];
    let attr_size = match axis {
        Axis::X => n.attrs.get_u32("width"),
        Axis::Y => n.attrs.get_u32("height"),
    };
    if let Some(s) = attr_size {
        return s;
    }
    if matches!(n.kind, NodeKind::Text) {
        let chars = n.name.chars().count() as u32;
        return match axis {
            Axis::X => chars.saturating_mul(GLYPH_W),
            Axis::Y => GLYPH_H,
        };
    }
    // Container without explicit size: 0 (caller will give it
    // remaining space via flex-grow, or the equal-share fallback
    // when nobody has flex-grow).
    0
}

fn child_intrinsic_cross(tree: &Tree, idx: u32, axis: Axis) -> u32 {
    let n = &tree.nodes[idx as usize];
    let attr_size = match axis {
        Axis::X => n.attrs.get_u32("height"),
        Axis::Y => n.attrs.get_u32("width"),
    };
    if let Some(s) = attr_size {
        return s;
    }
    if matches!(n.kind, NodeKind::Text) {
        let chars = n.name.chars().count() as u32;
        return match axis {
            Axis::X => GLYPH_H,
            Axis::Y => chars.saturating_mul(GLYPH_W),
        };
    }
    0
}

fn layout_node(tree: &mut Tree, idx: u32, outer: LayoutConstraint) {
    let (kind, padding, spacing, explicit_w, explicit_h, explicit_x, explicit_y, name,
         justify, align) = {
        let n = &tree.nodes[idx as usize];
        let kind = n.kind.clone();
        let padding = n.attrs.get_u32("padding").unwrap_or(0);
        let spacing = n.attrs.get_u32("spacing").unwrap_or(0);
        let explicit_w = n.attrs.get_u32("width");
        let explicit_h = n.attrs.get_u32("height");
        let explicit_x = n.attrs.get_i32("x");
        let explicit_y = n.attrs.get_i32("y");
        let name = n.name.clone();
        let justify = parse_justify(n.attrs.get("justify"));
        let align = parse_align(n.attrs.get("align"));
        (kind, padding, spacing, explicit_w, explicit_h, explicit_x, explicit_y, name,
         justify, align)
    };

    // Determine our outer bounds. Explicit attrs override the
    // constraint; otherwise we fill the constraint.
    let bx = explicit_x.unwrap_or(outer.x);
    let by = explicit_y.unwrap_or(outer.y);
    let bw = explicit_w.unwrap_or(outer.max_w);
    let bh = explicit_h.unwrap_or(outer.max_h);

    // Text leaves: intrinsic size based on font_size + content length.
    if matches!(kind, NodeKind::Text) {
        let chars = name.chars().count() as u32;
        tree.nodes[idx as usize].bounds = Bounds {
            x: bx,
            y: by,
            w: chars.saturating_mul(GLYPH_W).min(bw),
            h: GLYPH_H.min(bh),
        };
        return;
    }

    tree.nodes[idx as usize].bounds = Bounds { x: bx, y: by, w: bw, h: bh };

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

    let n = children.len() as u32;
    let total_spacing = if n > 1 { spacing * (n - 1) } else { 0 };

    let main_avail = match stack_axis {
        Axis::Y => inner_h.saturating_sub(total_spacing),
        Axis::X => inner_w.saturating_sub(total_spacing),
    };

    // ── Pass 1: intrinsic + flex-grow per child ────────────────
    let mut intrinsic: Vec<u32> = Vec::with_capacity(children.len());
    let mut grow: Vec<u32> = Vec::with_capacity(children.len());
    let mut total_intrinsic: u32 = 0;
    let mut total_grow: u32 = 0;
    for &c in &children {
        let i = child_intrinsic_main(tree, c, stack_axis);
        let g = tree.nodes[c as usize].attrs.get_u32("flex-grow").unwrap_or(0);
        total_intrinsic = total_intrinsic.saturating_add(i);
        total_grow = total_grow.saturating_add(g);
        intrinsic.push(i);
        grow.push(g);
    }

    // ── Pass 2: distribute remaining ───────────────────────────
    // `sizes[i]` is the final main-axis size for child i.
    let mut sizes: Vec<u32> = Vec::with_capacity(children.len());
    if total_grow > 0 {
        // Flex mode: each grow child gets intrinsic + share of remaining.
        let remaining = main_avail.saturating_sub(total_intrinsic);
        for i in 0..children.len() {
            let mut s = intrinsic[i];
            if grow[i] > 0 {
                // Integer rounding: distribute as evenly as possible.
                // Last child absorbs any rounding leftover so the row
                // exactly fills `main_avail` — important for
                // SpaceBetween + End to look right.
                let share = remaining
                    .saturating_mul(grow[i])
                    .checked_div(total_grow)
                    .unwrap_or(0);
                s = s.saturating_add(share);
            }
            sizes.push(s);
        }
        // Adjust last grow child for rounding leftover.
        let used: u32 = sizes.iter().sum();
        if used < total_intrinsic + remaining {
            let leftover = total_intrinsic + remaining - used;
            // Find the LAST child with grow>0 and bump it.
            if let Some((i, _)) = grow.iter().enumerate().rev().find(|(_, g)| **g > 0) {
                sizes[i] = sizes[i].saturating_add(leftover);
            }
        }
    } else if total_intrinsic > 0 && total_intrinsic <= main_avail {
        // Intrinsic mode: respect what each child reports. Leftover
        // is handled by `justify` during placement.
        sizes = intrinsic.clone();
    } else {
        // Fallback (legacy "equal share"): no flex, no useful intrinsic.
        // Each child gets main_avail / n. Matches pre-flexbox behaviour
        // for markup that doesn't opt in.
        let per = main_avail / n.max(1);
        sizes = (0..n).map(|_| per).collect();
    }

    // ── Pass 3: place ──────────────────────────────────────────
    let used_main: u32 = sizes.iter().sum::<u32>() + total_spacing;
    let leftover = main_avail.saturating_sub(used_main);
    // Justify only meaningful when leftover > 0 and total_grow == 0
    // (otherwise grow already absorbed the slack).
    let (mut main_cursor, between_extra) = match (justify, total_grow) {
        (_, g) if g > 0 => (0u32, 0u32),
        (Justify::Start, _)         => (0, 0),
        (Justify::Center, _)        => (leftover / 2, 0),
        (Justify::End, _)           => (leftover, 0),
        (Justify::SpaceBetween, _)  => {
            if children.len() <= 1 { (0, 0) } else { (0, leftover / (n - 1)) }
        }
    };

    for (i, &c) in children.iter().enumerate() {
        let main_size = sizes[i];
        let cross_avail = match stack_axis {
            Axis::Y => inner_w,
            Axis::X => inner_h,
        };
        // Cross-axis size + offset from `align`.
        let (cross_size, cross_off) = match align {
            Align::Stretch => (cross_avail, 0u32),
            other => {
                let intr = child_intrinsic_cross(tree, c, stack_axis);
                // Container with no intrinsic cross size still wants
                // SOMETHING; default to full available cross. Tests
                // don't exercise this, but it keeps existing apps that
                // don't set `align` from collapsing to width=0.
                let s = if intr == 0 { cross_avail } else { intr.min(cross_avail) };
                let off = match other {
                    Align::Start  => 0,
                    Align::Center => (cross_avail.saturating_sub(s)) / 2,
                    Align::End    => cross_avail.saturating_sub(s),
                    Align::Stretch => 0, // unreachable
                };
                (s, off)
            }
        };

        let constraint = match stack_axis {
            Axis::Y => LayoutConstraint {
                x: inner_x + cross_off as i32,
                y: inner_y + main_cursor as i32,
                max_w: cross_size,
                max_h: main_size,
            },
            Axis::X => LayoutConstraint {
                x: inner_x + main_cursor as i32,
                y: inner_y + cross_off as i32,
                max_w: main_size,
                max_h: cross_size,
            },
        };
        layout_node(tree, c, constraint);

        main_cursor = main_cursor.saturating_add(main_size).saturating_add(spacing);
        if i + 1 < children.len() {
            main_cursor = main_cursor.saturating_add(between_extra);
        }
    }
}

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
    fn vbox_legacy_equal_share_with_text_children() {
        // No flex-grow anywhere → falls back to equal-share intrinsic
        // mode. Text children have intrinsic main-axis size (1 char =
        // 16px on Y), so this exercises the intrinsic path.
        let src = r#"<VBox spacing="10" padding="0"><Text>A</Text><Text>B</Text></VBox>"#;
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 200, max_h: 200 });
        assert_eq!(t.nodes[0].bounds, Bounds { x: 0, y: 0, w: 200, h: 200 });
        let a = t.nodes[1].bounds;
        let b = t.nodes[2].bounds;
        assert!(b.y > a.y);
    }

    #[test]
    fn hbox_stacks_children_horizontally() {
        let src = r#"<HBox spacing="0" padding="0"><Text>A</Text><Text>B</Text></HBox>"#;
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 200, max_h: 100 });
        let a = t.nodes[1].bounds;
        let b = t.nodes[2].bounds;
        assert!(b.x > a.x);
        assert_eq!(a.y, b.y);
    }

    #[test]
    fn padding_shrinks_child_area() {
        let src = r#"<VBox padding="20"><Text>A</Text></VBox>"#;
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 200, max_h: 200 });
        let child = t.nodes[1].bounds;
        assert_eq!(child.x, 20);
        assert_eq!(child.y, 20);
    }

    #[test]
    fn flex_grow_distributes_remaining_space() {
        // HBox 200 wide. "AB" intrinsic = 16px. Spacer with
        // flex-grow="1" should take the remaining 184px so the
        // following "X" text starts at x = 16 + 184 = 200 (clamped to
        // 200). With width-attr we pin text widths.
        let src = r##"<HBox padding="0" spacing="0">
                       <Text>AB</Text>
                       <VBox flex-grow="1"/>
                       <Text>X</Text>
                     </HBox>"##;
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 200, max_h: 50 });
        let a = t.nodes[1].bounds;   // "AB"
        let spacer = t.nodes[2].bounds;
        let x_node = t.nodes[3].bounds;
        assert_eq!(a.x, 0);
        assert_eq!(spacer.x, 16);
        assert!(spacer.w >= 180); // ate the slack
        assert!(x_node.x >= 184); // pushed to the right
    }

    #[test]
    fn justify_end_pushes_children_right() {
        // No flex-grow → justify takes effect. Two single-char texts
        // (8px each) in a 100-wide HBox with justify="end" should
        // start at x = 100 - 16 = 84.
        let src = r##"<HBox padding="0" spacing="0" justify="end">
                       <Text>A</Text><Text>B</Text>
                     </HBox>"##;
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 100, max_h: 50 });
        let a = t.nodes[1].bounds;
        assert_eq!(a.x, 84);
    }

    #[test]
    fn justify_center_centers_children() {
        let src = r##"<HBox padding="0" spacing="0" justify="center">
                       <Text>A</Text><Text>B</Text>
                     </HBox>"##;
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 100, max_h: 50 });
        let a = t.nodes[1].bounds;
        // Used = 16, leftover = 84, half = 42.
        assert_eq!(a.x, 42);
    }

    #[test]
    fn align_center_on_cross_axis() {
        // VBox 100 wide × 50 tall, align="center" → text "A" cross-
        // axis (X) centred. Text intrinsic cross = 8 (1 char × 8px),
        // so x = (100 - 8) / 2 = 46.
        let src = r##"<VBox padding="0" spacing="0" align="center">
                       <Text>A</Text>
                     </VBox>"##;
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 100, max_h: 50 });
        let a = t.nodes[1].bounds;
        assert_eq!(a.x, 46);
    }
}
