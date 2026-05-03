//! Compositor Client for Folkering OS
//!
//! Provides type-safe wrappers for compositor IPC. Applications use this to
//! send UI trees, and AI agents use it to query semantic state.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────┐     IPC      ┌────────────────┐
//! │    Shell    │◄────────────►│   Compositor   │
//! │  (App/AI)   │              │  (WorldTree)   │
//! └─────────────┘              └────────────────┘
//! ```
//!
//! # Protocol
//!
//! Phase 6.1 uses simple IPC payloads with name hashing (no shmem for strings).
//! Future phases will add shared memory for full TreeUpdate with strings.

use crate::syscall::{syscall3, SYS_IPC_SEND};

// ============================================================================
// Well-Known Task ID
// ============================================================================

/// Compositor service task ID (spawned at boot as Task 4)
///
/// Task layout (post Phase A.6, PR #84):
/// - Task 1: Idle/kernel
/// - Task 2: Synapse (Data Kernel)
/// - Task 3: Shell
/// - Task 4: draug-daemon (explicit kernel spawn from ramdisk)
/// - Task 5: Compositor (first entry in the generic ramdisk loop)
///
/// Pre-A.6 compositor was task 4. Without bumping this const after
/// #84, daemon's `shmem_grant(handle, COMPOSITOR_TASK_ID)` granted
/// to itself, and compositor's later `shmem_map` returned `Unknown`.
/// The retry path in `attach_status_with_retry` couldn't help — the
/// grant target was wrong, not just slow.
pub const COMPOSITOR_TASK_ID: u32 = 5;

// ============================================================================
// Operation Codes (must match compositor/src/main.rs)
// ============================================================================

/// Create a new window
/// Request: [OP, 0]
/// Reply: window_id
pub const COMP_OP_CREATE_WINDOW: u64 = 0x01;

/// Update UI tree (simplified for Phase 6.1)
/// Request: op | (window_id << 16), node_id | (role << 32) | (name_hash << 40)
/// Reply: 0 on success
pub const COMP_OP_UPDATE: u64 = 0x02;

/// Close a window
/// Request: op | (window_id << 16)
/// Reply: 0 on success
pub const COMP_OP_CLOSE: u64 = 0x03;

/// Create a UI window from serialized widget tree in shmem
/// Request: op | (shmem_handle << 8)
/// Reply: window_id on success, u64::MAX on failure
pub const COMP_OP_CREATE_UI_WINDOW: u64 = 0x06;

/// Query: find node by name hash
/// Request: [OP, name_hash]
/// Reply: (window_id << 32) | node_id, or u64::MAX if not found
pub const COMP_OP_QUERY_NAME: u64 = 0x10;

/// Query: get current focus
/// Request: [OP, 0]
/// Reply: (window_id << 32) | node_id, or u64::MAX if no focus
pub const COMP_OP_QUERY_FOCUS: u64 = 0x11;

/// Register a granted graphics-ring shmem id with the compositor.
/// Request: opcode | (shmem_id << 8)
/// Reply: slot index on success, u64::MAX on failure.
pub const COMP_OP_GFX_REGISTER_RING: u64 = 0x20;

/// Unregister a previously registered ring slot.
/// Request: opcode | (slot << 8)
/// Reply: 0 on success, u64::MAX on failure.
pub const COMP_OP_GFX_UNREGISTER_RING: u64 = 0x21;

/// Bind an input ring to an existing gfx slot. The compositor will
/// push InputEvent records into this shmem when mouse/key events
/// land inside the slot's damage bbox.
/// Request: opcode | (slot << 8) | (input_shmem_id << 16)
/// Reply: 0 on success, u64::MAX on failure.
pub const COMP_OP_GFX_REGISTER_INPUT_RING: u64 = 0x22;

// ============================================================================
// Error Types
// ============================================================================

/// Compositor error types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompError {
    /// Compositor service not responding
    ServiceUnavailable,
    /// Invalid window ID
    InvalidWindow,
    /// Node not found
    NotFound,
    /// IPC error
    IpcFailed,
}

// ============================================================================
// Client API
// ============================================================================

/// Create a new window.
///
/// Returns the window ID assigned by the compositor.
///
/// # Example
/// ```ignore
/// let window_id = compositor::create_window()?;
/// ```
pub fn create_window() -> Result<u64, CompError> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, COMPOSITOR_TASK_ID as u64, COMP_OP_CREATE_WINDOW, 0)
    };

    if ret == u64::MAX {
        Err(CompError::ServiceUnavailable)
    } else {
        Ok(ret)
    }
}

/// Send a simplified node update to the compositor.
///
/// Phase 6.1 uses a compact single-payload encoding:
/// - `window_id`: Target window (4 bits)
/// - `node_id`: Node identifier (16 bits)
/// - `role`: Node role (8 bits)
/// - `name_hash`: FNV-1a hash truncated (24 bits)
///
/// # Encoding (all in payload0)
/// payload0: opcode(8) | window_id(4) | node_id(16) | role(8) | name_hash(24)
/// Bits:     [0-7]     | [8-11]       | [12-27]     | [28-35] | [36-59]
///
/// # Example
/// ```ignore
/// let name_hash = hash_name("Submit Form");
/// compositor::update_node(window_id, 42, Role::Button as u8, name_hash)?;
/// ```
pub fn update_node(window_id: u64, node_id: u64, role: u8, name_hash: u32) -> Result<(), CompError> {
    // Pack everything into payload0 for single-word IPC
    // Layout: [opcode:8][window:4][node:16][role:8][hash:24] = 60 bits used
    let payload0 = (COMP_OP_UPDATE & 0xFF)
        | ((window_id & 0xF) << 8)
        | ((node_id & 0xFFFF) << 12)
        | ((role as u64 & 0xFF) << 28)
        | ((name_hash as u64 & 0xFF_FFFF) << 36);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, COMPOSITOR_TASK_ID as u64, payload0, 0)
    };

    if ret == u64::MAX {
        Err(CompError::ServiceUnavailable)
    } else {
        Ok(())
    }
}

/// Close a window.
///
/// # Example
/// ```ignore
/// compositor::close_window(window_id)?;
/// ```
pub fn close_window(window_id: u64) -> Result<(), CompError> {
    let payload0 = COMP_OP_CLOSE | ((window_id & 0xFFFF) << 16);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, COMPOSITOR_TASK_ID as u64, payload0, 0)
    };

    if ret == u64::MAX {
        Err(CompError::ServiceUnavailable)
    } else {
        Ok(())
    }
}

/// Find a node by name hash.
///
/// This is the AI query interface: "Where is the Submit button?"
///
/// # Encoding
/// payload0: opcode(8) | name_hash(24) packed in bits [0-7] and [8-31]
///
/// # Returns
/// - `Ok((true, node_id, window_id))` if found
/// - `Ok((false, 0, 0))` if not found
/// - `Err(...)` on IPC error
///
/// # Example
/// ```ignore
/// let hash = hash_name("Submit Form");
/// match find_node_by_hash(hash)? {
///     (true, node_id, window_id) => println!("Found node {} in window {}", node_id, window_id),
///     (false, _, _) => println!("Not found"),
/// }
/// ```
pub fn find_node_by_hash(name_hash: u32) -> Result<(bool, u64, u64), CompError> {
    // Pack opcode and name_hash into payload0
    // Layout: [opcode:8][name_hash:24] in bits [0-31]
    let payload0 = (COMP_OP_QUERY_NAME & 0xFF) | ((name_hash as u64 & 0xFF_FFFF) << 8);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, COMPOSITOR_TASK_ID as u64, payload0, 0)
    };

    if ret == u64::MAX {
        // Not found
        Ok((false, 0, 0))
    } else if ret == u64::MAX - 1 {
        // IPC error
        Err(CompError::ServiceUnavailable)
    } else {
        // Found: window_id in upper 32 bits, node_id in lower 32 bits
        let window_id = ret >> 32;
        let node_id = ret & 0xFFFF_FFFF;
        Ok((true, node_id, window_id))
    }
}

/// Query the currently focused node.
///
/// # Returns
/// - `Ok(Some((node_id, window_id)))` if there is a focused node
/// - `Ok(None)` if no focus
/// - `Err(...)` on IPC error
pub fn query_focus() -> Result<Option<(u64, u64)>, CompError> {
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, COMPOSITOR_TASK_ID as u64, COMP_OP_QUERY_FOCUS, 0)
    };

    if ret == u64::MAX {
        Ok(None)
    } else if ret == u64::MAX - 1 {
        Err(CompError::ServiceUnavailable)
    } else {
        let window_id = ret >> 32;
        let node_id = ret & 0xFFFF_FFFF;
        Ok(Some((node_id, window_id)))
    }
}

// ============================================================================
// Graphics-ring registration
// ============================================================================

/// Register a previously created+granted display-list ring with the
/// compositor. Returns the slot index assigned.
///
/// Caller responsibilities (in order):
/// 1. `RingHandle::create_at(virt)` — allocates the shmem region.
/// 2. `handle.grant_to(COMPOSITOR_TASK_ID)` — gives compositor permission
///    to map the same id.
/// 3. Pass the resulting `handle.id` here.
///
/// The slot index is stable for the lifetime of the registration and
/// can be passed to `unregister_gfx_ring` when the producer shuts down.
pub fn register_gfx_ring(shmem_id: u32) -> Result<u32, CompError> {
    let payload0 = (COMP_OP_GFX_REGISTER_RING & 0xFF)
        | ((shmem_id as u64) << 8);
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, COMPOSITOR_TASK_ID as u64, payload0, 0)
    };
    if ret == u64::MAX {
        Err(CompError::ServiceUnavailable)
    } else {
        Ok(ret as u32)
    }
}

/// Unregister a slot returned by `register_gfx_ring`.
pub fn unregister_gfx_ring(slot: u32) -> Result<(), CompError> {
    let payload0 = (COMP_OP_GFX_UNREGISTER_RING & 0xFF)
        | ((slot as u64 & 0xFF) << 8);
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, COMPOSITOR_TASK_ID as u64, payload0, 0)
    };
    if ret == u64::MAX {
        Err(CompError::ServiceUnavailable)
    } else {
        Ok(())
    }
}

/// Bind an input ring (created via `InputRingHandle::create_at`
/// and granted to the compositor) to a previously registered gfx
/// slot. Future mouse/key events landing in the slot's damage bbox
/// flow into this shmem.
pub fn register_input_ring(slot: u32, input_shmem_id: u32) -> Result<(), CompError> {
    // payload0: op (8) | slot (8) | shmem_id (32). Shmem id occupies
    // bits 16..48; slot bits 8..16; opcode bits 0..8.
    let payload0 = (COMP_OP_GFX_REGISTER_INPUT_RING & 0xFF)
        | ((slot as u64 & 0xFF) << 8)
        | ((input_shmem_id as u64) << 16);
    let ret = unsafe {
        syscall3(SYS_IPC_SEND, COMPOSITOR_TASK_ID as u64, payload0, 0)
    };
    if ret == u64::MAX {
        Err(CompError::ServiceUnavailable)
    } else {
        Ok(())
    }
}

// ============================================================================
// Utility Functions
// ============================================================================

/// FNV-1a hash for name matching.
///
/// This is the same algorithm used in synapse.rs for consistency.
/// Returns a 32-bit hash of the input string.
///
/// # Example
/// ```ignore
/// let hash = hash_name("Submit Form");
/// assert_eq!(hash, hash_name("Submit Form")); // Deterministic
/// ```
pub fn hash_name(name: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5; // FNV offset basis
    for byte in name.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x01000193); // FNV prime
    }
    hash
}

// ============================================================================
// Role Constants (matching libaccesskit_folk::Role)
// ============================================================================

/// Role constants for use with update_node.
/// These match libaccesskit_folk::Role values.
pub mod role {
    pub const UNKNOWN: u8 = 0;
    pub const WINDOW: u8 = 1;
    pub const GROUP: u8 = 2;
    pub const SCROLL_VIEW: u8 = 3;
    pub const TAB_PANEL: u8 = 4;
    pub const DIALOG: u8 = 5;
    pub const ALERT: u8 = 6;
    pub const BUTTON: u8 = 10;
    pub const CHECKBOX: u8 = 11;
    pub const RADIO_BUTTON: u8 = 12;
    pub const COMBO_BOX: u8 = 13;
    pub const MENU_ITEM: u8 = 14;
    pub const LINK: u8 = 15;
    pub const SLIDER: u8 = 16;
    pub const TAB: u8 = 17;
    pub const STATIC_TEXT: u8 = 20;
    pub const TEXT_INPUT: u8 = 21;
    pub const TEXT_AREA: u8 = 22;
    pub const LABEL: u8 = 23;
    pub const HEADING: u8 = 24;
    pub const IMAGE: u8 = 30;
    pub const PROGRESS_BAR: u8 = 31;
    pub const SEPARATOR: u8 = 32;
    pub const LIST: u8 = 40;
    pub const LIST_ITEM: u8 = 41;
    pub const TABLE: u8 = 42;
    pub const TABLE_ROW: u8 = 43;
    pub const TABLE_CELL: u8 = 44;
    pub const TREE: u8 = 45;
    pub const TREE_ITEM: u8 = 46;
}
