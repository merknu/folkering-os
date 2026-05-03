//! Front-to-back occlusion culling + damage extraction.
//!
//! After the graph is z-sorted, `compute_occlusion` walks nodes from
//! highest z to lowest and decides:
//!   * whether each lower-z opaque rect *fully covers* a candidate node
//!     (then the candidate is `is_occluded = true` — its display list is
//!     skipped entirely);
//!   * whether the candidate is *partially* covered (then a clip-rect is
//!     injected on top of `clip_regions` so the renderer scissors out the
//!     hidden portion);
//!   * which screen regions are dirty enough to need a flush this frame.
//!
//! The fully-covers test uses `Rect::contains`. Partial-coverage is more
//! subtle: subtracting an axis-aligned rect from another can produce up
//! to four output rects. We do the simple "intersection rect goes into
//! `clip_regions`" form here and let the renderer handle the rest with
//! the existing `damage::DamageTracker` — that keeps the algorithm
//! simple while still producing correct visuals.

extern crate alloc;
use alloc::vec::Vec;

use super::graph::{MinimalRenderGraph, Rect, RenderNode};

/// Simple counter struct so callers can report how many nodes the pass
/// shaved off this frame. Useful for serial logs / TIMING output.
#[derive(Default, Clone, Copy, Debug)]
pub struct OcclusionStats {
    pub fully_occluded: u32,
    pub partially_occluded: u32,
    pub visible: u32,
}

/// Compute occlusion + accumulate damage for a graph that's already been
/// `sort_by_z()`-ed. Idempotent — running it twice produces the same
/// result.
pub fn compute_occlusion(graph: &mut MinimalRenderGraph) -> OcclusionStats {
    // Reset per-node occlusion state from any previous pass.
    for n in graph.nodes.iter_mut() {
        n.is_occluded = false;
        n.clip_regions.clear();
    }
    graph.damage_regions.clear();

    // Front-to-back walk: index the highest z first.
    //
    // For each node we look at every opaque node *above* it (higher z,
    // i.e. later in the sorted Vec since sort is ascending) and check
    // coverage. If any single one fully contains us → fully occluded.
    // Otherwise we accumulate clip rects from each partial coverer.
    let n_nodes = graph.nodes.len();
    let mut stats = OcclusionStats::default();

    for i in 0..n_nodes {
        let bounds_i = graph.nodes[i].global_bounds;

        // Skip nodes entirely off-screen — they contribute no damage and
        // can't be occluded by anything visible.
        let on_screen = match graph.clip_to_screen(bounds_i) {
            Some(r) => r,
            None => {
                graph.nodes[i].is_occluded = true;
                stats.fully_occluded += 1;
                continue;
            }
        };

        let mut fully_covered = false;
        let mut partial_clips: Vec<Rect> = Vec::new();

        for j in (i + 1)..n_nodes {
            let above = &graph.nodes[j];
            if !above.is_opaque { continue; }
            let above_b = above.global_bounds;

            if above_b.contains(&on_screen) {
                fully_covered = true;
                break;
            }
            // Partial: intersection rect is "what gets blocked", so we
            // store it so the renderer can scissor it out. Equivalent
            // formulations exist (subtract → up-to-4 visible rects); we
            // pick the simpler "block list" representation for now.
            if let Some(blocked) = on_screen.intersection(&above_b) {
                partial_clips.push(blocked);
            }
        }

        if fully_covered {
            graph.nodes[i].is_occluded = true;
            stats.fully_occluded += 1;
            continue;
        }

        if !partial_clips.is_empty() {
            graph.nodes[i].clip_regions = partial_clips;
            stats.partially_occluded += 1;
        } else {
            stats.visible += 1;
        }

        // Every visible-or-partially-occluded *dirty* node contributes a
        // damage rect. We use the on-screen-clipped bounds; the
        // `DamageTracker` peer module will coalesce overlapping rects.
        if graph.nodes[i].is_dirty {
            graph.damage_regions.push(on_screen);
        }
    }

    stats
}

/// Coalesce overlapping damage rects in place. Mirrors the shape of
/// `damage::DamageTracker::add_damage` but operates on the graph's
/// internal damage list. After this, callers can iterate
/// `graph.damage_regions` to produce VirtIO-GPU flush commands.
pub fn coalesce_damage(graph: &mut MinimalRenderGraph, max_rects: usize) {
    if graph.damage_regions.len() <= 1 { return; }

    // Repeatedly merge the first overlapping pair we find, until none
    // overlap. O(n²) worst case but n is small (≤ MAX_RECTS in
    // practice, otherwise we collapse).
    'outer: loop {
        for i in 0..graph.damage_regions.len() {
            for j in (i + 1)..graph.damage_regions.len() {
                let a = graph.damage_regions[i];
                let b = graph.damage_regions[j];
                if a.overlaps(&b) {
                    let merged = a.union(&b);
                    graph.damage_regions.swap_remove(j);
                    graph.damage_regions[i] = merged;
                    continue 'outer;
                }
            }
        }
        break;
    }

    // Safety clamp: if we still have too many disjoint rects, collapse
    // to one bounding box. The heuristic mirrors `damage.rs`.
    if graph.damage_regions.len() > max_rects {
        let mut bb = graph.damage_regions[0];
        for r in &graph.damage_regions[1..] {
            bb = bb.union(r);
        }
        graph.damage_regions.clear();
        graph.damage_regions.push(bb);
    }
}

// Helpers for tests — kept in this module since `super::graph` declares
// the test module privately.
#[cfg(test)]
fn make_node(z: i32, bounds: Rect, opaque: bool, dirty: bool) -> RenderNode {
    let dl = Vec::new();
    let mut n = if opaque {
        RenderNode::new_opaque(0, z, bounds, dl)
    } else {
        RenderNode::new_translucent(0, z, bounds, dl)
    };
    n.is_dirty = dirty;
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::graph::MinimalRenderGraph;

    #[test]
    fn fully_covered_window_is_occluded() {
        let mut g = MinimalRenderGraph::new(1024, 768);
        // Big background opaque, small foreground opaque covering it.
        g.add_node(make_node(1, Rect::new(0, 0, 100, 100), true, true));
        g.add_node(make_node(10, Rect::new(0, 0, 200, 200), true, true));
        g.sort_by_z();
        let stats = compute_occlusion(&mut g);
        assert_eq!(stats.fully_occluded, 1);
        assert!(g.nodes[0].is_occluded);
        assert!(!g.nodes[1].is_occluded);
    }

    #[test]
    fn partial_overlap_yields_clip_rect() {
        let mut g = MinimalRenderGraph::new(1024, 768);
        // Lower window 0..100, upper window 50..150.
        g.add_node(make_node(1, Rect::new(0, 0, 100, 100), true, true));
        g.add_node(make_node(10, Rect::new(50, 0, 100, 100), true, true));
        g.sort_by_z();
        let stats = compute_occlusion(&mut g);
        assert_eq!(stats.partially_occluded, 1);
        // The lower-z node should have one clip rect = the overlap region.
        assert_eq!(g.nodes[0].clip_regions.len(), 1);
        assert_eq!(g.nodes[0].clip_regions[0], Rect::new(50, 0, 50, 100));
    }

    #[test]
    fn translucent_top_does_not_occlude() {
        let mut g = MinimalRenderGraph::new(1024, 768);
        g.add_node(make_node(1, Rect::new(0, 0, 100, 100), true, true));
        g.add_node(make_node(10, Rect::new(0, 0, 200, 200), false, true));
        g.sort_by_z();
        let stats = compute_occlusion(&mut g);
        assert_eq!(stats.fully_occluded, 0);
        assert_eq!(stats.visible, 2);
    }

    #[test]
    fn off_screen_node_is_culled() {
        let mut g = MinimalRenderGraph::new(800, 600);
        g.add_node(make_node(1, Rect::new(2000, 2000, 100, 100), true, true));
        g.sort_by_z();
        let stats = compute_occlusion(&mut g);
        assert_eq!(stats.fully_occluded, 1);
    }

    #[test]
    fn dirty_visible_nodes_emit_damage() {
        let mut g = MinimalRenderGraph::new(1024, 768);
        g.add_node(make_node(1, Rect::new(0, 0, 50, 50), true, true));
        g.add_node(make_node(2, Rect::new(100, 100, 50, 50), true, false));
        g.sort_by_z();
        compute_occlusion(&mut g);
        // Only the dirty node should contribute damage.
        assert_eq!(g.damage_regions.len(), 1);
        assert_eq!(g.damage_regions[0], Rect::new(0, 0, 50, 50));
    }

    #[test]
    fn coalesce_merges_adjacent_rects() {
        let mut g = MinimalRenderGraph::new(1024, 768);
        g.damage_regions.push(Rect::new(0, 0, 50, 50));
        g.damage_regions.push(Rect::new(40, 0, 50, 50));
        g.damage_regions.push(Rect::new(500, 500, 10, 10));
        coalesce_damage(&mut g, 10);
        assert_eq!(g.damage_regions.len(), 2);
    }

    #[test]
    fn coalesce_collapses_when_above_max() {
        let mut g = MinimalRenderGraph::new(1024, 768);
        for i in 0..15i32 {
            // Strictly disjoint rects (gap of 5px between them).
            g.damage_regions.push(Rect::new(i * 50, 0, 30, 10));
        }
        coalesce_damage(&mut g, 10);
        // Above the cap → collapsed to one bounding box covering all.
        assert_eq!(g.damage_regions.len(), 1);
    }
}
