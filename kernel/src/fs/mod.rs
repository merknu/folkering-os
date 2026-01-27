//! Filesystem subsystem
//!
//! Currently provides:
//! - Folk-Pack format definitions (`format`)
//! - Ramdisk driver for boot-time initrd images (`ramdisk`)

pub mod format;
pub mod ramdisk;
