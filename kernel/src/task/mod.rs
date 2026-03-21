//! Task Management and Scheduling

pub mod scheduler;
pub mod task;
pub mod spawn;
pub mod elf;
pub mod switch;
pub mod statistics;
pub mod preempt;
pub mod preempt_lock;

pub use scheduler::{init as scheduler_init, start as scheduler_start, enqueue, yield_cpu};
pub use task::{Task, FXSAVE_CURRENT_PTR, FxsaveArea};
pub use spawn::{spawn, spawn_raw, SpawnError};
pub use preempt_lock::{preempt_disable, preempt_enable, is_preemption_enabled};
pub use switch::{switch_to, init_context, init_user_context};
pub use statistics::{
    record_context_switch,
    record_syscall,
    record_ipc_sent,
    record_ipc_received,
    record_ipc_replied,
    record_ipc_block,
    record_page_fault,
    record_deadline_miss,
    record_priority_boost,
    record_voluntary_yield,
    record_preemption,
    record_scheduler_invocation,
    get_task_stats,
    get_system_stats,
    print_task_stats,
    print_system_stats,
    print_all_task_stats,
    format_task_stats,
};

pub type TaskId = u32;
