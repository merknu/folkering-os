//! Folkering OS Phase D.3 — flat tensor file format (`.fbin`).
//!
//! Goal: a self-describing, alignment-friendly, no_std-parseable format
//! that the build-time tooling (LiteRT-LM / safetensors converter)
//! emits and the inference task slurps from Synapse VFS at boot.
//!
//! Why not GGUF, safetensors, or onnx directly:
//! - **GGUF** is great but it's a moving target with many quant
//!   schemes baked in; we want our quant decisions visible in our
//!   own format, not inherited from llama.cpp's release cadence.
//! - **safetensors** is JSON-headered which means pulling in a JSON
//!   parser into no_std. Avoidable.
//! - **ONNX** is a graph format. Overkill — we know the model
//!   topology at compile time (it's hardcoded in the forward-pass
//!   code).
//!
//! `.fbin` is the minimum viable container: a fixed header tells you
//! how many tensors live in the file and where their names + shapes
//! live; the data section is just raw bytes per tensor, page-aligned
//! so the kernel can DMA straight in once we have a real VFS pipe.
//!
//! ## Layout
//!
//! ```text
//! offset  size       field
//! ──────  ────       ─────
//! 0x00    4          magic = b"FBN1"
//! 0x04    2          version = 1
//! 0x06    2          n_tensors
//! 0x08    8          metadata_len   (bytes from 0x10 to data section)
//! 0x10    metadata_len
//!                    [TensorMetadata × n_tensors]
//! aligned-up-to-4096 (padding zeros)
//!                    [tensor_data]
//! ```
//!
//! Each `TensorMetadata` entry:
//! ```text
//! 0      2     name_len
//! 2      N     name (UTF-8, ASCII subset in practice)
//! 2+N    1     dtype  (0 = F32, 1 = Q8, 2 = Q4 — Q* are reserved
//!                       for D.3.3 onward)
//! 3+N    1     rank
//! 4+N    rank*4   shape (u32 each)
//! 4+N+r*4  8   data_offset (relative to start of file)
//! 12+N+r*4 8   data_len (bytes)
//! ```
//!
//! Variable-length records — caller iterates by walking the metadata
//! section in declaration order. Names are hashed to lookup ID at
//! parse time so subsequent lookups are O(1).

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

pub const MAGIC: u32 = u32::from_le_bytes(*b"FBN1");
pub const VERSION: u16 = 1;
/// Data section is page-aligned in the file. Public so the build-time
/// converter (D.3.1.2) and the kernel-side VFS reader can both align
/// on the same constant.
#[allow(dead_code)]
pub const PAGE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DType {
    F32 = 0,
    /// Quantized 8-bit. Reserved for D.3.3+.
    #[allow(dead_code)]
    Q8 = 1,
    /// Quantized 4-bit. Reserved for D.3.3+.
    #[allow(dead_code)]
    Q4 = 2,
}

impl DType {
    fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(DType::F32),
            1 => Some(DType::Q8),
            2 => Some(DType::Q4),
            _ => None,
        }
    }

    /// Bytes per element. For quantized types, returns the *unpacked*
    /// element size since the storage format is the consumer's
    /// problem; the metadata's `data_len` is the source of truth for
    /// raw bytes.
    #[allow(dead_code)] // used by D.3.3+ when quantized loaders land
    pub fn elem_size(self) -> usize {
        match self {
            DType::F32 => 4,
            DType::Q8 => 1,
            DType::Q4 => 1,  // 2 packed nibbles per byte; caller decodes
        }
    }
}

#[derive(Debug)]
pub struct TensorMeta {
    pub name: String,
    pub dtype: DType,
    pub shape: Vec<u32>,
    /// Offset within the file's data section, relative to start-of-file.
    pub data_offset: u64,
    pub data_len: u64,
}

#[derive(Debug)]
#[allow(dead_code)] // payload fields are only read via the derived Debug impl
pub enum LoadError {
    BadMagic,
    BadVersion(u16),
    Truncated { needed: usize, got: usize },
    InvalidDType(u8),
    NameNotUtf8,
    DataOutOfRange { tensor: usize },
}

/// Parsed `.fbin` view. Holds an immutable byte slice and the parsed
/// metadata. `data_for(...)` returns a borrowed slice into the
/// original buffer — no copy.
pub struct FbinView<'a> {
    bytes: &'a [u8],
    pub tensors: Vec<TensorMeta>,
}

impl<'a> FbinView<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, LoadError> {
        if bytes.len() < 0x10 {
            return Err(LoadError::Truncated { needed: 0x10, got: bytes.len() });
        }
        let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic != MAGIC { return Err(LoadError::BadMagic); }

        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != VERSION { return Err(LoadError::BadVersion(version)); }

        let n_tensors = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
        let metadata_len = u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11],
            bytes[12], bytes[13], bytes[14], bytes[15],
        ]) as usize;
        let metadata_end = 0x10 + metadata_len;
        if bytes.len() < metadata_end {
            return Err(LoadError::Truncated { needed: metadata_end, got: bytes.len() });
        }

        let mut cur = 0x10;
        let mut tensors = Vec::with_capacity(n_tensors);

        for ti in 0..n_tensors {
            if cur + 2 > metadata_end {
                return Err(LoadError::Truncated { needed: cur + 2, got: metadata_end });
            }
            let name_len = u16::from_le_bytes([bytes[cur], bytes[cur + 1]]) as usize;
            cur += 2;
            if cur + name_len + 2 > metadata_end {
                return Err(LoadError::Truncated {
                    needed: cur + name_len + 2,
                    got: metadata_end,
                });
            }
            let name = match core::str::from_utf8(&bytes[cur..cur + name_len]) {
                Ok(s) => String::from(s),
                Err(_) => return Err(LoadError::NameNotUtf8),
            };
            cur += name_len;
            let dtype_byte = bytes[cur]; cur += 1;
            let rank = bytes[cur] as usize; cur += 1;
            let shape_bytes = rank * 4;
            if cur + shape_bytes + 16 > metadata_end {
                return Err(LoadError::Truncated {
                    needed: cur + shape_bytes + 16,
                    got: metadata_end,
                });
            }
            let mut shape = Vec::with_capacity(rank);
            for k in 0..rank {
                let off = cur + k * 4;
                shape.push(u32::from_le_bytes([
                    bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3],
                ]));
            }
            cur += shape_bytes;
            let data_offset = u64::from_le_bytes([
                bytes[cur],     bytes[cur + 1], bytes[cur + 2], bytes[cur + 3],
                bytes[cur + 4], bytes[cur + 5], bytes[cur + 6], bytes[cur + 7],
            ]);
            cur += 8;
            let data_len = u64::from_le_bytes([
                bytes[cur],     bytes[cur + 1], bytes[cur + 2], bytes[cur + 3],
                bytes[cur + 4], bytes[cur + 5], bytes[cur + 6], bytes[cur + 7],
            ]);
            cur += 8;

            // Bounds-check the data range right here so subsequent
            // `data_for` calls can be one-line slice indexes.
            let end = data_offset.saturating_add(data_len) as usize;
            if end > bytes.len() {
                return Err(LoadError::DataOutOfRange { tensor: ti });
            }

            let dtype = DType::from_u8(dtype_byte)
                .ok_or(LoadError::InvalidDType(dtype_byte))?;

            tensors.push(TensorMeta {
                name, dtype, shape, data_offset, data_len,
            });
        }

        Ok(Self { bytes, tensors })
    }

    /// Find a tensor by name. Linear scan — fine for the tens-to-low-
    /// hundreds of tensors a 0.5B model contains; worth indexing if a
    /// future model pushes 10k+ tensors.
    pub fn find(&self, name: &str) -> Option<&TensorMeta> {
        self.tensors.iter().find(|t| t.name == name)
    }

    /// Borrow the raw bytes of a tensor's data region. Zero-copy.
    pub fn data_for(&self, t: &TensorMeta) -> &[u8] {
        let start = t.data_offset as usize;
        let end = start + t.data_len as usize;
        &self.bytes[start..end]
    }

    /// Decode an F32 tensor's bytes into a `Vec<f32>`. Panics (in
    /// debug) if the tensor isn't F32 or the byte length doesn't
    /// match `prod(shape) * 4`. This is the path D.3.3+ uses to feed
    /// embedding tables / projection matrices into matmul.
    pub fn read_f32(&self, t: &TensorMeta) -> Option<Vec<f32>> {
        if t.dtype != DType::F32 { return None; }
        let bytes = self.data_for(t);
        let n_elems: usize = t.shape.iter().map(|&d| d as usize).product();
        if bytes.len() != n_elems * 4 { return None; }
        let mut out = Vec::with_capacity(n_elems);
        let mut off = 0;
        while off + 4 <= bytes.len() {
            out.push(f32::from_le_bytes([
                bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3],
            ]));
            off += 4;
        }
        Some(out)
    }
}

/// Compute a simple FNV-1a 64-bit hash over the bytes — used for
/// integrity checks in boot tests. Not crypto; it's specifically
/// chosen because it's small enough to inline and fits the
/// "checksum the loaded tensor" pattern without pulling in a real
/// hash crate.
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xCBF29CE484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001B3);
    }
    h
}
