//! Timer Preemption Support
//!
//! Handles preemptive context switching when the timer interrupt fires.

use super::task::{self, Context, TaskState};
use super::scheduler;
use super::statistics;
use crate::timer;

/// Saved interrupt context from timer handler
/// This matches the layout pushed by the timer handler assembly
#[repr(C)]
pub struct SavedInterruptContext {
    // Pushed by our handler (in reverse order of push)
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    // Pushed by CPU (interrupt frame)
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// Timer preemption handler called from assembly
///
/// This function:
/// 1. Saves the interrupted context to the current task
/// 2. Increments the tick counter
/// 3. Calls the scheduler to pick the next task
/// 4. Returns pointer to the context to restore (may be same or different task)
///
/// # Arguments
/// * `saved_ctx` - Pointer to the saved context on the stack
///
/// # Returns
/// Pointer to the Context structure to restore (always valid)
///
/// # Safety
/// Must be called with interrupts disabled
#[no_mangle]
pub extern "C" fn timer_preempt_handler(saved_ctx: *const SavedInterruptContext) -> *const Context {
    // Increment tick counter first
    timer::tick();

    // Send EOI early so we don't miss interrupts
    crate::arch::x86_64::apic::send_eoi();

    // Get current task ID
    let current_id = task::get_current_task();

    // If no current task (shouldn't happen), just return the saved context location
    // converted to a Context pointer (they have the same layout for the fields we care about)
    if current_id == 0 {
        // No task running - this shouldn't happen during normal operation
        return unsafe { &(*(saved_ctx as *const Context)) };
    }

    // Get current task and save the interrupted context
    let current_task = match task::get_task(current_id) {
        Some(t) => t,
        None => {
            // Task disappeared? Just return to caller
            return unsafe { &(*(saved_ctx as *const Context)) };
        }
    };

    // Save the interrupted context to the current task
    {
        let mut task_locked = current_task.lock();
        let saved = unsafe { &*saved_ctx };

        task_locked.context.rax = saved.rax;
        task_locked.context.rbx = saved.rbx;
        task_locked.context.rcx = saved.rcx;
        task_locked.context.rdx = saved.rdx;
        task_locked.context.rsi = saved.rsi;
        task_locked.context.rdi = saved.rdi;
        task_locked.context.rbp = saved.rbp;
        task_locked.context.r8 = saved.r8;
        task_locked.context.r9 = saved.r9;
        task_locked.context.r10 = saved.r10;
        task_locked.context.r11 = saved.r11;
        task_locked.context.r12 = saved.r12;
        task_locked.context.r13 = saved.r13;
        task_locked.context.r14 = saved.r14;
        task_locked.context.r15 = saved.r15;
        task_locked.context.rip = saved.rip;
        task_locked.context.rsp = saved.rsp;
        task_locked.context.rflags = saved.rflags;
        task_locked.context.cs = saved.cs;
        task_locked.context.ss = saved.ss;
    }

    // Call scheduler to pick next task
    let next_id = match scheduler::schedule_next() {
        Some(id) => id,
        None => {
            // No runnable tasks - return to current task
            let task_locked = current_task.lock();
            return &task_locked.context as *const Context;
        }
    };

    // If same task, just return its context
    if next_id == current_id {
        let task_locked = current_task.lock();
        return &task_locked.context as *const Context;
    }

    // Different task - this is a preemption!
    // Record the preemption for statistics
    statistics::record_preemption(current_id);
    statistics::record_context_switch(next_id);

    // Debug output for first few preemptions
    static mut PREEMPT_COUNT: u64 = 0;
    unsafe {
        PREEMPT_COUNT += 1;
        if PREEMPT_COUNT <= 5 {
            crate::serial_str!("[PREEMPT] Task ");
            crate::drivers::serial::write_dec(current_id);
            crate::serial_str!(" -> Task ");
            crate::drivers::serial::write_dec(next_id);
            crate::serial_str!(" (count=");
            crate::drivers::serial::write_dec(PREEMPT_COUNT as u32);
            crate::serial_strln!(")");
        }
    }

    // Get next task
    let next_task = match task::get_task(next_id) {
        Some(t) => t,
        None => {
            // Next task disappeared? Return to current
            let task_locked = current_task.lock();
            return &task_locked.context as *const Context;
        }
    };

    // Switch page tables if needed
    let next_page_table_phys = {
        let next_locked = next_task.lock();
        next_locked.page_table_phys
    };

    if next_page_table_phys != 0 {
        unsafe {
            crate::memory::paging::switch_page_table(next_page_table_phys);
        }
    }

    // Update current task ID
    task::set_current_task(next_id);

    // Update syscall context pointer
    let next_ctx_ptr = {
        let next_locked = next_task.lock();
        &next_locked.context as *const Context
    };
    crate::arch::x86_64::syscall::set_current_context_ptr(next_ctx_ptr as *mut Context);

    // Return pointer to next task's context
    next_ctx_ptr
}
