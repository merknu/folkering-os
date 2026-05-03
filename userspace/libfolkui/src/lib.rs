//! libfolkui — declarative UI framework for AI-authored Folkering apps.
//!
//! The point of this crate is to give an AI agent (Draug) a target that's
//! easy to generate correctly: a small XML-shaped markup that the framework
//! turns into byte-accurate display-list output for the compositor.
//!
//! Pipeline:
//! 1. The agent emits DSML (`<Window>`, `<VBox>`, `<Text>`, `<Button>`, …).
//! 2. `parser::parse` turns it into a `dom::Node` tree.
//! 3. `layout::layout` walks the tree top-down/bottom-up to assign
//!    `(x, y, w, h)` to every node.
//! 4. `compiler::compile_to_display_list` traverses the laid-out tree and
//!    emits `libfolk::gfx::DisplayListBuilder` bytes.
//! 5. The bytes ride the SPSC ring to the compositor, which walks the
//!    `RenderGraph` to scissor and present.
//!
//! Scope of this PR:
//! - Parser: no_std, no regex, single-pass character scanner. Handles
//!   tags, attributes (quoted only), self-closing tags, plain-text
//!   children. No CDATA, no comments, no namespaces — keep the format
//!   surface small so a 7B model can produce it consistently.
//! - DOM: arena-free `Vec<Node>` with parent/child indices. Cheap to
//!   rebuild per frame.
//! - Layout: `VBox`/`HBox` with `padding` and `spacing` attributes.
//!   Flexbox-style `flex-grow` / `align` is deliberately deferred — the
//!   common case (top-down stacking with manual sizes) is enough to
//!   render a status panel today, and we can grow into flexbox without
//!   reshaping the rest of the pipeline.
//! - Compiler: emits `DrawRect` + `DrawText` per node. `<Button>`
//!   composes (rect + text). Color attributes parse `#RRGGBB`.
//!
//! Reactive bindings: `<Text bind_text="key">` resolves against an
//! `AppState` map at compile time. Apps call `state.set(key, value)`
//! once per frame; markup stays static. See `state.rs`.
//!
//! Not in this PR (deliberate follow-ups):
//! - Tree-diffing / virtual DOM reconciliation. Today every frame
//!   re-parses + re-emits. `Vec` capacity stays warm so this is
//!   alloc-light after the first frame.
//! - Real flexbox. The `layout::layout` API takes a width/height
//!   constraint and is shaped to grow into bidirectional passes when
//!   we add it.

#![no_std]

extern crate alloc;

pub mod parser;
pub mod dom;
pub mod layout;
pub mod compiler;
pub mod state;
pub mod diff;

pub use parser::{parse, ParseError};
pub use dom::{Node, NodeKind, Tree, AttrMap};
pub use layout::{layout, LayoutConstraint};
pub use compiler::{compile_to_display_list, compile_to_display_list_with_state, compile_into};
pub use state::AppState;
pub use diff::{DiffState, compile_diff_into};
