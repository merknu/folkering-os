//! Inter-Process Communication (IPC) syscalls
//!
//! Folkering OS uses synchronous IPC for inter-task communication.
//! Phase 6 adds Reply-Later IPC with CallerToken for async servers.

use crate::syscall::{
    syscall0, syscall1, syscall2, syscall3,
    SYS_IPC_SEND, SYS_IPC_RECEIVE, SYS_IPC_REPLY,
    SYS_IPC_RECV_ASYNC, SYS_IPC_REPLY_TOKEN, SYS_IPC_GET_RECV_PAYLOAD, SYS_IPC_GET_RECV_SENDER,
};

/// Error codes for IPC operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum IpcError {
    /// Target task not found
    NotFound = 1,
    /// Would block (no messages available)
    WouldBlock = 2,
    /// Invalid parameter
    InvalidParam = 3,
    /// Unknown error
    Unknown = 0xFFFF_FFFF_FFFF_FFFF,
}

/// Result of an IPC receive operation
#[derive(Debug, Clone, Copy)]
pub struct IpcMessage {
    /// Sender's task ID
    pub sender: u32,
    /// First payload word
    pub payload0: u64,
}

/// Send an IPC message to another task
///
/// # Arguments
/// * `target` - The target task ID
/// * `payload0` - First payload word
/// * `payload1` - Second payload word
///
/// # Returns
/// * `Ok(reply)` - The reply payload on success
/// * `Err(error)` - Error code on failure
pub fn send(target: u32, payload0: u64, payload1: u64) -> Result<u64, IpcError> {
    let ret = unsafe { syscall3(SYS_IPC_SEND, target as u64, payload0, payload1) };
    if ret == u64::MAX {
        Err(IpcError::Unknown)
    } else {
        Ok(ret)
    }
}

/// Receive an IPC message (non-blocking)
///
/// # Returns
/// * `Ok(message)` - The received message
/// * `Err(WouldBlock)` - No messages available
/// * `Err(error)` - Other error
pub fn receive() -> Result<IpcMessage, IpcError> {
    let ret = unsafe { syscall1(SYS_IPC_RECEIVE, 0) };

    // Check for error codes (high bit set = negative in signed)
    if ret >= 0xFFFF_FFFF_FFFF_FFFC {
        // Error: -3 (no message) or -4 (other error)
        return Err(IpcError::WouldBlock);
    }

    // Success: lower 32 bits = sender, upper 32 bits = payload[0]
    let sender = (ret & 0xFFFF_FFFF) as u32;
    let payload0 = ret >> 32;

    Ok(IpcMessage { sender, payload0 })
}

/// Reply to a received IPC message
///
/// # Arguments
/// * `payload0` - First reply payload word
/// * `payload1` - Second reply payload word
///
/// # Returns
/// * `Ok(())` - Reply sent successfully
/// * `Err(error)` - Error code on failure
pub fn reply(payload0: u64, payload1: u64) -> Result<(), IpcError> {
    let ret = unsafe { syscall2(SYS_IPC_REPLY, payload0, payload1) };
    if ret == u64::MAX {
        Err(IpcError::Unknown)
    } else {
        Ok(())
    }
}

// ============================================================================
// Phase 6: Reply-Later IPC (CallerToken)
// ============================================================================

/// Opaque token for deferred IPC reply.
///
/// When a server receives a request via `recv_async()`, it gets a CallerToken
/// that can be used to reply later, even after handling other messages.
///
/// # Security
/// The token is opaque and should not be modified. The kernel validates
/// that the token matches the original sender's waiting state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct CallerToken(pub u64);

impl CallerToken {
    /// Create from raw syscall value.
    #[inline]
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Get raw value for syscall.
    #[inline]
    pub fn as_raw(self) -> u64 {
        self.0
    }
}

/// Result of async IPC receive.
#[derive(Debug, Clone, Copy)]
pub struct AsyncIpcMessage {
    /// Token for later reply
    pub token: CallerToken,
    /// Sender's task ID
    pub sender: u32,
    /// First payload word
    pub payload0: u64,
}

/// Receive IPC message with CallerToken for deferred reply (non-blocking).
///
/// Unlike `receive()`, this returns a CallerToken that can be used to reply
/// later. The sender remains blocked until `reply_with_token()` is called.
///
/// # Use Case
/// Servers that need to do async/long-running work (LLM inference, disk I/O)
/// can receive a message, stash the token, and reply when ready.
///
/// # Example
/// ```no_run
/// loop {
///     match recv_async() {
///         Ok(msg) => {
///             // Stash token, spawn async work
///             let token = msg.token;
///             spawn(|| {
///                 let result = do_long_work(msg.payload0);
///                 reply_with_token(token, result, 0).unwrap();
///             });
///         }
///         Err(IpcError::WouldBlock) => {
///             // No messages, yield
///             yield_cpu();
///         }
///         Err(e) => panic!("IPC error: {:?}", e),
///     }
/// }
/// ```
///
/// # Returns
/// * `Ok(message)` - Message with token for later reply
/// * `Err(WouldBlock)` - No messages available
/// * `Err(error)` - Other error
pub fn recv_async() -> Result<AsyncIpcMessage, IpcError> {
    // First syscall: get the CallerToken (kernel returns raw token value)
    let token_raw = unsafe { syscall0(SYS_IPC_RECV_ASYNC) };

    // Check for error codes
    if token_raw >= 0xFFFF_FFFF_FFFF_FFFC {
        return Err(IpcError::WouldBlock);
    }

    // Second syscall: get the full 64-bit payload[0]
    let payload0 = unsafe { syscall0(SYS_IPC_GET_RECV_PAYLOAD) };

    if payload0 == u64::MAX {
        // Shouldn't happen if recv_async succeeded, but handle it
        return Err(IpcError::Unknown);
    }

    // Third syscall: get the sender
    let sender_ret = unsafe { syscall0(SYS_IPC_GET_RECV_SENDER) };
    let sender = if sender_ret == u64::MAX { 0 } else { sender_ret as u32 };

    // Use the actual token from kernel (properly encoded with sender_pid + msg_id)
    let token = CallerToken::from_raw(token_raw);

    Ok(AsyncIpcMessage {
        token,
        sender,
        payload0,
    })
}

/// Reply to a deferred request using CallerToken.
///
/// Unblocks the original sender that is waiting for a reply.
///
/// # Arguments
/// * `token` - CallerToken from `recv_async()`
/// * `payload0` - First reply payload word
/// * `payload1` - Second reply payload word
///
/// # Returns
/// * `Ok(())` - Reply sent, sender unblocked
/// * `Err(error)` - Invalid token or sender no longer waiting
pub fn reply_with_token(token: CallerToken, payload0: u64, payload1: u64) -> Result<(), IpcError> {
    let ret = unsafe { syscall3(SYS_IPC_REPLY_TOKEN, token.as_raw(), payload0, payload1) };
    if ret == u64::MAX {
        Err(IpcError::Unknown)
    } else {
        Ok(())
    }
}
