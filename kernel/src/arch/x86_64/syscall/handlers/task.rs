//! Task lifecycle syscalls: spawn, exit, yield, get_pid, task_list, uptime.

pub fn syscall_spawn(binary_ptr: u64, binary_len: u64) -> u64 {
    use crate::task::spawn;

    if binary_ptr == 0 || binary_len == 0 || binary_len > 100 * 1024 * 1024 {
        return u64::MAX;
    }

    let binary = unsafe {
        core::slice::from_raw_parts(binary_ptr as *const u8, binary_len as usize)
    };

    match spawn(binary, &[]) {
        Ok(task_id) => task_id as u64,
        Err(_) => u64::MAX,
    }
}

pub fn syscall_exit(exit_code: u64) -> u64 {
    use crate::task::task::{self, TaskState};

    let current_id = task::get_current_task();
    crate::serial_println!("syscall: exit(code={}) task={}", exit_code, current_id);

    if let Some(task_arc) = task::get_task(current_id) {
        let mut t = task_arc.lock();
        t.state = TaskState::Exited;
    }

    let _ = task::remove_task(current_id);

    crate::serial_println!("[EXIT] Task {} removed from scheduler", current_id);

    loop {
        x86_64::instructions::hlt();
    }
}

pub fn syscall_yield() -> u64 {
    // This should never be called - yield is handled directly in syscall_entry
    crate::serial_println!("[SYSCALL] ERROR: yield handler called (should be handled in assembly!)");
    0
}

pub fn syscall_get_pid() -> u64 {
    crate::task::task::get_current_task() as u64
}

pub fn syscall_task_list() -> u64 {
    use crate::task::task::TASK_TABLE;

    let table = TASK_TABLE.lock();
    let count = table.len();
    count as u64
}

pub fn syscall_task_list_detailed(buf_ptr: u64, buf_size: u64) -> u64 {
    use crate::task::task::{TASK_TABLE, TaskState};

    if buf_ptr == 0 || buf_size == 0 {
        let table = TASK_TABLE.lock();
        return table.len() as u64;
    }

    let buf = unsafe {
        core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_size as usize)
    };

    let table = TASK_TABLE.lock();
    let mut offset = 0usize;
    let mut written = 0u64;

    for (&id, task_arc) in table.iter() {
        if offset + 32 > buf.len() {
            break;
        }
        let task = task_arc.lock();

        buf[offset..offset+4].copy_from_slice(&id.to_le_bytes());

        let state_val: u32 = match task.state {
            TaskState::Runnable => 0,
            TaskState::Running => 1,
            TaskState::BlockedOnReceive => 2,
            TaskState::BlockedOnSend(_) => 3,
            TaskState::WaitingForReply(_) => 4,
            TaskState::Exited => 5,
        };
        buf[offset+4..offset+8].copy_from_slice(&state_val.to_le_bytes());

        buf[offset+8..offset+24].copy_from_slice(&task.name);

        buf[offset+24..offset+32].copy_from_slice(&task.cpu_time_used_ms.to_le_bytes());

        offset += 32;
        written += 1;
    }

    written
}

pub fn syscall_uptime() -> u64 {
    crate::timer::uptime_ms()
}
