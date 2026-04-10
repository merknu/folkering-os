//! All compile-time constants used by the inference server.

use libfolk::sys::block::SECTOR_SIZE;

// ── IPC opcodes (must match libfolk::sys::inference) ───────────────────

pub const INFERENCE_TASK_ID: u32 = 6;

pub const INFER_OP_PING: u64 = 0;
pub const INFER_OP_GENERATE: u64 = 1;
pub const INFER_OP_STATUS: u64 = 2;
pub const INFER_OP_ASK: u64 = 3;
pub const INFER_OP_ASK_ASYNC: u64 = 4;

// ── Memory layout ──────────────────────────────────────────────────────

/// Bump arena size: 16MB for tokenizer tables (152K vocab) + inference buffers
pub const ARENA_SIZE: usize = 16 * 1024 * 1024;

/// Maximum GGUF model size we'll attempt to load.
/// 4GB supports up to 7B Q4 quantized models.
pub const MAX_MODEL_SIZE: usize = 4 * 1024 * 1024 * 1024;

/// Virtual address for model mmap region
pub const MODEL_MMAP_BASE: usize = 0x1_0000_0000;

/// Virtual address for mapping request/response shmem (ULTRA 43)
/// Must not overlap with MMAP_BASE (0x4000_0000) region used by arena/KV-cache
pub const INFER_SHMEM_VADDR: usize = 0x20000000;

/// Virtual address for mapping TokenRing shmem (ULTRA 43: isolated from I/O shmem)
pub const RING_SHMEM_VADDR: usize = 0x22000000;

/// Phase B3: Pre-allocated logits buffer (eliminates per-token arena alloc).
/// 1 MB = room for up to 256K-token vocabularies.
pub const LOGITS_BUF_VADDR: usize = 0x60000000;
pub const LOGITS_BUF_SIZE: usize = 1024 * 1024;

// ── Inference parameters ───────────────────────────────────────────────

/// Maximum tokens to generate per request (512 to allow <think> + visible response)
pub const MAX_GEN_TOKENS: usize = 512;

/// KV-cache window size (power of 2).
/// With Q8_0 quantization, 1024 tokens uses ~3.1MB (same as 256 with f32).
pub const KV_WINDOW_SIZE: usize = 1024;

// ── Default sampling parameters (overridable via control sector 258) ──

pub const DEFAULT_TEMPERATURE: f32 = 0.8;
pub const DEFAULT_REP_PENALTY: f32 = 1.15;
pub const DEFAULT_TOP_P: f32 = 0.9;
pub const DEFAULT_TOP_K: u32 = 0;       // 0 = disabled (use Top-P only)
pub const DEFAULT_REP_WINDOW: usize = 32;

/// Control sector number — MCP tools write config here, inference server reads
pub const CONTROL_SECTOR: u64 = 258;

/// Health telemetry sector — MSE between consecutive logits
pub const HEALTH_SECTOR: u64 = 259;

/// Default MSE threshold for logit collapse detection
pub const DEFAULT_DRIFT_THRESHOLD: f32 = 0.001;

// ── Debug mailbox layout (sectors 1-257, 128KB) ────────────────────────

/// Debug mailbox: sector 1 = header, sectors 2-257 = data (max 32768 f32, 128KB)
pub const DUMP_HEADER_SECTOR: u64 = 1;
pub const DUMP_DATA_SECTOR: u64 = 2;
pub const DUMP_MAX_SECTORS: usize = 256;
pub const DUMP_MAX_FLOATS: usize = DUMP_MAX_SECTORS * SECTOR_SIZE / 4; // 32768
