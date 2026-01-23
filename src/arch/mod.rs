//! Architecture-specific code
//!
//! This module contains x86_64-specific implementation details.

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        pub mod x86_64;
        pub use x86_64::*;
    } else {
        compile_error!("Unsupported architecture");
    }
}
