//! Synapse Protocol - Data Kernel IPC Interface
//!
//! Synapse is the "Data Kernel" of Folkering OS. It manages all data operations
//! and provides a unified interface for file access, queries, and eventually
//! AI-powered semantic search.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────┐     IPC      ┌─────────┐
//! │  Shell  │◄────────────►│ Synapse │
//! │  (App)  │              │ (Data)  │
//! └─────────┘              └─────────┘
//! ```
//!
//! # Protocol
//!
//! All operations use the standard IPC message format:
//! - payload[0]: Operation code (SYN_OP_*)
//! - payload[1]: Parameter 1 (operation-specific)
//! - payload[2]: Parameter 2 (operation-specific)
//! - payload[3]: Parameter 3 (operation-specific)
//!
//! Replies use the same format with status in payload[0].

use crate::syscall::{syscall3, SYS_IPC_SEND};

// ============================================================================
// Well-Known Task IDs
// ============================================================================

/// Synapse service task ID (spawned at boot as Task 2)
///
/// Task layout: 1=Idle, 2=Synapse, 3=Shell, 4=Compositor
pub const SYNAPSE_TASK_ID: u32 = 2;

// ============================================================================
// Operation Codes
// ============================================================================

/// List files in the data store
/// Request: [OP, offset, limit, 0]
/// Reply: [count, 0, 0, 0] (entries follow via shared memory in v2)
pub const SYN_OP_LIST_FILES: u64 = 0x0001;

/// Read file metadata by name hash
/// Request: [OP, name_hash_lo, name_hash_hi, 0]
/// Reply: [size, entry_type, 0, 0]
pub const SYN_OP_FILE_INFO: u64 = 0x0002;

/// Read file contents (legacy - returns metadata only)
/// Request: [OP, file_id, offset, length]
/// Reply: [bytes_read, data_lo, data_hi, 0] (for small reads)
pub const SYN_OP_READ_FILE: u64 = 0x0003;

/// Look up file by name hash
/// Request: op | (name_hash << 16)
/// Reply: (size << 32) | file_id, or SYN_STATUS_NOT_FOUND
pub const SYN_OP_READ_FILE_BY_NAME: u64 = 0x0006;

/// Read 8-byte chunk from file
/// Request: op | (file_id << 16), offset as second arg
/// Reply: 8 bytes of file data (or fewer at EOF, padded with zeros)
pub const SYN_OP_READ_FILE_CHUNK: u64 = 0x0007;

/// Read file via shared memory (zero-copy)
/// Request: op | (name_hash << 16)
/// Reply: (size << 32) | shmem_handle, or SYN_STATUS_NOT_FOUND
/// The caller must map the shmem_handle to read the file contents
pub const SYN_OP_READ_FILE_SHMEM: u64 = 0x0008;

/// Get file count
/// Request: [OP, 0, 0, 0]
/// Reply: [count, 0, 0, 0]
pub const SYN_OP_FILE_COUNT: u64 = 0x0004;

/// Get file entry by index
/// Request: [OP, index, 0, 0]
/// Reply: [id, size, type, name_hash]
pub const SYN_OP_FILE_BY_INDEX: u64 = 0x0005;

/// Ping - check if Synapse is alive
/// Request: [OP, magic, 0, 0]
/// Reply: [magic ^ 0x5959, version, 0, 0]
pub const SYN_OP_PING: u64 = 0x0000;

/// Execute SQL query on files database
/// Request: op | (query_type << 16)
/// For simple lookups: query_type encodes operation
/// Results via shared memory for complex queries
pub const SYN_OP_SQL_QUERY: u64 = 0x0010;

/// Semantic vector search
/// Request: op | (k << 16) | (shmem_handle << 32)
///   where shmem_handle contains 1536-byte query embedding
/// Reply: (result_count << 32) | shmem_handle (contains VectorSearchResult entries)
pub const SYN_OP_VECTOR_SEARCH: u64 = 0x0020;

/// Get embedding for a file
/// Request: op | (file_id << 16)
/// Reply: (size << 32) | shmem_handle (contains 1536-byte embedding), or SYN_STATUS_NOT_FOUND
pub const SYN_OP_GET_EMBEDDING: u64 = 0x0021;

/// Get embedding count
/// Request: [OP, 0, 0, 0]
/// Reply: [count, 0, 0, 0]
pub const SYN_OP_EMBEDDING_COUNT: u64 = 0x0022;

/// Write file via shared memory
/// Request: op | (shmem_handle << 16) | (total_size << 32)
///   shmem contains: [name_len: u16 LE][name: bytes][content: bytes]
/// Reply: 0 = success, SYN_STATUS_ERROR = failure
pub const SYN_OP_WRITE_FILE: u64 = 0x0030;

/// Write intent metadata for a file (Semantic VFS)
/// Request: op | (shmem_handle << 16) | (total_size << 32)
///   shmem contains: [file_id: u32 LE][mime_len: u16 LE][mime: bytes][json: bytes]
/// Reply: 0 = success, SYN_STATUS_ERROR = failure
pub const SYN_OP_WRITE_INTENT: u64 = 0x0031;

/// Read intent metadata for a file (Semantic VFS)
/// Request: op | (file_id << 16)
/// Reply: (json_len << 32) | shmem_handle, or SYN_STATUS_NOT_FOUND
pub const SYN_OP_READ_INTENT: u64 = 0x0032;

/// Query files by MIME type
/// Request: op | (shmem_handle << 16) | (mime_hash << 32)
/// Reply: (count << 32) | shmem_handle
pub const SYN_OP_QUERY_MIME: u64 = 0x0033;

/// Semantic intent query — find file by purpose/concept
/// Request: op | (query_hash << 16)
/// Reply: (size << 32) | (file_id), or SYN_STATUS_NOT_FOUND
/// Searches file_intents.intent_json for substring match.
pub const SYN_OP_QUERY_INTENT: u64 = 0x0034;

// ============================================================================
// Status Codes
// ============================================================================

/// Operation succeeded
pub const SYN_STATUS_OK: u64 = 0;

/// File/resource not found
pub const SYN_STATUS_NOT_FOUND: u64 = 1;

/// Invalid operation or parameter
pub const SYN_STATUS_INVALID: u64 = 2;

/// Would block (try again)
pub const SYN_STATUS_BUSY: u64 = 3;

/// Internal error
pub const SYN_STATUS_ERROR: u64 = 0xFFFF;

// ============================================================================
// Version
// ============================================================================

/// Synapse protocol version (major.minor as u32: 0x00010000 = v1.0)
pub const SYNAPSE_VERSION: u64 = 0x0001_0000;

// ============================================================================
// Client API
// ============================================================================

/// Result type for Synapse operations
pub type SynapseResult<T> = Result<T, SynapseError>;

/// Synapse error types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynapseError {
    /// Synapse service not responding
    ServiceUnavailable,
    /// File or resource not found
    NotFound,
    /// Invalid request
    InvalidRequest,
    /// IPC error
    IpcFailed,
    /// Unknown error
    Unknown(u64),
}

/// File entry returned from Synapse
#[derive(Debug, Clone, Copy)]
pub struct FileEntry {
    pub id: u32,
    pub size: u32,
    pub entry_type: u8,
    pub name_hash: u32,
}

impl FileEntry {
    /// Check if this entry is an ELF executable
    pub fn is_elf(&self) -> bool {
        self.entry_type == 1
    }
}

/// Ping Synapse to check if it's alive
pub fn ping() -> SynapseResult<u64> {
    let magic: u64 = 0x464F4C4B; // "FOLK"

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, SYN_OP_PING, magic)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    // Check response magic
    let expected = magic ^ 0x5959;
    if ret != expected {
        return Err(SynapseError::Unknown(ret));
    }

    Ok(ret)
}

/// Get the number of files in the data store
pub fn file_count() -> SynapseResult<usize> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, SYN_OP_FILE_COUNT, 0)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    Ok(ret as usize)
}

/// Get file entry by index
pub fn file_by_index(index: usize) -> SynapseResult<FileEntry> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, SYN_OP_FILE_BY_INDEX, index as u64)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    if ret == SYN_STATUS_NOT_FOUND {
        return Err(SynapseError::NotFound);
    }

    // Decode: id in low 16 bits, size in next 16, type in next 8, hash in high 24
    // Actually, let's use a simpler encoding for now:
    // ret = (id << 48) | (size << 16) | type
    let id = ((ret >> 48) & 0xFFFF) as u32;
    let size = ((ret >> 16) & 0xFFFF_FFFF) as u32;
    let entry_type = (ret & 0xFF) as u8;

    Ok(FileEntry {
        id,
        size,
        entry_type,
        name_hash: 0, // Not returned in simple encoding
    })
}

// ============================================================================
// Simple hash function for file names
// ============================================================================

/// FNV-1a hash for file names (32-bit)
pub fn hash_name(name: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in name.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

/// File info returned from read_file_by_name
#[derive(Debug, Clone, Copy)]
pub struct FileInfo {
    pub file_id: u16,
    pub size: u32,
}

/// Look up a file by name and get its ID and size
/// This is the first step in reading a file via Synapse
pub fn read_file_by_name(name: &str) -> SynapseResult<FileInfo> {
    let name_hash = hash_name(name);

    // Pack: op in low 16 bits, name_hash in upper bits
    let request = SYN_OP_READ_FILE_BY_NAME | ((name_hash as u64) << 16);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    if ret == SYN_STATUS_NOT_FOUND {
        return Err(SynapseError::NotFound);
    }

    // Decode: file_id in low 16 bits, size in upper 32 bits
    let file_id = (ret & 0xFFFF) as u16;
    let size = ((ret >> 32) & 0xFFFFFFFF) as u32;

    Ok(FileInfo { file_id, size })
}

/// Read an 8-byte chunk from a file at the given offset
/// Returns the chunk data (may be less than 8 bytes at EOF, padded with zeros)
pub fn read_file_chunk(file_id: u16, offset: u32) -> SynapseResult<u64> {
    // Pack everything into payload0 since IPC only passes first payload
    // Format: (offset << 32) | (file_id << 16) | op
    let request = SYN_OP_READ_FILE_CHUNK
        | ((file_id as u64) << 16)
        | ((offset as u64) << 32);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    // Note: SYN_STATUS_NOT_FOUND (1) is a valid return value at EOF
    // We just return the data as-is; caller checks if offset >= size

    Ok(ret)
}

/// Response from zero-copy file read
#[derive(Debug, Clone, Copy)]
pub struct ShmemFileResponse {
    /// Shared memory handle (pass to shmem_map)
    pub shmem_handle: u32,
    /// File size in bytes
    pub size: u32,
}

/// Read a file via shared memory (zero-copy)
///
/// This is the high-performance way to read files. Synapse loads the file
/// into a shared memory buffer and grants access to the caller.
///
/// # Usage
/// 1. Call `read_file_shmem(filename)` to get shmem_handle and size
/// 2. Call `shmem_map(handle, your_virt_addr)` to map the buffer
/// 3. Read directly from the mapped memory
///
/// # Arguments
/// * `name` - The filename to read
///
/// # Returns
/// * `Ok(ShmemFileResponse)` - Contains shmem_handle and file size
/// * `Err(...)` - File not found or other error
pub fn read_file_shmem(name: &str) -> SynapseResult<ShmemFileResponse> {
    let name_hash = hash_name(name);

    // Pack: op in low 16 bits, name_hash in upper bits
    let request = SYN_OP_READ_FILE_SHMEM | ((name_hash as u64) << 16);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    if ret == SYN_STATUS_NOT_FOUND {
        return Err(SynapseError::NotFound);
    }

    // Decode: shmem_handle in low 32 bits, size in upper 32 bits
    let shmem_handle = (ret & 0xFFFFFFFF) as u32;
    let size = ((ret >> 32) & 0xFFFFFFFF) as u32;

    // Handle 0 is invalid (error case)
    if shmem_handle == 0 {
        return Err(SynapseError::IpcFailed);
    }

    Ok(ShmemFileResponse { shmem_handle, size })
}

// ============================================================================
// Vector Search API
// ============================================================================

/// Result from vector search
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct VectorSearchResult {
    /// File ID
    pub file_id: u32,
    /// Cosine similarity score (higher = more similar)
    pub similarity: f32,
}

impl Default for VectorSearchResult {
    fn default() -> Self {
        Self {
            file_id: 0,
            similarity: -1.0,
        }
    }
}

/// Response from vector search
#[derive(Debug, Clone, Copy)]
pub struct VectorSearchResponse {
    /// Number of results
    pub count: usize,
    /// Shared memory handle containing results
    pub shmem_handle: u32,
}

/// Response from get_embedding
#[derive(Debug, Clone, Copy)]
pub struct EmbeddingResponse {
    /// Size of embedding in bytes (should be 1536)
    pub size: u32,
    /// Shared memory handle containing embedding
    pub shmem_handle: u32,
}

/// Perform semantic vector search
///
/// # Arguments
/// * `query_shmem` - Shared memory handle containing 1536-byte query embedding
/// * `k` - Maximum number of results to return
///
/// # Returns
/// * `Ok(VectorSearchResponse)` - Contains result count and shmem handle
/// * `Err(...)` - Error occurred
pub fn vector_search(query_shmem: u32, k: usize) -> SynapseResult<VectorSearchResponse> {
    let k = k.min(255) as u64; // Limit k to fit in 8 bits
    // Pack: op in bits 0-15, k in bits 16-23, shmem_handle in bits 32-63
    let request = SYN_OP_VECTOR_SEARCH | (k << 16) | ((query_shmem as u64) << 32);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    if ret == SYN_STATUS_NOT_FOUND {
        return Err(SynapseError::NotFound);
    }

    if ret == SYN_STATUS_ERROR {
        return Err(SynapseError::IpcFailed);
    }

    let shmem_handle = (ret & 0xFFFFFFFF) as u32;
    let count = ((ret >> 32) & 0xFFFFFFFF) as usize;

    if shmem_handle == 0 {
        return Err(SynapseError::IpcFailed);
    }

    Ok(VectorSearchResponse { count, shmem_handle })
}

/// Get embedding for a specific file
///
/// # Arguments
/// * `file_id` - File ID to get embedding for
///
/// # Returns
/// * `Ok(EmbeddingResponse)` - Contains size and shmem handle
/// * `Err(NotFound)` - File has no embedding
pub fn get_embedding(file_id: u32) -> SynapseResult<EmbeddingResponse> {
    let request = SYN_OP_GET_EMBEDDING | ((file_id as u64) << 16);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    if ret == SYN_STATUS_NOT_FOUND {
        return Err(SynapseError::NotFound);
    }

    if ret == SYN_STATUS_ERROR {
        return Err(SynapseError::IpcFailed);
    }

    let shmem_handle = (ret & 0xFFFFFFFF) as u32;
    let size = ((ret >> 32) & 0xFFFFFFFF) as u32;

    if shmem_handle == 0 {
        return Err(SynapseError::IpcFailed);
    }

    Ok(EmbeddingResponse { size, shmem_handle })
}

/// Write a file to the VFS via Synapse (SQLite cell insert + disk flush)
///
/// Creates a shared memory buffer with `[name_len: u16 LE][name][content]`,
/// sends it to Synapse via IPC, and waits for the synchronous reply.
pub fn write_file(name: &str, content: &[u8]) -> SynapseResult<()> {
    use crate::sys::{shmem_create, shmem_map, shmem_grant, shmem_unmap, shmem_destroy};

    let name_bytes = name.as_bytes();
    let total_size = 2 + name_bytes.len() + content.len();

    // Create shmem (page-aligned)
    let shmem_size = ((total_size + 4095) / 4096) * 4096;
    let shmem_size = if shmem_size == 0 { 4096 } else { shmem_size };
    let handle = shmem_create(shmem_size).map_err(|_| SynapseError::IpcFailed)?;

    // Grant to Synapse
    let _ = shmem_grant(handle, SYNAPSE_TASK_ID);

    // Map in shell address space
    const WRITE_SHMEM_VADDR: usize = 0x20000000;
    if shmem_map(handle, WRITE_SHMEM_VADDR).is_err() {
        let _ = shmem_destroy(handle);
        return Err(SynapseError::IpcFailed);
    }

    // Write protocol: [name_len: u16 LE][name bytes][content bytes]
    unsafe {
        let ptr = WRITE_SHMEM_VADDR as *mut u8;
        let name_len = name_bytes.len() as u16;
        core::ptr::copy_nonoverlapping(name_len.to_le_bytes().as_ptr(), ptr, 2);
        core::ptr::copy_nonoverlapping(name_bytes.as_ptr(), ptr.add(2), name_bytes.len());
        core::ptr::copy_nonoverlapping(content.as_ptr(), ptr.add(2 + name_bytes.len()), content.len());
    }

    let _ = shmem_unmap(handle, WRITE_SHMEM_VADDR);

    // IPC: op | (shmem_handle << 16) | (total_size << 32)
    let request = SYN_OP_WRITE_FILE
        | ((handle as u64) << 16)
        | ((total_size as u64) << 32);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };

    let _ = shmem_destroy(handle);

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }
    if ret != 0 {
        return Err(SynapseError::IpcFailed);
    }

    Ok(())
}

/// Get the count of embeddings in the database
pub fn embedding_count() -> SynapseResult<usize> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, SYN_OP_EMBEDDING_COUNT, 0)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    Ok(ret as usize)
}

// ============================================================================
// Semantic VFS — Intent Metadata Operations
// ============================================================================

/// Write intent metadata for a file.
/// `file_id` is the rowid from a previous write_file call.
/// `mime_type` is e.g. "application/wasm", "text/plain"
/// `intent_json` is the semantic intent, e.g. '{"purpose":"calculator"}'
pub fn write_intent(file_id: u32, mime_type: &str, intent_json: &str) -> SynapseResult<()> {
    use crate::sys::{shmem_create, shmem_map, shmem_grant, shmem_unmap, shmem_destroy};

    let mime_bytes = mime_type.as_bytes();
    let json_bytes = intent_json.as_bytes();
    // Format: [file_id: u32 LE][mime_len: u16 LE][mime bytes][json bytes]
    let total_size = 4 + 2 + mime_bytes.len() + json_bytes.len();

    let shmem_size = ((total_size + 4095) / 4096) * 4096;
    let shmem_size = if shmem_size == 0 { 4096 } else { shmem_size };
    let handle = shmem_create(shmem_size).map_err(|_| SynapseError::IpcFailed)?;
    let _ = shmem_grant(handle, SYNAPSE_TASK_ID);

    const INTENT_SHMEM_VADDR: usize = 0x21000000;
    if shmem_map(handle, INTENT_SHMEM_VADDR).is_err() {
        let _ = shmem_destroy(handle);
        return Err(SynapseError::IpcFailed);
    }

    unsafe {
        let ptr = INTENT_SHMEM_VADDR as *mut u8;
        core::ptr::copy_nonoverlapping(file_id.to_le_bytes().as_ptr(), ptr, 4);
        let mime_len = mime_bytes.len() as u16;
        core::ptr::copy_nonoverlapping(mime_len.to_le_bytes().as_ptr(), ptr.add(4), 2);
        core::ptr::copy_nonoverlapping(mime_bytes.as_ptr(), ptr.add(6), mime_bytes.len());
        core::ptr::copy_nonoverlapping(json_bytes.as_ptr(), ptr.add(6 + mime_bytes.len()), json_bytes.len());
    }

    let _ = shmem_unmap(handle, INTENT_SHMEM_VADDR);

    let request = SYN_OP_WRITE_INTENT
        | ((handle as u64) << 16)
        | ((total_size as u64) << 32);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };

    let _ = shmem_destroy(handle);

    if ret == u64::MAX { return Err(SynapseError::ServiceUnavailable); }
    if ret != 0 { return Err(SynapseError::IpcFailed); }
    Ok(())
}

/// Semantic query — find a file by concept/purpose.
/// Scans file_intents table for intent_json containing the query string.
/// Returns the best-matching file's info (file_id, size).
pub fn query_intent(query: &str) -> SynapseResult<FileInfo> {
    use crate::sys::{shmem_create, shmem_map, shmem_grant, shmem_unmap, shmem_destroy};

    let query_bytes = query.as_bytes();
    let total_size = query_bytes.len();
    if total_size == 0 { return Err(SynapseError::NotFound); }

    let shmem_size = 4096;
    let handle = shmem_create(shmem_size).map_err(|_| SynapseError::IpcFailed)?;
    let _ = shmem_grant(handle, SYNAPSE_TASK_ID);

    const QUERY_SHMEM_VADDR: usize = 0x22000000;
    if shmem_map(handle, QUERY_SHMEM_VADDR).is_err() {
        let _ = shmem_destroy(handle);
        return Err(SynapseError::IpcFailed);
    }

    unsafe {
        let ptr = QUERY_SHMEM_VADDR as *mut u8;
        core::ptr::copy_nonoverlapping(query_bytes.as_ptr(), ptr, total_size);
    }

    let _ = shmem_unmap(handle, QUERY_SHMEM_VADDR);

    let request = SYN_OP_QUERY_INTENT
        | ((handle as u64) << 16)
        | ((total_size as u64) << 32);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };

    let _ = shmem_destroy(handle);

    if ret == u64::MAX { return Err(SynapseError::ServiceUnavailable); }
    if ret == SYN_STATUS_NOT_FOUND { return Err(SynapseError::NotFound); }

    let file_id = (ret & 0xFFFF) as u16;
    let size = ((ret >> 32) & 0xFFFFFFFF) as u32;

    Ok(FileInfo { file_id, size })
}

/// Read intent metadata for a file. Returns (mime_type, intent_json) via shmem.
pub fn read_intent(file_id: u32) -> SynapseResult<ShmemFileResponse> {
    let request = SYN_OP_READ_INTENT | ((file_id as u64) << 16);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX { return Err(SynapseError::ServiceUnavailable); }
    if ret == SYN_STATUS_NOT_FOUND { return Err(SynapseError::NotFound); }

    let shmem_handle = (ret & 0xFFFF) as u32;
    let size = ((ret >> 32) & 0xFFFFFFFF) as u32;

    Ok(ShmemFileResponse { shmem_handle, size })
}
