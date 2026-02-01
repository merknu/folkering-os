//! Task Spawning
//!
//! Creates new tasks from ELF binaries.
//! Unlike Unix fork/exec, we only have spawn() - create a new task from a binary.

use super::task::{Task, allocate_task_id, insert_task, PageTablePtr};
use super::elf::{ElfBinary, ElfError};
use super::TaskId;
use crate::memory::{PageTable, paging};
use alloc::boxed::Box;

/// Task spawn error codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    /// Invalid ELF binary
    InvalidElf(ElfError),
    /// Out of memory
    OutOfMemory,
    /// Permission denied
    PermissionDenied,
    /// Binary not found
    NotFound,
}

impl From<ElfError> for SpawnError {
    fn from(err: ElfError) -> Self {
        SpawnError::InvalidElf(err)
    }
}

/// Spawn a new task from an ELF binary
///
/// # Arguments
/// * `binary` - ELF binary data
/// * `args` - Command-line arguments (future enhancement)
///
/// # Returns
/// TaskId of the newly created task, or SpawnError
///
/// # Design Notes
/// - NO fork/exec - only spawn() creates new tasks
/// - Each task gets a fresh page table
/// - Capabilities are explicitly granted (no inheritance by default)
/// - Entry point is read from ELF header
pub fn spawn(binary: &[u8], _args: &[&str]) -> Result<TaskId, SpawnError> {
    use crate::memory::paging::flags;
    use crate::memory;
    use super::elf::pf;
    use core::mem::MaybeUninit;

    crate::serial_str!("[SPAWN_ELF] Starting ELF spawn, binary size=");
    crate::drivers::serial::write_dec(binary.len() as u32);
    crate::drivers::serial::write_newline();

    // 1. Parse ELF binary
    let elf = ElfBinary::parse(binary)?;
    let entry_point = elf.entry_point();
    crate::serial_str!("[SPAWN_ELF] ELF parsed, entry=");
    crate::drivers::serial::write_hex(entry_point);
    crate::drivers::serial::write_newline();

    // 2. Allocate new task ID
    let task_id = allocate_task_id();
    crate::serial_str!("[SPAWN_ELF] Allocated task ID: ");
    crate::drivers::serial::write_dec(task_id);
    crate::drivers::serial::write_newline();

    // 3. Create per-task page table (copies kernel mappings)
    let page_table_phys = paging::create_task_page_table()
        .map_err(|_| SpawnError::OutOfMemory)?;
    crate::serial_str!("[SPAWN_ELF] Page table created at phys ");
    crate::drivers::serial::write_hex(page_table_phys as u64);
    crate::drivers::serial::write_newline();

    // 4. Load all PT_LOAD segments into the task's address space
    for segment in elf.loadable_segments() {
        let vaddr = segment.p_vaddr;
        let filesz = segment.p_filesz as usize;
        let memsz = segment.p_memsz as usize;
        let offset = segment.p_offset as usize;
        let seg_flags = segment.p_flags;

        crate::serial_str!("[SPAWN_ELF] Loading segment: vaddr=");
        crate::drivers::serial::write_hex(vaddr);
        crate::serial_str!(", filesz=");
        crate::drivers::serial::write_dec(filesz as u32);
        crate::serial_str!(", memsz=");
        crate::drivers::serial::write_dec(memsz as u32);
        crate::serial_str!(", flags=");
        crate::drivers::serial::write_hex(seg_flags as u64);
        crate::drivers::serial::write_newline();

        // Skip empty segments
        if memsz == 0 {
            continue;
        }

        // Calculate number of pages needed
        let start_page = vaddr & !0xFFF; // Page-align start
        let end_addr = vaddr + memsz as u64;
        let end_page = (end_addr + 0xFFF) & !0xFFF; // Page-align end (round up)
        let num_pages = ((end_page - start_page) / 4096) as usize;

        crate::serial_str!("[SPAWN_ELF] Segment spans ");
        crate::drivers::serial::write_dec(num_pages as u32);
        crate::serial_str!(" pages: ");
        crate::drivers::serial::write_hex(start_page);
        crate::serial_str!(" - ");
        crate::drivers::serial::write_hex(end_page);
        crate::drivers::serial::write_newline();

        // Determine page flags based on segment flags
        let page_flags = if seg_flags & pf::W != 0 {
            flags::USER_DATA  // RW
        } else {
            flags::USER_CODE  // RX (code is typically R+X, no W)
        };

        // Allocate and map pages for this segment
        for page_idx in 0..num_pages {
            let page_vaddr = start_page + (page_idx as u64 * 4096);

            // Allocate physical page
            let phys_page = memory::physical::alloc_page()
                .ok_or(SpawnError::OutOfMemory)?;

            // Zero the page first via HHDM
            let hhdm_addr = crate::phys_to_virt(phys_page);
            unsafe {
                core::ptr::write_bytes(hhdm_addr as *mut u8, 0, 4096);
            }

            // Map into task's page table
            paging::map_page_in_table(
                page_table_phys,
                page_vaddr as usize,
                phys_page,
                page_flags,
            ).map_err(|_| SpawnError::OutOfMemory)?;

            // Copy segment data for this page
            let page_start_in_segment = if page_vaddr < vaddr {
                0
            } else {
                (page_vaddr - vaddr) as usize
            };

            let copy_offset_in_page = if page_vaddr < vaddr {
                (vaddr - page_vaddr) as usize
            } else {
                0
            };

            // Calculate how much data to copy into this page
            let remaining_in_file = if page_start_in_segment < filesz {
                filesz - page_start_in_segment
            } else {
                0
            };
            let copy_len = core::cmp::min(remaining_in_file, 4096 - copy_offset_in_page);

            if copy_len > 0 && offset + page_start_in_segment < binary.len() {
                let src_offset = offset + page_start_in_segment;
                let src_end = core::cmp::min(src_offset + copy_len, binary.len());
                let actual_copy_len = src_end - src_offset;

                unsafe {
                    core::ptr::copy_nonoverlapping(
                        binary.as_ptr().add(src_offset),
                        (hhdm_addr as *mut u8).add(copy_offset_in_page),
                        actual_copy_len,
                    );
                }
            }
        }
    }

    // 5. Allocate user stack (64KB = 16 pages)
    // Stack at a high address in user space
    let stack_pages = 64usize;
    let stack_size = stack_pages * 4096; // 256KB (increased from 64KB for compositor)
    let stack_top_addr = 0x7FFF_FFFF_0000u64;
    let stack_base = stack_top_addr - stack_size as u64;

    for i in 0..stack_pages {
        let page_vaddr = stack_base + (i * 4096) as u64;
        let page_phys = memory::physical::alloc_page()
            .ok_or(SpawnError::OutOfMemory)?;

        // Zero the page
        let hhdm = crate::phys_to_virt(page_phys);
        unsafe {
            core::ptr::write_bytes(hhdm as *mut u8, 0, 4096);
        }

        // Map into task's page table
        paging::map_page_in_table(
            page_table_phys,
            page_vaddr as usize,
            page_phys,
            flags::USER_STACK,
        ).map_err(|_| SpawnError::OutOfMemory)?;
    }

    let user_stack_top = stack_top_addr - 8;
    crate::serial_str!("[SPAWN_ELF] User stack at ");
    crate::drivers::serial::write_hex(stack_base);
    crate::serial_str!(", top=");
    crate::drivers::serial::write_hex(user_stack_top);
    crate::drivers::serial::write_newline();

    // 6. Create placeholder PageTablePtr (legacy, will be removed)
    let page_table_box: Box<PageTable> = unsafe {
        let mut uninit: Box<MaybeUninit<PageTable>> = Box::new_uninit();
        core::ptr::write_bytes(uninit.as_mut_ptr(), 0, 1);
        uninit.assume_init()
    };
    let page_table_ptr = PageTablePtr::new(Box::into_raw(page_table_box));

    // 7. Create task structure
    let mut task = Task::new(task_id, page_table_ptr, entry_point);
    task.page_table_phys = page_table_phys;
    task.context.rsp = user_stack_top;
    task.context.rbp = user_stack_top;

    crate::serial_str!("[SPAWN_ELF] Task created: id=");
    crate::drivers::serial::write_dec(task_id);
    crate::serial_str!(", entry=");
    crate::drivers::serial::write_hex(entry_point);
    crate::serial_str!(", rsp=");
    crate::drivers::serial::write_hex(user_stack_top);
    crate::drivers::serial::write_newline();

    // 8. Insert into global task table
    insert_task(task);

    // 9. Add to scheduler runqueue
    crate::task::scheduler::enqueue(task_id);

    crate::serial_str!("[SPAWN_ELF] Task ");
    crate::drivers::serial::write_dec(task_id);
    crate::serial_strln!(" spawn complete!");
    Ok(task_id)
}


/// Spawn a new task from raw code (bypass ELF parsing)
///
/// This is a simplified version for testing and bootstrapping.
/// Use when you have raw executable code without ELF wrapping.
///
/// # Arguments
/// * `code` - Raw executable bytes
/// * `entry_offset` - Offset into code where execution begins
///
/// # Returns
/// TaskId of the newly created task
///
/// # Example
/// ```no_run
/// let task_id = spawn_raw(&user_code, 0)?;
/// ```
pub fn spawn_raw(code: &[u8], entry_offset: u64) -> Result<TaskId, SpawnError> {
    use crate::arch::x86_64::usermode::{map_and_load_user_code_in_table, allocate_user_stack_in_table};
    use crate::memory::PageTable;
    use x86_64::VirtAddr;

    // 1. Allocate new task ID
    let task_id = allocate_task_id();

    // 2. Create per-task page table (copies kernel mappings)
    crate::serial_strln!("[SPAWN_RAW] Step 2: Creating per-task page table...");
    let page_table_phys = paging::create_task_page_table()
        .map_err(|_| SpawnError::OutOfMemory)?;
    crate::serial_str!("[SPAWN_RAW] Step 2: Page table created at phys ");
    crate::drivers::serial::write_hex(page_table_phys as u64);
    crate::drivers::serial::write_newline();

    // 3. Map and load code into task's page table at task-specific address
    // Each task gets 1 GB of address space: 0x400000 + (task_id - 1) * 0x40000000
    let code_base = 0x400000u64 + ((task_id - 1) as u64 * 0x40000000);
    crate::serial_str!("[SPAWN_RAW] Step 3: About to map code at ");
    crate::drivers::serial::write_hex(code_base);
    crate::drivers::serial::write_newline();
    let entry_point = map_and_load_user_code_in_table(page_table_phys, code, code_base);
    let entry_addr = entry_point.as_u64() + entry_offset;

    // 4. Allocate user stack in task's page table at task-specific address
    // Stack at top of task's 1GB region: code_base + 1GB - 4KB
    let stack_base = code_base + 0x40000000 - 4096;
    let user_stack = allocate_user_stack_in_table(page_table_phys, stack_base);

    // 5. Create placeholder PageTablePtr (legacy, will be removed)
    use alloc::boxed::Box;
    use core::mem::MaybeUninit;
    let page_table_box: Box<PageTable> = unsafe {
        let mut uninit: Box<MaybeUninit<PageTable>> = Box::new_uninit();
        core::ptr::write_bytes(uninit.as_mut_ptr(), 0, 1);
        uninit.assume_init()
    };
    let page_table_ptr = PageTablePtr::new(Box::into_raw(page_table_box));

    crate::serial_strln!("[SPAWN_RAW] Step 6: about to call Task::new()...");
    // 6. Create task structure using global buffer
    let mut task = Task::new(task_id, page_table_ptr, entry_addr);
    crate::serial_strln!("[SPAWN_RAW] Step 6: Task::new() returned");

    // 7. Set the per-task page table physical address
    task.page_table_phys = page_table_phys;
    crate::serial_str!("[SPAWN_RAW] Step 7: page_table_phys set to ");
    crate::drivers::serial::write_hex(page_table_phys as u64);
    crate::drivers::serial::write_newline();

    // 8. Update task's stack pointer in context
    crate::serial_str!("[SPAWN_RAW] Step 8: updating context.rsp/rbp to ");
    crate::drivers::serial::write_hex(user_stack.as_u64());
    crate::drivers::serial::write_newline();
    task.context.rsp = user_stack.as_u64();
    task.context.rbp = user_stack.as_u64();
    crate::serial_strln!("[SPAWN_RAW] Step 8: context updated");

    // 9. Insert into global task table
    crate::serial_strln!("[SPAWN_RAW] Step 9: about to insert_task()...");
    insert_task(task);
    crate::serial_strln!("[SPAWN_RAW] Step 9: insert_task() done");

    // 10. Add to scheduler runqueue
    crate::serial_strln!("[SPAWN_RAW] Step 10: about to enqueue()...");
    crate::task::scheduler::enqueue(task_id);
    crate::serial_strln!("[SPAWN_RAW] Step 10: enqueue() done");

    crate::serial_str!("[SPAWN_RAW] Task ");
    crate::drivers::serial::write_dec(task_id);
    crate::serial_str!(" spawn complete with page table ");
    crate::drivers::serial::write_hex(page_table_phys as u64);
    crate::serial_strln!("!");
    Ok(task_id)
}
