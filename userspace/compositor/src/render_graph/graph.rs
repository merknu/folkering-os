//! Core Render Graph data types.

extern crate alloc;
use alloc::vec::Vec;

/// Signed rectangle so a node can sit (partially) off-screen without
/// distorting bounding-box math.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Right edge (exclusive) as i64 to avoid overflow at INT_MAX edges.
    #[inline]
    pub fn right(&self) -> i64 { self.x as i64 + self.w as i64 }
    #[inline]
    pub fn bottom(&self) -> i64 { self.y as i64 + self.h as i64 }

    /// AABB intersection.
    #[inline]
    pub fn overlaps(&self, other: &Rect) -> bool {
        (self.x as i64) < other.right()
            && self.right() > other.x as i64
            && (self.y as i64) < other.bottom()
            && self.bottom() > other.y as i64
    }

    /// `self` fully covers `inner` (boundary inclusive).
    #[inline]
    pub fn contains(&self, inner: &Rect) -> bool {
        self.x as i64 <= inner.x as i64
            && self.y as i64 <= inner.y as i64
            && self.right() >= inner.right()
            && self.bottom() >= inner.bottom()
    }

    /// Geometric union — smallest rect that covers both.
    pub fn union(&self, other: &Rect) -> Rect {
        let x = core::cmp::min(self.x, other.x);
        let y = core::cmp::min(self.y, other.y);
        let r = core::cmp::max(self.right(), other.right());
        let b = core::cmp::max(self.bottom(), other.bottom());
        Rect {
            x, y,
            w: (r - x as i64) as u32,
            h: (b - y as i64) as u32,
        }
    }

    /// Geometric intersection. `None` if disjoint.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        if !self.overlaps(other) { return None; }
        let x = core::cmp::max(self.x, other.x);
        let y = core::cmp::max(self.y, other.y);
        let r = core::cmp::min(self.right(), other.right());
        let b = core::cmp::min(self.bottom(), other.bottom());
        Some(Rect {
            x, y,
            w: (r - x as i64) as u32,
            h: (b - y as i64) as u32,
        })
    }

    #[inline]
    pub fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
}

/// Stable, lightweight handle for a node inside one graph. Becomes
/// invalid as soon as the graph is rebuilt for the next frame — that's
/// fine, we don't store these across frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeId(pub u32);

/// One window / overlay / fullscreen-app surface.
///
/// `display_list` is intentionally `Vec<u8>` rather than parsed into
/// commands here — the consumer side of `libfolk::gfx` walks it lazily.
/// Keeping it bytes means `RenderNode` is `Clone` and cheap to move
/// between graph-build and graph-consume passes.
#[derive(Clone, Debug)]
pub struct RenderNode {
    /// Identifier of the producing process. Diagnostic only — graph
    /// logic doesn't depend on it.
    pub process_id: u32,
    /// Higher z is closer to the user. Topo sort is stable on equal z.
    pub z_index: i32,
    /// World-space bounds. Used for occlusion + damage.
    pub global_bounds: Rect,
    /// Display-list bytes (the rapport's Del 1 wire format).
    pub display_list: Vec<u8>,
    /// `true` if `display_list` changed since last frame. Drives damage.
    pub is_dirty: bool,
    /// Whether the surface is opaque. Only opaque nodes count toward
    /// occluding lower z's.
    pub is_opaque: bool,
    /// Set by `compute_occlusion` when something fully on top. The
    /// renderer skips fully occluded nodes' display lists outright.
    pub is_occluded: bool,
    /// Clip rects injected by partial-occlusion handling. Renderer
    /// must scissor every command in `display_list` against this list.
    /// Empty means "no clip" (full bounds visible).
    pub clip_regions: Vec<Rect>,
}

impl RenderNode {
    pub fn new_opaque(process_id: u32, z_index: i32, bounds: Rect, display_list: Vec<u8>) -> Self {
        Self {
            process_id, z_index, global_bounds: bounds, display_list,
            is_dirty: true, is_opaque: true, is_occluded: false,
            clip_regions: Vec::new(),
        }
    }

    pub fn new_translucent(process_id: u32, z_index: i32, bounds: Rect, display_list: Vec<u8>) -> Self {
        Self {
            process_id, z_index, global_bounds: bounds, display_list,
            is_dirty: true, is_opaque: false, is_occluded: false,
            clip_regions: Vec::new(),
        }
    }
}

/// Per-frame graph. Rebuilt from scratch each frame; the heap
/// allocations happen once, the contents churn, the `Vec` capacity
/// stays warm so repeated frames are alloc-light.
pub struct MinimalRenderGraph {
    pub nodes: Vec<RenderNode>,
    /// Damage rects accumulated by the most recent occlusion + dirty
    /// pass. In screen-clipped coords.
    pub damage_regions: Vec<Rect>,
    pub screen_width: u32,
    pub screen_height: u32,
}

impl MinimalRenderGraph {
    pub fn new(screen_width: u32, screen_height: u32) -> Self {
        Self {
            nodes: Vec::with_capacity(16),
            damage_regions: Vec::with_capacity(16),
            screen_width, screen_height,
        }
    }

    /// Reset for a new frame without dropping the backing Vecs.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.damage_regions.clear();
    }

    pub fn add_node(&mut self, node: RenderNode) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(node);
        id
    }

    /// Stable topological sort by z_index ascending (back-to-front, the
    /// painter's algorithm order). For occlusion we iterate the reverse.
    pub fn sort_by_z(&mut self) {
        // `sort_by_key` is a stable sort — equal z_index keeps insertion
        // order, which the renderer relies on for sub-stages of the same
        // window stack.
        self.nodes.sort_by_key(|n| n.z_index);
    }

    /// Whole screen as a damage rect — useful when a global state change
    /// (e.g. wallpaper swap) invalidates everything.
    pub fn screen_rect(&self) -> Rect {
        Rect::new(0, 0, self.screen_width, self.screen_height)
    }

    /// Clip a node-space rect to the visible screen. Returns `None` if
    /// the rect is entirely off-screen.
    pub fn clip_to_screen(&self, r: Rect) -> Option<Rect> {
        let screen = self.screen_rect();
        r.intersection(&screen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_overlap_and_contains() {
        let outer = Rect::new(0, 0, 100, 100);
        let inside = Rect::new(10, 10, 50, 50);
        let crossing = Rect::new(50, 50, 100, 100);
        let outside = Rect::new(200, 200, 10, 10);

        assert!(outer.overlaps(&inside));
        assert!(outer.contains(&inside));

        assert!(outer.overlaps(&crossing));
        assert!(!outer.contains(&crossing));

        assert!(!outer.overlaps(&outside));
    }

    #[test]
    fn rect_intersection_basic() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(50, 50, 100, 100);
        let i = a.intersection(&b).unwrap();
        assert_eq!(i, Rect::new(50, 50, 50, 50));
    }

    #[test]
    fn rect_intersection_disjoint() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(20, 20, 10, 10);
        assert_eq!(a.intersection(&b), None);
    }

    #[test]
    fn rect_with_negative_origin() {
        // Window dragged partly off the left edge.
        let off_left = Rect::new(-50, 0, 100, 100);
        let screen = Rect::new(0, 0, 1024, 768);
        let clipped = screen.intersection(&off_left).unwrap();
        assert_eq!(clipped, Rect::new(0, 0, 50, 100));
    }

    #[test]
    fn graph_topo_sort_is_stable() {
        let mut g = MinimalRenderGraph::new(800, 600);
        let dl = alloc::vec::Vec::new();
        g.add_node(RenderNode::new_opaque(1, 5, Rect::new(0,0,10,10), dl.clone()));
        g.add_node(RenderNode::new_opaque(2, 5, Rect::new(0,0,10,10), dl.clone()));
        g.add_node(RenderNode::new_opaque(3, 1, Rect::new(0,0,10,10), dl));
        g.sort_by_z();
        // process 3 (z=1) first; then 1 and 2 in original order (stable).
        assert_eq!(g.nodes[0].process_id, 3);
        assert_eq!(g.nodes[1].process_id, 1);
        assert_eq!(g.nodes[2].process_id, 2);
    }
}
