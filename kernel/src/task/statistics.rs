//! Task Statistics and Performance Monitoring
//!
//! Provides functions for tracking and querying task performance metrics.

use super::TaskId;
use super::task::{get_task, TaskStatistics};
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

/// Global system statistics
pub struct SystemStatistics {
    pub total_context_switches: u64,
    pub total_syscalls: u64,
    pub total_ipc_messages: u64,
    pub total_page_faults: u64,
    pub scheduler_invocations: u64,
}

impl SystemStatistics {
    pub const fn new() -> Self {
        Self {
            total_context_switches: 0,
            total_syscalls: 0,
            total_ipc_messages: 0,
            total_page_faults: 0,
            scheduler_invocations: 0,
        }
    }
}

use spin::Mutex;
static SYSTEM_STATS: Mutex<SystemStatistics> = Mutex::new(SystemStatistics::new());

/// Increment context switch counter
pub fn record_context_switch(task_id: TaskId) {
    // Update task stats
    if let Some(task_arc) = get_task(task_id) {
        let mut task = task_arc.lock();
        task.stats.context_switches += 1;
    }

    // Update system stats
    SYSTEM_STATS.lock().total_context_switches += 1;
}

/// Increment syscall counter
pub fn record_syscall(task_id: TaskId) {
    if let Some(task_arc) = get_task(task_id) {
        let mut task = task_arc.lock();
        task.stats.syscalls += 1;
    }

    SYSTEM_STATS.lock().total_syscalls += 1;
}

/// Record IPC message sent
pub fn record_ipc_sent(task_id: TaskId) {
    if let Some(task_arc) = get_task(task_id) {
        let mut task = task_arc.lock();
        task.stats.ipc_sent += 1;
    }

    SYSTEM_STATS.lock().total_ipc_messages += 1;
}

/// Record IPC message received
pub fn record_ipc_received(task_id: TaskId) {
    if let Some(task_arc) = get_task(task_id) {
        let mut task = task_arc.lock();
        task.stats.ipc_received += 1;
    }
}

/// Record IPC reply sent
pub fn record_ipc_replied(task_id: TaskId) {
    if let Some(task_arc) = get_task(task_id) {
        let mut task = task_arc.lock();
        task.stats.ipc_replied += 1;
    }
}

/// Record IPC block event
pub fn record_ipc_block(task_id: TaskId) {
    if let Some(task_arc) = get_task(task_id) {
        let mut task = task_arc.lock();
        task.stats.ipc_blocks += 1;
    }
}

/// Record page fault
pub fn record_page_fault(task_id: TaskId) {
    if let Some(task_arc) = get_task(task_id) {
        let mut task = task_arc.lock();
        task.stats.page_faults += 1;
    }

    SYSTEM_STATS.lock().total_page_faults += 1;
}

/// Record deadline miss
pub fn record_deadline_miss(task_id: TaskId) {
    if let Some(task_arc) = get_task(task_id) {
        let mut task = task_arc.lock();
        task.stats.deadline_misses += 1;
    }
}

/// Record priority boost
pub fn record_priority_boost(task_id: TaskId) {
    if let Some(task_arc) = get_task(task_id) {
        let mut task = task_arc.lock();
        task.stats.priority_boosts += 1;
    }
}

/// Record voluntary yield
pub fn record_voluntary_yield(task_id: TaskId) {
    if let Some(task_arc) = get_task(task_id) {
        let mut task = task_arc.lock();
        task.stats.voluntary_yields += 1;
    }
}

/// Record preemption
pub fn record_preemption(task_id: TaskId) {
    if let Some(task_arc) = get_task(task_id) {
        let mut task = task_arc.lock();
        task.stats.preemptions += 1;
    }
}

/// Record scheduler invocation
pub fn record_scheduler_invocation() {
    SYSTEM_STATS.lock().scheduler_invocations += 1;
}

/// Get task statistics
pub fn get_task_stats(task_id: TaskId) -> Option<TaskStatistics> {
    get_task(task_id).map(|task_arc| {
        let task = task_arc.lock();
        task.stats
    })
}

/// Get system-wide statistics
pub fn get_system_stats() -> SystemStatistics {
    let stats = SYSTEM_STATS.lock();
    SystemStatistics {
        total_context_switches: stats.total_context_switches,
        total_syscalls: stats.total_syscalls,
        total_ipc_messages: stats.total_ipc_messages,
        total_page_faults: stats.total_page_faults,
        scheduler_invocations: stats.scheduler_invocations,
    }
}

/// Print task statistics
pub fn print_task_stats(task_id: TaskId) {
    if let Some(task_arc) = get_task(task_id) {
        let task = task_arc.lock();
        let stats = &task.stats;
        let uptime_ms = crate::timer::uptime_ms();
        let lifetime_ms = uptime_ms - stats.created_at_ms;

        crate::serial_println!("\n[STATS] Task {} Statistics:", task_id);
        crate::serial_println!("[STATS] =====================");

        // Execution metrics
        crate::serial_println!("[STATS] Execution:");
        crate::serial_println!("[STATS]   Context switches: {}", stats.context_switches);
        crate::serial_println!("[STATS]   Syscalls: {}", stats.syscalls);
        crate::serial_println!("[STATS]   CPU cycles: {}", stats.cpu_cycles);
        crate::serial_println!("[STATS]   Lifetime: {} ms", lifetime_ms);
        crate::serial_println!("[STATS]   Total runtime: {} ms", stats.total_runtime_ms);

        // IPC metrics
        crate::serial_println!("[STATS] IPC:");
        crate::serial_println!("[STATS]   Messages sent: {}", stats.ipc_sent);
        crate::serial_println!("[STATS]   Messages received: {}", stats.ipc_received);
        crate::serial_println!("[STATS]   Replies sent: {}", stats.ipc_replied);
        crate::serial_println!("[STATS]   Blocks on IPC: {}", stats.ipc_blocks);

        // Memory metrics
        crate::serial_println!("[STATS] Memory:");
        crate::serial_println!("[STATS]   Page faults: {}", stats.page_faults);
        crate::serial_println!("[STATS]   Heap allocations: {}", stats.heap_allocations);
        crate::serial_println!("[STATS]   Heap frees: {}", stats.heap_frees);

        // Scheduling metrics
        crate::serial_println!("[STATS] Scheduling:");
        crate::serial_println!("[STATS]   Priority: {} (base: {})", task.priority, task.base_priority);
        crate::serial_println!("[STATS]   Deadline misses: {}", stats.deadline_misses);
        crate::serial_println!("[STATS]   Priority boosts: {}", stats.priority_boosts);
        crate::serial_println!("[STATS]   Voluntary yields: {}", stats.voluntary_yields);
        crate::serial_println!("[STATS]   Preemptions: {}", stats.preemptions);
        crate::serial_println!();
    } else {
        crate::serial_println!("[STATS] Task {} not found", task_id);
    }
}

/// Print system-wide statistics
pub fn print_system_stats() {
    let stats = SYSTEM_STATS.lock();
    let uptime_ms = crate::timer::uptime_ms();

    crate::serial_println!("\n[STATS] System Statistics:");
    crate::serial_println!("[STATS] ==================");
    crate::serial_println!("[STATS] Uptime: {} ms ({} seconds)", uptime_ms, uptime_ms / 1000);
    crate::serial_println!("[STATS] Total context switches: {}", stats.total_context_switches);
    crate::serial_println!("[STATS] Total syscalls: {}", stats.total_syscalls);
    crate::serial_println!("[STATS] Total IPC messages: {}", stats.total_ipc_messages);
    crate::serial_println!("[STATS] Total page faults: {}", stats.total_page_faults);
    crate::serial_println!("[STATS] Scheduler invocations: {}", stats.scheduler_invocations);

    // Calculate rates (per second)
    if uptime_ms > 0 {
        let uptime_sec = uptime_ms as f64 / 1000.0;
        let ctx_per_sec = stats.total_context_switches as f64 / uptime_sec;
        let syscalls_per_sec = stats.total_syscalls as f64 / uptime_sec;
        let ipc_per_sec = stats.total_ipc_messages as f64 / uptime_sec;

        crate::serial_println!("\n[STATS] Rates (per second):");
        crate::serial_println!("[STATS]   Context switches: {:.2}", ctx_per_sec);
        crate::serial_println!("[STATS]   Syscalls: {:.2}", syscalls_per_sec);
        crate::serial_println!("[STATS]   IPC messages: {:.2}", ipc_per_sec);
    }

    crate::serial_println!();
}

/// Get all task statistics as a vector
pub fn get_all_task_stats() -> Vec<(TaskId, TaskStatistics)> {
    use super::task::get_task_table;

    let table = get_task_table().lock();
    table.iter()
        .map(|(&id, task_arc)| {
            let task = task_arc.lock();
            (id, task.stats)
        })
        .collect()
}

/// Print statistics for all tasks
pub fn print_all_task_stats() {
    use super::task::get_task_table;

    crate::serial_println!("\n[STATS] All Task Statistics:");
    crate::serial_println!("[STATS] =====================\n");

    let table = get_task_table().lock();
    for &id in table.keys() {
        print_task_stats(id);
    }
}

/// Format task statistics as a string
pub fn format_task_stats(task_id: TaskId) -> Option<String> {
    get_task(task_id).map(|task_arc| {
        let task = task_arc.lock();
        let stats = &task.stats;
        let uptime_ms = crate::timer::uptime_ms();
        let lifetime_ms = uptime_ms - stats.created_at_ms;

        format!(
            "Task {} | Lifetime: {}ms | Ctx: {} | Syscalls: {} | IPC: {}/{}/{} | PF: {}",
            task_id,
            lifetime_ms,
            stats.context_switches,
            stats.syscalls,
            stats.ipc_sent,
            stats.ipc_received,
            stats.ipc_replied,
            stats.page_faults
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_stats() {
        let mut stats = SystemStatistics::new();
        assert_eq!(stats.total_context_switches, 0);
        assert_eq!(stats.total_syscalls, 0);

        stats.total_context_switches += 1;
        assert_eq!(stats.total_context_switches, 1);
    }
}
