//! Test C — macro MLP, 256 → 512 → 256 → 1.
//!
//! Memory layout (relative to BASE = 0x1000):
//!   0x000000  inputs[256]                    (  1024 B)
//!   0x000400  W1[256 × 512]   = 131072 f32   (524288 B = 512 KiB)
//!   0x080400  b1[512]                        (  2048 B)
//!   0x080C00  W2[512 × 256]   = 131072 f32   (524288 B = 512 KiB)
//!   0x100C00  b2[256]                        (  1024 B)
//!   0x101000  W3[256]                        (  1024 B)
//!   0x101400  b3                             (     4 B)
//!   0x101410  hidden1[512]                   (  2048 B)
//!   0x101C10  hidden2[256]                   (  1024 B)
//!
//! Total payload: ~1.05 MiB. Requires daemon LINEAR_MEM_SIZE ≥ 2 MiB.

#![no_std]

use core::panic::PanicInfo;
#[panic_handler] fn panic(_: &PanicInfo) -> ! { loop {} }

const BASE: u32 = 0x1000;
const OFF_INPUTS: u32  = 0x000000;
const OFF_W1: u32      = 0x000400;
const OFF_B1: u32      = 0x080400;
const OFF_W2: u32      = 0x080C00;
const OFF_B2: u32      = 0x100C00;
const OFF_W3: u32      = 0x101000;
const OFF_B3: u32      = 0x101400;
const OFF_HIDDEN1: u32 = 0x101410;
const OFF_HIDDEN2: u32 = 0x101C10;

#[inline(always)] fn lf(off: u32) -> f32 {
    unsafe { core::ptr::read((BASE + off) as *const f32) }
}
#[inline(always)] fn sf(off: u32, v: f32) {
    unsafe { core::ptr::write((BASE + off) as *mut f32, v) }
}

#[inline(never)]
fn layer1() {
    let mut j = 0u32;
    while j < 512 {
        let mut s = lf(OFF_B1 + j * 4);
        let mut k = 0u32;
        while k < 256 {
            s += lf(OFF_INPUTS + k * 4) * lf(OFF_W1 + (k * 512 + j) * 4);
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
    while j < 256 {
        let mut s = lf(OFF_B2 + j * 4);
        let mut k = 0u32;
        while k < 512 {
            s += lf(OFF_HIDDEN1 + k * 4) * lf(OFF_W2 + (k * 256 + j) * 4);
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
    while k < 256 {
        out += lf(OFF_HIDDEN2 + k * 4) * lf(OFF_W3 + k * 4);
        k += 1;
    }
    (out * 1000.0) as i32
}
