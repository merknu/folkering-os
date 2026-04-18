//! Test A — micro MLP, 16 → 32 → 16 → 1.
//!
//! Memory layout (relative to BASE = 0x1000):
//!   0x0000  inputs[16]              (   64 B)
//!   0x0040  W1[16 × 32] = 512 f32   ( 2048 B)
//!   0x0840  b1[32]                  (  128 B)
//!   0x08C0  W2[32 × 16] = 512 f32   ( 2048 B)
//!   0x10C0  b2[16]                  (   64 B)
//!   0x1100  W3[16]                  (   64 B)
//!   0x1140  b3                      (    4 B)
//!   0x1150  hidden1[32]             (  128 B)
//!   0x11D0  hidden2[16]             (   64 B)

#![no_std]

use core::panic::PanicInfo;
#[panic_handler] fn panic(_: &PanicInfo) -> ! { loop {} }

const BASE: u32 = 0x1000;
const OFF_INPUTS: u32  = 0x0000;
const OFF_W1: u32      = 0x0040;
const OFF_B1: u32      = 0x0840;
const OFF_W2: u32      = 0x08C0;
const OFF_B2: u32      = 0x10C0;
const OFF_W3: u32      = 0x1100;
const OFF_B3: u32      = 0x1140;
const OFF_HIDDEN1: u32 = 0x1150;
const OFF_HIDDEN2: u32 = 0x11D0;

#[inline(always)] fn lf(off: u32) -> f32 {
    unsafe { core::ptr::read((BASE + off) as *const f32) }
}
#[inline(always)] fn sf(off: u32, v: f32) {
    unsafe { core::ptr::write((BASE + off) as *mut f32, v) }
}

#[inline(never)]
fn layer1() {
    let mut j = 0u32;
    while j < 32 {
        let mut s = lf(OFF_B1 + j * 4);
        let mut k = 0u32;
        while k < 16 {
            s += lf(OFF_INPUTS + k * 4) * lf(OFF_W1 + (k * 32 + j) * 4);
            k += 1;
        }
        if s < 0.0 { s = 0.0; }
        sf(OFF_HIDDEN1 + j * 4, s);
        j += 1;
    }
}

#[inline(never)]
fn layer2() {
    let mut j = 0u32;
    while j < 16 {
        let mut s = lf(OFF_B2 + j * 4);
        let mut k = 0u32;
        while k < 32 {
            s += lf(OFF_HIDDEN1 + k * 4) * lf(OFF_W2 + (k * 16 + j) * 4);
            k += 1;
        }
        if s < 0.0 { s = 0.0; }
        sf(OFF_HIDDEN2 + j * 4, s);
        j += 1;
    }
}

#[no_mangle]
pub extern "C" fn entry() -> i32 {
    layer1();
    layer2();
    let mut out = lf(OFF_B3);
    let mut k = 0u32;
    while k < 16 {
        out += lf(OFF_HIDDEN2 + k * 4) * lf(OFF_W3 + k * 4);
        k += 1;
    }
    (out * 1000.0) as i32
}
