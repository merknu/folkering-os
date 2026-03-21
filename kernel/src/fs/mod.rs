//! Filesystem subsystem
//!
//! Currently provides:
//! - Folk-Pack format definitions (`format`)
//! - Ramdisk driver for boot-time initrd images (`ramdisk`)
//! - Global ramdisk access for syscalls

pub mod format;
pub mod ramdisk;

use spin::Once;
use ramdisk::Ramdisk;

/// Global ramdisk instance, initialized once at boot.
static GLOBAL_RAMDISK: Once<Ramdisk> = Once::new();

/// Store the ramdisk globally (called once during boot).
pub fn init_ramdisk(rd: Ramdisk) {
    GLOBAL_RAMDISK.call_once(|| rd);
}

/// Get a reference to the global ramdisk, if initialized.
pub fn ramdisk() -> Option<&'static Ramdisk> {
    GLOBAL_RAMDISK.get()
}
