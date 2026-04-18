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

    // Release task-owned IPC resources BEFORE removing the task entry.
    // Without these, a crashing/exiting task leaks:
    //   - Async TCP slots (4 total — exhausted after 4 bad exits)
    //   - Shared-memory regions it created (pages + table entry)
    //   - Global capability table slots (4096 total — including the
    //     DmaRegion caps auto-granted by `syscall_dma_alloc`)
    crate::net::tcp_async::free_task_slots(current_id);
    crate::ipc::shared_memory::free_task_regions(current_id);

    // Wake every task currently blocked waiting on us. Without this,
    // a crashing/exiting server (e.g. synapse, intent) leaves every
    // client sitting in `BlockedOnSend(current_id)` or
    // `WaitingForReply(...)` against it hung forever — the reply
    // they're waiting on will never arrive. Waiters are signaled
    // with u64::MAX in rax so their syscall returns the standard
    // kernel-failure sentinel.
    let unblocked = crate::ipc::unblock_waiters_for(current_id);
    if unblocked > 0 {
        crate::serial_str!("[EXIT] Unblocked ");
        crate::drivers::serial::write_dec(unblocked);
        crate::serial_str!(" waiter(s) on task ");
        crate::drivers::serial::write_dec(current_id);
        crate::serial_strln!(" exit");
    }

    // Drain the task's capability list and revoke each entry so the
    // global table slot becomes reclaimable. Drain first while the
    // task is still in TASK_TABLE, then walk the Vec outside the
    // task lock so `revoke()` (which takes the global cap mutex) can
    // run without holding a per-task lock.
    let cap_ids: alloc::vec::Vec<u32> = if let Some(task_arc) = task::get_task(current_id) {
        let mut t = task_arc.lock();
        core::mem::take(&mut t.capabilities)
    } else {
        alloc::vec::Vec::new()
    };
    for cap_id in cap_ids {
        // `revoke_with_cleanup` frees backing resources (e.g.
        // `alloc_contiguous` pages held by `DmaRegion` caps) in
        // addition to marking the cap-table slot reusable.
        let _ = crate::capability::revoke_with_cleanup(cap_id);
    }

    let _ = task::remove_task(current_id);

    // Free user-space page tables (if task had its own address space).
    //
    // CRITICAL: switch CR3 to the kernel PML4 BEFORE calling free
    // here. `syscall_exit` is still running inside the dying task's
    // address space, so CR3 currently points at `page_table_phys`.
    // If we free those pages first, the HLT loop below (and any
    // interrupt that fires in between) runs with CR3 referencing
    // PMM-reclaimed frames — the kernel's instruction fetches and
    // stack accesses go through HHDM which stays valid, but any
    // code path that walks the current address space (page fault,
    // nested IRQ, signal delivery) would dereference freed pages.
    if page_table_phys != 0 {
        let kernel_pml4 = crate::memory::paging::kernel_pml4_phys();
        if kernel_pml4 != 0 {
            unsafe {
                use x86_64::registers::control::{Cr3, Cr3Flags};
                use x86_64::structures::paging::PhysFrame;
                let frame = PhysFrame::containing_address(
                    x86_64::PhysAddr::new(kernel_pml4)
                );
                Cr3::write(frame, Cr3Flags::empty());
            }
        }
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

    // Pointer must land entirely in userspace. Without this, a task
    // could pass `buf_ptr = 0xFFFF_FFFF_8000_0000` and we'd write task
    // names + state bytes into kernel memory — corruption that
    // persists for the rest of the boot.
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    if buf_ptr < 0x200000 || buf_ptr >= USERSPACE_TOP || buf_size > 64 * 1024 {
        return 0;
    }
    let buf_end = match buf_ptr.checked_add(buf_size) {
        Some(e) => e,
        None => return 0,
    };
    if buf_end > USERSPACE_TOP { return 0; }

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
