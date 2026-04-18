//! Globals smoke test — exercises the new GlobalGet/GlobalSet
//! lowering and the `__stack_pointer` infrastructure.
//!
//! Forces Rust to emit:
//!   * a `__stack_pointer` global (mut i32) for stack-frame allocation
//!   * `global.get $__stack_pointer` / subtract / `global.set` to
//!     allocate a stack frame
//!   * `i32.store` / `i32.load` against the stack-allocated array
//!
//! The function builds an array on the stack, sums its elements,
//! and returns the sum. If our globals lowering and stack-pointer
//! initialization both work, the sum returns 1 + 2 + ... + 8 = 36.

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

/// `#[inline(never)]` + `core::hint::black_box` keep LLVM from
/// const-folding the entire body away. We need the actual stack
/// allocation + reads so we exercise __stack_pointer.
#[no_mangle]
pub extern "C" fn test_globals() -> i32 {
    let mut arr = [0i32; 8];
    for i in 0..8 {
        arr[i] = (i + 1) as i32;
    }
    let mut sum = 0i32;
    for i in 0..8 {
        sum += core::hint::black_box(arr[i]);
    }
    sum
}
