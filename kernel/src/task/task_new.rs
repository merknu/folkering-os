// Minimal Task::new() without debug output
use super::*;
use alloc::boxed::Box;
use core::mem::MaybeUninit;
use core::ptr;

#[inline(never)]
pub fn new_task(id: TaskId, page_table_ptr: PageTablePtr, entry_point: u64) -> Box<Task> {
    let mut uninit: Box<MaybeUninit<Task>> = Box::new_uninit();

    unsafe {
        let p = uninit.as_mut_ptr();

        ptr::addr_of_mut!((*p).id).write(id);
        ptr::addr_of_mut!((*p).state).write(TaskState::Runnable);
        ptr::addr_of_mut!((*p).page_table).write(page_table_ptr);

        // Context
        ptr::addr_of_mut!((*p).context.rsp).write(0x0000_7FFF_FFFF_F000);
        ptr::addr_of_mut!((*p).context.rbp).write(0x0000_7FFF_FFFF_F000);
        ptr::addr_of_mut!((*p).context.rax).write(0);
        ptr::addr_of_mut!((*p).context.rbx).write(0);
        ptr::addr_of_mut!((*p).context.rcx).write(0);
        ptr::addr_of_mut!((*p).context.rdx).write(0);
        ptr::addr_of_mut!((*p).context.rsi).write(0);
        ptr::addr_of_mut!((*p).context.rdi).write(0);
        ptr::addr_of_mut!((*p).context.r8).write(0);
        ptr::addr_of_mut!((*p).context.r9).write(0);
        ptr::addr_of_mut!((*p).context.r10).write(0);
        ptr::addr_of_mut!((*p).context.r11).write(0);
        ptr::addr_of_mut!((*p).context.r12).write(0);
        ptr::addr_of_mut!((*p).context.r13).write(0);
        ptr::addr_of_mut!((*p).context.r14).write(0);
        ptr::addr_of_mut!((*p).context.r15).write(0);
        ptr::addr_of_mut!((*p).context.rip).write(entry_point);
        ptr::addr_of_mut!((*p).context.rflags).write(0x202);
        ptr::addr_of_mut!((*p).context.cs).write(0x23);  // user code segment | RPL3
        ptr::addr_of_mut!((*p).context.ss).write(0x1B);  // user data segment | RPL3

        // Kernel context (for switch_context — zeroed except CS/SS/RFLAGS)
        ptr::addr_of_mut!((*p).kernel_context).write(Context::zero());

        // IPC
        ptr::addr_of_mut!((*p).recv_queue).write(MessageQueue::with_capacity(64));
        ptr::addr_of_mut!((*p).ipc_reply).write(None);
        ptr::addr_of_mut!((*p).blocked_on).write(None);

        // FPU/SSE state: must be valid before first FXRSTOR
        ptr::addr_of_mut!((*p).fxsave_area).write(FxsaveArea::default_init());

        // Security
        ptr::addr_of_mut!((*p).capabilities).write(Vec::new());
        ptr::addr_of_mut!((*p).credentials).write(Credentials {
            uid: 0,
            gid: 0,
            sandbox_level: SandboxLevel::Untrusted,
        });

        uninit.assume_init()
    }
}
