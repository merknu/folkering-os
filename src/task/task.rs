//! Task Structure and Global Task Table

use super::TaskId;
use crate::ipc::{IpcMessage, MessageQueue};
use crate::memory::PageTable;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

/// Global task table - maps TaskId to Task structure
static TASK_TABLE: Mutex<BTreeMap<TaskId, Arc<Mutex<Task>>>> = Mutex::new(BTreeMap::new());

/// Current task ID per CPU (single-core for now)
static CURRENT_TASK_ID: AtomicU32 = AtomicU32::new(0);

/// Next available task ID
static NEXT_TASK_ID: AtomicU32 = AtomicU32::new(1);

/// Task (process) structure
pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    pub page_table: PageTable,
    pub context: Context,

    // IPC fields
    pub recv_queue: MessageQueue,
    pub ipc_reply: Option<IpcMessage>,
    pub blocked_on: Option<TaskId>,

    // Security fields
    pub capabilities: Vec<u32>, // CapabilityId = u32
    pub credentials: Credentials,
}

/// CPU context for task switching
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Context {
    pub rsp: u64,
    pub rbp: u64,
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
    pub cs: u64,
    pub ss: u64,
}

impl Context {
    pub const fn zero() -> Self {
        Self {
            rsp: 0, rbp: 0, rax: 0, rbx: 0, rcx: 0, rdx: 0,
            rsi: 0, rdi: 0, r8: 0, r9: 0, r10: 0, r11: 0,
            r12: 0, r13: 0, r14: 0, r15: 0, rip: 0,
            rflags: 0x202, // IF=1 (interrupts enabled)
            cs: 0x08, // Kernel code segment
            ss: 0x10, // Kernel data segment
        }
    }
}

/// Task credentials (user, group, sandbox level)
#[derive(Clone, Copy, Debug)]
pub struct Credentials {
    pub uid: u32,
    pub gid: u32,
    pub sandbox_level: SandboxLevel,
}

/// Sandbox isolation level
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxLevel {
    System,    // Kernel and critical services
    Trusted,   // Authenticated user services
    Untrusted, // Third-party applications
    Confined,  // Maximum isolation
}

/// Task state
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Runnable,
    Running,
    BlockedOnReceive,
    BlockedOnSend(TaskId),
    Exited,
}

impl Task {
    /// Create a new task
    pub fn new(id: TaskId, page_table: PageTable, entry_point: u64) -> Self {
        // Initialize context with user-mode segments
        let stack_top = 0x0000_7FFF_FFFF_F000; // User stack top
        let context = super::switch::init_user_context(entry_point, stack_top);

        Self {
            id,
            state: TaskState::Runnable,
            page_table,
            context,
            recv_queue: MessageQueue::with_capacity(64),
            ipc_reply: None,
            blocked_on: None,
            capabilities: Vec::new(),
            credentials: Credentials {
                uid: 0,
                gid: 0,
                sandbox_level: SandboxLevel::Untrusted,
            },
        }
    }
}

// ===== Global Task Table Operations =====

/// Insert a task into the global task table
pub fn insert_task(task: Task) -> TaskId {
    let id = task.id;
    TASK_TABLE.lock().insert(id, Arc::new(Mutex::new(task)));
    id
}

/// Get a task by ID
pub fn get_task(id: TaskId) -> Option<Arc<Mutex<Task>>> {
    TASK_TABLE.lock().get(&id).cloned()
}

/// Remove a task from the task table
pub fn remove_task(id: TaskId) -> Option<Arc<Mutex<Task>>> {
    TASK_TABLE.lock().remove(&id)
}

/// Get the current running task
pub fn current_task() -> Arc<Mutex<Task>> {
    let current_id = CURRENT_TASK_ID.load(Ordering::Acquire);
    get_task(current_id).expect("No current task!")
}

/// Set the current running task
pub fn set_current_task(id: TaskId) {
    CURRENT_TASK_ID.store(id, Ordering::Release);
}

/// Allocate a new unique TaskId
pub fn allocate_task_id() -> TaskId {
    NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed)
}
