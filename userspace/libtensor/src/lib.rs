//! libtensor — no_std SIMD-accelerated tensor library for Folkering OS
//!
//! Provides Q4_0/Q8_0 quantized GEMM with AVX2 acceleration,
//! transformer operations (RMSNorm, Softmax, SiLU, RoPE),
//! GGUF model parsing, and KV-cache management.
//!
//! All operations use a static bump arena — zero heap allocation
//! during inference.

#![no_std]
#![allow(unused_unsafe)]
#![allow(dead_code)]

pub mod arena;
pub mod simd;
pub mod quantize;
pub mod gemm;
pub mod ops;
pub mod kv_cache;
pub mod gguf;
pub mod bow_embed;
pub mod transformer;
pub mod tokenizer;

/// Tensor data types supported by libtensor
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DType {
    F32 = 0,
    F16 = 1,
    Q4_0 = 2,
    Q8_0 = 8,
}

/// Fused operation to apply after GEMM accumulation (in-register)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseOp {
    /// No fused operation
    None,
    /// SiLU activation: x * sigmoid(x)
    SiLU,
}

/// Tensor view — does NOT own data, points into mmap'd or arena memory
#[derive(Clone, Copy)]
pub struct Tensor<'a> {
    pub data: &'a [u8],
    pub shape: [usize; 4],
    pub ndim: usize,
    pub dtype: DType,
}

impl<'a> Tensor<'a> {
    /// Create a new tensor view
    pub fn new(data: &'a [u8], shape: &[usize], dtype: DType) -> Self {
        let mut s = [1usize; 4];
        let ndim = shape.len().min(4);
        for i in 0..ndim {
            s[i] = shape[i];
        }
        Self { data, shape: s, ndim, dtype }
    }

    /// Total number of elements
    pub fn numel(&self) -> usize {
        self.shape[0] * self.shape[1] * self.shape[2] * self.shape[3]
    }

    /// Number of bytes for the tensor data given its dtype
    pub fn byte_size(numel: usize, dtype: DType) -> usize {
        match dtype {
            DType::F32 => numel * 4,
            DType::F16 => numel * 2,
            DType::Q4_0 => {
                // Q4_0: groups of 32 values, each group = 2 bytes scale + 16 bytes data = 18 bytes
                let n_blocks = (numel + 31) / 32;
                n_blocks * quantize::Q4_0_BLOCK_SIZE
            }
            DType::Q8_0 => {
                // Q8_0: groups of 32 values, each group = 4 bytes scale + 32 bytes data = 36 bytes
                let n_blocks = (numel + 31) / 32;
                n_blocks * quantize::Q8_0_BLOCK_SIZE
            }
        }
    }

    /// Interpret tensor data as f32 slice (only valid for F32 dtype)
    pub fn as_f32(&self) -> &[f32] {
        assert!(self.dtype == DType::F32);
        let ptr = self.data.as_ptr() as *const f32;
        let len = self.data.len() / 4;
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }
}
