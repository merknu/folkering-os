//! Real ML inference compiled to WebAssembly.
//!
//! Computes a 4 → 4 → 4 → 1 MLP (16+4+16+4+4+1 = 45 weights) by
//! reading inputs and weights from linear memory at fixed offsets,
//! applying ReLU activations, and returning the scaled output as i32.
//!
//! Memory layout (matches `mlp_memory_on_pi.rs`):
//!   offset 0:    4 input f32      (16 B)
//!   offset 16:   w1[4][4]         (64 B)
//!   offset 80:   b1[4]            (16 B)
//!   offset 96:   w2[4][4]         (64 B)
//!   offset 160:  b2[4]            (16 B)
//!   offset 176:  w3[4]            (16 B)
//!   offset 192:  b3 f32           (4 B)
//!
//! Compiled with `cargo build --target wasm32-unknown-unknown --release`,
//! parsed by `a64-encoder::parse_module`, JIT-compiled to AArch64,
//! executed on a Raspberry Pi 5 via `a64-stream-daemon`.

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

/// Base offset within linear memory where the host writes our
/// inputs+weights via DATA frame. Must be ≥ 4 KiB so LLVM's
/// null-page UB optimization (which folds reads from low
/// addresses to undef → 0.0) doesn't kill the inputs. The host
/// matches this offset when populating the buffer.
pub const BASE: u32 = 0x1000;

/// Read an aligned f32 from linear-memory offset `BASE + off`.
#[inline(always)]
fn lf(off: u32) -> f32 {
    unsafe { core::ptr::read((BASE + off) as *const f32) }
}

/// ReLU activation — branchless via `select_unpredictable`-style
/// pattern that LLVM lowers to a `select` op in WASM.
#[inline(always)]
fn relu(x: f32) -> f32 {
    if x > 0.0 { x } else { 0.0 }
}

/// Run inference and return `(output * 100) as i32`.
///
/// The host populates linear memory with inputs+weights via a DATA
/// frame, then EXECs this function. The exit code carries the result.
#[no_mangle]
pub extern "C" fn infer() -> i32 {
    // Inputs
    let i0 = lf(0);
    let i1 = lf(4);
    let i2 = lf(8);
    let i3 = lf(12);

    // Layer 1: 4 neurons, each dot4(inputs, w1[n]) + b1[n], then ReLU
    let h1_0 = relu(i0*lf(16)  + i1*lf(20)  + i2*lf(24)  + i3*lf(28)  + lf(80));
    let h1_1 = relu(i0*lf(32)  + i1*lf(36)  + i2*lf(40)  + i3*lf(44)  + lf(84));
    let h1_2 = relu(i0*lf(48)  + i1*lf(52)  + i2*lf(56)  + i3*lf(60)  + lf(88));
    let h1_3 = relu(i0*lf(64)  + i1*lf(68)  + i2*lf(72)  + i3*lf(76)  + lf(92));

    // Layer 2: 4 neurons, each dot4(h1, w2[n]) + b2[n], then ReLU
    let h2_0 = relu(h1_0*lf(96)  + h1_1*lf(100) + h1_2*lf(104) + h1_3*lf(108) + lf(160));
    let h2_1 = relu(h1_0*lf(112) + h1_1*lf(116) + h1_2*lf(120) + h1_3*lf(124) + lf(164));
    let h2_2 = relu(h1_0*lf(128) + h1_1*lf(132) + h1_2*lf(136) + h1_3*lf(140) + lf(168));
    let h2_3 = relu(h1_0*lf(144) + h1_1*lf(148) + h1_2*lf(152) + h1_3*lf(156) + lf(172));

    // Output: dot4(h2, w3) + b3, scaled
    let out = h2_0*lf(176) + h2_1*lf(180) + h2_2*lf(184) + h2_3*lf(188) + lf(192);
    (out * 100.0) as i32
}
