//! Task Structure and Global Task Table

use super::TaskId;
use crate::ipc::{IpcMessage, MessageQueue};
use crate::memory::PageTable;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

/// Send wrapper for PageTable pointer (we manage synchronization via Task's Mutex)
pub struct PageTablePtr(*mut PageTable);
unsafe impl Send for PageTablePtr {}
impl PageTablePtr {
    pub fn new(ptr: *mut PageTable) -> Self { Self(ptr) }
    pub fn as_ptr(&self) -> *mut PageTable { self.0 }
    pub fn as_ref(&self) -> &PageTable { unsafe { &*self.0 } }
}

/// Global task table - maps TaskId to Task structure
pub static TASK_TABLE: Mutex<BTreeMap<TaskId, Arc<Mutex<Task>>>> = Mutex::new(BTreeMap::new());

/// Current task ID per CPU (single-core for now)
static CURRENT_TASK_ID: AtomicU32 = AtomicU32::new(0);

/// Next available task ID
static NEXT_TASK_ID: AtomicU32 = AtomicU32::new(1);

/// Global task creation buffer (avoid stack allocation - stack is tiny!)
static TASK_CREATION_BUFFER: Mutex<core::mem::MaybeUninit<Task>> = Mutex::new(core::mem::MaybeUninit::uninit());

/// Task (process) structure
pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    pub page_table: PageTablePtr,  // Wrapped pointer (Send-safe, we manage lifetime manually)
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
    /// Create a new task using global buffer (kernel stack is tiny!)
    /// Returns Task (not Box) - caller must handle ownership
    #[inline(never)]
    pub fn new(id: TaskId, page_table_ptr: PageTablePtr, entry_point: u64) -> Self {
        use core::ptr;

        crate::serial_println!("[Task::new] START");
        let mut buffer = TASK_CREATION_BUFFER.lock();
        crate::serial_println!("[Task::new] Locked");
        unsafe {
            let task_ptr = buffer.as_mut_ptr();
            crate::serial_println!("[Task::new] Got ptr, zeroing...");
            ptr::write_bytes(task_ptr, 0, 1);
            crate::serial_println!("[Task::new] Zeroed OK");

            // Write non-zero fields directly to global buffer
            ptr::addr_of_mut!((*task_ptr).id).write(id);
            ptr::addr_of_mut!((*task_ptr).state).write(TaskState::Runnable);
            ptr::addr_of_mut!((*task_ptr).page_table).write(page_table_ptr);
            crate::serial_println!("[Task::new] Fields OK");
            // Context - stack and entry point
            ptr::addr_of_mut!((*task_ptr).context.rsp).write(0x0000_7FFF_FFFF_F000);
            ptr::addr_of_mut!((*task_ptr).context.rbp).write(0x0000_7FFF_FFFF_F000);
            ptr::addr_of_mut!((*task_ptr).context.rip).write(entry_point);
            ptr::addr_of_mut!((*task_ptr).context.rflags).write(0x202);
            ptr::addr_of_mut!((*task_ptr).context.cs).write(0x1B);
            ptr::addr_of_mut!((*task_ptr).context.ss).write(0x23);
            crate::serial_println!("[Task::new] Context OK");

            // IPC - Initialize in-place (zero-stack method)
            // recv_queue was already zeroed by write_bytes above
            // MessageQueue::init_at_ptr just sets the max_size field
            MessageQueue::init_at_ptr(ptr::addr_of_mut!((*task_ptr).recv_queue));
            crate::serial_println!("[Task::new] recv_queue initialized");

            ptr::addr_of_mut!((*task_ptr).ipc_reply).write(None);
            ptr::addr_of_mut!((*task_ptr).blocked_on).write(None);
            crate::serial_println!("[Task::new] IPC OK");

            // Security
            ptr::addr_of_mut!((*task_ptr).capabilities).write(Vec::new());
            ptr::addr_of_mut!((*task_ptr).credentials).write(Credentials {
                uid: 0,
                gid: 0,
                sandbox_level: SandboxLevel::Untrusted,
            });
            crate::serial_println!("[Task::new] Security OK");

            // Move out of buffer (assume_init returns by value, perfect!)
            crate::serial_println!("[Task::new] Reading out");
            buffer.assume_init_read()
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

/// Insert a boxed task into the global task table (avoids stack overflow)
pub fn insert_task_boxed(task: Box<Task>) -> TaskId {
    let id = task.id;
    // Convert Box<Task> directly to Arc<Mutex<Task>> without intermediate stack allocation
    TASK_TABLE.lock().insert(id, Arc::from(Mutex::new(*task)));
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

/// Get the current running task ID
pub fn get_current_task() -> TaskId {
    CURRENT_TASK_ID.load(Ordering::Acquire)
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

/// Get the task table (for internal use)
pub fn get_task_table() -> &'static Mutex<BTreeMap<TaskId, Arc<Mutex<Task>>>> {
    &TASK_TABLE
}

/// Allocate a new unique TaskId
pub fn allocate_task_id() -> TaskId {
    NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed)
}
