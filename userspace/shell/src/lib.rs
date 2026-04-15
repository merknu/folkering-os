//! Folkering Shell library — modules shared between the shell binary and
//! tests. The thin `bin/main.rs` only wires up `entry!()` and the IPC loop;
//! everything else lives here so it can be unit-tested in isolation.

#![no_std]

pub mod commands;
pub mod input;
pub mod ipc;
pub mod state;
pub mod ui;
pub mod wasm;
