//! Inter-Process Communication (IPC) syscalls
//!
//! Folkering OS uses synchronous IPC for inter-task communication.

use crate::syscall::{
    syscall1, syscall2, syscall3,
    SYS_IPC_SEND, SYS_IPC_RECEIVE, SYS_IPC_REPLY,
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
