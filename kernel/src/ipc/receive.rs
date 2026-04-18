//! IPC Receive Operations
//!
//! Implements blocking and non-blocking IPC receive operations, plus reply mechanism.
//! Critical for request-reply pattern performance.

use crate::ipc::message::{IpcMessage, IpcType, TaskId, CallerToken};
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

    // Check if messages in queue (fast path)
    {
        let mut current_lock = current.lock();
        if let Some(msg) = current_lock.recv_queue.pop() {
            return Ok(msg);
        }
    }

    // No messages - mark task as blocked and return EWOULDBLOCK
    // The syscall handler will yield properly
    {
        let mut current_lock = current.lock();
        current_lock.state = TaskState::BlockedOnReceive;
    }

    // Return error to indicate syscall should yield
    Err(Errno::EWOULDBLOCK)
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
    let sender_id = request.sender;
    let sender_task = get_task(sender_id).ok_or(Errno::EINVAL)?;

    // Read current task ID ONCE — avoids lock ordering violation.
    // Previously: sender_task.lock() THEN current.lock() → deadlock risk.
    let current_id = current.lock().id;

    // Single sender lock acquisition for verify + update + unblock
    {
        let mut sender_lock = sender_task.lock();

        // Verify sender is blocked on us (security check)
        match sender_lock.state {
            TaskState::BlockedOnSend(target) if target == current_id => {}
            _ => return Err(Errno::EINVAL),
        }

        // Create reply and deliver in one critical section
        let mut reply_msg = IpcMessage::new_reply(reply_payload);
        reply_msg.sender = current_id;
        reply_msg.msg_id = crate::ipc::next_message_id();
        sender_lock.ipc_reply = Some(reply_msg);
        sender_lock.context.rax = reply_payload[0];
        sender_lock.state = TaskState::Runnable;
        // Clear blocked_on so exit-time sweep doesn't touch us.
        sender_lock.blocked_on = None;
    }

    // Reset priority inheritance (we finished serving the sender's request)
    {
        let mut current_lock = current.lock();
        current_lock.inherited_priority = 0;
    }

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

// ============================================================================
// Reply-Later IPC (Phase 6 - Semantic Mirror Foundation)
// ============================================================================

/// Async IPC receive - returns CallerToken for deferred reply.
///
/// Unlike `ipc_receive()`, this function returns immediately with a token
/// that can be used to reply later. The sender remains BLOCKED until
/// `ipc_reply_with_token()` is called with the matching token.
///
/// # Use Case
/// Servers that need to do long-running work (LLM inference, disk I/O)
/// without blocking other IPC operations.
///
/// # Flow
/// 1. Server calls `ipc_recv_async()` - returns token + message
/// 2. Server does long-running work (sender stays blocked)
/// 3. Server calls `ipc_reply_with_token(token, data)` - unblocks sender
///
/// # Returns
/// - `Ok((token, message))`: Token for later reply + received message
/// - `Err(EWOULDBLOCK)`: No messages in queue
///
/// # Example
/// ```no_run
/// loop {
///     match ipc_recv_async() {
///         Ok((token, msg)) => {
///             // Spawn async work
///             spawn_task(|| {
///                 let result = do_inference(msg.payload);
///                 ipc_reply_with_token(token, result).unwrap();
///             });
///         }
///         Err(Errno::EWOULDBLOCK) => {
///             // No messages, do other work
///             process_background_tasks();
///         }
///         Err(e) => panic!("IPC error: {:?}", e),
///     }
/// }
/// ```
#[inline(never)]
pub fn ipc_recv_async() -> Result<(CallerToken, IpcMessage), Errno> {
    let current = current_task();

    // Check if messages in queue
    let msg = {
        let mut current_lock = current.lock();
        current_lock.recv_queue.pop()
    };

    match msg {
        Some(msg) => {
            // Generate token from sender PID and message ID
            let token = CallerToken::new(msg.sender, msg.msg_id);

            // Mark sender as WaitingForReply (if it's a Request)
            if msg.msg_type == IpcType::Request {
                if let Some(sender_task) = get_task(msg.sender) {
                    let mut sender_lock = sender_task.lock();
                    // Only update if sender is currently blocked on us
                    let current_id = current.lock().id;
                    if sender_lock.state == TaskState::BlockedOnSend(current_id) {
                        sender_lock.state = TaskState::WaitingForReply(msg.msg_id);
                        // Keep `blocked_on` stamped — we remain the
                        // owing party until we reply via token.
                    }
                }
            }

            Ok((token, msg))
        }
        None => Err(Errno::EWOULDBLOCK),
    }
}

/// Reply to a deferred request using CallerToken.
///
/// This function unblocks the original sender that is waiting in
/// `WaitingForReply` state. The token must match the original request.
///
/// # Security
/// - Token is decoded to extract sender_pid and request_id
/// - Kernel verifies sender is in WaitingForReply(request_id) state
/// - Reply is rejected if states don't match (prevents spoofing)
///
/// # Arguments
/// - `token`: CallerToken from `ipc_recv_async()`
/// - `reply_payload`: 4x u64 payload to send back
///
/// # Returns
/// - `Ok(())`: Reply sent, sender unblocked
/// - `Err(EINVAL)`: Invalid token or sender not waiting
/// - `Err(ESRCH)`: Sender task no longer exists
///
/// # Performance
/// ~200-500 cycles (similar to `ipc_reply`)
#[inline(never)]
pub fn ipc_reply_with_token(token: CallerToken, reply_payload: [u64; 4]) -> Result<(), Errno> {
    let current = current_task();
    let (sender_pid, request_id) = token.decode().ok_or(Errno::EINVAL)?;
    let sender_task = get_task(sender_pid).ok_or(Errno::ESRCH)?;

    // Read current ID ONCE — avoids lock ordering violation
    let current_id = current.lock().id;

    // Single sender lock acquisition for verify + update + unblock
    {
        let mut sender_lock = sender_task.lock();

        match sender_lock.state {
            TaskState::WaitingForReply(req_id) if req_id == request_id => {}
            _ => return Err(Errno::EINVAL),
        }

        let mut reply_msg = IpcMessage::new_reply(reply_payload);
        reply_msg.sender = current_id;
        reply_msg.msg_id = crate::ipc::next_message_id();
        sender_lock.ipc_reply = Some(reply_msg);
        sender_lock.context.rax = reply_payload[0];
        sender_lock.state = TaskState::Runnable;
        // Clear blocked_on so exit-time sweep doesn't touch us.
        sender_lock.blocked_on = None;
    }

    // Reset priority inheritance on current task (we just finished
    // serving the high-priority sender's request)
    {
        let mut current_lock = current.lock();
        current_lock.inherited_priority = 0;
    }

    crate::task::scheduler::enqueue(sender_pid);
    Ok(())
}

/// Wake every task blocked on `exiting_task` with an error sentinel.
///
/// Called from `syscall_exit`. Without this, if a server task (e.g.
/// synapse, intent-service) crashes, every client currently sitting
/// in `BlockedOnSend(server)` or `WaitingForReply(...)` against it
/// hangs indefinitely — the reply they're waiting for will never
/// arrive and no other code path unblocks them.
///
/// We identify such waiters via the `blocked_on` stamp that
/// `ipc_send` sets when the sender blocks. On unblock we stash
/// `u64::MAX` in the waiter's `context.rax` so its syscall returns
/// the standard error sentinel, and re-enqueue it in the scheduler.
///
/// Returns the number of tasks unblocked (for logging).
pub fn unblock_waiters_for(exiting_task: crate::ipc::message::TaskId) -> u32 {
    use crate::task::task::{TASK_TABLE, TaskState};

    let mut unblocked = 0u32;
    // Snapshot task IDs first so we can release TASK_TABLE before
    // taking per-task locks — avoids holding two locks simultaneously.
    let candidate_ids: alloc::vec::Vec<crate::ipc::message::TaskId> = {
        let table = TASK_TABLE.lock();
        table.iter().map(|(&id, _)| id).collect()
    };

    for tid in candidate_ids {
        let task_arc = match crate::task::task::get_task(tid) {
            Some(t) => t,
            None => continue,
        };
        let mut t = task_arc.lock();
        if t.blocked_on != Some(exiting_task) {
            continue;
        }
        let should_wake = matches!(
            t.state,
            TaskState::BlockedOnSend(x) if x == exiting_task
        ) || matches!(t.state, TaskState::WaitingForReply(_));
        if !should_wake {
            continue;
        }
        // Signal failure via syscall return register. The u64::MAX
        // sentinel is what every affected syscall path already uses
        // to indicate "kernel-level failure".
        t.context.rax = u64::MAX;
        t.state = TaskState::Runnable;
        t.blocked_on = None;
        t.ipc_reply = None;
        unblocked += 1;
        drop(t);
        crate::task::scheduler::enqueue(tid);
    }
    unblocked
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
