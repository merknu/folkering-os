//! Task Management and Scheduling

pub mod scheduler;
pub mod task;
pub mod spawn;
pub mod elf;
pub mod switch;

pub use scheduler::{init as scheduler_init, start as scheduler_start, enqueue, yield_cpu};
pub use task::Task;
pub use spawn::{spawn, spawn_raw, SpawnError};
pub use switch::{switch_to, init_context, init_user_context};

pub type TaskId = u32;
