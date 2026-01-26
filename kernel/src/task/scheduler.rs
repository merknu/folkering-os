//! Priority + Deadline Scheduler with Brain Bridge Integration
//!
//! Enhanced scheduler with priority levels and deadline support for AI workloads.
//! Features:
//! - Priority-based scheduling (0-255, higher = more important)
//! - Deadline scheduling for time-critical AI inference
//! - Dynamic priority adjustments from BrainBridge hints
//! - Aging to prevent starvation
//! - Fairness for same-priority tasks

use alloc::collections::VecDeque;
use spin::Mutex;
use super::TaskId;
use super::task::{Priority, PRIORITY_REALTIME, PRIORITY_HIGH, PRIORITY_NORMAL};
use crate::bridge::{read_hints, BrainBridgeSnapshot, IntentType};

/// Priority + Deadline scheduler with Brain Bridge integration
struct EnhancedScheduler {
    tasks: VecDeque<TaskId>,
    last_hint_check: u64,          // Last time we checked for hints
    hint_check_interval: u64,       // Check every N milliseconds
    current_cpu_boost: bool,        // Whether CPU is currently boosted
    current_workload: IntentType,   // Current workload type
    aging_interval_ms: u64,         // Age tasks every N milliseconds
    last_aging_ms: u64,             // Last time we aged tasks
}

impl EnhancedScheduler {
    const fn new() -> Self {
        Self {
            tasks: VecDeque::new(),
            last_hint_check: 0,
            hint_check_interval: 10,    // Check every 10ms
            current_cpu_boost: false,
            current_workload: IntentType::Idle,
            aging_interval_ms: 100,     // Age every 100ms
            last_aging_ms: 0,
        }
    }

    fn add_task(&mut self, task_id: TaskId) {
        self.tasks.push_back(task_id);
    }

    /// Select next task to run using priority + deadline scheduling
    fn schedule_next(&mut self) -> Option<TaskId> {
        use super::task;

        let current_time = crate::timer::uptime_ms();

        // Record scheduler invocation
        super::statistics::record_scheduler_invocation();

        // Check for brain hints periodically
        if current_time - self.last_hint_check >= self.hint_check_interval {
            self.last_hint_check = current_time;
            self.check_brain_hints();
        }

        // Age tasks periodically to prevent starvation
        if current_time - self.last_aging_ms >= self.aging_interval_ms {
            self.last_aging_ms = current_time;
            self.age_tasks();
        }

        if self.tasks.is_empty() {
            return None;
        }

        // Find highest priority task, considering deadlines
        let mut best_task_id = None;
        let mut best_priority = 0u16; // Extended to u16 for deadline boost
        let mut best_deadline = u64::MAX;

        for &task_id in &self.tasks {
            if let Some(task_arc) = task::get_task(task_id) {
                let task_locked = task_arc.lock();

                // Skip non-runnable tasks
                if task_locked.state != super::task::TaskState::Runnable {
                    continue;
                }

                let mut effective_priority = task_locked.priority as u16;

                // Boost priority for deadline tasks
                if let Some(deadline) = task_locked.deadline_ms {
                    let time_to_deadline = deadline.saturating_sub(current_time);

                    // Critical: deadline within 10ms -> max priority
                    if time_to_deadline < 10 {
                        effective_priority = u16::MAX;
                    }
                    // Urgent: deadline within 50ms -> high boost
                    else if time_to_deadline < 50 {
                        effective_priority = effective_priority.saturating_add(100);
                    }
                    // Soon: deadline within 200ms -> medium boost
                    else if time_to_deadline < 200 {
                        effective_priority = effective_priority.saturating_add(50);
                    }

                    // Track best deadline for tie-breaking
                    if effective_priority == best_priority && deadline < best_deadline {
                        best_deadline = deadline;
                        best_task_id = Some(task_id);
                    }
                }

                // Select task with highest effective priority
                if effective_priority > best_priority {
                    best_priority = effective_priority;
                    best_task_id = Some(task_id);
                    if let Some(deadline) = task_locked.deadline_ms {
                        best_deadline = deadline;
                    }
                }
            }
        }

        // Move selected task to back of queue for fairness
        if let Some(selected_id) = best_task_id {
            if let Some(pos) = self.tasks.iter().position(|&id| id == selected_id) {
                self.tasks.remove(pos);
                self.tasks.push_back(selected_id);
            }

            // Update last_scheduled_ms
            if let Some(task_arc) = task::get_task(selected_id) {
                let mut task_locked = task_arc.lock();
                task_locked.last_scheduled_ms = current_time;
            }
        }

        best_task_id
    }

    /// Age tasks to prevent starvation (gradually increase priority of waiting tasks)
    fn age_tasks(&mut self) {
        use super::task;

        let current_time = crate::timer::uptime_ms();

        for &task_id in &self.tasks {
            if let Some(task_arc) = task::get_task(task_id) {
                let mut task_locked = task_arc.lock();

                // Only age runnable tasks
                if task_locked.state != super::task::TaskState::Runnable {
                    continue;
                }

                // If task hasn't been scheduled in >1 second, boost priority
                let wait_time = current_time.saturating_sub(task_locked.last_scheduled_ms);
                if wait_time > 1000 && task_locked.priority < PRIORITY_REALTIME {
                    // Boost by 10 every second (capped at 200 to leave room for deadline tasks)
                    let boost = (wait_time / 1000) as u8 * 10;
                    task_locked.priority = task_locked.priority
                        .saturating_add(boost)
                        .min(200); // Cap to leave room for deadline tasks
                }
            }
        }
    }

    /// Check for brain hints and apply them
    fn check_brain_hints(&mut self) {
        if let Some(hint) = read_hints() {
            self.current_workload = hint.current_intent;
            apply_brain_hint(&hint, &mut self.current_cpu_boost);

            // Adjust task priorities based on workload
            self.adjust_priorities_for_workload(&hint);
        }
    }

    /// Dynamically adjust task priorities based on current workload
    fn adjust_priorities_for_workload(&mut self, hint: &BrainBridgeSnapshot) {
        use super::task;

        // Only adjust on high-confidence hints
        if hint.confidence < 180 {
            return;
        }

        match hint.current_intent {
            IntentType::Compiling | IntentType::MLTraining => {
                // CPU-bound workload: boost CPU-intensive tasks
                for &task_id in &self.tasks {
                    if let Some(task_arc) = task::get_task(task_id) {
                        let mut task_locked = task_arc.lock();
                        // Boost priority by 30 for CPU-bound tasks
                        // (In a real system, would check task characteristics)
                        task_locked.priority = task_locked.base_priority.saturating_add(30);
                    }
                }
            },

            IntentType::Gaming => {
                // Latency-sensitive: boost priority and reduce deadline targets
                for &task_id in &self.tasks {
                    if let Some(task_arc) = task::get_task(task_id) {
                        let mut task_locked = task_arc.lock();
                        // Boost priority to high for responsiveness
                        task_locked.priority = PRIORITY_HIGH;
                    }
                }
            },

            IntentType::Idle => {
                // Return tasks to base priority
                for &task_id in &self.tasks {
                    if let Some(task_arc) = task::get_task(task_id) {
                        let mut task_locked = task_arc.lock();
                        task_locked.priority = task_locked.base_priority;
                    }
                }
            },

            _ => {}
        }
    }
}

static SCHEDULER: Mutex<EnhancedScheduler> = Mutex::new(EnhancedScheduler::new());

/// Initialize scheduler
pub fn init() {
    // Enhanced scheduler is already initialized
    crate::serial_println!("[SCHED] Priority + Deadline Scheduler initialized");
    crate::serial_println!("[SCHED] Features:");
    crate::serial_println!("[SCHED]   - Priority scheduling (0-255)");
    crate::serial_println!("[SCHED]   - Deadline support for time-critical tasks");
    crate::serial_println!("[SCHED]   - Dynamic priority adjustment via BrainBridge");
    crate::serial_println!("[SCHED]   - Aging to prevent starvation");
    crate::serial_println!("[SCHED] Brain Bridge integration enabled (hints checked every 10ms)");

    // Note: BrainBridge reader will be initialized later when shared memory is set up
    // via bridge::reader_init(phys_addr) after userspace creates the bridge page
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
#[inline(never)]
#[no_mangle]
pub fn yield_cpu() {
    use super::task;

    // CRITICAL DEBUG: Print immediately at function entry
    let marker_value = crate::arch::x86_64::syscall::get_debug_marker();
    crate::serial_println!("[YIELD_CPU] Function entered! DEBUG_MARKER = {:#x}", marker_value);

    // Record voluntary yield
    let current_id = task::get_current_task();
    super::statistics::record_voluntary_yield(current_id);

    // DEBUG: Set marker 100 at yield entry
    crate::arch::x86_64::syscall::set_debug_marker(100);

    // Disable interrupts during context switch
    x86_64::instructions::interrupts::disable();

    // DEBUG: Set marker 101 after disable interrupts
    crate::arch::x86_64::syscall::set_debug_marker(101);

    // Get next task to run
    let next_id = match schedule_next() {
        Some(id) => id,
        None => {
            // No tasks to run, re-enable interrupts and return
            x86_64::instructions::interrupts::enable();
            return;
        }
    };

    // DEBUG: Set marker 102 after schedule_next
    crate::arch::x86_64::syscall::set_debug_marker(102);

    // Get current task ID
    let current_id = task::get_current_task();

    // DEBUG: Set marker 103 after get_current_task
    crate::arch::x86_64::syscall::set_debug_marker(103);

    if current_id == next_id {
        // Same task, just return
        // CRITICAL FIX: Must update context pointer before returning!
        // Otherwise next syscall will get stale/NULL pointer
        if let Some(task_arc) = task::get_task(current_id) {
            let task_locked = task_arc.lock();
            let ctx_ptr = &task_locked.context as *const task::Context as usize;
            crate::arch::x86_64::syscall::set_current_context_ptr(ctx_ptr as *mut task::Context);
            crate::serial_println!("[YIELD_CPU] Same task, updated context ptr to {:#x}", ctx_ptr);
        }
        x86_64::instructions::interrupts::enable();
        return;
    }

    // DEBUG: Set marker 104 before get_task
    crate::arch::x86_64::syscall::set_debug_marker(104);

    // Get target task's context pointer and page table
    let target = task::get_task(next_id).expect("Target task not found");

    // DEBUG: Set marker 105 after get_task
    crate::arch::x86_64::syscall::set_debug_marker(105);

    let (target_ctx_ptr, target_page_table_phys) = {
        let target_locked = target.lock();
        (
            &target_locked.context as *const task::Context as usize,
            target_locked.page_table_phys,
        )
    };

    // DEBUG: Set marker 106 after getting context pointer
    crate::arch::x86_64::syscall::set_debug_marker(106);

    // Switch to target task's page table
    if target_page_table_phys != 0 {
        crate::serial_println!("[YIELD_CPU] Switching to page table {:#x}", target_page_table_phys);
        unsafe {
            crate::memory::paging::switch_page_table(target_page_table_phys);
        }
    }

    // Update current task
    task::set_current_task(next_id);

    // Record context switch
    super::statistics::record_context_switch(next_id);

    // Update current context pointer for syscalls
    crate::arch::x86_64::syscall::set_current_context_ptr(target_ctx_ptr as *mut task::Context);

    // CANARY CHECKPOINT #2: SWITCH_POINT - Verify target task Context before switch
    crate::serial_println!("[YIELD_CPU] About to switch to task {}", next_id);
    crate::arch::x86_64::syscall::verify_task_context(next_id, "SWITCH_POINT");

    // If we're switching AWAY from a task, also verify we saved its context correctly
    if current_id != 0 && current_id != next_id {
        crate::serial_println!("[YIELD_CPU] Also verifying outgoing task {}", current_id);
        crate::arch::x86_64::syscall::verify_task_context(current_id, "SWITCH_POINT_OUTGOING");
    }

    // DEBUG: Set marker 107 before restore_context_only
    crate::arch::x86_64::syscall::set_debug_marker(107);

    // Jump to new task using IRETQ (does not return!)
    // Note: Current task's context was saved when it last yielded via syscall
    unsafe {
        super::switch::restore_context_only(target_ctx_ptr);
    }

    // Never reached - restore_context_only does not return
}

/// Get next task to run
pub fn schedule_next() -> Option<TaskId> {
    SCHEDULER.lock().schedule_next()
}

/// Apply brain hint to scheduler state
///
/// Takes semantic context hints from the BrainBridge and applies them
/// to scheduling decisions. This enables proactive optimization.
///
/// # Examples of Hint Application
///
/// - **Compiling**: Boost CPU frequency, extend time slices
/// - **Gaming**: Reduce latency, prioritize foreground tasks
/// - **Rendering**: Balance CPU/GPU, optimize memory bandwidth
fn apply_brain_hint(hint: &BrainBridgeSnapshot, cpu_boost: &mut bool) {
    // Log hint for visibility
    crate::serial_println!(
        "[SCHED_HINT] Intent: {:?}, Confidence: {}, Duration: {}s, CPU: {}%",
        hint.current_intent,
        hint.confidence,
        hint.expected_burst_sec,
        hint.predicted_cpu
    );

    match hint.current_intent {
        IntentType::Compiling if hint.confidence > 180 => {
            // High-confidence compilation detected
            if !*cpu_boost {
                crate::serial_println!("[SCHED_HINT] Boosting CPU for compilation");
                // Boost CPU to maximum performance
                crate::arch::x86_64::set_cpu_freq(3500); // 3.5GHz
                *cpu_boost = true;
            }

            // Could also adjust:
            // - Increase time slice for CPU-bound tasks
            // - Reduce context switch frequency
            // - Prefetch commonly used pages
        },

        IntentType::Gaming if hint.confidence > 180 => {
            // Gaming workload - optimize for low latency
            crate::serial_println!("[SCHED_HINT] Optimizing for gaming (low latency)");
            // Could adjust:
            // - Reduce scheduling quantum (more responsive)
            // - Prioritize foreground tasks
            // - Pin gaming process to dedicated cores
        },

        IntentType::Rendering if hint.confidence > 180 => {
            // Rendering workload
            crate::serial_println!("[SCHED_HINT] Optimizing for rendering");
            // Could adjust:
            // - Balance CPU/GPU scheduling
            // - Optimize memory bandwidth allocation
            // - Enable turbo boost if available
        },

        IntentType::Idle => {
            // No specific workload, reduce to power-saving mode
            if *cpu_boost {
                crate::serial_println!("[SCHED_HINT] Returning to power-saving CPU frequency");
                // Return to base frequency or power save
                crate::arch::x86_64::set_base(); // Base frequency (typically 2.0-2.4 GHz)
                *cpu_boost = false;
            }
        },

        _ => {
            // Other intents or low confidence - no action
            if hint.confidence < 128 {
                crate::serial_println!("[SCHED_HINT] Low confidence ({}), ignoring", hint.confidence);
            }
        }
    }
}

/// Start scheduler (enter idle loop)
pub fn start() -> ! {
    crate::serial_println!("[SCHED] Scheduler started, entering task execution loop");

    // Disable interrupts during initial context switch
    x86_64::instructions::interrupts::disable();

    let mut iterations = 0u64;

    loop {
        // Print syscall counter every 1000 iterations
        if iterations % 1000 == 0 && iterations > 0 {
            let count = crate::arch::x86_64::syscall::get_syscall_count();
            crate::serial_println!("[SCHED] Iteration {}, syscalls: {}", iterations, count);
        }
        iterations += 1;

        if let Some(task_id) = schedule_next() {
            // Perform context switch
            unsafe {
                super::switch::switch_to(task_id);
            }
        } else {
            // No tasks runnable, halt until interrupt
            crate::serial_println!("[SCHED] No runnable tasks, halting");
            x86_64::instructions::interrupts::enable();
            x86_64::instructions::hlt();
            x86_64::instructions::interrupts::disable();
        }
    }
}

/// Time slice in ticks (10ms each, so 5 ticks = 50ms time slice)
const TIME_SLICE_TICKS: usize = 5;

/// Current remaining time slice for the running task
static TICKS_REMAINING: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(TIME_SLICE_TICKS);

/// Called from timer interrupt to handle potential preemption
///
/// Returns true if preemption should occur.
#[no_mangle]
pub extern "C" fn timer_tick_preempt() -> bool {
    use core::sync::atomic::Ordering;

    // Decrement tick counter
    let remaining = TICKS_REMAINING.fetch_sub(1, Ordering::Relaxed);

    if remaining <= 1 {
        // Time slice expired - reset counter and signal preemption
        TICKS_REMAINING.store(TIME_SLICE_TICKS, Ordering::Relaxed);

        // Record involuntary preemption
        let current_id = super::task::get_current_task();
        super::statistics::record_preemption(current_id);

        return true;
    }

    false
}

/// Reset time slice counter (called when a task voluntarily yields)
pub fn reset_time_slice() {
    use core::sync::atomic::Ordering;
    TICKS_REMAINING.store(TIME_SLICE_TICKS, Ordering::Relaxed);
}

/// Handle timer preemption - save current context and get next task
///
/// # Arguments
/// * `saved_rsp` - RSP pointing to saved registers on stack
///
/// # Returns
/// Pointer to the Context to restore (may be same or different task)
#[no_mangle]
pub extern "C" fn do_preemption(saved_rsp: usize) -> usize {
    use super::task;

    let current_id = task::get_current_task();

    // Save interrupted context from stack to task's Context
    if let Some(current_arc) = task::get_task(current_id) {
        let mut current = current_arc.lock();

        // Stack layout (pushed in reverse order in handler):
        // [ss, rsp, rflags, cs, rip, rax, rcx, rdx, rsi, rdi, r8, r9, r10, r11]
        // At saved_rsp: r11 is first (top of stack)
        unsafe {
            let frame_ptr = saved_rsp as *const u64;

            current.context.r11 = *frame_ptr.add(0);
            current.context.r10 = *frame_ptr.add(1);
            current.context.r9 = *frame_ptr.add(2);
            current.context.r8 = *frame_ptr.add(3);
            current.context.rdi = *frame_ptr.add(4);
            current.context.rsi = *frame_ptr.add(5);
            current.context.rdx = *frame_ptr.add(6);
            current.context.rcx = *frame_ptr.add(7);
            current.context.rax = *frame_ptr.add(8);
            current.context.rip = *frame_ptr.add(9);
            current.context.cs = *frame_ptr.add(10);
            current.context.rflags = *frame_ptr.add(11);
            current.context.rsp = *frame_ptr.add(12);
            current.context.ss = *frame_ptr.add(13);
        }
    }

    // Re-enqueue current task
    enqueue(current_id);

    // Get next task
    let next_id = match schedule_next() {
        Some(id) => id,
        None => current_id, // No other task, continue current
    };

    if next_id != current_id {
        crate::serial_println!("[PREEMPT] {} -> {}", current_id, next_id);
    }

    // Update current task
    task::set_current_task(next_id);

    // Record context switch
    super::statistics::record_context_switch(next_id);

    // Get next task's context
    let next_arc = task::get_task(next_id).expect("Next task not found");
    let next = next_arc.lock();

    // Switch page table if different
    if next.page_table_phys != 0 {
        unsafe {
            crate::memory::paging::switch_page_table(next.page_table_phys);
        }
    }

    // Update context pointer for syscalls
    let ctx_ptr = &next.context as *const task::Context;
    crate::arch::x86_64::syscall::set_current_context_ptr(ctx_ptr as *mut task::Context);

    // Return pointer to context for restore
    ctx_ptr as usize
}
