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

    // Scheduling fields
    pub priority: Priority,          // Task priority (0-255, higher = more important)
    pub base_priority: Priority,     // Base priority (before dynamic adjustments)
    pub deadline_ms: Option<u64>,    // Absolute deadline in milliseconds (None = no deadline)
    pub cpu_time_used_ms: u64,       // Total CPU time used (for scheduling fairness)
    pub last_scheduled_ms: u64,      // Last time this task was scheduled
}

/// CPU context for task switching
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Context {
    pub rsp: u64,    // Offset 0
    pub rbp: u64,    // Offset 8
    pub rax: u64,    // Offset 16
    pub rbx: u64,    // Offset 24
    pub rcx: u64,    // Offset 32
    pub rdx: u64,    // Offset 40
    pub rsi: u64,    // Offset 48
    pub rdi: u64,    // Offset 56
    pub r8: u64,     // Offset 64
    pub r9: u64,     // Offset 72
    pub r10: u64,    // Offset 80
    pub r11: u64,    // Offset 88
    pub r12: u64,    // Offset 96
    pub r13: u64,    // Offset 104
    pub r14: u64,    // Offset 112
    pub r15: u64,    // Offset 120
    pub rip: u64,    // Offset 128
    pub rflags: u64, // Offset 136
    pub cs: u64,     // Offset 144
    pub ss: u64,     // Offset 152
}

// Compile-time assertions to verify Context structure layout
const _: () = {
    use core::mem::{size_of, offset_of};

    // Verify total size is exactly 160 bytes (20 u64 fields)
    const SIZE: usize = size_of::<Context>();
    assert!(SIZE == 160, "Context size must be exactly 160 bytes");

    // Verify each field is at the expected offset
    assert!(offset_of!(Context, rsp) == 0, "rsp offset must be 0");
    assert!(offset_of!(Context, rbp) == 8, "rbp offset must be 8");
    assert!(offset_of!(Context, rax) == 16, "rax offset must be 16");
    assert!(offset_of!(Context, rbx) == 24, "rbx offset must be 24");
    assert!(offset_of!(Context, rcx) == 32, "rcx offset must be 32");
    assert!(offset_of!(Context, rdx) == 40, "rdx offset must be 40");
    assert!(offset_of!(Context, rsi) == 48, "rsi offset must be 48");
    assert!(offset_of!(Context, rdi) == 56, "rdi offset must be 56");
    assert!(offset_of!(Context, r8) == 64, "r8 offset must be 64");
    assert!(offset_of!(Context, r9) == 72, "r9 offset must be 72");
    assert!(offset_of!(Context, r10) == 80, "r10 offset must be 80");
    assert!(offset_of!(Context, r11) == 88, "r11 offset must be 88");
    assert!(offset_of!(Context, r12) == 96, "r12 offset must be 96");
    assert!(offset_of!(Context, r13) == 104, "r13 offset must be 104");
    assert!(offset_of!(Context, r14) == 112, "r14 offset must be 112");
    assert!(offset_of!(Context, r15) == 120, "r15 offset must be 120");
    assert!(offset_of!(Context, rip) == 128, "rip offset must be 128");
    assert!(offset_of!(Context, rflags) == 136, "rflags offset must be 136");
    assert!(offset_of!(Context, cs) == 144, "cs offset must be 144");
    assert!(offset_of!(Context, ss) == 152, "ss offset must be 152");
};

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

/// Task priority (0-255, higher = more important)
pub type Priority = u8;

/// Standard priority levels
pub const PRIORITY_IDLE: Priority = 0;
pub const PRIORITY_LOW: Priority = 64;
pub const PRIORITY_NORMAL: Priority = 128;
pub const PRIORITY_HIGH: Priority = 192;
pub const PRIORITY_REALTIME: Priority = 255;

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

        let mut buffer = TASK_CREATION_BUFFER.lock();
        unsafe {
            let task_ptr = buffer.as_mut_ptr();

            // DEBUG: Print address of global buffer
            static mut PRINTED: bool = false;
            if !PRINTED {
                crate::serial_println!("[TASK_NEW] TASK_CREATION_BUFFER at {:#x}", task_ptr as usize);
                PRINTED = true;
            }
            ptr::write_bytes(task_ptr, 0, 1);

            // Write non-zero fields directly to global buffer
            ptr::addr_of_mut!((*task_ptr).id).write(id);
            ptr::addr_of_mut!((*task_ptr).state).write(TaskState::Runnable);
            ptr::addr_of_mut!((*task_ptr).page_table).write(page_table_ptr);
            // Context - stack and entry point
            ptr::addr_of_mut!((*task_ptr).context.rsp).write(0x0000_7FFF_FFFF_F000);
            ptr::addr_of_mut!((*task_ptr).context.rbp).write(0x0000_7FFF_FFFF_F000);
            ptr::addr_of_mut!((*task_ptr).context.rip).write(entry_point);
            ptr::addr_of_mut!((*task_ptr).context.rflags).write(0x202);
            ptr::addr_of_mut!((*task_ptr).context.cs).write(0x23);  // User code (0x20 | RPL=3)
            ptr::addr_of_mut!((*task_ptr).context.ss).write(0x1B);  // User data (0x18 | RPL=3)

            // DEBUG: Verify what we just wrote
            let cs_val = ptr::addr_of!((*task_ptr).context.cs).read();
            let ss_val = ptr::addr_of!((*task_ptr).context.ss).read();
            crate::serial_println!("[TASK_NEW] Set CS={:#x}, SS={:#x}", cs_val, ss_val);

            // IPC - Initialize in-place (zero-stack method)
            // recv_queue was already zeroed by write_bytes above
            // MessageQueue::init_at_ptr just sets the max_size field
            MessageQueue::init_at_ptr(ptr::addr_of_mut!((*task_ptr).recv_queue));

            ptr::addr_of_mut!((*task_ptr).ipc_reply).write(None);
            ptr::addr_of_mut!((*task_ptr).blocked_on).write(None);

            // Security
            ptr::addr_of_mut!((*task_ptr).capabilities).write(Vec::new());
            ptr::addr_of_mut!((*task_ptr).credentials).write(Credentials {
                uid: 0,
                gid: 0,
                sandbox_level: SandboxLevel::Untrusted,
            });

            // Scheduling
            ptr::addr_of_mut!((*task_ptr).priority).write(PRIORITY_NORMAL);
            ptr::addr_of_mut!((*task_ptr).base_priority).write(PRIORITY_NORMAL);
            ptr::addr_of_mut!((*task_ptr).deadline_ms).write(None);
            ptr::addr_of_mut!((*task_ptr).cpu_time_used_ms).write(0);
            ptr::addr_of_mut!((*task_ptr).last_scheduled_ms).write(0);

            // Move out of buffer (assume_init returns by value, perfect!)
            buffer.assume_init_read()
        }
    }
}

// ===== Global Task Table Operations =====

/// Insert a task into the global task table
pub fn insert_task(task: Task) -> TaskId {
    let id = task.id;
    let arc = Arc::new(Mutex::new(task));

    // DEBUG: Show where Arc allocated the task
    let arc_ptr = {
        let locked = arc.lock();
        let ctx_ptr = &locked.context as *const Context as usize;
        crate::serial_println!("[INSERT_TASK] Task {} Arc-allocated context at {:#x}", id, ctx_ptr);
        ctx_ptr
    };

    TASK_TABLE.lock().insert(id, arc);
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
