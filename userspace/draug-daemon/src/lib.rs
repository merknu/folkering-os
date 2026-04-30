//! draug-daemon — Folkering OS autonomous agent (library half).
//!
//! Phase A skeleton: the binary in `main.rs` is the only consumer right
//! now. As the migration from `compositor::draug` proceeds, modules get
//! moved here one at a time:
//!   * `state` — the `DraugDaemon` struct (currently in
//!     `compositor/src/draug.rs`)
//!   * `async_orchestrator` — TCP-driven LLM/PATCH/cargo_check pipeline
//!     (currently `mcp_handler/draug_async.rs`)
//!   * `agent_logic`, `agent_planner`, `autodream`, `knowledge_hunt`,
//!     `refactor_loop`, `task_store`, `token_stream` — same name in
//!     compositor today.
//!
//! The lib half exists so integration tests and host-side tools can
//! depend on the same modules without dragging in the `no_std` binary
//! entry point.

#![no_std]

extern crate alloc;
