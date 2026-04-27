//! Syscall handler implementations, organized by domain.
//!
//! Each submodule contains the `pub fn syscall_*` implementations for one
//! kernel subsystem. The dispatcher in `super::dispatch` reaches them via
//! `use super::handlers::*` — the wildcard re-exports below flatten the
//! whole tree into `handlers::*`.

mod ipc;
mod memory;
mod task;
mod io;
mod fs;
pub(super) mod net;
mod audio;
mod compute;
mod gpu;
mod pci;
mod dma;

pub use ipc::*;
pub use memory::*;
pub use task::*;
pub use io::*;
pub use fs::*;
pub use net::*;
pub use audio::*;
pub use compute::*;
pub use gpu::*;
pub use pci::*;
pub use dma::*;
