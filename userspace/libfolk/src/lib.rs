//! LibFolk - Userspace SDK for Folkering OS
//!
//! This `#![no_std]` library provides the foundation for writing userspace
//! applications in Rust for Folkering OS.
//!
//! # Getting Started
//!
//! ```no_run
//! #![no_std]
//! #![no_main]
//!
//! use libfolk::{entry, println};
//! use libfolk::sys::{yield_cpu, exit};
//!
//! entry!(main);
//!
//! fn main() -> ! {
//!     println!("Hello from Folkering OS!");
//!
//!     loop {
//!         yield_cpu();
//!     }
//! }
//! ```
//!
//! # Modules
//!
//! - [`sys`] - Safe syscall wrappers (task, io, ipc, memory, system)
//! - [`syscall`] - Raw syscall interface
//! - [`fmt`] - Formatting and printing support
//!
//! # Macros
//!
//! - [`entry!`] - Define the program entry point
//! - [`print!`] / [`println!`] - Output formatted text to console

#![no_std]

pub mod entry;
pub mod syscall;
pub mod fmt;
pub mod sys;
pub mod ui;

// Re-export print macros at crate root
pub use fmt::_print;
