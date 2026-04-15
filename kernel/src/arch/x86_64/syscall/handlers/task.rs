//! Task lifecycle syscalls: spawn, exit, yield, get_pid, task_list, uptime.

extern crate alloc;

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

    // Collect resources to free BEFORE removing from task table
    let (page_table_phys, stack_base) = if let Some(task_arc) = task::get_task(current_id) {
        let mut t = task_arc.lock();
        t.state = TaskState::Exited;
        let pt = t.page_table_phys;
        let sb = t.kernel_stack_base.as_ref().map(|p| p.as_ptr());
        (pt, sb)
    } else {
        (0, None)
    };

    let _ = task::remove_task(current_id);

    // Free user-space page tables (if task had its own address space)
    if page_table_phys != 0 {
        if let Err(_) = crate::memory::paging::free_task_page_table(page_table_phys) {
            crate::serial_println!("[EXIT] WARN: failed to free page table for task {}", current_id);
        }
    }

    // Free kernel stack (was leaked via mem::forget during allocation)
    if let Some(base) = stack_base {
        unsafe {
            // Reconstruct the Vec and let it drop, freeing the 8KB heap allocation.
            // SAFETY: We are the only owner; the task has been removed from the scheduler
            // and will never be scheduled again (we're in the exit handler's HLT loop).
            let _ = alloc::vec::Vec::from_raw_parts(base, 0, 8192);
        }
    }

    crate::serial_println!("[EXIT] Task {} resources freed", current_id);

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
