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
