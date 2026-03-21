//! Inference Server IPC Client (M39, M43)
//!
//! Client API for communicating with the Inference Server (Task 6).
//! Provides RAG-enhanced text generation via IPC + shared memory.

use crate::syscall::{syscall3, SYS_IPC_SEND};

/// Inference Server task ID (spawned at boot as Task 6)
pub const INFERENCE_TASK_ID: u32 = 6;

// ============================================================================
// IPC Opcodes
// ============================================================================

/// Ping inference server
/// Request: INFER_OP_PING
/// Reply: 1 = model loaded, 0 = stub mode
pub const INFER_OP_PING: u64 = 0;

/// Generate text from prompt
/// Request: INFER_OP_GENERATE | (shmem_handle << 16) | (prompt_len << 32)
///   shmem contains: [prompt bytes] (UTF-8)
/// Reply: (output_len << 32) | shmem_handle with generated text
pub const INFER_OP_GENERATE: u64 = 1;

/// Get inference server status
/// Request: INFER_OP_STATUS
/// Reply: (arena_size << 32) | has_model
pub const INFER_OP_STATUS: u64 = 2;

/// RAG-enhanced query (M39)
/// Request: INFER_OP_ASK | (shmem_handle << 16) | (query_len << 32)
///   shmem contains: [query bytes] (UTF-8)
/// Reply: (output_len << 32) | shmem_handle with answer
pub const INFER_OP_ASK: u64 = 3;

/// Async inference with token streaming (M43)
/// Request: INFER_OP_ASK_ASYNC
///   payload0: opcode[0:16] | query_shmem[16:32] | query_len[32:48] | ring_shmem[48:64]
/// Reply: 0 = accepted, u64::MAX = busy/error
pub const INFER_OP_ASK_ASYNC: u64 = 4;

// ============================================================================
// Error types
// ============================================================================

/// Inference error
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferError {
    /// Server not responding
    ServiceUnavailable,
    /// No model loaded
    NoModel,
    /// Out of memory (arena exhausted)
    OutOfMemory,
    /// IPC error
    IpcFailed,
}

// ============================================================================
// Client API
// ============================================================================

/// Ping the inference server.
/// Returns true if model is loaded, false if running in stub mode.
pub fn ping() -> Result<bool, InferError> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, INFERENCE_TASK_ID as u64, INFER_OP_PING, 0)
    };

    if ret == u64::MAX {
        return Err(InferError::ServiceUnavailable);
    }

    Ok(ret == 1)
}

/// Get inference server status.
/// Returns (has_model, arena_size_bytes).
pub fn status() -> Result<(bool, usize), InferError> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, INFERENCE_TASK_ID as u64, INFER_OP_STATUS, 0)
    };

    if ret == u64::MAX {
        return Err(InferError::ServiceUnavailable);
    }

    let has_model = (ret & 0xFFFFFFFF) != 0;
    let arena_size = ((ret >> 32) & 0xFFFFFFFF) as usize;

    Ok((has_model, arena_size))
}

/// Send a generate request to the inference server.
///
/// # Arguments
/// * `shmem_handle` - Shared memory containing prompt text (granted to Task 6)
/// * `prompt_len` - Length of prompt in bytes
///
/// # Returns
/// * `Ok((output_shmem, output_len))` - Shmem handle and length of generated text
/// * `Err(...)` - Error
pub fn generate(shmem_handle: u32, prompt_len: usize) -> Result<(u32, usize), InferError> {
    let request = INFER_OP_GENERATE
        | ((shmem_handle as u64) << 16)
        | ((prompt_len as u64) << 32);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, INFERENCE_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        return Err(InferError::ServiceUnavailable);
    }

    let out_shmem = (ret & 0xFFFF) as u32;
    let out_len = ((ret >> 32) & 0xFFFFFFFF) as usize;

    if out_shmem == 0 && out_len == 0 {
        return Err(InferError::NoModel);
    }

    Ok((out_shmem, out_len))
}

/// Send an ask request (RAG-enhanced generation).
///
/// Same protocol as generate but with automatic RAG context retrieval.
pub fn ask(shmem_handle: u32, query_len: usize) -> Result<(u32, usize), InferError> {
    let request = INFER_OP_ASK
        | ((shmem_handle as u64) << 16)
        | ((query_len as u64) << 32);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, INFERENCE_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        return Err(InferError::ServiceUnavailable);
    }

    let out_shmem = (ret & 0xFFFF) as u32;
    let out_len = ((ret >> 32) & 0xFFFFFFFF) as usize;

    if out_shmem == 0 && out_len == 0 {
        return Err(InferError::NoModel);
    }

    Ok((out_shmem, out_len))
}

/// Send an async inference request with token streaming.
///
/// Returns immediately. Tokens are streamed to the ring_shmem buffer.
/// Compositor polls ring.write_idx (Acquire) and ring.status for completion.
///
/// # Arguments
/// * `query_shmem` - Shmem handle containing query text (granted to Task 6)
/// * `query_len` - Length of query in bytes
/// * `ring_shmem` - Shmem handle for TokenRing (16KB, 4 pages, granted to Task 6)
///
/// # Returns
/// * `Ok(())` - Request accepted, tokens will stream
/// * `Err(InferError::ServiceUnavailable)` - Server busy or unavailable
pub fn ask_async(query_shmem: u32, query_len: usize, ring_shmem: u32) -> Result<(), InferError> {
    // Pack all values into payload0:
    // opcode[0:16] | query_shmem[16:32] | query_len[32:48] | ring_shmem[48:64]
    let request = INFER_OP_ASK_ASYNC
        | ((query_shmem as u64) << 16)
        | ((query_len as u64) << 32)
        | ((ring_shmem as u64) << 48);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, INFERENCE_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        return Err(InferError::ServiceUnavailable);
    }

    Ok(())
}
