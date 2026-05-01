//! Morning Briefing state — pending creative dream results awaiting
//! user approval.
//!
//! This used to live on `DraugDaemon::pending_creative` while the
//! compositor and the agent shared a process. Now that the daemon is
//! isolated (Phase A), the briefing is purely compositor UI state:
//! it holds the WASM bytes the LLM produced overnight, the human-
//! readable description for the briefing window, and the user's
//! accept/reject decision per item. Nothing here belongs in the
//! daemon's address space.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Cap on simultaneously-pending items. Each item carries a full
/// WASM binary; three is plenty for a single night's dreaming and
/// keeps the heap footprint bounded.
pub const MAX_PENDING_BRIEFING: usize = 3;

/// One creative dream result waiting for user approval.
pub struct BriefingItem {
    pub app_name: String,
    /// Human-readable summary shown in the briefing window.
    pub description: String,
    /// The full evolved WASM binary, applied to the cache on accept.
    pub wasm_bytes: Vec<u8>,
    /// `None` = pending, `Some(true)` = accepted, `Some(false)` = rejected.
    pub accepted: Option<bool>,
}

/// Compositor-side queue of pending Morning Briefing items.
pub struct BriefingState {
    pub items: Vec<BriefingItem>,
}

impl BriefingState {
    pub const fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Queue a creative-dream result for user approval. Drops the
    /// oldest item if the queue is full.
    pub fn queue(&mut self, app_name: &str, description: &str, wasm_bytes: Vec<u8>) {
        if self.items.len() >= MAX_PENDING_BRIEFING {
            self.items.remove(0);
        }
        self.items.push(BriefingItem {
            app_name: app_name.to_string(),
            description: description.to_string(),
            wasm_bytes,
            accepted: None,
        });
    }

    /// True if at least one item is still awaiting a decision.
    pub fn has_pending(&self) -> bool {
        self.items.iter().any(|p| p.accepted.is_none())
    }

    /// How many items are awaiting a decision.
    pub fn pending_count(&self) -> usize {
        self.items.iter().filter(|p| p.accepted.is_none()).count()
    }

    /// Mark the item at `idx` as accepted. No-op if `idx` out of range.
    pub fn accept(&mut self, idx: usize) {
        if let Some(p) = self.items.get_mut(idx) {
            p.accepted = Some(true);
        }
    }

    /// Mark the item at `idx` as rejected. No-op if `idx` out of range.
    pub fn reject(&mut self, idx: usize) {
        if let Some(p) = self.items.get_mut(idx) {
            p.accepted = Some(false);
        }
    }

    /// Mark all undecided items as accepted.
    pub fn accept_all(&mut self) {
        for p in &mut self.items {
            if p.accepted.is_none() {
                p.accepted = Some(true);
            }
        }
    }

    /// Drain accepted items as `(app_name, wasm_bytes)` pairs and
    /// remove all decided items (accepted *or* rejected) from the
    /// queue. Pending items stay.
    pub fn drain_accepted(&mut self) -> Vec<(String, Vec<u8>)> {
        let mut result = Vec::new();
        self.items.retain(|p| match p.accepted {
            Some(true) => {
                result.push((p.app_name.clone(), p.wasm_bytes.clone()));
                false
            }
            Some(false) => false,
            None => true,
        });
        result
    }
}

impl Default for BriefingState {
    fn default() -> Self { Self::new() }
}
