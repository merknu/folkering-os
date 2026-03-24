//! GGUF Parser — Zero-Copy Model Loading (ULTRA 2)
//!
//! Parses GGML Unified Format (GGUF v3) files. Returns slices into
//! mmap'd memory — zero allocation for tensor data.
//!
//! GGUF spec: https://github.com/ggerganov/ggml/blob/master/docs/gguf.md

extern crate alloc;
use alloc::vec::Vec;

/// GGUF magic number: "GGUF" as little-endian u32
/// ASCII: G(0x47) G(0x47) U(0x55) F(0x46) → LE u32 = 0x46554747
const GGUF_MAGIC: u32 = 0x46554747;

/// GGUF metadata value types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum GgufValueType {
    Uint8 = 0,
    Int8 = 1,
    Uint16 = 2,
    Int16 = 3,
    Uint32 = 4,
    Int32 = 5,
    Float32 = 6,
    Bool = 7,
    String = 8,
    Array = 9,
    Uint64 = 10,
    Int64 = 11,
    Float64 = 12,
}

/// GGUF tensor data types (matching GGML)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum GgufDtype {
    F32 = 0,
    F16 = 1,
    Q4_0 = 2,
    Q4_1 = 3,
    Q5_0 = 6,
    Q5_1 = 7,
    Q8_0 = 8,
    Q8_1 = 9,
    Q2K = 10,
    Q3K = 11,
    Q4K = 12,
    Q5K = 13,
    Q6K = 14,
    IQ2XXS = 16,
    IQ2XS = 17,
    Unknown = 255,
}

impl GgufDtype {
    fn from_u32(v: u32) -> Self {
        match v {
            0 => Self::F32,
            1 => Self::F16,
            2 => Self::Q4_0,
            3 => Self::Q4_1,
            6 => Self::Q5_0,
            7 => Self::Q5_1,
            8 => Self::Q8_0,
            9 => Self::Q8_1,
            10 => Self::Q2K,
            11 => Self::Q3K,
            12 => Self::Q4K,
            13 => Self::Q5K,
            14 => Self::Q6K,
            16 => Self::IQ2XXS,
            17 => Self::IQ2XS,
            _ => Self::Unknown,
        }
    }

    /// Bytes per block for this dtype
    pub fn block_size(&self) -> usize {
        match self {
            Self::F32 => 4,     // 1 element per "block"
            Self::F16 => 2,
            Self::Q4_0 => 18,   // 32 values per block
            Self::Q4_1 => 20,
            Self::Q8_0 => 34,   // 32 values per block (f16 scale + 32 i8)
            Self::Q8_1 => 40,
            Self::Q2K => 84,    // 256 values per block
            Self::Q3K => 110,
            Self::Q4K => 144,
            Self::Q5K => 176,
            Self::Q6K => 210,   // 256 values per block (ql[128]+qh[64]+sc[16]+d[2])
            _ => 0, // unsupported
        }
    }

    /// Values per block
    pub fn values_per_block(&self) -> usize {
        match self {
            Self::F32 => 1,
            Self::F16 => 1,
            Self::Q4_0 | Self::Q4_1 => 32,
            Self::Q8_0 | Self::Q8_1 => 32,
            Self::Q2K | Self::Q3K | Self::Q4K | Self::Q5K | Self::Q6K => 256,
            _ => 1,
        }
    }
}

/// Parsed GGUF tensor — zero-copy reference into mmap'd data
#[derive(Clone)]
pub struct GgufTensor<'a> {
    pub name: &'a str,
    pub data: &'a [u8],
    pub shape: [u32; 4],
    pub ndim: usize,
    pub dtype: GgufDtype,
}

impl<'a> GgufTensor<'a> {
    /// Total number of elements in this tensor
    pub fn numel(&self) -> usize {
        let mut n = 1usize;
        for i in 0..self.ndim {
            n *= self.shape[i] as usize;
        }
        n
    }

    /// Expected byte size of this tensor's data
    pub fn byte_size(&self) -> usize {
        let numel = self.numel();
        let vpb = self.dtype.values_per_block();
        let n_blocks = (numel + vpb - 1) / vpb;
        n_blocks * self.dtype.block_size()
    }
}

/// Parsed GGUF model metadata
#[derive(Debug, Clone)]
pub struct GgufMetadata {
    pub architecture: GgufString,
    pub vocab_size: u32,
    pub embedding_dim: u32,
    pub n_heads: u32,
    pub n_kv_heads: u32,
    pub n_layers: u32,
    pub context_length: u32,
    pub head_dim: u32,
    pub intermediate_size: u32,
    pub rope_base: f32,
    pub rms_norm_eps: f32,
    /// BOS token ID from tokenizer.ggml.bos_token_id
    pub bos_token_id: u32,
    /// EOS token ID from tokenizer.ggml.eos_token_id
    pub eos_token_id: u32,
    /// Byte offset in GGUF data where the tokens string array starts
    pub vocab_data_offset: usize,
    /// Byte offset in GGUF data where the scores float array starts
    pub scores_data_offset: usize,
    /// Byte offset in GGUF data where the merges string array starts
    pub merges_data_offset: usize,
    /// Number of BPE merge rules
    pub merges_count: u32,
    /// Unknown/replacement token ID (U+FFFD fallback for invalid bytes)
    pub unknown_token_id: u32,
    /// Byte offset to tokenizer.ggml.token_type array (i32 per token)
    pub token_type_offset: usize,
    /// Tokenizer model type string (e.g. "gpt2", "llama")
    pub tokenizer_model: GgufString,
    /// Whether to add BOS token automatically
    pub add_bos_token: bool,
}

/// Small inline string for metadata (avoids String allocation)
#[derive(Debug, Clone)]
pub struct GgufString {
    buf: [u8; 64],
    len: usize,
}

impl GgufString {
    fn new() -> Self {
        Self { buf: [0; 64], len: 0 }
    }

    fn from_bytes(s: &[u8]) -> Self {
        let mut gs = Self::new();
        let copy_len = s.len().min(63);
        gs.buf[..copy_len].copy_from_slice(&s[..copy_len]);
        gs.len = copy_len;
        gs
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

/// Parsed GGUF model — zero-copy tensor views into mmap'd data
pub struct GgufModel<'a> {
    pub metadata: GgufMetadata,
    pub tensors: Vec<GgufTensor<'a>>,
}

/// GGUF parse error
#[derive(Debug)]
pub enum GgufError {
    InvalidMagic,
    UnsupportedVersion(u32),
    TruncatedData,
    InvalidMetadata,
    InvalidTensor,
}

/// Cursor for reading through GGUF binary data
struct GgufCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> GgufCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read_u8(&mut self) -> Result<u8, GgufError> {
        if self.pos >= self.data.len() { return Err(GgufError::TruncatedData); }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn read_u16(&mut self) -> Result<u16, GgufError> {
        if self.pos + 2 > self.data.len() { return Err(GgufError::TruncatedData); }
        let v = u16::from_le_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    fn read_u32(&mut self) -> Result<u32, GgufError> {
        if self.pos + 4 > self.data.len() { return Err(GgufError::TruncatedData); }
        let v = u32::from_le_bytes([
            self.data[self.pos], self.data[self.pos + 1],
            self.data[self.pos + 2], self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    fn read_u64(&mut self) -> Result<u64, GgufError> {
        if self.pos + 8 > self.data.len() { return Err(GgufError::TruncatedData); }
        let v = u64::from_le_bytes([
            self.data[self.pos], self.data[self.pos + 1],
            self.data[self.pos + 2], self.data[self.pos + 3],
            self.data[self.pos + 4], self.data[self.pos + 5],
            self.data[self.pos + 6], self.data[self.pos + 7],
        ]);
        self.pos += 8;
        Ok(v)
    }

    fn read_i8(&mut self) -> Result<i8, GgufError> {
        Ok(self.read_u8()? as i8)
    }

    fn read_i32(&mut self) -> Result<i32, GgufError> {
        Ok(self.read_u32()? as i32)
    }

    fn read_f32(&mut self) -> Result<f32, GgufError> {
        let bits = self.read_u32()?;
        Ok(f32::from_bits(bits))
    }

    fn read_bool(&mut self) -> Result<bool, GgufError> {
        Ok(self.read_u8()? != 0)
    }

    fn read_string(&mut self) -> Result<&'a str, GgufError> {
        let len = self.read_u64()? as usize;
        if self.pos + len > self.data.len() { return Err(GgufError::TruncatedData); }
        let s = core::str::from_utf8(&self.data[self.pos..self.pos + len])
            .map_err(|_| GgufError::InvalidMetadata)?;
        self.pos += len;
        Ok(s)
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], GgufError> {
        if self.pos + len > self.data.len() { return Err(GgufError::TruncatedData); }
        let s = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(s)
    }

    /// Skip a GGUF value based on its type
    fn skip_value(&mut self, vtype: u32) -> Result<(), GgufError> {
        match vtype {
            0 => { self.pos += 1; } // u8
            1 => { self.pos += 1; } // i8
            2 => { self.pos += 2; } // u16
            3 => { self.pos += 2; } // i16
            4 => { self.pos += 4; } // u32
            5 => { self.pos += 4; } // i32
            6 => { self.pos += 4; } // f32
            7 => { self.pos += 1; } // bool
            8 => { let _ = self.read_string()?; } // string
            9 => {
                // Array: type + count + elements
                let elem_type = self.read_u32()?;
                let count = self.read_u64()? as usize;
                for _ in 0..count {
                    self.skip_value(elem_type)?;
                }
            }
            10 => { self.pos += 8; } // u64
            11 => { self.pos += 8; } // i64
            12 => { self.pos += 8; } // f64
            _ => return Err(GgufError::InvalidMetadata),
        }
        if self.pos > self.data.len() {
            return Err(GgufError::TruncatedData);
        }
        Ok(())
    }

    /// Align position to the given boundary
    fn align(&mut self, alignment: usize) {
        let rem = self.pos % alignment;
        if rem != 0 {
            self.pos += alignment - rem;
        }
    }
}

impl<'a> GgufModel<'a> {
    /// Parse a GGUF file from mmap'd data.
    ///
    /// Returns zero-copy tensor slices pointing into `data`.
    /// The `data` slice must remain valid for the lifetime of the returned model.
    pub fn parse(data: &'a [u8]) -> Result<Self, GgufError> {
        let mut cursor = GgufCursor::new(data);

        // Header
        let magic = cursor.read_u32()?;
        if magic != GGUF_MAGIC {
            return Err(GgufError::InvalidMagic);
        }

        let version = cursor.read_u32()?;
        if version < 2 || version > 3 {
            return Err(GgufError::UnsupportedVersion(version));
        }

        let tensor_count = cursor.read_u64()? as usize;
        let metadata_kv_count = cursor.read_u64()? as usize;

        // Parse metadata KV pairs
        let mut metadata = GgufMetadata {
            architecture: GgufString::new(),
            vocab_size: 0,
            embedding_dim: 0,
            n_heads: 0,
            n_kv_heads: 0,
            n_layers: 0,
            context_length: 2048,
            head_dim: 0,
            intermediate_size: 0,
            rope_base: 10000.0,
            rms_norm_eps: 1e-5,
            bos_token_id: u32::MAX, // sentinel: not specified in GGUF
            eos_token_id: u32::MAX, // sentinel: not specified in GGUF
            vocab_data_offset: 0,
            scores_data_offset: 0,
            merges_data_offset: 0,
            merges_count: 0,
            unknown_token_id: u32::MAX, // MAX = not specified
            token_type_offset: 0,
            tokenizer_model: GgufString::new(),
            add_bos_token: false,
        };

        for _ in 0..metadata_kv_count {
            let key = cursor.read_string()?;
            let vtype = cursor.read_u32()?;

            // Extract known metadata fields
            match key {
                "general.architecture" => {
                    if vtype == 8 {
                        let val = cursor.read_string()?;
                        metadata.architecture = GgufString::from_bytes(val.as_bytes());
                    } else {
                        cursor.skip_value(vtype)?;
                    }
                }
                k if k.ends_with(".vocab_size") => {
                    if vtype == 4 { metadata.vocab_size = cursor.read_u32()?; }
                    else if vtype == 5 { metadata.vocab_size = cursor.read_i32()? as u32; }
                    else { cursor.skip_value(vtype)?; }
                }
                k if k.ends_with(".embedding_length") => {
                    if vtype == 4 { metadata.embedding_dim = cursor.read_u32()?; }
                    else if vtype == 5 { metadata.embedding_dim = cursor.read_i32()? as u32; }
                    else { cursor.skip_value(vtype)?; }
                }
                k if k.ends_with(".attention.head_count") && !k.contains("head_count_kv") => {
                    if vtype == 4 { metadata.n_heads = cursor.read_u32()?; }
                    else if vtype == 5 { metadata.n_heads = cursor.read_i32()? as u32; }
                    else { cursor.skip_value(vtype)?; }
                }
                k if k.ends_with(".attention.head_count_kv") => {
                    if vtype == 4 { metadata.n_kv_heads = cursor.read_u32()?; }
                    else if vtype == 5 { metadata.n_kv_heads = cursor.read_i32()? as u32; }
                    else { cursor.skip_value(vtype)?; }
                }
                k if k.ends_with(".block_count") => {
                    if vtype == 4 { metadata.n_layers = cursor.read_u32()?; }
                    else if vtype == 5 { metadata.n_layers = cursor.read_i32()? as u32; }
                    else { cursor.skip_value(vtype)?; }
                }
                k if k.ends_with(".context_length") => {
                    if vtype == 4 { metadata.context_length = cursor.read_u32()?; }
                    else if vtype == 5 { metadata.context_length = cursor.read_i32()? as u32; }
                    else { cursor.skip_value(vtype)?; }
                }
                k if k.ends_with(".feed_forward_length") => {
                    if vtype == 4 { metadata.intermediate_size = cursor.read_u32()?; }
                    else if vtype == 5 { metadata.intermediate_size = cursor.read_i32()? as u32; }
                    else { cursor.skip_value(vtype)?; }
                }
                k if k.ends_with(".rope.freq_base") => {
                    if vtype == 6 { metadata.rope_base = cursor.read_f32()?; }
                    else { cursor.skip_value(vtype)?; }
                }
                k if k.ends_with(".attention.layer_norm_rms_epsilon") => {
                    if vtype == 6 { metadata.rms_norm_eps = cursor.read_f32()?; }
                    else { cursor.skip_value(vtype)?; }
                }
                k if k.ends_with(".attention.key_length") => {
                    // Explicit head_dim from GGUF (overrides dim/n_heads default)
                    if vtype == 4 { metadata.head_dim = cursor.read_u32()?; }
                    else if vtype == 5 { metadata.head_dim = cursor.read_i32()? as u32; }
                    else { cursor.skip_value(vtype)?; }
                }
                "tokenizer.ggml.bos_token_id" => {
                    if vtype == 4 { metadata.bos_token_id = cursor.read_u32()?; }
                    else if vtype == 5 { metadata.bos_token_id = cursor.read_i32()? as u32; }
                    else { cursor.skip_value(vtype)?; }
                }
                "tokenizer.ggml.eos_token_id" => {
                    if vtype == 4 { metadata.eos_token_id = cursor.read_u32()?; }
                    else if vtype == 5 { metadata.eos_token_id = cursor.read_i32()? as u32; }
                    else { cursor.skip_value(vtype)?; }
                }
                "tokenizer.ggml.tokens" => {
                    // Array of strings — record offset AND set vocab_size if not yet set
                    if vtype == 9 {
                        let elem_type = cursor.read_u32()?;
                        let count = cursor.read_u64()? as usize;
                        metadata.vocab_data_offset = cursor.pos;
                        // Set vocab_size from token array if not already set by arch key
                        if metadata.vocab_size == 0 {
                            metadata.vocab_size = count as u32;
                        }
                        // Skip all string elements
                        for _ in 0..count {
                            cursor.skip_value(elem_type)?;
                        }
                    } else {
                        cursor.skip_value(vtype)?;
                    }
                }
                "tokenizer.ggml.scores" => {
                    // Array of f32 — record offset before skipping
                    if vtype == 9 {
                        let _elem_type = cursor.read_u32()?;
                        let count = cursor.read_u64()? as usize;
                        metadata.scores_data_offset = cursor.pos;
                        // Skip all f32 elements
                        cursor.pos += count * 4;
                        if cursor.pos > cursor.data.len() {
                            return Err(GgufError::TruncatedData);
                        }
                    } else {
                        cursor.skip_value(vtype)?;
                    }
                }
                "tokenizer.ggml.unknown_token_id" => {
                    if vtype == 4 { metadata.unknown_token_id = cursor.read_u32()?; }
                    else if vtype == 5 { metadata.unknown_token_id = cursor.read_i32()? as u32; }
                    else { cursor.skip_value(vtype)?; }
                }
                "tokenizer.ggml.token_type" => {
                    // Array of i32 — token type per vocab entry
                    // 1=normal, 2=unknown, 3=control, 4=user_defined, 5=unused, 6=byte
                    if vtype == 9 {
                        let _elem_type = cursor.read_u32()?;
                        let count = cursor.read_u64()? as usize;
                        metadata.token_type_offset = cursor.pos;
                        cursor.pos += count * 4;
                        if cursor.pos > cursor.data.len() {
                            return Err(GgufError::TruncatedData);
                        }
                    } else {
                        cursor.skip_value(vtype)?;
                    }
                }
                "tokenizer.ggml.model" => {
                    if vtype == 8 {
                        let s = cursor.read_string()?;
                        metadata.tokenizer_model = GgufString::from_bytes(s.as_bytes());
                    } else {
                        cursor.skip_value(vtype)?;
                    }
                }
                "tokenizer.ggml.add_bos_token" => {
                    if vtype == 7 { metadata.add_bos_token = cursor.read_u8()? != 0; }
                    else { cursor.skip_value(vtype)?; }
                }
                "tokenizer.ggml.merges" => {
                    // Array of strings — BPE merge rules (e.g. "Ġ t", "i n")
                    // Index = merge rank (lower = higher priority)
                    if vtype == 9 {
                        let elem_type = cursor.read_u32()?;
                        let count = cursor.read_u64()? as usize;
                        metadata.merges_count = count as u32;
                        metadata.merges_data_offset = cursor.pos;
                        for _ in 0..count {
                            cursor.skip_value(elem_type)?;
                        }
                    } else {
                        cursor.skip_value(vtype)?;
                    }
                }
                _ => {
                    cursor.skip_value(vtype)?;
                }
            }
        }

        // Derive head_dim if not set
        if metadata.head_dim == 0 && metadata.n_heads > 0 {
            metadata.head_dim = metadata.embedding_dim / metadata.n_heads;
        }
        // Default n_kv_heads to n_heads (MHA)
        if metadata.n_kv_heads == 0 {
            metadata.n_kv_heads = metadata.n_heads;
        }

        // Parse tensor info (name, shape, dtype, offset)
        struct TensorInfo<'b> {
            name: &'b str,
            shape: [u32; 4],
            ndim: usize,
            dtype: GgufDtype,
            offset: u64,
            byte_size: usize,
        }

        let mut tensor_infos = Vec::with_capacity(tensor_count);

        for _ in 0..tensor_count {
            let name = cursor.read_string()?;
            let ndim = cursor.read_u32()? as usize;
            if ndim > 4 {
                return Err(GgufError::InvalidTensor);
            }

            let mut shape = [1u32; 4];
            for d in 0..ndim {
                shape[d] = cursor.read_u64()? as u32;
            }

            let dtype_raw = cursor.read_u32()?;
            let dtype = GgufDtype::from_u32(dtype_raw);
            let offset = cursor.read_u64()?;

            // Compute byte size
            let numel = {
                let mut n = 1usize;
                for d in 0..ndim { n *= shape[d] as usize; }
                n
            };
            let vpb = dtype.values_per_block();
            let n_blocks = (numel + vpb - 1) / vpb;
            let byte_size = n_blocks * dtype.block_size();

            tensor_infos.push(TensorInfo { name, shape, ndim, dtype, offset, byte_size });
        }

        // Align to data start (GGUF alignment, typically 32 bytes)
        cursor.align(32);
        let data_start = cursor.pos;

        // Create zero-copy tensor slices
        let mut tensors = Vec::with_capacity(tensor_count);
        for info in &tensor_infos {
            let abs_offset = data_start + info.offset as usize;
            if abs_offset + info.byte_size > data.len() {
                return Err(GgufError::TruncatedData);
            }

            tensors.push(GgufTensor {
                name: info.name,
                data: &data[abs_offset..abs_offset + info.byte_size],
                shape: info.shape,
                ndim: info.ndim,
                dtype: info.dtype,
            });
        }

        Ok(GgufModel { metadata, tensors })
    }

    /// Find a tensor by name.
    pub fn tensor(&self, name: &str) -> Option<&GgufTensor<'a>> {
        self.tensors.iter().find(|t| t.name == name)
    }

    /// Find a tensor by name prefix (e.g., "blk.0.attn_q").
    pub fn tensor_prefix(&self, prefix: &str) -> Option<&GgufTensor<'a>> {
        self.tensors.iter().find(|t| t.name.starts_with(prefix))
    }
}
