//! Render Graph (Del 2 of the architecture rapport).
//!
//! Today the compositor is "immediate-mode": each stage of `render_frame()`
//! draws straight into the shadow buffer in a fixed sequence. That works,
//! but it can't make global decisions like "Node B is fully covered by
//! Node A, skip drawing it" or "these two damaged regions are close enough
//! that one larger flush is cheaper than two".
//!
//! This module is the *data layer* for moving toward a Render Graph: a
//! lightweight DAG of `RenderNode`s with z-indexed sorting, front-to-back
//! occlusion culling, partial-occlusion clip-rect injection, and
//! coalesced damage extraction. It deliberately doesn't touch the active
//! render pipeline — it's added as a peer of `damage.rs` so callers can
//! adopt it incrementally. Once render stages emit nodes instead of
//! drawing directly, this becomes the orchestration core.
//!
//! Design notes:
//! - `Vec<RenderNode>` is the storage. Cache-friendly, predictable,
//!   freed in one shot per frame (arena-style without bringing in an
//!   actual arena allocator yet).
//! - All geometry is `i32` for coords and `u32` for sizes — no floating
//!   point in the kernel-side path.
//! - Bounding-box arithmetic is exposed via `Rect` here even though
//!   `damage::Rect` already exists; we use a slightly different shape
//!   (`i32` x/y) to handle nodes that conceptually sit at negative
//!   coordinates (e.g. partially off-screen windows). Conversion to
//!   `damage::Rect` happens at the screen boundary.

extern crate alloc;

pub mod graph;
pub mod occlusion;

pub use graph::{Rect, RenderNode, MinimalRenderGraph, NodeId};
pub use occlusion::{compute_occlusion, OcclusionStats};
