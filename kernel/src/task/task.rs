//! Task Structure and Global Task Table

use super::TaskId;
use crate::ipc::{IpcMessage, MessageQueue};
use crate::memory::PageTable;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;

/// 512-byte FXSAVE area, must be 16-byte aligned for FXSAVE/FXRSTOR instructions.
/// Stores x87 FPU + SSE/AVX MXCSR + XMM0–XMM15 state (FXSAVE64 format).
#[repr(C, align(16))]
pub struct FxsaveArea(pub [u8; 512]);

impl FxsaveArea {
    /// Create a valid initial FPU/SSE state:
    ///   FPU CW  (offset  0) = 0x037F  — all exceptions masked, double precision
    ///   MXCSR   (offset 24) = 0x1F80  — all SSE exceptions masked, round-nearest
    pub const fn default_init() -> Self {
        let mut data = [0u8; 512];
        // FPU Control Word at bytes 0-1 (little-endian)
        data[0] = 0x7F;
        data[1] = 0x03;
        // MXCSR at bytes 24-27 (little-endian)
        data[24] = 0x80;
        data[25] = 0x1F;
        Self(data)
    }
}

/// Raw pointer to the current task's FXSAVE area.
///
/// Set by `timer_preempt_handler` on every context switch so that `irq_timer`
/// assembly can call FXSAVE / FXRSTOR without holding any Rust locks.
///
/// Invariant: always points into a live Task's `fxsave_area` field, or is 0
/// (before the first userspace task starts).
pub static FXSAVE_CURRENT_PTR: AtomicUsize = AtomicUsize::new(0);

/// Send wrapper for PageTable pointer (we manage synchronization via Task's Mutex)
pub struct PageTablePtr(*mut PageTable);
unsafe impl Send for PageTablePtr {}
impl PageTablePtr {
    pub fn new(ptr: *mut PageTable) -> Self { Self(ptr) }
    pub fn as_ptr(&self) -> *mut PageTable { self.0 }
    pub fn as_ref(&self) -> &PageTable { unsafe { &*self.0 } }
}

/// Send wrapper for raw pointers (we manage synchronization via Task's Mutex)
#[derive(Clone, Copy, Debug)]
pub struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
impl SendPtr {
    pub fn new(ptr: *mut u8) -> Self { Self(ptr) }
    pub fn as_ptr(&self) -> *mut u8 { self.0 }
    pub fn is_null(&self) -> bool { self.0.is_null() }
}

/// Global task table - maps TaskId to Task structure
pub static TASK_TABLE: Mutex<BTreeMap<TaskId, Arc<Mutex<Task>>>> = Mutex::new(BTreeMap::new());

/// Current task ID per CPU (single-core for now)
static CURRENT_TASK_ID: AtomicU32 = AtomicU32::new(0);

/// Next available task ID
static NEXT_TASK_ID: AtomicU32 = AtomicU32::new(1);

/// Global task creation buffer (avoid stack allocation - stack is tiny!)
static TASK_CREATION_BUFFER: Mutex<core::mem::MaybeUninit<Task>> = Mutex::new(core::mem::MaybeUninit::uninit());

/// Kernel stack size per task (8KB should be enough for syscall nesting)
const KERNEL_STACK_SIZE: usize = 8192;

/// Allocate a kernel stack for a task
///
/// Returns (stack_base, stack_top) where stack_top is ready for pushing InterruptFrame
fn allocate_kernel_stack() -> (*mut u8, *mut u8) {
    use alloc::vec::Vec;

    // Allocate on kernel heap using Vec (zeroed by default)
    let mut stack_vec: Vec<u8> = alloc::vec![0u8; KERNEL_STACK_SIZE];

    // Get pointer before leaking
    let stack_base = stack_vec.as_mut_ptr();

    // Leak the Vec so it's not dropped (task owns it now)
    core::mem::forget(stack_vec);

    // Stack grows downward, so top is base + size
    let stack_top = unsafe { stack_base.add(KERNEL_STACK_SIZE) };

    crate::serial_str!("[KSTACK] Allocated ");
    crate::drivers::serial::write_hex(stack_base as u64);
    crate::serial_str!("..");
    crate::drivers::serial::write_hex(stack_top as u64);
    crate::drivers::serial::write_newline();

    (stack_base, stack_top)
}

/// Task (process) structure
pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    pub page_table: PageTablePtr,  // Wrapped pointer (Send-safe, we manage lifetime manually)
    pub page_table_phys: u64,      // Physical address of PML4 for CR3 switching

    // Stack-based context (NEW approach)
    // Instead of storing register values in a struct, we store a pointer to
    // the kernel stack where an InterruptFrame has been pushed.
    // The stack pointer itself IS the context!
    pub kernel_stack_base: Option<SendPtr>,  // Base of kernel stack (for deallocation)
    pub kernel_stack_ptr: Option<SendPtr>,   // Current stack pointer (points to saved InterruptFrame)

    // User-mode context: saved by syscall_entry, used to build IRETQ frame.
    // MUST NOT be overwritten by switch_context — only syscall_entry/preempt write here.
    pub context: Context,

    // Kernel-mode context: saved/restored by switch_context during task switches.
    // Holds kernel RSP/RIP/callee-saved regs so switch_context can resume in the
    // yield path without corrupting the user context above.
    pub kernel_context: Context,

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
    pub inherited_priority: Priority, // Priority inheritance: temporarily boosted when high-pri task blocks on us
    pub deadline_ms: Option<u64>,    // Absolute deadline in milliseconds (None = no deadline)
    pub cpu_time_used_ms: u64,       // Total CPU time used (for scheduling fairness)
    pub last_scheduled_ms: u64,      // Last time this was scheduled

    // AI-Native scheduling fields
    pub semantic_priority: u8,       // 0-255, Synapse can boost tasks dynamically (128 = neutral)
    pub is_background_ai: bool,      // True for Synapse background work (vector embeddings, etc.)

    // Statistics fields
    pub stats: TaskStatistics,

    // Interrupt handling
    pub interrupt_pending: bool,  // Set when Ctrl+C is received

    /// Human-readable task name (e.g. "synapse", "shell", "compositor")
    pub name: [u8; 16],

    /// FXSAVE/FXRSTOR area — stores x87 + SSE state across context switches.
    /// 512 bytes, 16-byte aligned (required by FXSAVE64).
    pub fxsave_area: FxsaveArea,
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
    // NOTE: fs_base/gs_base removed - they caused format! crashes
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

/// Task performance statistics
#[derive(Clone, Copy, Debug, Default)]
pub struct TaskStatistics {
    // Execution metrics
    pub context_switches: u64,       // Total context switches
    pub syscalls: u64,                // Total syscalls made
    pub cpu_cycles: u64,             // Total CPU cycles used
    pub created_at_ms: u64,          // Creation timestamp
    pub total_runtime_ms: u64,       // Total time in runnable/running state

    // IPC metrics
    pub ipc_sent: u64,               // Messages sent
    pub ipc_received: u64,           // Messages received
    pub ipc_replied: u64,            // Replies sent
    pub ipc_blocks: u64,             // Times blocked on IPC

    // Memory metrics
    pub page_faults: u64,            // Page faults handled
    pub heap_allocations: u64,       // Heap allocations
    pub heap_frees: u64,             // Heap frees

    // Scheduling metrics
    pub deadline_misses: u64,        // Deadlines missed
    pub priority_boosts: u64,        // Times priority was boosted
    pub voluntary_yields: u64,       // Times yielded voluntarily
    pub preemptions: u64,            // Times preempted by scheduler
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
    /// Waiting for async reply via CallerToken (Phase 6 Reply-Later IPC).
    /// The u64 is the request_id that will be used to verify the reply.
    WaitingForReply(u64),
    Exited,
}

impl Task {
    /// Create a new task using global buffer (kernel stack is tiny!)
    /// Returns Task (not Box) - caller must handle ownership
    #[inline(never)]
    pub fn new(id: TaskId, page_table_ptr: PageTablePtr, entry_point: u64) -> Self {
        use core::ptr;
        use crate::arch::x86_64::interrupt_frame::InterruptFrame;

        let mut buffer = TASK_CREATION_BUFFER.lock();
        unsafe {
            let task_ptr = buffer.as_mut_ptr();
            ptr::write_bytes(task_ptr, 0, 1);

            // Core fields
            ptr::addr_of_mut!((*task_ptr).id).write(id);
            ptr::addr_of_mut!((*task_ptr).state).write(TaskState::Runnable);
            ptr::addr_of_mut!((*task_ptr).page_table).write(page_table_ptr);
            ptr::addr_of_mut!((*task_ptr).page_table_phys).write(0);

            // Allocate kernel stack and write InterruptFrame
            let (stack_base, stack_top) = allocate_kernel_stack();
            let user_stack_top = 0x0000_7FFF_FFFF_F000u64;
            let initial_frame = InterruptFrame::new_user(entry_point, user_stack_top);
            let frame_ptr = (stack_top as usize - core::mem::size_of::<InterruptFrame>()) as *mut InterruptFrame;
            ptr::write_volatile(frame_ptr, initial_frame);

            // Kernel stack pointers
            ptr::addr_of_mut!((*task_ptr).kernel_stack_base).write(Some(SendPtr::new(stack_base)));
            ptr::addr_of_mut!((*task_ptr).kernel_stack_ptr).write(Some(SendPtr::new(frame_ptr as *mut u8)));

            crate::serial_str!("[TASK_NEW] Task "); crate::drivers::serial::write_dec(id);
            crate::serial_str!(" entry="); crate::drivers::serial::write_hex(entry_point);
            crate::serial_str!(" stack="); crate::drivers::serial::write_hex(stack_base as u64);
            crate::serial_str!(".."); crate::drivers::serial::write_hex(stack_top as u64);
            crate::drivers::serial::write_newline();

            // User context (for IRETQ frame)
            ptr::addr_of_mut!((*task_ptr).context.rsp).write(user_stack_top);
            ptr::addr_of_mut!((*task_ptr).context.rbp).write(user_stack_top);
            ptr::addr_of_mut!((*task_ptr).context.rip).write(entry_point);
            ptr::addr_of_mut!((*task_ptr).context.rflags).write(0x202);
            ptr::addr_of_mut!((*task_ptr).context.cs).write(0x23);
            ptr::addr_of_mut!((*task_ptr).context.ss).write(0x1B);

            // Kernel context (for switch_context)
            ptr::addr_of_mut!((*task_ptr).kernel_context.cs).write(0x08);
            ptr::addr_of_mut!((*task_ptr).kernel_context.ss).write(0x10);
            ptr::addr_of_mut!((*task_ptr).kernel_context.rflags).write(0x202);

            crate::serial_str!("[TASK_NEW] CS=");
            crate::drivers::serial::write_hex(0x23);
            crate::serial_str!(", SS=");
            crate::drivers::serial::write_hex(0x1B);
            crate::drivers::serial::write_newline();

            // IPC
            MessageQueue::init_at_ptr(ptr::addr_of_mut!((*task_ptr).recv_queue));
            ptr::addr_of_mut!((*task_ptr).ipc_reply).write(None);
            ptr::addr_of_mut!((*task_ptr).blocked_on).write(None);

            // Security
            ptr::addr_of_mut!((*task_ptr).capabilities).write(Vec::new());
            ptr::addr_of_mut!((*task_ptr).credentials).write(Credentials {
                uid: 0, gid: 0, sandbox_level: SandboxLevel::Untrusted,
            });

            // Scheduling
            ptr::addr_of_mut!((*task_ptr).priority).write(PRIORITY_NORMAL);
            ptr::addr_of_mut!((*task_ptr).base_priority).write(PRIORITY_NORMAL);
            ptr::addr_of_mut!((*task_ptr).inherited_priority).write(0);
            ptr::addr_of_mut!((*task_ptr).deadline_ms).write(None);
            ptr::addr_of_mut!((*task_ptr).cpu_time_used_ms).write(0);
            ptr::addr_of_mut!((*task_ptr).last_scheduled_ms).write(0);

            // AI-Native scheduling
            ptr::addr_of_mut!((*task_ptr).semantic_priority).write(128); // Neutral
            ptr::addr_of_mut!((*task_ptr).is_background_ai).write(false);

            // Statistics
            let current_time = crate::timer::uptime_ms();
            let stats_ptr = ptr::addr_of_mut!((*task_ptr).stats);
            ptr::write_bytes(stats_ptr as *mut u8, 0, core::mem::size_of::<TaskStatistics>());
            ptr::addr_of_mut!((*stats_ptr).created_at_ms).write(current_time);

            // Interrupt handling
            ptr::addr_of_mut!((*task_ptr).interrupt_pending).write(false);

            // Task name (zeroed by default, set via set_name after creation)
            // Already zeroed by write_bytes above

            // FPU/SSE: valid initial state (MXCSR=0x1F80, FPU CW=0x037F)
            ptr::addr_of_mut!((*task_ptr).fxsave_area).write(FxsaveArea::default_init());
            buffer.assume_init_read()
        }
    }

    /// Set the task's human-readable name (max 15 chars, null-terminated)
    pub fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = bytes.len().min(15);
        self.name[..len].copy_from_slice(&bytes[..len]);
        self.name[len] = 0;
    }
}

// ===== Global Task Table Operations =====

/// Insert a task into the global task table
pub fn insert_task(task: Task) -> TaskId {
    let id = task.id;
    let arc = Arc::new(Mutex::new(task));

    // DEBUG: Show where Arc allocated the task (using bypass functions)
    {
        let locked = arc.lock();
        let ctx_ptr = &locked.context as *const Context as usize;
        crate::drivers::serial::write_str("[INSERT_TASK] Task ");
        crate::drivers::serial::write_dec(id);
        crate::drivers::serial::write_str(" Arc-allocated context at ");
        crate::drivers::serial::write_hex(ctx_ptr as u64);
        crate::drivers::serial::write_newline();

        // CRITICAL: Verify CS/SS values after Arc allocation
        crate::drivers::serial::write_str("[INSERT_TASK] Verifying CS=");
        crate::drivers::serial::write_hex(locked.context.cs);
        crate::drivers::serial::write_str(", SS=");
        crate::drivers::serial::write_hex(locked.context.ss);
        crate::drivers::serial::write_newline();

        if locked.context.cs != 0x23 || locked.context.ss != 0x1B {
            crate::drivers::serial::write_str("[INSERT_TASK] ERROR: CS/SS corrupted during Arc move!\n");
        }
    }

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
