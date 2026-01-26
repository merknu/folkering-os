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

    crate::serial_println!("[KSTACK] Allocated {:#x}..{:#x}",
                          stack_base as usize, stack_top as usize);

    (stack_base, stack_top)
}

/// Task (process) structure
pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    pub page_table: PageTablePtr,  // Wrapped pointer (Send-safe, we manage lifetime manually)

    // Stack-based context (NEW approach)
    // Instead of storing register values in a struct, we store a pointer to
    // the kernel stack where an InterruptFrame has been pushed.
    // The stack pointer itself IS the context!
    pub kernel_stack_base: Option<SendPtr>,  // Base of kernel stack (for deallocation)
    pub kernel_stack_ptr: Option<SendPtr>,   // Current stack pointer (points to saved InterruptFrame)

    // Legacy context (DEPRECATED - kept for compatibility during transition)
    #[deprecated(note = "Use kernel_stack_ptr instead - stack-based context is the new approach")]
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
    pub last_scheduled_ms: u64,      // Last time this was scheduled

    // Statistics fields
    pub stats: TaskStatistics,
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

            // NEW: Allocate kernel stack and set up InterruptFrame
            let (stack_base, stack_top) = allocate_kernel_stack();
            crate::serial_println!("[TASK_NEW] Step 1: stack allocated");

            // Create initial InterruptFrame for this task
            let user_stack_top = 0x0000_7FFF_FFFF_F000u64;
            crate::serial_println!("[TASK_NEW] Step 2: about to create InterruptFrame");
            let initial_frame = InterruptFrame::new_user(entry_point, user_stack_top);
            crate::serial_println!("[TASK_NEW] Step 3: InterruptFrame created");

            // Push InterruptFrame to kernel stack (stack grows down)
            let frame_ptr = (stack_top as usize - core::mem::size_of::<InterruptFrame>()) as *mut InterruptFrame;
            crate::serial_println!("[TASK_NEW] Step 4: frame_ptr={:#x}", frame_ptr as usize);

            // DEBUG: Try reading from the address first to verify it's accessible
            crate::serial_println!("[TASK_NEW] Step 4a: testing read from frame_ptr...");
            let test_read = ptr::read_volatile(frame_ptr as *const u8);
            crate::serial_println!("[TASK_NEW] Step 4b: read test OK (val={})", test_read);

            // DEBUG: Try writing a single byte first
            crate::serial_println!("[TASK_NEW] Step 4c: testing write to frame_ptr...");
            ptr::write_volatile(frame_ptr as *mut u8, 0xAA);
            crate::serial_println!("[TASK_NEW] Step 4d: write test OK");

            // Now write the full InterruptFrame - field by field to debug
            crate::serial_println!("[TASK_NEW] Step 4e: writing InterruptFrame fields...");

            // Write each field individually using volatile writes
            let frame = frame_ptr;
            ptr::write_volatile(ptr::addr_of_mut!((*frame).r15), initial_frame.r15);
            crate::serial_println!("[TASK_NEW] r15 OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).r14), initial_frame.r14);
            crate::serial_println!("[TASK_NEW] r14 OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).r13), initial_frame.r13);
            crate::serial_println!("[TASK_NEW] r13 OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).r12), initial_frame.r12);
            crate::serial_println!("[TASK_NEW] r12 OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).r11), initial_frame.r11);
            crate::serial_println!("[TASK_NEW] r11 OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).r10), initial_frame.r10);
            crate::serial_println!("[TASK_NEW] r10 OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).r9), initial_frame.r9);
            crate::serial_println!("[TASK_NEW] r9 OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).r8), initial_frame.r8);
            crate::serial_println!("[TASK_NEW] r8 OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).rbp), initial_frame.rbp);
            crate::serial_println!("[TASK_NEW] rbp OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).rdi), initial_frame.rdi);
            crate::serial_println!("[TASK_NEW] rdi OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).rsi), initial_frame.rsi);
            crate::serial_println!("[TASK_NEW] rsi OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).rdx), initial_frame.rdx);
            crate::serial_println!("[TASK_NEW] rdx OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).rcx), initial_frame.rcx);
            crate::serial_println!("[TASK_NEW] rcx OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).rbx), initial_frame.rbx);
            crate::serial_println!("[TASK_NEW] rbx OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).rax), initial_frame.rax);
            crate::serial_println!("[TASK_NEW] rax OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).rip), initial_frame.rip);
            crate::serial_println!("[TASK_NEW] rip OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).cs), initial_frame.cs);
            crate::serial_println!("[TASK_NEW] cs OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).rflags), initial_frame.rflags);
            crate::serial_println!("[TASK_NEW] rflags OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).rsp), initial_frame.rsp);
            crate::serial_println!("[TASK_NEW] rsp OK");
            ptr::write_volatile(ptr::addr_of_mut!((*frame).ss), initial_frame.ss);
            crate::serial_println!("[TASK_NEW] ss OK - all fields written!");

            crate::serial_println!("[TASK_NEW] Step 5: InterruptFrame written to stack");

            // Update kernel stack pointers
            ptr::addr_of_mut!((*task_ptr).kernel_stack_base).write(Some(SendPtr::new(stack_base)));
            crate::serial_println!("[TASK_NEW] Step 6: kernel_stack_base written");
            ptr::addr_of_mut!((*task_ptr).kernel_stack_ptr).write(Some(SendPtr::new(frame_ptr as *mut u8)));
            crate::serial_println!("[TASK_NEW] Step 7: kernel_stack_ptr written");

            crate::serial_println!("[TASK_NEW] Task {} kernel stack: base={:#x}, frame={:#x}",
                                  id, stack_base as usize, frame_ptr as usize);

            // LEGACY: Also fill old Context for compatibility during transition
            #[allow(deprecated)]
            {
                ptr::addr_of_mut!((*task_ptr).context.rsp).write(user_stack_top);
                ptr::addr_of_mut!((*task_ptr).context.rbp).write(user_stack_top);
                ptr::addr_of_mut!((*task_ptr).context.rip).write(entry_point);
                ptr::addr_of_mut!((*task_ptr).context.rflags).write(0x202);
                ptr::addr_of_mut!((*task_ptr).context.cs).write(0x23);  // User code (0x20 | RPL=3)
                ptr::addr_of_mut!((*task_ptr).context.ss).write(0x1B);  // User data (0x18 | RPL=3)
            }

            // DEBUG: Verify what we just wrote (using bypass functions to avoid toolchain hang)
            let cs_val = ptr::addr_of!((*task_ptr).context.cs).read();
            let ss_val = ptr::addr_of!((*task_ptr).context.ss).read();
            crate::drivers::serial::write_str("[TASK_NEW] Set CS=");
            crate::drivers::serial::write_hex(cs_val);
            crate::drivers::serial::write_str(", SS=");
            crate::drivers::serial::write_hex(ss_val);
            crate::drivers::serial::write_newline();

            // CRITICAL: Verify CS and SS are NOT swapped!
            if cs_val != 0x23 || ss_val != 0x1B {
                crate::drivers::serial::write_str("[TASK_NEW] ERROR: CS/SS values are wrong!\n");
                crate::drivers::serial::write_str("  Expected CS=0x23, SS=0x1B\n");
            }

            crate::serial_println!("[TASK_NEW] Step 8: about to init IPC...");
            // IPC - Initialize in-place (zero-stack method)
            // recv_queue was already zeroed by write_bytes above
            // MessageQueue::init_at_ptr just sets the max_size field
            MessageQueue::init_at_ptr(ptr::addr_of_mut!((*task_ptr).recv_queue));
            crate::serial_println!("[TASK_NEW] Step 8a: recv_queue init OK");

            ptr::addr_of_mut!((*task_ptr).ipc_reply).write(None);
            crate::serial_println!("[TASK_NEW] Step 8b: ipc_reply OK");
            ptr::addr_of_mut!((*task_ptr).blocked_on).write(None);
            crate::serial_println!("[TASK_NEW] Step 8c: blocked_on OK");

            // Security
            crate::serial_println!("[TASK_NEW] Step 9: about to init security...");
            ptr::addr_of_mut!((*task_ptr).capabilities).write(Vec::new());
            crate::serial_println!("[TASK_NEW] Step 9a: capabilities OK");
            ptr::addr_of_mut!((*task_ptr).credentials).write(Credentials {
                uid: 0,
                gid: 0,
                sandbox_level: SandboxLevel::Untrusted,
            });
            crate::serial_println!("[TASK_NEW] Step 9b: credentials OK");

            // Scheduling
            crate::serial_println!("[TASK_NEW] Step 10: about to init scheduling...");
            crate::serial_println!("[TASK_NEW] Step 10a: priority addr={:#x}",
                                  ptr::addr_of_mut!((*task_ptr).priority) as usize);
            ptr::addr_of_mut!((*task_ptr).priority).write(PRIORITY_NORMAL);
            crate::serial_println!("[TASK_NEW] Step 10a: priority OK");
            ptr::addr_of_mut!((*task_ptr).base_priority).write(PRIORITY_NORMAL);
            crate::serial_println!("[TASK_NEW] Step 10b: base_priority OK");
            ptr::addr_of_mut!((*task_ptr).deadline_ms).write(None);
            crate::serial_println!("[TASK_NEW] Step 10c: deadline_ms OK");
            ptr::addr_of_mut!((*task_ptr).cpu_time_used_ms).write(0);
            crate::serial_println!("[TASK_NEW] Step 10d: cpu_time_used OK");
            ptr::addr_of_mut!((*task_ptr).last_scheduled_ms).write(0);
            crate::serial_println!("[TASK_NEW] Step 10e: last_scheduled OK");

            // Statistics
            crate::serial_println!("[TASK_NEW] Step 11: about to init stats...");
            crate::serial_println!("[TASK_NEW] Step 11a: calling uptime_ms()...");
            let current_time = crate::timer::uptime_ms();
            crate::serial_println!("[TASK_NEW] Step 11b: uptime_ms returned {}", current_time);

            let stats_ptr = ptr::addr_of_mut!((*task_ptr).stats);
            crate::serial_println!("[TASK_NEW] Step 11c: stats addr={:#x}, size={}",
                                  stats_ptr as usize, core::mem::size_of::<TaskStatistics>());

            // Test read first
            crate::serial_println!("[TASK_NEW] Step 11d: testing read from stats...");
            let test_byte = ptr::read_volatile(stats_ptr as *const u8);
            crate::serial_println!("[TASK_NEW] Step 11e: read OK (val={})", test_byte);

            // Test single byte write
            crate::serial_println!("[TASK_NEW] Step 11f: testing single byte write to stats...");
            ptr::write_volatile(stats_ptr as *mut u8, 0xAA);
            crate::serial_println!("[TASK_NEW] Step 11g: single byte OK");

            // Now try zeroing
            crate::serial_println!("[TASK_NEW] Step 11h: zeroing stats memory...");
            for i in 0..core::mem::size_of::<TaskStatistics>() {
                ptr::write_volatile((stats_ptr as *mut u8).add(i), 0);
            }
            crate::serial_println!("[TASK_NEW] Step 11i: zeroing done");

            // Write created_at_ms
            crate::serial_println!("[TASK_NEW] Step 11j: writing created_at_ms...");
            ptr::addr_of_mut!((*stats_ptr).created_at_ms).write(current_time);
            crate::serial_println!("[TASK_NEW] Step 11: stats init OK");

            // Move out of buffer (assume_init returns by value, perfect!)
            crate::serial_println!("[TASK_NEW] Step 12: about to return Task...");
            buffer.assume_init_read()
        }
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
