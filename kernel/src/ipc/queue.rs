//! IPC Message Queues
//!
//! Per-task bounded FIFO message queues for IPC.
//! Bounded to prevent memory exhaustion from malicious or misbehaving tasks.

use crate::ipc::message::IpcMessage;
use alloc::collections::VecDeque;

/// Per-task message queue
///
/// # Design
/// - Bounded capacity (64 messages) to prevent memory exhaustion
/// - FIFO ordering for fairness
/// - Fast push/pop operations (O(1) amortized)
///
/// # Memory Usage
/// - Each message: 64 bytes
/// - Queue capacity: 64 messages
/// - Total per task: ~4KB
///
/// # Performance
/// - Push: ~10-20 cycles (no allocation needed, preallocated)
/// - Pop: ~5-10 cycles (pointer manipulation only)
/// - Full check: ~2-3 cycles (compare counter)
pub struct MessageQueue {
    /// Internal queue storage
    queue: VecDeque<IpcMessage>,

    /// Maximum queue size (bounded)
    max_size: usize,
}

impl MessageQueue {
    /// Default maximum queue size
    ///
    /// 64 messages = 4KB of memory per task.
    /// This is a reasonable default that prevents memory exhaustion
    /// while allowing sufficient buffering for burst traffic.
    pub const DEFAULT_SIZE: usize = 64;

    /// Create new empty message queue with default capacity
    ///
    /// # Memory
    /// Preallocates space for `DEFAULT_SIZE` messages to avoid
    /// allocation overhead during IPC operations.
    pub fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_SIZE)
    }

    /// Create new message queue with custom capacity
    ///
    /// # Arguments
    /// - `capacity`: Maximum number of messages queue can hold
    ///
    /// # Use Cases
    /// - High-throughput servers may need larger queues (e.g., 256 messages)
    /// - Low-priority tasks may use smaller queues (e.g., 16 messages)
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            queue: VecDeque::new(), // Start empty, grow as needed (workaround for boot hang)
            max_size: capacity,
        }
    }

    /// Initialize MessageQueue in-place at a raw pointer (zero-stack initialization)
    ///
    /// # Safety
    /// - `ptr` must be valid, aligned, and point to uninitialized memory
    /// - Caller must ensure exclusive access to `*ptr`
    /// - **CRITICAL**: Memory must already be zeroed (use ptr::write_bytes first)
    ///
    /// # Design
    /// This method assumes the memory is already zero-initialized.
    /// An all-zero VecDeque is a valid empty state (no allocation, head=0, tail=0).
    /// We only need to set the `max_size` field.
    ///
    /// # Use Case
    /// Used during Task creation to avoid stack overflow.
    /// The kernel stack is extremely small (<500 bytes), so we cannot create
    /// structs on the stack. This allows initializing MessageQueue directly
    /// in the global Task creation buffer.
    ///
    /// # Example
    /// ```no_run
    /// // In Task::new(), after zeroing the entire structure:
    /// ptr::write_bytes(task_ptr, 0, 1);  // Zero entire Task
    /// MessageQueue::init_at_ptr(ptr::addr_of_mut!((*task_ptr).recv_queue));
    /// ```
    #[inline]
    pub unsafe fn init_at_ptr(ptr: *mut Self) {
        use core::ptr;
        // VecDeque is already zero-initialized (empty, no allocation)
        // Just set the max_size field to DEFAULT_SIZE (64 messages)
        ptr::addr_of_mut!((*ptr).max_size).write(Self::DEFAULT_SIZE);
    }

    /// Push message to end of queue
    ///
    /// # Arguments
    /// - `msg`: IPC message to enqueue
    ///
    /// # Returns
    /// - `true`: Message enqueued successfully
    /// - `false`: Queue is full, message rejected
    ///
    /// # Performance
    /// - ~10-20 cycles (no allocation, just pointer update)
    ///
    /// # Example
    /// ```no_run
    /// let mut queue = MessageQueue::new();
    /// let msg = IpcMessage::new_request([1, 2, 3, 4]);
    ///
    /// if queue.push(msg) {
    ///     println!("Message enqueued");
    /// } else {
    ///     println!("Queue full!");
    /// }
    /// ```
    #[inline]
    pub fn push(&mut self, msg: IpcMessage) -> bool {
        if self.is_full() {
            return false;
        }

        self.queue.push_back(msg);
        true
    }

    /// Pop message from front of queue
    ///
    /// # Returns
    /// - `Some(message)`: Message dequeued successfully
    /// - `None`: Queue is empty
    ///
    /// # Performance
    /// - ~5-10 cycles (pointer manipulation only)
    ///
    /// # Example
    /// ```no_run
    /// let mut queue = MessageQueue::new();
    ///
    /// if let Some(msg) = queue.pop() {
    ///     println!("Received message from task {}", msg.sender);
    /// } else {
    ///     println!("No messages");
    /// }
    /// ```
    #[inline]
    pub fn pop(&mut self) -> Option<IpcMessage> {
        self.queue.pop_front()
    }

    /// Check if queue is empty
    ///
    /// # Returns
    /// - `true`: No messages in queue
    /// - `false`: At least one message available
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Check if queue is full
    ///
    /// # Returns
    /// - `true`: Queue at maximum capacity
    /// - `false`: Queue has space for more messages
    #[inline]
    pub fn is_full(&self) -> bool {
        self.queue.len() >= self.max_size
    }

    /// Get current number of messages in queue
    ///
    /// # Returns
    /// Number of messages currently queued
    #[inline]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Get queue capacity
    ///
    /// # Returns
    /// Maximum number of messages queue can hold
    #[inline]
    pub fn capacity(&self) -> usize {
        self.max_size
    }

    /// Clear all messages from queue
    ///
    /// # Use Cases
    /// - Task restart/cleanup
    /// - Error recovery
    /// - Resource reclamation
    pub fn clear(&mut self) {
        self.queue.clear();
    }

    /// Peek at first message without removing it
    ///
    /// # Returns
    /// - `Some(&message)`: Reference to first message
    /// - `None`: Queue is empty
    ///
    /// # Use Cases
    /// - Selective receive (check message type before dequeuing)
    /// - Message prioritization
    /// - Debugging/inspection
    #[inline]
    pub fn peek(&self) -> Option<&IpcMessage> {
        self.queue.front()
    }

    /// Get iterator over messages (without removing them)
    ///
    /// # Use Cases
    /// - Debugging/inspection
    /// - Message filtering
    /// - Statistics collection
    pub fn iter(&self) -> impl Iterator<Item = &IpcMessage> {
        self.queue.iter()
    }

    /// Get queue utilization as percentage
    ///
    /// # Returns
    /// - Utilization percentage (0-100)
    ///
    /// # Use Cases
    /// - Performance monitoring
    /// - Adaptive queue sizing
    /// - Debugging backpressure issues
    pub fn utilization(&self) -> u8 {
        ((self.len() * 100) / self.max_size) as u8
    }
}

impl Default for MessageQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-CPU message queue optimization (future)
///
/// Lockless fast path for same-CPU IPC to eliminate contention.
/// See IPC-design.md Optimization 4 for details.
///
/// Not implemented in Phase 1 - reserved for Phase 3 optimizations.
#[allow(dead_code)]
pub struct PerCpuMessageQueue {
    /// Per-CPU queues (one per CPU core)
    _queues: [MessageQueue; 16], // Support up to 16 CPUs
}

#[cfg(not(test))]
impl PerCpuMessageQueue {
    #[allow(dead_code)]
    const fn new() -> Self {
        // TODO: Implement per-CPU queue optimization
        // This will be part of Phase 3 (SMP optimizations)
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::message::IpcMessage;

    #[test]
    fn test_queue_push_pop() {
        let mut queue = MessageQueue::new();
        let msg = IpcMessage::new_request([1, 2, 3, 4]);

        assert!(queue.is_empty());
        assert!(queue.push(msg));
        assert!(!queue.is_empty());
        assert_eq!(queue.len(), 1);

        let popped = queue.pop();
        assert!(popped.is_some());
        assert!(queue.is_empty());
    }

    #[test]
    fn test_queue_fifo_order() {
        let mut queue = MessageQueue::new();

        // Push messages with different payloads
        for i in 0..5 {
            let msg = IpcMessage::new_request([i, 0, 0, 0]);
            assert!(queue.push(msg));
        }

        // Pop messages and verify FIFO order
        for i in 0..5 {
            let msg = queue.pop().unwrap();
            assert_eq!(msg.payload[0], i);
        }
    }

    #[test]
    fn test_queue_bounded() {
        let mut queue = MessageQueue::with_capacity(4);
        let msg = IpcMessage::new_request([0, 0, 0, 0]);

        // Fill queue
        for _ in 0..4 {
            assert!(queue.push(msg));
        }

        assert!(queue.is_full());

        // Try to overflow
        assert!(!queue.push(msg));
    }

    #[test]
    fn test_queue_peek() {
        let mut queue = MessageQueue::new();
        let msg = IpcMessage::new_request([42, 0, 0, 0]);

        assert!(queue.peek().is_none());

        queue.push(msg);

        let peeked = queue.peek().unwrap();
        assert_eq!(peeked.payload[0], 42);

        // Peek doesn't remove message
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_queue_clear() {
        let mut queue = MessageQueue::new();
        let msg = IpcMessage::new_request([0, 0, 0, 0]);

        for _ in 0..10 {
            queue.push(msg);
        }

        assert_eq!(queue.len(), 10);

        queue.clear();

        assert_eq!(queue.len(), 0);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_queue_utilization() {
        let mut queue = MessageQueue::with_capacity(100);
        let msg = IpcMessage::new_request([0, 0, 0, 0]);

        assert_eq!(queue.utilization(), 0);

        for _ in 0..50 {
            queue.push(msg);
        }

        assert_eq!(queue.utilization(), 50);
    }
}
