//! Memory Management Subsystem

pub mod physical;
pub mod paging;
pub mod heap;

pub use physical::{alloc_pages, free_pages};
pub use x86_64::structures::paging::PageTable;
