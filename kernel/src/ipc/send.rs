//! IPC Send Operations
//!
//! Implements synchronous (blocking) and asynchronous (non-blocking) IPC send operations.
//! Critical performance path - target <1000 cycles for fast path.

use crate::ipc::message::{IpcMessage, IpcType, TaskId};
use crate::task::task::{get_task, current_task, Task, TaskState};
use alloc::sync::Arc;
use spin::Mutex;

// Temporary stub until capability system is implemented
#[derive(Debug, Clone, Copy)]
pub enum CapabilityType {
    IpcSend(TaskId),
}

fn capability_check(_task: &Arc<Mutex<Task>>, _cap_type: CapabilityType) -> bool {
    // TODO: Implement capability checking
    // For now, allow all IPC sends
    true
}

fn transfer_capability(
    _sender: &Arc<Mutex<Task>>,
    _target: &Arc<Mutex<Task>>,
    _cap_id: u32,
) -> Result<(), Errno> {
    // TODO: Implement capability transfer
    Ok(())
}

/// IPC error codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Errno {
    /// Invalid target task ID
    EINVAL,
    /// Permission denied (missing capability)
    EPERM,
    /// Target's receive queue is full
    ENOBUFS,
    /// Target task is dead/exited
    EDEAD,
    /// Capability transfer failed
    ECAPFAIL,
    /// Task not found
    ENOTASK,
}

/// Synchronous IPC send (blocking until reply)
///
/// # Flow
/// 1. Validate target exists
/// 2. Check IpcSendCap capability
/// 3. Copy message to kernel buffer (64 bytes)
/// 4. Set sender field (authenticated by kernel)
/// 5. Transfer capability if present
/// 6. Enqueue to target's receive queue
/// 7. Block current task (wait for reply)
/// 8. Wake target if blocked on receive
/// 9. Context switch
/// 10. Return reply when unblocked
///
/// # Arguments
/// - `target`: Target task ID
/// - `msg`: Message to send (sender field ignored, will be set by kernel)
///
/// # Returns
/// - `Ok(reply)`: Reply message from target
/// - `Err(errno)`: Error code
///
/// # Performance
/// - Fast path (target waiting): ~1000 cycles (direct context switch)
/// - Slow path (target busy): ~3000 cycles (queue + scheduler overhead)
///
/// # Example
/// ```no_run
/// use folkering_kernel::ipc::{IpcMessage, ipc_send};
///
/// let msg = IpcMessage::new_request([1, 2, 3, 4]);
/// let reply = ipc_send(server_task_id, &msg)?;
/// ```
#[inline(never)] // Prevent inlining to keep instruction cache clean
pub fn ipc_send(target: TaskId, msg: &IpcMessage) -> Result<IpcMessage, Errno> {
    // 1. Validate target task exists
    let target_task = get_task(target).ok_or(Errno::EINVAL)?;

    // Check if target is dead
    if target_task.lock().state == TaskState::Exited {
        return Err(Errno::EDEAD);
    }

    // 2. Check IpcSend capability
    let current = current_task();

    if !capability_check(&current, CapabilityType::IpcSend(target)) {
        return Err(Errno::EPERM);
    }

    // 3. Copy message to kernel buffer (64 bytes - exactly one cache line)
    // This is fast: ~3-4 cycles with cache hit
    let mut kernel_msg = *msg;

    // 4. Set sender field (security: authenticated by kernel)
    kernel_msg.sender = current.lock().id;
    kernel_msg.msg_type = IpcType::Request;

    // Assign unique message ID
    kernel_msg.msg_id = crate::ipc::next_message_id();

    // 5. Transfer capability if present
    if let Some(cap_id) = kernel_msg.cap {
        transfer_capability(&current, &target_task, cap_id.get())?;
    }

    // 6. Enqueue message to target's receive queue
    {
        let mut target_lock = target_task.lock();
        if !target_lock.recv_queue.push(kernel_msg) {
            return Err(Errno::ENOBUFS);
        }
    }

    // 7. Block current task (wait for reply)
    {
        let mut current_lock = current.lock();
        current_lock.state = TaskState::BlockedOnSend(target);
        current_lock.ipc_reply = None; // Clear previous reply
    }

    // 8. Wake target if it's waiting on receive
    {
        let mut target_lock = target_task.lock();
        if target_lock.state == TaskState::BlockedOnReceive {
            target_lock.state = TaskState::Runnable;
            // Add to scheduler runqueue
            crate::task::scheduler::enqueue(target);
        }
    }

    // 9. Context switch (yield CPU to scheduler)
    // This will block until target calls ipc_reply()
    crate::task::scheduler::yield_cpu();

    // 10. (Resumed after reply) Return reply message
    let reply = current.lock()
        .ipc_reply
        .take()
        .ok_or(Errno::EINVAL)?;

    Ok(reply)
}

/// Asynchronous IPC send (non-blocking, fire-and-forget)
///
/// # Flow
/// 1. Validate target exists
/// 2. Check IpcSendCap capability
/// 3. Copy message to kernel buffer (64 bytes)
/// 4. Set sender field
/// 5. Transfer capability if present
/// 6. Enqueue to target's receive queue
/// 7. Wake target if blocked on receive
/// 8. Return immediately (no blocking)
///
/// # Arguments
/// - `target`: Target task ID
/// - `msg`: Message to send
///
/// # Returns
/// - `Ok(())`: Message enqueued successfully
/// - `Err(errno)`: Error code
///
/// # Performance
/// - ~200-500 cycles (no context switch)
///
/// # Use Cases
/// - Notifications (e.g., "interrupt occurred")
/// - Logging (fire-and-forget)
/// - Events (e.g., "key pressed")
///
/// # Example
/// ```no_run
/// use folkering_kernel::ipc::{IpcMessage, ipc_send_async};
///
/// let notification = IpcMessage::new_notification([42, 0, 0, 0]);
/// ipc_send_async(driver_task_id, &notification)?;
/// ```
#[inline(never)]
pub fn ipc_send_async(target: TaskId, msg: &IpcMessage) -> Result<(), Errno> {
    // 1. Validate target task exists
    let target_task = get_task(target).ok_or(Errno::EINVAL)?;

    // Check if target is dead
    if target_task.lock().state == TaskState::Exited {
        return Err(Errno::EDEAD);
    }

    // 2. Check IpcSend capability
    let current = current_task();

    if !capability_check(&current, CapabilityType::IpcSend(target)) {
        return Err(Errno::EPERM);
    }

    // 3. Copy message to kernel buffer
    let mut kernel_msg = *msg;

    // 4. Set sender field (authenticated by kernel)
    kernel_msg.sender = current.lock().id;
    kernel_msg.msg_type = IpcType::Notification;

    // Assign unique message ID
    kernel_msg.msg_id = crate::ipc::next_message_id();

    // 5. Transfer capability if present
    if let Some(cap_id) = kernel_msg.cap {
        transfer_capability(&current, &target_task, cap_id.get())?;
    }

    // 6. Enqueue message to target's receive queue
    {
        let mut target_lock = target_task.lock();
        if !target_lock.recv_queue.push(kernel_msg) {
            return Err(Errno::ENOBUFS);
        }
    }

    // 7. Wake target if it's waiting on receive
    {
        let mut target_lock = target_task.lock();
        if target_lock.state == TaskState::BlockedOnReceive {
            target_lock.state = TaskState::Runnable;
            // Add to scheduler runqueue
            crate::task::scheduler::enqueue(target);
        }
    }

    // 8. Return immediately (no blocking)
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_errno_size() {
        // Errno should be small (single byte)
        assert_eq!(core::mem::size_of::<Errno>(), 1);
    }

    #[test]
    fn test_errno_values() {
        // Verify error codes are distinct
        assert_ne!(Errno::EINVAL, Errno::EPERM);
        assert_ne!(Errno::EPERM, Errno::ENOBUFS);
        assert_ne!(Errno::ENOBUFS, Errno::EDEAD);
    }
}
