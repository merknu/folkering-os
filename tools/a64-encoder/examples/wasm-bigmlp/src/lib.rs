//! Larger multi-function MLP — 8 → 16 → 8 → 1.
//!
//! Memory layout (relative to BASE = 0x1000):
//!
//!   0x000  inputs[8]                  ( 32 B)
//!   0x020  W1[8 × 16]   = 128 f32     (512 B)
//!   0x220  b1[16]                     ( 64 B)
//!   0x260  W2[16 × 8]   = 128 f32     (512 B)
//!   0x460  b2[8]                      ( 32 B)
//!   0x480  W3[8]                      ( 32 B)
//!   0x4A0  b3                         (  4 B)
//!   0x4B0  hidden1 buffer[16]         ( 64 B)
//!   0x4F0  hidden2 buffer[ 8]         ( 32 B)
//!
//! Helpers are SPECIALISED on the layer dimensions so Rust can const-
//! fold the loop bounds and offset computations into the body. A
//! generic 6-arg `linear` produced 38 locals (over our 9-i32 limit);
//! these 4-arg versions stay under it.

#![no_std]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! { loop {} }

const BASE: u32 = 0x1000;

const OFF_INPUTS:  u32 = 0x000;
const OFF_W1:      u32 = 0x020;
const OFF_B1:      u32 = 0x220;
const OFF_W2:      u32 = 0x260;
const OFF_B2:      u32 = 0x460;
const OFF_W3:      u32 = 0x480;
const OFF_B3:      u32 = 0x4A0;
const OFF_HIDDEN1: u32 = 0x4B0;
const OFF_HIDDEN2: u32 = 0x4F0;

#[inline(always)]
fn lf(off: u32) -> f32 {
    unsafe { core::ptr::read((BASE + off) as *const f32) }
}

#[inline(always)]
fn sf(off: u32, v: f32) {
    unsafe { core::ptr::write((BASE + off) as *mut f32, v) }
}

/// Layer 1: input[8] @ W1[8 × 16] + b1[16] → hidden1[16] + ReLU.
#[inline(never)]
fn layer1_8_to_16() {
    let mut j = 0u32;
    while j < 16 {
        let mut s = lf(OFF_B1 + j * 4);
        let mut k = 0u32;
        while k < 8 {
            s += lf(OFF_INPUTS + k * 4) * lf(OFF_W1 + (k * 16 + j) * 4);
            k += 1;
        }
        if s < 0.0 { s = 0.0; }
        sf(OFF_HIDDEN1 + j * 4, s);
        j += 1;
    }
}

/// Layer 2: hidden1[16] @ W2[16 × 8] + b2[8] → hidden2[8] + ReLU.
#[inline(never)]
fn layer2_16_to_8() {
    let mut j = 0u32;
    while j < 8 {
        let mut s = lf(OFF_B2 + j * 4);
        let mut k = 0u32;
        while k < 16 {
            s += lf(OFF_HIDDEN1 + k * 4) * lf(OFF_W2 + (k * 8 + j) * 4);
            k += 1;
        }
        if s < 0.0 { s = 0.0; }
        sf(OFF_HIDDEN2 + j * 4, s);
        j += 1;
    }
}

#[no_mangle]
pub extern "C" fn entry() -> i32 {
    layer1_8_to_16();
    layer2_16_to_8();

    // Layer 3: hidden2[8] @ W3[8] + b3 → scalar
    let mut out = lf(OFF_B3);
    let mut k = 0u32;
    while k < 8 {
        out += lf(OFF_HIDDEN2 + k * 4) * lf(OFF_W3 + k * 4);
        k += 1;
    }

    (out * 1000.0) as i32
}
