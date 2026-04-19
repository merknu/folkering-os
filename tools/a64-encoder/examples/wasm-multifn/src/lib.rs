//! Multi-function smoke test — exercises cross-function BL calls.
//!
//! Rust compiles this to a WASM module with at least 2 functions:
//! `entry` and `helper_mul3`. `entry` calls `helper_mul3(14)` which
//! adds 14 + 14 + 14 = 42.
//!
//! `#[inline(never)]` forces Rust to keep each function separate in
//! the output — otherwise LTO would collapse them into one function.
//! We also use `core::hint::black_box` to stop constant folding that
//! would otherwise resolve the entire call chain at compile time.

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

#[inline(never)]
fn helper_mul3(x: i32) -> i32 {
    let mut sum = 0i32;
    for _ in 0..3 {
        sum = sum.wrapping_add(core::hint::black_box(x));
    }
    sum
}

#[no_mangle]
pub extern "C" fn entry() -> i32 {
    helper_mul3(core::hint::black_box(14))
}
