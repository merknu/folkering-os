//! IPC Send Operations
//!
//! Implements synchronous (blocking) and asynchronous (non-blocking) IPC send operations.
//! Critical performance path - target <1000 cycles for fast path.

use crate::ipc::message::{IpcMessage, IpcType, TaskId};
use crate::task::task::{get_task, current_task, Task, TaskState};
use crate::capability::{self, CapabilityType};
use alloc::sync::Arc;
use spin::Mutex;

/// Check if task has capability to send IPC to target
fn capability_check(task: &Arc<Mutex<Task>>, target: TaskId) -> bool {
    let task_id = task.lock().id;
    capability::has_ipc_send(task_id, target)
}

fn transfer_capability(
    sender: &Arc<Mutex<Task>>,
    target: &Arc<Mutex<Task>>,
    cap_id: u32,
) -> Result<(), Errno> {
    let sender_id = sender.lock().id;
    let target_id = target.lock().id;

    // Type-enforced gate: TransferableCap::check rejects hardware-
    // bound caps (DmaRegion, MmioRegion, IoPort, Framebuffer,
    // DriverPrivilege, RawBlockIO) at construction time. The
    // compiler won't let us pass a raw `cap_id` to `transfer()`.
    let cap = capability::TransferableCap::check(cap_id)
        .ok_or(Errno::ECAPFAIL)?;

    capability::transfer(sender_id, target_id, cap)
        .map_err(|_| Errno::ECAPFAIL)?;
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
    /// Operation would block (syscall should yield)
    EWOULDBLOCK,
    /// No such process (task no longer exists) - used by reply-later IPC
    ESRCH,
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

    if !capability_check(&current, target) {
        return Err(Errno::EPERM);
    }

    // 3. Copy message to kernel buffer (64 bytes - exactly one cache line)
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

    // 6-9: Enqueue, block, wake, yield — all within interrupt-disabled section.
    // CRITICAL: Prevents timer preemption from seeing inconsistent state
    // (e.g., task marked BlockedOnSend but still running). The closure
    // ensures IF is restored on ALL exit paths including early Err returns.
    let enqueue_result = x86_64::instructions::interrupts::without_interrupts(|| {
        // 6. Enqueue message to target's receive queue
        {
            let mut target_lock = target_task.lock();
            if !target_lock.recv_queue.push(kernel_msg) {
                return Err(Errno::ENOBUFS);
            }
        }

        // 7. Block current task (wait for reply) + priority inheritance
        let sender_priority;
        {
            let mut current_lock = current.lock();
            sender_priority = current_lock.priority;
            current_lock.state = TaskState::BlockedOnSend(target);
            // Stamp the target so `syscall_exit` on the receiver can
            // find us via TASK_TABLE scan and unblock us with an
            // error instead of letting us hang forever.
            current_lock.blocked_on = Some(target);
            current_lock.ipc_reply = None;
        }

        // 8. Wake target if it's waiting on receive.
        //    Priority inheritance: if sender has higher priority than target,
        //    temporarily boost target so it runs sooner (avoids priority inversion).
        {
            let mut target_lock = target_task.lock();
            if sender_priority > target_lock.inherited_priority {
                target_lock.inherited_priority = sender_priority;
            }
            if target_lock.state == TaskState::BlockedOnReceive {
                target_lock.state = TaskState::Runnable;
                crate::task::scheduler::enqueue(target);
            }
        }

        Ok(())
    });

    // Propagate ENOBUFS if enqueue failed (interrupts already restored)
    enqueue_result?;

    // 9. Context switch (yield CPU to scheduler)
    // Interrupts are re-enabled here — scheduler will preempt when ready
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

    if !capability_check(&current, target) {
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
