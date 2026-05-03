//! DOM → display-list compiler.
//!
//! Walks the laid-out tree and emits a `libfolk::gfx::DisplayListBuilder`
//! that the producer side of the SPSC ring expects. One pass, depth-first,
//! pre-order: a `<Window>` paints its background first, then children land
//! on top in source order (which is the agent's intent — markup order
//! matches z order). For overlapping siblings the later-defined sibling
//! wins, matching CSS `position: relative` source-order semantics.
//!
//! Element handlers are hard-coded for the small set of tags this PR
//! supports. Adding `<Image>` / `<TextInput>` later means another match
//! arm here, plus probably a `DrawTexture` opcode emission and an input
//! routing follow-up.

extern crate alloc;

use libfolk::gfx::{DisplayListBuilder, DrawRectCmd};

use crate::dom::{NodeKind, Tree};
use crate::state::AppState;

/// Compile `tree` into a builder using an empty state map. Equivalent
/// to `compile_to_display_list_with_state(tree, &AppState::empty())`,
/// kept as a convenience for apps that don't have any bindings.
pub fn compile_to_display_list(tree: &Tree) -> DisplayListBuilder {
    compile_to_display_list_with_state(tree, &AppState::empty())
}

/// Compile `tree` into a builder, resolving `bind_text="key"` on
/// `<Text>` elements against `state`. The builder is returned with an
/// already-appended `Sync` end-of-frame marker.
///
/// Resolution rules for `<Text>`:
/// - If `bind_text` is set AND `state.get(key)` returns `Some(v)` →
///   emit a `DrawText` carrying `v` directly. The element's child
///   text (if any) is ignored.
/// - If `bind_text` is set but the key is absent from `state` → fall
///   back to whatever child text the markup has. Useful for the
///   "first frame before state is populated" case.
/// - If `bind_text` isn't set → emit children as before.
pub fn compile_to_display_list_with_state(tree: &Tree, state: &AppState) -> DisplayListBuilder {
    let mut b = DisplayListBuilder::new();
    compile_into(tree, state, &mut b);
    b
}

/// Reuse-friendly variant: clears `b` and re-fills it with the
/// current frame's display list. Apps that emit a fresh frame each
/// tick should hold onto a single `DisplayListBuilder` and call this
/// instead of `compile_to_display_list_with_state` to avoid
/// allocating a new heap buffer per frame — important for callers on
/// bump allocators that don't deallocate.
pub fn compile_into(tree: &Tree, state: &AppState, b: &mut DisplayListBuilder) {
    b.clear();
    if let Some(root) = tree.root() {
        emit_node(tree, root, state, b);
    }
    b.end_frame();
}

fn emit_node(tree: &Tree, idx: u32, state: &AppState, b: &mut DisplayListBuilder) {
    let node = &tree.nodes[idx as usize];

    match node.kind {
        NodeKind::Element => match node.name.as_str() {
            "Window" => {
                // Background fill from `bg_color`, default black.
                if let Some(color) = node.attrs.get_color("bg_color") {
                    let radius = node.attrs.get_u32("corner_radius").unwrap_or(0) as u16;
                    b.draw_rect(DrawRectCmd {
                        x: node.bounds.x,
                        y: node.bounds.y,
                        width: node.bounds.w,
                        height: node.bounds.h,
                        color_rgba: color,
                        corner_radius: radius,
                    });
                }
                emit_children(tree, idx, state, b);
            }
            "Button" => {
                let color = node.attrs.get_color("bg_color").unwrap_or(0x3A_3A_3A_FF);
                let radius = node.attrs.get_u32("corner_radius").unwrap_or(4) as u16;
                b.draw_rect(DrawRectCmd {
                    x: node.bounds.x,
                    y: node.bounds.y,
                    width: node.bounds.w,
                    height: node.bounds.h,
                    color_rgba: color,
                    corner_radius: radius,
                });
                // Children (typically a `<Text>`) draw on top of the
                // button background.
                emit_children(tree, idx, state, b);
            }
            "ProgressBar" => {
                // Track
                let track = node.attrs.get_color("track_color").unwrap_or(0x2A_2A_2A_FF);
                b.draw_rect(DrawRectCmd {
                    x: node.bounds.x,
                    y: node.bounds.y,
                    width: node.bounds.w,
                    height: node.bounds.h,
                    color_rgba: track,
                    corner_radius: 2,
                });
                // Fill — `value` is 0.0..=1.0; we parse as "0.<digits>" or "1".
                let v = parse_unit(node.attrs.get("value").unwrap_or("0"));
                // Truncate-toward-zero is fine for a progress fill: a
                // sub-pixel difference doesn't matter visually and we
                // avoid pulling libm in for `f64::round` (no_std).
                let fill_w = ((node.bounds.w as f64) * v) as u32;
                if fill_w > 0 {
                    let fill = node.attrs.get_color("fill_color").unwrap_or(0x4A_C0_FF_FF);
                    b.draw_rect(DrawRectCmd {
                        x: node.bounds.x,
                        y: node.bounds.y,
                        width: fill_w,
                        height: node.bounds.h,
                        color_rgba: fill,
                        corner_radius: 2,
                    });
                }
            }
            "Text" => {
                // Reactive binding: `<Text bind_text="key">` resolves
                // against AppState. If the key is present we emit the
                // bound value at this node's own bounds; otherwise we
                // fall through to whatever child text the markup has,
                // so a "first frame before state populated" doesn't
                // produce a blank panel.
                if let Some(key) = node.attrs.get("bind_text") {
                    if let Some(value) = state.get(key) {
                        let color = node.attrs.get_color("color").unwrap_or(0xFF_FF_FF_FF);
                        let font_size = node.attrs.get_u32("font_size").unwrap_or(14) as u16;
                        b.draw_text(node.bounds.x, node.bounds.y, color, font_size, value);
                        return;
                    }
                }
                emit_children(tree, idx, state, b);
            }
            "VBox" | "HBox" => {
                // Layout containers don't paint themselves — they only
                // position children. Pure structural.
                emit_children(tree, idx, state, b);
            }
            _ => {
                // Unknown element: no warning, no draw. Children still
                // render, so a future tag we forgot to handle (`<Card>`,
                // `<Spacer>`) at least stacks layout-wise.
                emit_children(tree, idx, state, b);
            }
        },
        NodeKind::Text => {
            let color = 0xFF_FF_FF_FFu32; // white default; Text colour
                                           // is set on parent <Text>'s
                                           // `color` attr in this PR.
            let font_size = 14u16;
            b.draw_text(
                node.bounds.x,
                node.bounds.y,
                color,
                font_size,
                &node.name,
            );
        }
    }
}

fn emit_children(tree: &Tree, idx: u32, state: &AppState, b: &mut DisplayListBuilder) {
    let children = &tree.nodes[idx as usize].children;
    for &c in children {
        emit_node(tree, c, state, b);
    }
}

/// Parse a value string into 0.0..=1.0. Handles "0", "1", "0.5", and
/// "{progress_ratio}" (binding placeholders return 0.0 — proper
/// reactive resolution is a follow-up).
fn parse_unit(s: &str) -> f64 {
    if s.starts_with('{') { return 0.0; }
    s.parse::<f64>().unwrap_or(0.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{layout, LayoutConstraint};
    use crate::parser::parse;
    use libfolk::gfx::CommandOpCode;

    #[test]
    fn window_emits_bg_rect_then_children() {
        let mut t = parse(r##"<Window width="100" height="100" bg_color="#000000"/>"##).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 100, max_h: 100 });
        let b = compile_to_display_list(&t);
        let bytes = b.as_slice();
        // First opcode = DrawRect, last = Sync.
        assert_eq!(bytes[0], CommandOpCode::DrawRect as u8);
        let n = bytes.len();
        assert_eq!(bytes[n - 3], CommandOpCode::Sync as u8);
    }

    #[test]
    fn button_emits_rect_then_text() {
        let src = r##"<Button bg_color="#3A3A3A">Hi</Button>"##;
        let mut t = parse(src).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 100, max_h: 30 });
        let b = compile_to_display_list(&t);
        let bytes = b.as_slice();
        assert_eq!(bytes[0], CommandOpCode::DrawRect as u8);
        // Find the DrawText opcode after the rect.
        let header_size: usize = 3 + core::mem::size_of::<DrawRectCmd>();
        assert_eq!(bytes[header_size], CommandOpCode::DrawText as u8);
    }

    #[test]
    fn unknown_tag_passes_through_to_children() {
        let mut t = parse(r#"<Frobnicate><Button>X</Button></Frobnicate>"#).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 100, max_h: 30 });
        let b = compile_to_display_list(&t);
        // First emitted command should be the Button's DrawRect, not a
        // panic.
        assert_eq!(b.as_slice()[0], CommandOpCode::DrawRect as u8);
    }
}
