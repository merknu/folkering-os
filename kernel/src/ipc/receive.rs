//! IPC Receive Operations
//!
//! Implements blocking and non-blocking IPC receive operations, plus reply mechanism.
//! Critical for request-reply pattern performance.

use crate::ipc::message::{IpcMessage, IpcType, TaskId};
use crate::task::task::{current_task, get_task, TaskState};
use super::send::Errno;

/// Blocking IPC receive
///
/// Blocks until a message arrives in the current task's receive queue.
/// If messages are already queued, returns immediately (fast path).
///
/// # Flow
/// 1. Get current task
/// 2. Check if messages in receive queue
/// 3. If yes: dequeue and return (fast path)
/// 4. If no: block until message arrives (slow path)
/// 5. When woken by sender: dequeue message and return
///
/// # Returns
/// - `Ok(message)`: Received IPC message
/// - `Err(errno)`: Error code
///
/// # Performance
/// - Fast path (message ready): ~100 cycles (queue pop only)
/// - Slow path (block): ~1000 cycles + context switch overhead
///
/// # Example
/// ```no_run
/// use folkering_kernel::ipc::ipc_receive;
///
/// loop {
///     let request = ipc_receive()?;
///
///     // Process request
///     let result = process_request(&request);
///
///     // Send reply
///     ipc_reply(&result)?;
/// }
/// ```
#[inline(never)] // Keep instruction cache clean
pub fn ipc_receive() -> Result<IpcMessage, Errno> {
    let current = current_task();

    loop {
        // Check if messages in queue (fast path)
        {
            let mut current_lock = current.lock();
            if let Some(msg) = current_lock.recv_queue.pop() {
                return Ok(msg);
            }
        }

        // No messages - block until one arrives (slow path)
        {
            let mut current_lock = current.lock();
            current_lock.state = TaskState::BlockedOnReceive;
        }

        // Yield CPU to scheduler - will resume when message arrives
        crate::task::scheduler::yield_cpu();

        // Will resume here when message arrives
        // Loop back to check queue again
    }
}

/// Non-blocking IPC receive
///
/// Returns immediately if no messages are available.
/// Useful for polling-style message processing.
///
/// # Returns
/// - `Ok(Some(message))`: Received IPC message
/// - `Ok(None)`: No messages available
/// - `Err(errno)`: Error code
///
/// # Performance
/// - ~50-100 cycles (queue check only, no blocking)
///
/// # Use Cases
/// - Event loops that need to check multiple sources
/// - Servers that want to batch process messages
/// - Non-blocking I/O patterns
///
/// # Example
/// ```no_run
/// use folkering_kernel::ipc::ipc_try_receive;
///
/// loop {
///     // Check for messages
///     if let Some(msg) = ipc_try_receive()? {
///         process_message(msg);
///     }
///
///     // Do other work
///     do_background_work();
/// }
/// ```
#[inline]
pub fn ipc_try_receive() -> Result<Option<IpcMessage>, Errno> {
    let current = current_task();

    // Non-blocking check
    let msg = current.lock().recv_queue.pop();
    Ok(msg)
}

/// IPC reply (for request-reply pattern)
///
/// Sends a reply message to the task that sent the original request.
/// Unblocks the sender and allows it to resume execution.
///
/// # Flow
/// 1. Get current task
/// 2. Find sender task (from message.sender)
/// 3. Verify sender is blocked on us
/// 4. Copy reply to sender's buffer
/// 5. Unblock sender
/// 6. Add sender to scheduler runqueue
///
/// # Arguments
/// - `reply`: Reply message to send back to sender
///
/// # Returns
/// - `Ok(())`: Reply sent successfully
/// - `Err(errno)`: Error code
///
/// # Performance
/// - ~200-500 cycles (message copy + task wakeup)
///
/// # Security
/// - Can only reply to tasks that sent us a Request message
/// - Cannot spoof replies to arbitrary tasks
///
/// # Example
/// ```no_run
/// use folkering_kernel::ipc::{IpcMessage, ipc_receive, ipc_reply};
///
/// // Server loop
/// loop {
///     let request = ipc_receive()?;
///
///     // Process request
///     let result = match request.payload[0] {
///         1 => handle_open(request),
///         2 => handle_read(request),
///         3 => handle_write(request),
///         _ => Err(Errno::EINVAL),
///     };
///
///     // Send reply
///     let reply = IpcMessage::new_reply([result as u64, 0, 0, 0]);
///     ipc_reply(&reply)?;
/// }
/// ```
#[inline]
pub fn ipc_reply(request: &IpcMessage, reply_payload: [u64; 4]) -> Result<(), Errno> {
    let current = current_task();

    // CRITICAL FIX: Use request.sender (the task that sent us the request),
    // not reply.sender (which would be wrong!)
    let sender_id = request.sender;
    let sender_task = get_task(sender_id).ok_or(Errno::EINVAL)?;

    // Verify sender is blocked on us (security check)
    {
        let sender_lock = sender_task.lock();
        let current_id = current.lock().id;

        match sender_lock.state {
            TaskState::BlockedOnSend(target) if target == current_id => {
                // Valid: sender is blocked waiting for our reply
            }
            _ => {
                // Invalid: sender is not blocked on us
                return Err(Errno::EINVAL);
            }
        }
    }

    // Create reply message
    let mut reply_msg = IpcMessage::new_reply(reply_payload);
    reply_msg.sender = current.lock().id;
    reply_msg.msg_id = crate::ipc::next_message_id();

    // Copy reply to sender's buffer
    {
        let mut sender_lock = sender_task.lock();
        sender_lock.ipc_reply = Some(reply_msg);
    }

    // Unblock sender (make it runnable)
    {
        let mut sender_lock = sender_task.lock();
        sender_lock.state = TaskState::Runnable;
    }

    // Add sender to scheduler runqueue
    crate::task::scheduler::enqueue(sender_id);

    Ok(())
}

/// Receive with timeout (future enhancement)
///
/// Blocks until a message arrives or timeout expires.
/// Not implemented in Phase 1 - reserved for future use.
///
/// # Arguments
/// - `timeout_us`: Timeout in microseconds
///
/// # Returns
/// - `Ok(Some(message))`: Received message before timeout
/// - `Ok(None)`: Timeout expired, no messages
/// - `Err(errno)`: Error code
#[allow(dead_code)]
pub fn ipc_receive_timeout(_timeout_us: u64) -> Result<Option<IpcMessage>, Errno> {
    // TODO: Implement timeout mechanism
    // Requires timer integration
    unimplemented!("ipc_receive_timeout not yet implemented")
}

/// Selective receive (future enhancement)
///
/// Receive only messages matching a predicate.
/// Useful for servers that want to prioritize certain message types.
///
/// # Arguments
/// - `predicate`: Function to filter messages
///
/// # Returns
/// - `Ok(message)`: First message matching predicate
/// - `Err(errno)`: Error code
///
/// # Example (future)
/// ```ignore
/// // Only receive messages from trusted tasks
/// let msg = ipc_receive_selective(|msg| {
///     trusted_tasks.contains(&msg.sender)
/// })?;
/// ```
#[allow(dead_code)]
pub fn ipc_receive_selective<F>(_predicate: F) -> Result<IpcMessage, Errno>
where
    F: Fn(&IpcMessage) -> bool,
{
    // TODO: Implement selective receive
    // Requires queue scanning logic
    unimplemented!("ipc_receive_selective not yet implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_receive_operations_exist() {
        // Verify functions are exported
        let _: fn() -> Result<IpcMessage, Errno> = ipc_receive;
        let _: fn() -> Result<Option<IpcMessage>, Errno> = ipc_try_receive;
        let _: fn(&IpcMessage) -> Result<(), Errno> = ipc_reply;
    }
}
