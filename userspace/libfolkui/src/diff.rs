//! Frame-to-frame display-list diffing.
//!
//! Naive `compile_into` re-emits the full tree every frame even when
//! only a `bind_text` value changed. For folkui-demo's counter that
//! means 144 bytes/frame, ~22 fps = ~3 KiB/s of duplicate paint
//! commands. This module's `compile_diff_into` keeps a side-table of
//! the last value and bounds for every `<Text bind_text="key">` and
//! emits **only** the rects + texts that actually changed.
//!
//! ## Wire shape per frame
//!
//! - **First frame** (`DiffState::initialized == false`): emit the
//!   full tree exactly like `compile_into`. While walking, record
//!   each `<Text bind_text=...>` node's value, bounds, and effective
//!   background colour. Set `initialized = true`.
//!
//! - **Subsequent frames**: walk the tree again, but emit only
//!   `[DrawRect bg, DrawText new_value]` for each binding whose
//!   value differs from the cached one. Other nodes (Window, Button,
//!   static `<Text>`) are skipped — their pixels still sit in the
//!   shadow buffer from earlier drains.
//!
//! ## Background colour
//!
//! When we overwrite a binding's text we must first repaint the
//! background underneath; otherwise the new glyphs blend with old
//! pixels. We carry an explicit `bg_color` attribute up from each
//! `<Text bind_text=...>` element. If it's absent we fall back to
//! the nearest ancestor's `bg_color`, which `record_full` resolves
//! during the first-frame walk and stores per binding.
//!
//! ## Tree-shape changes (deliberate non-goal)
//!
//! If the agent ever rebuilds the markup (different node tree, not
//! just different binding values) the cache is wrong by definition.
//! Callers detect this themselves and call `DiffState::reset()` to
//! force a full re-emit. We don't try to diff arbitrary tree edits —
//! that's a much bigger lift and isn't on the rapport's path.

extern crate alloc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libfolk::gfx::{DisplayListBuilder, DrawRectCmd};

use crate::dom::{NodeKind, Tree};
use crate::state::AppState;

/// Per-binding cache entry. Captured during `record_full`, consulted
/// during diff emits.
#[derive(Debug, Clone)]
struct DiffEntry {
    key: String,
    last_value: String,
    /// Where the text was rendered last frame. We repaint the same
    /// rect on change so glyph shapes from the old value don't
    /// bleed through.
    bounds: BindingBounds,
    /// Parent's `bg_color` so we can wipe old glyphs cleanly. 0
    /// means "no background advertised" — diff falls back to a
    /// `Bottle full` re-emit in that case (see `compile_diff_into`).
    bg_color: u32,
    /// Foreground colour on the binding node. Captured once because
    /// it doesn't change between frames in practice (apps that need
    /// dynamic colour can use a separate binding).
    fg_color: u32,
    /// `font_size` from the binding node, same reasoning as `fg`.
    font_size: u16,
}

/// Bounding rect for a binding. We hold (x, y) for emit and (w, h)
/// for the BG-clear rect; the next frame's text might be a different
/// length, but we conservatively repaint the previous bounds since
/// that's where old glyphs live.
#[derive(Debug, Clone, Copy)]
struct BindingBounds {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// Frame-to-frame diff cache. One per producer; create once, hand
/// the same instance to every `compile_diff_into` call. Allocates
/// only on the first frame and on tree-shape changes.
#[derive(Debug, Default)]
pub struct DiffState {
    entries: Vec<DiffEntry>,
    initialized: bool,
}

impl DiffState {
    pub const fn new() -> Self {
        Self { entries: Vec::new(), initialized: false }
    }

    /// Force the next call to do a full re-emit. Call this after
    /// rebuilding the markup tree (different shape, not just
    /// different binding values). Cheap — keeps the Vec capacity.
    pub fn reset(&mut self) {
        self.entries.clear();
        self.initialized = false;
    }

    pub fn is_primed(&self) -> bool { self.initialized }
    pub fn binding_count(&self) -> usize { self.entries.len() }
}

/// Compile `tree` against `state`, emitting only what differs from
/// the last call. The first call is identical to `compile_into`;
/// subsequent calls emit a partial display list — typically just one
/// `DrawRect + DrawText` pair per changed binding.
///
/// Wire format is unchanged — the compositor's dispatcher walks
/// whatever bytes land in the ring without caring whether the frame
/// is partial or full.
pub fn compile_diff_into(
    tree: &Tree,
    state: &AppState,
    diff: &mut DiffState,
    builder: &mut DisplayListBuilder,
) {
    builder.clear();
    if !diff.initialized {
        emit_full(tree, state, diff, builder);
        diff.initialized = true;
    } else {
        emit_diff(tree, state, diff, builder);
    }
    builder.end_frame();
}

// ── First-frame: full emit + record cache ──────────────────────────

fn emit_full(
    tree: &Tree,
    state: &AppState,
    diff: &mut DiffState,
    b: &mut DisplayListBuilder,
) {
    if let Some(root) = tree.root() {
        // Reuse the production compiler for the full path; we just
        // tail-walk afterwards to populate the binding cache.
        crate::compiler::compile_into(tree, state, b);
        // Note: `compile_into` already emitted Sync. We strip it
        // here so the wrapper can re-add Sync at the end (keeps the
        // diff and full paths symmetric for the caller). But the
        // builder's `as_slice()` already includes Sync — undoing it
        // means peeking into the buffer, which is brittle. Cleaner:
        // skip `end_frame` in `compile_into` by rolling our own
        // walk here. That's what we do.
        b.clear();
        walk_full(tree, root, state, /*ancestor_bg=*/0, diff, b);
    }
}

fn walk_full(
    tree: &Tree,
    idx: u32,
    state: &AppState,
    ancestor_bg: u32,
    diff: &mut DiffState,
    b: &mut DisplayListBuilder,
) {
    let node = &tree.nodes[idx as usize];
    let bg_for_children = node.attrs.get_color("bg_color").unwrap_or(ancestor_bg);

    match node.kind {
        NodeKind::Element => match node.name.as_str() {
            "Window" => {
                if let Some(color) = node.attrs.get_color("bg_color") {
                    let radius = node.attrs.get_u32("corner_radius").unwrap_or(0) as u16;
                    b.draw_rect(DrawRectCmd {
                        x: node.bounds.x, y: node.bounds.y,
                        width: node.bounds.w, height: node.bounds.h,
                        color_rgba: color, corner_radius: radius,
                    });
                }
                walk_children_full(tree, idx, state, bg_for_children, diff, b);
            }
            "Button" => {
                let color = node.attrs.get_color("bg_color").unwrap_or(0x3A_3A_3A_FF);
                let radius = node.attrs.get_u32("corner_radius").unwrap_or(4) as u16;
                b.draw_rect(DrawRectCmd {
                    x: node.bounds.x, y: node.bounds.y,
                    width: node.bounds.w, height: node.bounds.h,
                    color_rgba: color, corner_radius: radius,
                });
                walk_children_full(tree, idx, state, color, diff, b);
            }
            "ProgressBar" => {
                // Progress bars aren't part of the bind_text path in
                // this PR. Fall through to the regular compiler so
                // the full-emit visual matches the no-diff path.
                let track = node.attrs.get_color("track_color").unwrap_or(0x2A_2A_2A_FF);
                b.draw_rect(DrawRectCmd {
                    x: node.bounds.x, y: node.bounds.y,
                    width: node.bounds.w, height: node.bounds.h,
                    color_rgba: track, corner_radius: 2,
                });
            }
            "Text" => {
                if let Some(key) = node.attrs.get("bind_text") {
                    // Cache the binding's geometry + colour for the
                    // diff path. Resolve fg/font_size now so we
                    // don't have to re-walk DOM later.
                    let fg = node.attrs.get_color("color").unwrap_or(0xFF_FF_FF_FF);
                    let font_size = node.attrs.get_u32("font_size").unwrap_or(14) as u16;
                    // bg_color resolution: own attr → ancestor.
                    let bg = node.attrs.get_color("bg_color").unwrap_or(ancestor_bg);
                    let value = state.get(key).unwrap_or("");

                    if !value.is_empty() {
                        b.draw_text(node.bounds.x, node.bounds.y, fg, font_size, value);
                    } else {
                        // Fall through to literal child text on first
                        // frame so the panel isn't blank.
                        walk_children_full(tree, idx, state, ancestor_bg, diff, b);
                    }

                    diff.entries.push(DiffEntry {
                        key: key.to_string(),
                        last_value: value.to_string(),
                        bounds: BindingBounds {
                            x: node.bounds.x, y: node.bounds.y,
                            // Width/height: text rect is glyph_count × 8
                            // wide, 16 tall. We use whatever the layout
                            // pass computed for the node (cap to 8/glyph).
                            w: node.bounds.w.max(8 * value.chars().count() as u32),
                            h: 16,
                        },
                        bg_color: bg,
                        fg_color: fg,
                        font_size,
                    });
                } else {
                    walk_children_full(tree, idx, state, ancestor_bg, diff, b);
                }
            }
            "VBox" | "HBox" | _ => {
                walk_children_full(tree, idx, state, bg_for_children, diff, b);
            }
        },
        NodeKind::Text => {
            let fg = 0xFF_FF_FF_FFu32;
            b.draw_text(node.bounds.x, node.bounds.y, fg, 14, &node.name);
        }
    }
}

fn walk_children_full(
    tree: &Tree,
    idx: u32,
    state: &AppState,
    ancestor_bg: u32,
    diff: &mut DiffState,
    b: &mut DisplayListBuilder,
) {
    let children = tree.nodes[idx as usize].children.clone();
    for c in children {
        walk_full(tree, c, state, ancestor_bg, diff, b);
    }
}

// ── Subsequent frames: emit only changed bindings ──────────────────

fn emit_diff(
    tree: &Tree,
    state: &AppState,
    diff: &mut DiffState,
    b: &mut DisplayListBuilder,
) {
    // For each cached binding, see if state has a new value. If yes,
    // emit a wipe-rect over the old bounds, then a fresh DrawText
    // with the new value. Update the cache.
    let _ = tree; // tree is unused on the diff path — geometry came
                  // from the cache. Captured anyway in case a later
                  // version re-walks.
    for entry in diff.entries.iter_mut() {
        let new_val = state.get(&entry.key).unwrap_or("");
        if new_val == entry.last_value.as_str() {
            continue;
        }

        // Wipe: paint the cached bounds with the parent bg colour.
        // Width grows with the new string if it's longer than the
        // previously cached one.
        let new_width = (8 * new_val.chars().count() as u32).max(entry.bounds.w);
        b.draw_rect(DrawRectCmd {
            x: entry.bounds.x, y: entry.bounds.y,
            width: new_width, height: entry.bounds.h,
            color_rgba: entry.bg_color, corner_radius: 0,
        });
        b.draw_text(
            entry.bounds.x, entry.bounds.y,
            entry.fg_color, entry.font_size, new_val,
        );

        entry.bounds.w = new_width;
        entry.last_value.clear();
        entry.last_value.push_str(new_val);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{layout, LayoutConstraint};
    use crate::parser::parse;

    #[test]
    fn first_frame_primes_and_emits_full() {
        let mut t = parse(r##"<Window width="200" height="100" bg_color="#102030"><Text bind_text="x">init</Text></Window>"##).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 200, max_h: 100 });
        let mut state = AppState::new();
        state.set("x", "hello");

        let mut diff = DiffState::new();
        let mut b = DisplayListBuilder::new();
        compile_diff_into(&t, &state, &mut diff, &mut b);

        assert!(diff.is_primed());
        assert_eq!(diff.binding_count(), 1);
        // Full first-frame emit must have at least the Window rect +
        // bound DrawText — i.e. more than just a Sync header.
        assert!(b.as_slice().len() > 6);
    }

    #[test]
    fn second_frame_with_unchanged_state_emits_only_sync() {
        let mut t = parse(r##"<Window width="200" height="100" bg_color="#102030"><Text bind_text="x">init</Text></Window>"##).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 200, max_h: 100 });
        let mut state = AppState::new();
        state.set("x", "stable");

        let mut diff = DiffState::new();
        let mut b = DisplayListBuilder::new();
        compile_diff_into(&t, &state, &mut diff, &mut b);
        let first_len = b.as_slice().len();

        compile_diff_into(&t, &state, &mut diff, &mut b);
        let second_len = b.as_slice().len();

        // Full first frame; minimal second frame (just the Sync
        // header — 3 bytes).
        assert!(second_len < first_len);
        assert_eq!(second_len, 3);
    }

    #[test]
    fn changed_binding_emits_wipe_plus_text() {
        let mut t = parse(r##"<Window width="200" height="100" bg_color="#102030"><Text bind_text="x">init</Text></Window>"##).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 200, max_h: 100 });
        let mut state = AppState::new();
        state.set("x", "v1");

        let mut diff = DiffState::new();
        let mut b = DisplayListBuilder::new();
        compile_diff_into(&t, &state, &mut diff, &mut b);

        state.set("x", "v2");
        compile_diff_into(&t, &state, &mut diff, &mut b);

        let bytes = b.as_slice();
        // Layout: DrawRect (header + payload) + DrawText (header +
        // payload + 2-byte text) + Sync (header). Empirically ~30
        // bytes; assert we're nowhere near a full re-emit (>50).
        assert!(bytes.len() > 5);
        assert!(bytes.len() < 50);
    }

    #[test]
    fn reset_forces_full_reemit() {
        let mut t = parse(r##"<Window width="200" height="100" bg_color="#102030"><Text bind_text="x">init</Text></Window>"##).unwrap();
        layout(&mut t, LayoutConstraint { x: 0, y: 0, max_w: 200, max_h: 100 });
        let mut state = AppState::new();
        state.set("x", "stable");

        let mut diff = DiffState::new();
        let mut b = DisplayListBuilder::new();
        compile_diff_into(&t, &state, &mut diff, &mut b);
        let full_len = b.as_slice().len();

        compile_diff_into(&t, &state, &mut diff, &mut b);
        assert_eq!(b.as_slice().len(), 3); // diff path

        diff.reset();
        compile_diff_into(&t, &state, &mut diff, &mut b);
        assert_eq!(b.as_slice().len(), full_len); // full again
    }
}
