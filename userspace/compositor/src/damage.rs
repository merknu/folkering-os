//! Dirty Rectangle Tracking for VirtIO-GPU Flush Optimization
//!
//! Tracks damaged screen regions and coalesces overlapping rectangles.
//! Reduces VirtIO bus traffic from 4MB/frame to ~50KB/frame.
//!
//! Safety: MAX_RECTS=10 guarantees VirtIO command page (4096 bytes)
//! never overflows. If exceeded, collapses all into one bounding box.

use alloc::vec::Vec;
use core::cmp::{max, min};

/// Maximum disjoint rects before collapse to bounding box.
/// 10 rects × ~48 bytes (Transfer+Flush pair) = 480 bytes << 4096.
const MAX_RECTS: usize = 10;

#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// AABB intersection test.
    #[inline]
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.x + other.w
            && self.x + self.w > other.x
            && self.y < other.y + other.h
            && self.y + self.h > other.y
    }

    /// Geometric union of two rectangles.
    #[inline]
    pub fn union(&self, other: &Rect) -> Rect {
        let x = min(self.x, other.x);
        let y = min(self.y, other.y);
        let right = max(self.x + self.w, other.x + other.w);
        let bottom = max(self.y + self.h, other.y + other.h);
        Rect { x, y, w: right - x, h: bottom - y }
    }
}

pub struct DamageTracker {
    regions: Vec<Rect>,
    screen_w: u32,
    screen_h: u32,
}

impl DamageTracker {
    /// Create with pre-allocated capacity to avoid runtime allocations.
    pub fn new(screen_w: u32, screen_h: u32) -> Self {
        Self {
            regions: Vec::with_capacity(MAX_RECTS * 2),
            screen_w,
            screen_h,
        }
    }

    /// Add a dirty rectangle. Coalesces with overlapping existing regions.
    /// If MAX_RECTS exceeded, collapses all into one bounding box.
    pub fn add_damage(&mut self, new_rect: Rect) {
        // Clamp to screen bounds
        let x = min(new_rect.x, self.screen_w);
        let y = min(new_rect.y, self.screen_h);
        let w = min(new_rect.w, self.screen_w - x);
        let h = min(new_rect.h, self.screen_h - y);
        if w == 0 || h == 0 {
            return;
        }

        let mut merged = Rect::new(x, y, w, h);

        // Coalesce with any intersecting existing regions
        let mut i = 0;
        while i < self.regions.len() {
            if self.regions[i].intersects(&merged) {
                merged = self.regions[i].union(&merged);
                self.regions.swap_remove(i);
                i = 0; // Restart — new union may overlap others
            } else {
                i += 1;
            }
        }

        self.regions.push(merged);

        // Safety clamp: if too many disjoint rects, collapse to bounding box
        if self.regions.len() > MAX_RECTS {
            self.collapse_to_bounding_box();
        }
    }

    /// Mark the entire screen as damaged (e.g., after window creation).
    pub fn damage_full(&mut self) {
        self.regions.clear();
        self.regions.push(Rect::new(0, 0, self.screen_w, self.screen_h));
    }

    /// Collapse all regions into a single bounding box.
    fn collapse_to_bounding_box(&mut self) {
        if self.regions.is_empty() {
            return;
        }
        let mut bbox = self.regions[0];
        for r in &self.regions[1..] {
            bbox = bbox.union(r);
        }
        self.regions.clear();
        self.regions.push(bbox);
    }

    /// Get the disjoint dirty regions for this frame.
    pub fn regions(&self) -> &[Rect] {
        &self.regions
    }

    /// Clear after flush.
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    /// Check if any damage exists.
    pub fn has_damage(&self) -> bool {
        !self.regions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Rect tests ──

    #[test]
    fn rect_intersects_overlap() {
        let a = Rect::new(10, 10, 50, 50);
        let b = Rect::new(30, 30, 50, 50);
        assert!(a.intersects(&b));
        assert!(b.intersects(&a));
    }

    #[test]
    fn rect_intersects_no_overlap() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(20, 20, 10, 10);
        assert!(!a.intersects(&b));
    }

    #[test]
    fn rect_intersects_adjacent_no_overlap() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(10, 0, 10, 10); // touching edge
        assert!(!a.intersects(&b));
    }

    #[test]
    fn rect_intersects_contained() {
        let outer = Rect::new(0, 0, 100, 100);
        let inner = Rect::new(25, 25, 50, 50);
        assert!(outer.intersects(&inner));
        assert!(inner.intersects(&outer));
    }

    #[test]
    fn rect_union_basic() {
        let a = Rect::new(10, 10, 20, 20);
        let b = Rect::new(25, 25, 20, 20);
        let u = a.union(&b);
        assert_eq!(u.x, 10);
        assert_eq!(u.y, 10);
        assert_eq!(u.w, 35); // 10 to 45
        assert_eq!(u.h, 35); // 10 to 45
    }

    #[test]
    fn rect_union_contained() {
        let outer = Rect::new(0, 0, 100, 100);
        let inner = Rect::new(25, 25, 10, 10);
        let u = outer.union(&inner);
        assert_eq!(u.x, 0);
        assert_eq!(u.y, 0);
        assert_eq!(u.w, 100);
        assert_eq!(u.h, 100);
    }

    // ── DamageTracker tests ──

    #[test]
    fn tracker_empty_initially() {
        let t = DamageTracker::new(1024, 768);
        assert!(!t.has_damage());
        assert_eq!(t.regions().len(), 0);
    }

    #[test]
    fn tracker_damage_full() {
        let mut t = DamageTracker::new(1024, 768);
        t.damage_full();
        assert!(t.has_damage());
        assert_eq!(t.regions().len(), 1);
        let r = &t.regions()[0];
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 0);
        assert_eq!(r.w, 1024);
        assert_eq!(r.h, 768);
    }

    #[test]
    fn tracker_add_single() {
        let mut t = DamageTracker::new(1024, 768);
        t.add_damage(Rect::new(100, 200, 50, 30));
        assert_eq!(t.regions().len(), 1);
        let r = &t.regions()[0];
        assert_eq!(r.x, 100);
        assert_eq!(r.y, 200);
        assert_eq!(r.w, 50);
        assert_eq!(r.h, 30);
    }

    #[test]
    fn tracker_coalesce_overlapping() {
        let mut t = DamageTracker::new(1024, 768);
        t.add_damage(Rect::new(10, 10, 50, 50));
        t.add_damage(Rect::new(30, 30, 50, 50));
        // Should coalesce into one rect
        assert_eq!(t.regions().len(), 1);
        let r = &t.regions()[0];
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 10);
        assert_eq!(r.w, 70); // 10 to 80
        assert_eq!(r.h, 70); // 10 to 80
    }

    #[test]
    fn tracker_disjoint_kept_separate() {
        let mut t = DamageTracker::new(1024, 768);
        t.add_damage(Rect::new(0, 0, 10, 10));
        t.add_damage(Rect::new(100, 100, 10, 10));
        assert_eq!(t.regions().len(), 2);
    }

    #[test]
    fn tracker_clamp_to_screen() {
        let mut t = DamageTracker::new(100, 100);
        t.add_damage(Rect::new(90, 90, 50, 50));
        let r = &t.regions()[0];
        assert_eq!(r.w, 10); // clamped to screen
        assert_eq!(r.h, 10);
    }

    #[test]
    fn tracker_zero_size_ignored() {
        let mut t = DamageTracker::new(1024, 768);
        t.add_damage(Rect::new(50, 50, 0, 0));
        assert!(!t.has_damage());
    }

    #[test]
    fn tracker_clear() {
        let mut t = DamageTracker::new(1024, 768);
        t.damage_full();
        assert!(t.has_damage());
        t.clear();
        assert!(!t.has_damage());
    }

    #[test]
    fn tracker_collapse_when_full() {
        let mut t = DamageTracker::new(1024, 768);
        // Add 12 disjoint rects (exceeds MAX_RECTS=10)
        for i in 0..12 {
            t.add_damage(Rect::new(i * 80, 0, 10, 10));
        }
        // Should have collapsed to 1 bounding box
        assert_eq!(t.regions().len(), 1);
        let r = &t.regions()[0];
        assert_eq!(r.x, 0);
        assert!(r.w >= 880 + 10); // 11*80 + 10
    }
}
