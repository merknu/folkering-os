//! Wire types shared between callers (e.g. draug-daemon) and the
//! inference task. Lives in this crate today; will graduate into a
//! shared `libfolk-inference` crate once a second consumer exists.

extern crate alloc;

/// Status codes returned to the caller. Mirror libfolk's
/// `llm_generate` `PatchStatus.status` values where they overlap, so
/// existing draug-daemon code that already handles those codes keeps
/// working when it switches from direct-syscall to IPC.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceStatus {
    Ok = 0,
    /// The router picked a backend that can't handle this request
    /// shape today (e.g. local backend before D.2 lands).
    NotImplemented = 1,
    /// Proxy unreachable / TCP timeout / model not loaded host-side.
    ProxyFailed = 2,
    /// Caller's result buffer too small for the produced output.
    /// Reserved for D.2's local backend; proxy never sets it (it
    /// truncates instead).
    #[allow(dead_code)]
    BufferTooSmall = 3,
    /// Shmem map failed or wire header was malformed.
    BadRequest = 4,
}

/// Header at the start of the request shmem region. Caller fills
/// `model`, `prompt_len`, `result_max` before sending. Server fills
/// `status` and `output_len` before replying.
///
/// Layout is `#[repr(C)]` so we can read/write fields via volatile
/// pointer arithmetic without depending on Rust's struct ordering
/// guarantees. Total size ≤ 256 bytes; the rest of the shmem page
/// is `[prompt_bytes ... result_buffer ...]`.
#[repr(C)]
pub struct InferenceWire {
    /// Magic for crash-tolerance: caller poisons it on alloc, server
    /// rejects requests with the wrong magic. 'F' 'I' 'N' 'F' = 0x464E4946 LE.
    pub magic: u32,

    /// Wire version. v1 = this layout. Bump when fields move.
    pub version: u16,

    /// Status (server-written). Mirrors `InferenceStatus` discriminants.
    pub status: u16,

    /// Bytes written by the server into the result buffer.
    pub output_len: u32,

    /// Length of the prompt in `prompt_bytes`. Caller-written.
    pub prompt_len: u32,

    /// Max bytes the caller's result buffer can hold. Caller-written.
    pub result_max: u32,

    /// Offset (within the shmem page, from the start of the page) of
    /// the prompt's first byte. Typically `sizeof(InferenceWire)`,
    /// rounded to 16 for alignment.
    pub prompt_off: u32,

    /// Offset of the result buffer's first byte. Must be after the
    /// prompt's last byte. Caller's responsibility to compute.
    pub result_off: u32,

    /// Null-padded model name (e.g. "qwen2.5-coder:7b"). 64 bytes
    /// is plenty — Ollama's longest production names are <30 chars.
    pub model: [u8; 64],
}

pub const WIRE_MAGIC: u32 = 0x464E_4946; // 'FINF' little-endian
pub const WIRE_VERSION: u16 = 1;
