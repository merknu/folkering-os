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
pub mod intent;
pub mod block;
pub mod dns;
pub mod ping;
pub mod random;
pub mod time;
pub mod inference;
pub mod pci;

// Re-export commonly used functions at the sys level
pub use task::{exit, yield_cpu, get_pid, spawn, parallel_gemm, ask_gemini, http_fetch, http_post, fbp_request, fbp_interact, fbp_patch, llm_generate, wasm_compile, proxy_ping, draug_bridge_update, PatchStatus, udp_send, udp_send_recv, ntp_query, audio_play, audio_beep, gpu_flush, gpu_flush_batch, gpu_vsync, gpu_info, gpu_move_cursor, com3_read, com3_write, com2_write_raw, com2_async_send, com2_async_poll, com2_async_read, com2_async_poll_legacy, wait_for_irq, get_rtc, get_rtc_packed, DateTime, memory_stats, iqe_read, iqe_tsc_freq, telemetry_log, telemetry_drain, telemetry_stats, ws_connect, ws_send, ws_poll_recv, ws_close};
pub use io::{read_key, read_mouse, write_char, poweroff, check_interrupt, clear_interrupt, MouseEvent};
pub use ipc::{send, receive, reply};
pub use memory::{shmem_create, shmem_map, shmem_grant, shmem_unmap, shmem_destroy, mmap, mmap_at, munmap, PROT_READ, PROT_WRITE, PROT_EXEC, MmapError};
pub use system::{task_list, task_list_detailed, uptime};

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

// Re-export Intent service client
pub use intent::{
    INTENT_TASK_ID, IntentError,
};

// Re-export Shell client
pub use shell::{
    SHELL_TASK_ID, ShellError, ShellResult,
    list_files as shell_list_files, cat_file as shell_cat_file,
    search as shell_search, ps as shell_ps, get_uptime as shell_uptime,
};

// Re-export Block device
pub use block::{block_read, block_write, read_sector, write_sector, BlockError, SECTOR_SIZE, DATA_START_SECTOR};
