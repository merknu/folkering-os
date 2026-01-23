//! Task Management and Scheduling

pub mod scheduler;
pub mod task;
pub mod spawn;
pub mod elf;
pub mod switch;

pub use scheduler::{init as scheduler_init, start as scheduler_start};
pub use task::Task;
pub use spawn::{spawn, SpawnError};
pub use switch::{switch_to, init_context, init_user_context};

pub type TaskId = u32;
