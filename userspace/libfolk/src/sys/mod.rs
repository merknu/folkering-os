//! Safe syscall wrappers for Folkering OS
//!
//! This module provides safe, ergonomic wrappers around the raw syscall interface.

pub mod task;
pub mod io;
pub mod ipc;
pub mod memory;
pub mod system;
pub mod fs;
pub mod synapse;
pub mod compositor;
pub mod shell;
pub mod boot_info;
pub mod map_physical;

// Re-export commonly used functions at the sys level
pub use task::{exit, yield_cpu, get_pid, spawn};
pub use io::{read_key, read_mouse, write_char, poweroff, check_interrupt, clear_interrupt, MouseEvent};
pub use ipc::{send, receive, reply};
pub use memory::{shmem_create, shmem_map, shmem_grant, shmem_unmap, shmem_destroy};
pub use system::{task_list, uptime};

// Re-export Synapse protocol
pub use synapse::{SYNAPSE_TASK_ID, SynapseError, SynapseResult};

// Re-export Compositor client
pub use compositor::{
    COMPOSITOR_TASK_ID, CompError,
    create_window, update_node, close_window,
    find_node_by_hash, query_focus, hash_name,
};

// Re-export boot info
pub use boot_info::{get_boot_info, boot_info, FolkeringBootInfo, FramebufferConfig, BOOT_INFO_VADDR};

// Re-export physical memory mapping
pub use map_physical::{map_physical, map_framebuffer, MapFlags, MapError};

// Re-export Shell client
pub use shell::{
    SHELL_TASK_ID, ShellError, ShellResult,
    list_files as shell_list_files, cat_file as shell_cat_file,
    search as shell_search, ps as shell_ps, get_uptime as shell_uptime,
};
