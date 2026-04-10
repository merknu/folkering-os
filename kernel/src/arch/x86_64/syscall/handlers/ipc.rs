//! IPC syscalls: send, receive, reply (sync), recv_async/reply_token (Phase 6).

pub fn syscall_ipc_send(target: u64, payload0: u64, payload1: u64) -> u64 {
    use crate::ipc::{IpcMessage, ipc_send};
    use crate::task::task::get_current_task;

    let mut msg = IpcMessage::new_request([payload0, payload1, 0, 0]);
    msg.sender = get_current_task();

    let target_id = target as u32;
    match ipc_send(target_id, &msg) {
        Ok(reply) => {
            crate::task::statistics::record_ipc_sent(get_current_task());
            reply.payload[0]
        }
        Err(_err) => {
            u64::MAX
        }
    }
}

pub fn syscall_ipc_receive(_from_filter: u64) -> u64 {
    use crate::ipc::{ipc_receive, send::Errno};

    // Non-blocking receive - userspace handles retries
    // This is necessary because yield_cpu() returns to userspace, not to the kernel loop
    // NOTE: Return value 0xFFFFFFFFFFFFFFFE triggers yield_path in syscall_entry,
    // so we use a different error code to avoid that.
    match ipc_receive() {
        Ok(msg) => {
            let current_task_id = crate::task::task::get_current_task();
            crate::task::statistics::record_ipc_received(current_task_id);

            // Save received message for later reply
            if let Some(task) = crate::task::task::get_task(current_task_id) {
                task.lock().ipc_reply = Some(msg);
            }

            // Return sender ID in lower 32 bits, first payload in upper 32 bits
            ((msg.payload[0] & 0xFFFFFFFF) << 32) | (msg.sender as u64)
        }
        Err(Errno::EWOULDBLOCK) => 0xFFFF_FFFF_FFFF_FFFD,
        Err(_err) => 0xFFFF_FFFF_FFFF_FFFC,
    }
}

pub fn syscall_ipc_reply(payload0: u64, payload1: u64) -> u64 {
    use crate::ipc::{ipc_reply, IpcMessage};
    use crate::task::task;

    crate::serial_println!("[SYSCALL] ipc_reply_simple(payload0={:#x}, payload1={:#x})",
                          payload0, payload1);

    let current_task_id = task::get_current_task();

    let task_arc = match task::get_task(current_task_id) {
        Some(t) => t,
        None => {
            crate::serial_println!("[SYSCALL] ipc_reply FAILED - task not found");
            return u64::MAX;
        }
    };

    let request_msg: IpcMessage = {
        let task_guard = task_arc.lock();
        match &task_guard.ipc_reply {
            Some(req) => *req,
            None => {
                drop(task_guard);
                crate::serial_println!("[SYSCALL] ipc_reply FAILED - no pending request");
                return u64::MAX;
            }
        }
    };

    let reply_payload = [payload0, payload1, 0, 0];

    match ipc_reply(&request_msg, reply_payload) {
        Ok(()) => {
            crate::serial_println!("[SYSCALL] ipc_reply SUCCESS");
            crate::task::statistics::record_ipc_replied(current_task_id);
            0
        }
        Err(err) => {
            crate::serial_println!("[SYSCALL] ipc_reply FAILED - error: {:?}", err);
            u64::MAX
        }
    }
}

/// Async IPC receive - returns CallerToken for deferred reply (syscall 0x20)
pub fn syscall_ipc_recv_async() -> u64 {
    use crate::ipc::{ipc_recv_async, send::Errno};

    match ipc_recv_async() {
        Ok((token, msg)) => {
            let current_task_id = crate::task::task::get_current_task();
            crate::task::statistics::record_ipc_received(current_task_id);

            if let Some(task) = crate::task::task::get_task(current_task_id) {
                task.lock().ipc_reply = Some(msg);
            }

            token.as_raw()
        }
        Err(Errno::EWOULDBLOCK) => 0xFFFF_FFFF_FFFF_FFFD,
        Err(_) => 0xFFFF_FFFF_FFFF_FFFC,
    }
}

/// Reply using CallerToken (syscall 0x21)
pub fn syscall_ipc_reply_token(token_raw: u64, payload0: u64, payload1: u64) -> u64 {
    use crate::ipc::{ipc_reply_with_token, CallerToken};

    let token = CallerToken::from_raw(token_raw);
    let reply_payload = [payload0, payload1, 0, 0];

    match ipc_reply_with_token(token, reply_payload) {
        Ok(()) => {
            let current_task_id = crate::task::task::get_current_task();
            crate::task::statistics::record_ipc_replied(current_task_id);
            0
        }
        Err(_) => u64::MAX,
    }
}

/// Get payload from last recv_async (syscall 0x22)
pub fn syscall_ipc_get_recv_payload() -> u64 {
    let current_task_id = crate::task::task::get_current_task();

    if let Some(task) = crate::task::task::get_task(current_task_id) {
        let task_guard = task.lock();
        if let Some(ref msg) = task_guard.ipc_reply {
            return msg.payload[0];
        }
    }

    u64::MAX
}

/// Get sender from last recv_async (syscall 0x23)
pub fn syscall_ipc_get_recv_sender() -> u64 {
    let current_task_id = crate::task::task::get_current_task();

    if let Some(task) = crate::task::task::get_task(current_task_id) {
        let task_guard = task.lock();
        if let Some(ref msg) = task_guard.ipc_reply {
            return msg.sender as u64;
        }
    }

    u64::MAX
}
