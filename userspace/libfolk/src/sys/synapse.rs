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

/// Phase C: delete a file row from Synapse's `files` table.
/// Request: op | (name_hash << 16)
/// Reply: 0 on success, SYN_STATUS_NOT_FOUND if no row matches the
/// hash, SYN_STATUS_ERROR on btree corruption / IO failure.
///
/// Counterpart to write_file. The btree implementation removes the
/// row's leaf-page cell pointer; the cell's payload bytes become
/// dead space until the next page rewrite (page-level reclamation
/// is the open follow-up). Overflow page chains attached to the
/// row are NOT walked + freed yet — small cost given Synapse's
/// 256 KB max-db cap, but issue #100 leaves it as the next step.
pub const SYN_OP_DELETE_FILE: u64 = 0x0036;

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

// ── Phase 9: Bi-Temporal Knowledge Graph ops ────────────────────────

/// Upsert a new row into the `entities` table.
/// Request: op | (shmem_handle << 16) | (total_size << 32)
///   shmem: [eid_len:u16 LE][eid][name_len:u16 LE][name][type_len:u16 LE][type]
/// Reply: `(rowid << 16)` on success, `SYN_STATUS_ERROR` on failure.
/// The shift keeps rowid out of the low-16-bit status-code window.
pub const SYN_OP_UPSERT_ENTITY: u64 = 0x0040;

/// Upsert a new row into the `edges` table with temporal supersession.
/// Before inserting, any existing active edge with the same
/// (subject_id, predicate) has its valid_to rewritten in-place to
/// the current timestamp, so only the new edge remains active.
/// Request: op | (shmem_handle << 16) | (total_size << 32)
///   shmem: [eid_len:u16 LE][eid]
///          [subj_len:u16 LE][subj]
///          [pred_len:u16 LE][pred]
///          [obj_len:u16 LE][obj]
/// Reply: `(rowid << 16)` on success, `SYN_STATUS_ERROR` on failure.
pub const SYN_OP_UPSERT_EDGE: u64 = 0x0041;

/// Walk the knowledge graph from a starting entity.
/// Request: op | (shmem_handle << 16) | (max_depth << 32)
///   shmem (request): [start_len:u16 LE][start_id]
///   shmem (reply, written in place): [hop_count:u16 LE]
///     repeated hop_count times:
///       [eid_len:u16 LE][eid][depth:u16 LE]
/// Reply: (hop_count << 16), or SYN_STATUS_ERROR on failure.
/// Shift is 16 (not 32) so `ret == 1` can't collide with
/// `SYN_STATUS_NOT_FOUND`; client unpacks via `(ret >> 16) & 0xFFFF`.
pub const SYN_OP_GRAPH_WALK: u64 = 0x0042;

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

/// Reply struct for `list_files` — mirrors Synapse's wire shape.
/// Caller is responsible for `shmem_destroy(shmem_handle)` once done.
///
/// Each entry in the shmem buffer is 32 bytes:
///   [name(24) zero-padded][size: u32 LE][entry_type: u32 LE]
///
/// **Name truncation**: Synapse's directory cache stores names in a
/// 32-byte field but only the first 24 bytes are copied into shmem.
/// Names longer than 24 bytes are silently truncated. Phase C
/// callers (e.g. `Project::list`) should keep project + filename
/// combinations within that budget for now.
#[derive(Debug, Clone)]
pub struct ListFilesResponse {
    pub count: usize,
    pub shmem_handle: u32,
}

/// Layout of one entry inside the `list_files` shmem buffer.
pub const LIST_FILES_ENTRY_SIZE: usize = 32;
/// How many bytes of the entry are name (zero-padded).
pub const LIST_FILES_NAME_BYTES: usize = 24;

/// Enumerate every file Synapse knows about.
///
/// Returns the count and a shmem handle to a buffer of
/// `count × LIST_FILES_ENTRY_SIZE` bytes. The caller maps the shmem
/// (e.g. via `shmem_map`), walks the entries, then destroys the shmem.
///
/// Higher-level wrappers (`Project::list`) should be preferred for
/// most callers; this is the raw IPC primitive.
pub fn list_files() -> SynapseResult<ListFilesResponse> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, SYN_OP_LIST_FILES, 0)
    };

    if ret == u64::MAX {
        return Err(SynapseError::ServiceUnavailable);
    }

    // Wire format: ((count << 32) | shmem_handle). Empty cache or
    // shmem_create failure both return 0 in the low 32 bits — caller
    // should treat that as "no files" rather than an error.
    let count = ((ret >> 32) & 0xFFFF_FFFF) as usize;
    let shmem_handle = (ret & 0xFFFF_FFFF) as u32;

    Ok(ListFilesResponse { count, shmem_handle })
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

// ============================================================================
// D.3.7.virtio: model-disk file fetch
// ============================================================================

/// Stream a file from the kernel's model-disk into a fresh shmem
/// region — bypasses Synapse entirely. The model disk is a
/// dedicated VirtIO block device whose sector 0 carries an FMDL
/// header naming a single payload. The kernel verifies
/// `hash_name(name)` matches the disk's filename hash before
/// allocating + filling the shmem.
///
/// On success returns `Ok((shmem_handle, size))`; the caller should
/// `shmem_map(handle, vaddr)` and read the bytes directly from
/// the mapping. The shmem persists until explicitly destroyed or
/// task teardown (it's owned by the calling task).
///
/// Falls back-compatibly to `Err(SynapseError::NotFound)` when no
/// model disk is attached or the name doesn't match — same error
/// type the Synapse path uses, so a single fallthrough works in
/// `vfs_loader::read_file`.
pub fn read_model_file_shmem(name: &str) -> SynapseResult<ShmemFileResponse> {
    use crate::syscall::{syscall1, SYS_READ_MODEL_FILE_SHMEM};

    let h = hash_name(name);
    let ret = unsafe { syscall1(SYS_READ_MODEL_FILE_SHMEM, h as u64) };
    if ret == u64::MAX {
        return Err(SynapseError::NotFound);
    }
    let shmem_handle = ((ret >> 32) & 0xFFFFFFFF) as u32;
    let size = (ret & 0xFFFFFFFF) as u32;
    if shmem_handle == 0 {
        return Err(SynapseError::IpcFailed);
    }
    Ok(ShmemFileResponse { shmem_handle, size })
}

/// File info returned from read_file_by_name
#[derive(Debug, Clone, Copy)]
pub struct FileInfo {
    pub file_id: u16,
    pub size: u32,
}

/// Phase C: delete a file from Synapse's `files` table by name.
///
/// Wire format: op | (name_hash << 16). The synapse-service handler
/// finds the matching rowid via name_hash, then calls the btree
/// `sqlite_delete_file_by_rowid` to remove the cell pointer.
///
/// Returns `Ok(())` on successful delete, `Err(SynapseError::NotFound)`
/// if no row matches, `Err(SynapseError::IpcFailed)` on btree errors.
/// Idempotent at the SynapseError::NotFound level — caller can treat
/// "not found" as the same shape as "deleted" if that's what they
/// need (Project::delete does this).
pub fn delete_file(name: &str) -> SynapseResult<()> {
    let name_hash = hash_name(name);
    let request = SYN_OP_DELETE_FILE | ((name_hash as u64) << 16);

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
    Ok(())
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
    // Success encoding: `handle_write_file` in synapse-service returns
    // `0` on in-place overwrite OR the newly-assigned rowid on a fresh
    // insert. Both are non-error. `SYN_STATUS_ERROR` (0xFFFF) is the
    // only explicit error code. Theoretically a rowid of exactly
    // 0xFFFF could collide with the error sentinel, but we only ship
    // a few dozen MemPalace entries today, so the risk is zero.
    if ret == SYN_STATUS_ERROR {
        return Err(SynapseError::IpcFailed);
    }

    Ok(())
}

/// Write a file to the VFS via Synapse and return the newly-assigned
/// rowid (0 if the file already existed and was overwritten in place).
///
/// Same underlying IPC as `write_file`, but surfaces the rowid the
/// handler sends back so callers can stamp semantic intents on the
/// fresh row without a second `read_file_by_name` round-trip.
pub fn write_file_get_rowid(name: &str, content: &[u8]) -> SynapseResult<u32> {
    use crate::sys::{shmem_create, shmem_map, shmem_grant, shmem_unmap, shmem_destroy};

    let name_bytes = name.as_bytes();
    let total_size = 2 + name_bytes.len() + content.len();

    let shmem_size = ((total_size + 4095) / 4096) * 4096;
    let shmem_size = if shmem_size == 0 { 4096 } else { shmem_size };
    let handle = shmem_create(shmem_size).map_err(|_| SynapseError::IpcFailed)?;
    let _ = shmem_grant(handle, SYNAPSE_TASK_ID);

    const WRITE_SHMEM_VADDR: usize = 0x20000000;
    if shmem_map(handle, WRITE_SHMEM_VADDR).is_err() {
        let _ = shmem_destroy(handle);
        return Err(SynapseError::IpcFailed);
    }

    unsafe {
        let ptr = WRITE_SHMEM_VADDR as *mut u8;
        let name_len = name_bytes.len() as u16;
        core::ptr::copy_nonoverlapping(name_len.to_le_bytes().as_ptr(), ptr, 2);
        core::ptr::copy_nonoverlapping(name_bytes.as_ptr(), ptr.add(2), name_bytes.len());
        core::ptr::copy_nonoverlapping(content.as_ptr(), ptr.add(2 + name_bytes.len()), content.len());
    }

    let _ = shmem_unmap(handle, WRITE_SHMEM_VADDR);

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
    if ret == SYN_STATUS_ERROR {
        return Err(SynapseError::IpcFailed);
    }
    Ok(ret as u32)
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

// ============================================================================
// Phase 9: Bi-Temporal Knowledge Graph Client API
// ============================================================================

/// Append a length-prefixed string to a byte cursor. Helper for the
/// shmem layout shared by upsert_entity / upsert_edge / graph_walk.
fn push_str(buf: &mut [u8], pos: &mut usize, s: &str) -> Result<(), ()> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    if *pos + 2 + len > buf.len() { return Err(()); }
    buf[*pos..*pos + 2].copy_from_slice(&(len as u16).to_le_bytes());
    *pos += 2;
    buf[*pos..*pos + len].copy_from_slice(bytes);
    *pos += len;
    Ok(())
}

/// Upsert an entity into the knowledge graph.
///
/// Returns the assigned rowid on success. A later insert with the
/// same `entity_id` will create a duplicate row — Draug's reasoning
/// scans all rows and dedupes at query time.
pub fn upsert_entity(
    entity_id: &str,
    name: &str,
    entity_type: &str,
) -> SynapseResult<u32> {
    use crate::sys::{shmem_create, shmem_map, shmem_grant, shmem_unmap, shmem_destroy};

    let shmem_size = 4096;
    let handle = shmem_create(shmem_size).map_err(|_| SynapseError::IpcFailed)?;
    let _ = shmem_grant(handle, SYNAPSE_TASK_ID);

    const ENTITY_SHMEM_VADDR: usize = 0x23000000;
    if shmem_map(handle, ENTITY_SHMEM_VADDR).is_err() {
        let _ = shmem_destroy(handle);
        return Err(SynapseError::IpcFailed);
    }

    let total_size;
    unsafe {
        let ptr = ENTITY_SHMEM_VADDR as *mut u8;
        let buf = core::slice::from_raw_parts_mut(ptr, shmem_size);
        let mut pos = 0usize;
        if push_str(buf, &mut pos, entity_id).is_err()
            || push_str(buf, &mut pos, name).is_err()
            || push_str(buf, &mut pos, entity_type).is_err()
        {
            let _ = shmem_unmap(handle, ENTITY_SHMEM_VADDR);
            let _ = shmem_destroy(handle);
            return Err(SynapseError::InvalidRequest);
        }
        total_size = pos;
    }

    let _ = shmem_unmap(handle, ENTITY_SHMEM_VADDR);

    let request = SYN_OP_UPSERT_ENTITY
        | ((handle as u64) << 16)
        | ((total_size as u64) << 32);
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };
    let _ = shmem_destroy(handle);

    if ret == u64::MAX { return Err(SynapseError::ServiceUnavailable); }
    if ret == SYN_STATUS_ERROR { return Err(SynapseError::IpcFailed); }
    // Handler encodes rowid in bits 16..48 so it can't collide with
    // the low-16-bit status sentinels. Unpack with a right shift.
    Ok(((ret >> 16) & 0xFFFFFFFF) as u32)
}

/// Upsert an edge with temporal supersession. If an active edge
/// exists for the same `(subject_id, predicate)` pair, its
/// `valid_to` field is rewritten to the current timestamp before the
/// new edge is inserted.
///
/// Returns the new edge's rowid.
pub fn upsert_edge(
    edge_id: &str,
    subject_id: &str,
    predicate: &str,
    object_id: &str,
) -> SynapseResult<u32> {
    use crate::sys::{shmem_create, shmem_map, shmem_grant, shmem_unmap, shmem_destroy};

    let shmem_size = 4096;
    let handle = shmem_create(shmem_size).map_err(|_| SynapseError::IpcFailed)?;
    let _ = shmem_grant(handle, SYNAPSE_TASK_ID);

    const EDGE_SHMEM_VADDR: usize = 0x24000000;
    if shmem_map(handle, EDGE_SHMEM_VADDR).is_err() {
        let _ = shmem_destroy(handle);
        return Err(SynapseError::IpcFailed);
    }

    let total_size;
    unsafe {
        let ptr = EDGE_SHMEM_VADDR as *mut u8;
        let buf = core::slice::from_raw_parts_mut(ptr, shmem_size);
        let mut pos = 0usize;
        if push_str(buf, &mut pos, edge_id).is_err()
            || push_str(buf, &mut pos, subject_id).is_err()
            || push_str(buf, &mut pos, predicate).is_err()
            || push_str(buf, &mut pos, object_id).is_err()
        {
            let _ = shmem_unmap(handle, EDGE_SHMEM_VADDR);
            let _ = shmem_destroy(handle);
            return Err(SynapseError::InvalidRequest);
        }
        total_size = pos;
    }

    let _ = shmem_unmap(handle, EDGE_SHMEM_VADDR);

    let request = SYN_OP_UPSERT_EDGE
        | ((handle as u64) << 16)
        | ((total_size as u64) << 32);
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };
    let _ = shmem_destroy(handle);

    if ret == u64::MAX { return Err(SynapseError::ServiceUnavailable); }
    if ret == SYN_STATUS_ERROR { return Err(SynapseError::IpcFailed); }
    // Same rowid shift as `upsert_entity` — see the comment there.
    Ok(((ret >> 16) & 0xFFFFFFFF) as u32)
}

/// One hop returned by `graph_walk`. Uses a fixed-capacity byte
/// buffer so libfolk stays `no_std` without `alloc`. Entity IDs
/// longer than `MAX_GRAPH_HOP_ID_LEN` are truncated.
pub const MAX_GRAPH_HOP_ID_LEN: usize = 64;

#[derive(Clone, Copy)]
pub struct GraphHop {
    pub depth: u16,
    pub id_len: u8,
    pub id_bytes: [u8; MAX_GRAPH_HOP_ID_LEN],
}

impl GraphHop {
    pub fn as_str(&self) -> &str {
        let end = (self.id_len as usize).min(MAX_GRAPH_HOP_ID_LEN);
        core::str::from_utf8(&self.id_bytes[..end]).unwrap_or("")
    }
}

/// Walk the knowledge graph starting from `start_entity_id`, bounded
/// by `max_depth` hops. Writes discovered hops into the caller's
/// `out` slice and returns the number written.
///
/// Only active edges (`valid_to == 0`) are traversed, so superseded
/// facts are ignored — the returned reachability reflects Draug's
/// *current* semantic reality.
pub fn graph_walk(
    start_entity_id: &str,
    max_depth: u32,
    out: &mut [GraphHop],
) -> SynapseResult<usize> {
    use crate::sys::{shmem_create, shmem_map, shmem_grant, shmem_unmap, shmem_destroy};

    let shmem_size = 4096;
    let handle = shmem_create(shmem_size).map_err(|_| SynapseError::IpcFailed)?;
    let _ = shmem_grant(handle, SYNAPSE_TASK_ID);

    const WALK_SHMEM_VADDR: usize = 0x25000000;
    if shmem_map(handle, WALK_SHMEM_VADDR).is_err() {
        let _ = shmem_destroy(handle);
        return Err(SynapseError::IpcFailed);
    }

    // Serialize the request: [start_len: u16 LE][start_id]
    unsafe {
        let ptr = WALK_SHMEM_VADDR as *mut u8;
        let buf = core::slice::from_raw_parts_mut(ptr, shmem_size);
        let mut pos = 0usize;
        if push_str(buf, &mut pos, start_entity_id).is_err() {
            let _ = shmem_unmap(handle, WALK_SHMEM_VADDR);
            let _ = shmem_destroy(handle);
            return Err(SynapseError::InvalidRequest);
        }
    }

    let request = SYN_OP_GRAPH_WALK
        | ((handle as u64) << 16)
        | ((max_depth as u64) << 32);
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, SYNAPSE_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        let _ = shmem_unmap(handle, WALK_SHMEM_VADDR);
        let _ = shmem_destroy(handle);
        return Err(SynapseError::ServiceUnavailable);
    }
    if ret == SYN_STATUS_NOT_FOUND || ret == SYN_STATUS_ERROR {
        let _ = shmem_unmap(handle, WALK_SHMEM_VADDR);
        let _ = shmem_destroy(handle);
        return Err(SynapseError::NotFound);
    }

    // Reply layout in shmem:
    //   [hop_count: u16 LE]
    //   repeated: [eid_len: u16 LE][eid bytes][depth: u16 LE]
    //
    // The IPC reply encodes `hop_count` in bits 16..31 so that a
    // result of 1 doesn't collide with `SYN_STATUS_NOT_FOUND` (= 1).
    let hop_count_reported = ((ret >> 16) & 0xFFFF) as usize;
    let max_out = out.len();
    let mut written = 0usize;
    unsafe {
        let ptr = WALK_SHMEM_VADDR as *const u8;
        let buf = core::slice::from_raw_parts(ptr, shmem_size);
        let mut pos = 2usize; // skip the hop_count prefix
        for _ in 0..hop_count_reported {
            if written >= max_out { break; }
            if pos + 2 > buf.len() { break; }
            let eid_len = u16::from_le_bytes([buf[pos], buf[pos + 1]]) as usize;
            pos += 2;
            if pos + eid_len + 2 > buf.len() { break; }
            let mut hop = GraphHop {
                depth: 0,
                id_len: 0,
                id_bytes: [0u8; MAX_GRAPH_HOP_ID_LEN],
            };
            let copy_len = eid_len.min(MAX_GRAPH_HOP_ID_LEN);
            hop.id_bytes[..copy_len].copy_from_slice(&buf[pos..pos + copy_len]);
            hop.id_len = copy_len as u8;
            pos += eid_len;
            hop.depth = u16::from_le_bytes([buf[pos], buf[pos + 1]]);
            pos += 2;
            out[written] = hop;
            written += 1;
        }
    }

    let _ = shmem_unmap(handle, WALK_SHMEM_VADDR);
    let _ = shmem_destroy(handle);
    Ok(written)
}
