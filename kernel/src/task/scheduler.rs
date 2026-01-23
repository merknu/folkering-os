//! Bootstrap Round-Robin Scheduler
//!
//! Simple scheduler used during early boot before userspace scheduler starts.

use alloc::collections::VecDeque;
use spin::Mutex;
use super::TaskId;

/// Bootstrap scheduler
struct BootstrapScheduler {
    tasks: VecDeque<TaskId>,
}

impl BootstrapScheduler {
    const fn new() -> Self {
        Self {
            tasks: VecDeque::new(),
        }
    }

    fn add_task(&mut self, task_id: TaskId) {
        self.tasks.push_back(task_id);
    }

    fn schedule_next(&mut self) -> Option<TaskId> {
        if let Some(task_id) = self.tasks.pop_front() {
            self.tasks.push_back(task_id);
            Some(task_id)
        } else {
            None
        }
    }
}

static SCHEDULER: Mutex<BootstrapScheduler> = Mutex::new(BootstrapScheduler::new());

/// Initialize scheduler
pub fn init() {
    // Bootstrap scheduler is already initialized
}

/// Add a task to the scheduler runqueue
pub fn enqueue(task_id: TaskId) {
    SCHEDULER.lock().add_task(task_id);
}

/// Yield CPU to scheduler (context switch to next task)
///
/// Saves current task state and switches to next runnable task.
///
/// # Performance
/// <500 cycles (context switch overhead)
pub fn yield_cpu() {
    // Disable interrupts during context switch
    x86_64::instructions::interrupts::disable();

    // Get next task to run
    let next_id = match schedule_next() {
        Some(id) => id,
        None => {
            // No tasks to run, re-enable interrupts and return
            x86_64::instructions::interrupts::enable();
            return;
        }
    };

    // Perform context switch
    unsafe {
        super::switch::switch_to(next_id);
    }

    // Re-enable interrupts (will happen after switch completes)
    x86_64::instructions::interrupts::enable();
}

/// Get next task to run
pub fn schedule_next() -> Option<TaskId> {
    SCHEDULER.lock().schedule_next()
}

/// Start scheduler (enter idle loop)
pub fn start() -> ! {
    crate::serial_println!("[SCHED] Scheduler started, entering task execution loop");

    // Disable interrupts during initial context switch
    x86_64::instructions::interrupts::disable();

    loop {
        if let Some(task_id) = schedule_next() {
            crate::serial_println!("[SCHED] Switching to task {}", task_id);

            // Perform context switch
            unsafe {
                super::switch::switch_to(task_id);
            }

            // After returning from task (via yield or blocking)
            crate::serial_println!("[SCHED] Task {} yielded", task_id);
        } else {
            // No tasks runnable, halt until interrupt
            crate::serial_println!("[SCHED] No runnable tasks, halting");
            x86_64::instructions::interrupts::enable();
            x86_64::instructions::hlt();
            x86_64::instructions::interrupts::disable();
        }
    }
}
