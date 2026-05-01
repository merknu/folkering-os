//! Intent Service Protocol - Semantic Intent Routing
//!
//! The Intent Service routes commands to the appropriate handler task
//! based on keyword matching and registered capabilities.
//!
//! # Architecture
//!
//! ```text
//! ┌────────────┐     IPC      ┌─────────────┐     IPC      ┌─────────┐
//! │ Compositor │────────────►│ Intent Svc  │────────────►│  Shell  │
//! │  (Omnibar) │◄────────────│  (Router)   │◄────────────│ (Exec)  │
//! └────────────┘              └─────────────┘              └─────────┘
//! ```
//!
//! # Protocol
//!
//! Operations use low 8 bits of payload0:
//! - INTENT_OP_REGISTER (0x40): Register handler capabilities
//! - INTENT_OP_SUBMIT (0x41): Submit command for routing
//! - INTENT_OP_QUERY (0x42): Query which handler handles a command

use crate::syscall::{syscall3, SYS_IPC_SEND};

// ============================================================================
// Well-Known Task ID
// ============================================================================

/// Intent Service task ID. Phase A.6 (#84) inserted draug-daemon at
/// task 4, shifting everything in the generic ramdisk loop by one.
/// Task layout (post-A.6):
///   1 = Idle / kernel
///   2 = Synapse
///   3 = Shell
///   4 = draug-daemon  (explicit kernel spawn)
///   5 = Compositor    (first generic-loop entry)
///   6 = Intent Service (next generic-loop entry)
///   7 = draug-streamer
pub const INTENT_TASK_ID: u32 = 6;

// ============================================================================
// Operation Codes
// ============================================================================

/// Register handler capabilities
/// Request: op | (capability_flags << 16)
/// Reply: 0 on success
pub const INTENT_OP_REGISTER: u64 = 0x40;

/// Submit a command for routing
/// Request: op | (shell_opcode << 8) | (arg_data << 16)
/// The intent service routes to the appropriate handler and returns the result.
/// Reply: same as what the handler returns
pub const INTENT_OP_SUBMIT: u64 = 0x41;

/// Query which handler would handle a command
/// Request: op | (command_hash << 16)
/// Reply: handler_task_id | (confidence << 32)
pub const INTENT_OP_QUERY: u64 = 0x42;

// ============================================================================
// Capability Flags (bit flags for registration)
// ============================================================================

/// File operations: ls, cat, find
pub const CAP_FILE_OPS: u64 = 0x01;

/// Process operations: ps
pub const CAP_PROCESS_OPS: u64 = 0x02;

/// System operations: uptime
pub const CAP_SYSTEM_OPS: u64 = 0x04;

/// Command execution: arbitrary commands
pub const CAP_EXEC_OPS: u64 = 0x08;

/// Search operations: find, vector search
pub const CAP_SEARCH_OPS: u64 = 0x10;

// ============================================================================
// Status Codes
// ============================================================================

/// No handler found for this intent
pub const INTENT_STATUS_NO_HANDLER: u64 = 0xFFFE;

/// Invalid operation
pub const INTENT_STATUS_INVALID: u64 = 0xFFFF;

// ============================================================================
// Client API
// ============================================================================

/// Error types for Intent operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentError {
    /// Intent service not responding
    ServiceUnavailable,
    /// No handler registered for this intent
    NoHandler,
    /// Invalid request
    InvalidRequest,
}

/// Register this task's capabilities with the Intent Service
pub fn register(capability_flags: u64) -> Result<(), IntentError> {
    let request = INTENT_OP_REGISTER | (capability_flags << 16);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, INTENT_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        Err(IntentError::ServiceUnavailable)
    } else {
        Ok(())
    }
}

/// Submit a command to the Intent Service for routing
///
/// The `shell_opcode` is the original opcode (e.g., SHELL_OP_LIST_FILES).
/// The `arg_data` is forwarded as-is to the handler.
///
/// Returns the handler's reply directly.
pub fn submit(shell_opcode: u64, arg_data: u64) -> Result<u64, IntentError> {
    // Pack: op in bits 0-7, shell_opcode in bits 8-15, arg_data in bits 16+
    let request = INTENT_OP_SUBMIT | (shell_opcode << 8) | (arg_data << 16);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, INTENT_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        Err(IntentError::ServiceUnavailable)
    } else if ret == INTENT_STATUS_NO_HANDLER {
        Err(IntentError::NoHandler)
    } else {
        Ok(ret)
    }
}

/// Query which handler would handle a command hash
pub fn query_handler(command_hash: u32) -> Result<(u32, u32), IntentError> {
    let request = INTENT_OP_QUERY | ((command_hash as u64) << 16);

    let ret = unsafe {
        syscall3(SYS_IPC_SEND, INTENT_TASK_ID as u64, request, 0)
    };

    if ret == u64::MAX {
        Err(IntentError::ServiceUnavailable)
    } else if ret == INTENT_STATUS_NO_HANDLER {
        Err(IntentError::NoHandler)
    } else {
        let task_id = (ret & 0xFFFF) as u32;
        let confidence = ((ret >> 32) & 0xFFFF) as u32;
        Ok((task_id, confidence))
    }
}
