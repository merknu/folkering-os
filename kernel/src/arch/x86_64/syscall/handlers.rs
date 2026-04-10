//! Syscall handler implementations.
//!
//! Every public function here corresponds to one entry in the dispatcher.
//! They are intentionally kept as `pub(super) fn` so they're only callable
//! through the dispatch table — never directly from outside the syscall module.

use crate::task::task;

// ── IPC ────────────────────────────────────────────────────────────────

pub(super) fn syscall_ipc_send(target: u64, payload0: u64, payload1: u64) -> u64 {
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

pub(super) fn syscall_ipc_receive(_from_filter: u64) -> u64 {
    use crate::ipc::{ipc_receive, send::Errno};

    // Non-blocking receive - userspace handles retries
    // This is necessary because yield_cpu() returns to userspace, not to the kernel loop
    // NOTE: Return value 0xFFFFFFFFFFFFFFFE triggers yield_path in syscall_entry,
    // so we use a different error code to avoid that.
    match ipc_receive() {
        Ok(msg) => {
            // Record IPC receive
            let current_task_id = crate::task::task::get_current_task();
            crate::task::statistics::record_ipc_received(current_task_id);

            // Save received message for later reply
            if let Some(task) = crate::task::task::get_task(current_task_id) {
                task.lock().ipc_reply = Some(msg);
            }

            // Return sender ID in lower 32 bits, first payload in upper 32 bits
            let result = ((msg.payload[0] & 0xFFFFFFFF) << 32) | (msg.sender as u64);
            result
        }
        Err(Errno::EWOULDBLOCK) => {
            // No messages available - return -3 as error code
            // IMPORTANT: NOT 0xFFFFFFFFFFFFFFFE which triggers yield_path!
            0xFFFF_FFFF_FFFF_FFFD
        }
        Err(_err) => {
            0xFFFF_FFFF_FFFF_FFFC
        }
    }
}

pub(super) fn syscall_ipc_reply(payload0: u64, payload1: u64) -> u64 {
    use crate::ipc::{ipc_reply, IpcMessage};
    use crate::task::task;

    crate::serial_println!("[SYSCALL] ipc_reply_simple(payload0={:#x}, payload1={:#x})",
                          payload0, payload1);

    // Get current task to find the pending IPC reply context
    let current_task_id = task::get_current_task();

    // Get the task structure to access the pending reply
    let task_arc = match task::get_task(current_task_id) {
        Some(t) => t,
        None => {
            crate::serial_println!("[SYSCALL] ipc_reply FAILED - task not found");
            return u64::MAX;
        }
    };

    let request_msg: IpcMessage = {
        let task_guard = task_arc.lock();
        // Get the IPC reply context (the original request we received)
        match &task_guard.ipc_reply {
            Some(req) => *req, // Copy the message
            None => {
                drop(task_guard);
                crate::serial_println!("[SYSCALL] ipc_reply FAILED - no pending request");
                return u64::MAX; // No pending reply
            }
        }
    };

    // Create reply payload from register values
    let reply_payload = [payload0, payload1, 0, 0];

    // Send reply
    match ipc_reply(&request_msg, reply_payload) {
        Ok(()) => {
            crate::serial_println!("[SYSCALL] ipc_reply SUCCESS");
            // Record IPC reply
            crate::task::statistics::record_ipc_replied(current_task_id);
            0 // Success
        }
        Err(err) => {
            crate::serial_println!("[SYSCALL] ipc_reply FAILED - error: {:?}", err);
            u64::MAX
        }
    }
}

/// Async IPC receive - returns CallerToken for deferred reply (syscall 0x20)
pub(super) fn syscall_ipc_recv_async() -> u64 {
    use crate::ipc::{ipc_recv_async, send::Errno};

    match ipc_recv_async() {
        Ok((token, msg)) => {
            let current_task_id = crate::task::task::get_current_task();
            crate::task::statistics::record_ipc_received(current_task_id);

            // Store the original message for payload retrieval
            if let Some(task) = crate::task::task::get_task(current_task_id) {
                task.lock().ipc_reply = Some(msg);
            }

            // Return the raw token value - userspace MUST use this for reply_with_token
            token.as_raw()
        }
        Err(Errno::EWOULDBLOCK) => {
            0xFFFF_FFFF_FFFF_FFFD
        }
        Err(_) => {
            0xFFFF_FFFF_FFFF_FFFC
        }
    }
}

/// Reply using CallerToken (syscall 0x21)
pub(super) fn syscall_ipc_reply_token(token_raw: u64, payload0: u64, payload1: u64) -> u64 {
    use crate::ipc::{ipc_reply_with_token, CallerToken};

    let token = CallerToken::from_raw(token_raw);
    let reply_payload = [payload0, payload1, 0, 0];

    match ipc_reply_with_token(token, reply_payload) {
        Ok(()) => {
            let current_task_id = crate::task::task::get_current_task();
            crate::task::statistics::record_ipc_replied(current_task_id);
            0
        }
        Err(_) => {
            u64::MAX
        }
    }
}

/// Get payload from last recv_async (syscall 0x22)
pub(super) fn syscall_ipc_get_recv_payload() -> u64 {
    let current_task_id = crate::task::task::get_current_task();

    if let Some(task) = crate::task::task::get_task(current_task_id) {
        let task_guard = task.lock();
        if let Some(ref msg) = task_guard.ipc_reply {
            // Return full 64-bit payload[0]
            return msg.payload[0];
        }
    }

    u64::MAX
}

/// Get sender from last recv_async (syscall 0x23)
pub(super) fn syscall_ipc_get_recv_sender() -> u64 {
    let current_task_id = crate::task::task::get_current_task();

    if let Some(task) = crate::task::task::get_task(current_task_id) {
        let task_guard = task.lock();
        if let Some(ref msg) = task_guard.ipc_reply {
            return msg.sender as u64;
        }
    }

    u64::MAX
}

// ── Shared Memory ──────────────────────────────────────────────────────

pub(super) fn syscall_shmem_create(size: u64) -> u64 {
    use crate::ipc::shared_memory::{shmem_create, ShmemPerms};

    if size == 0 || size > 1024 * 1024 * 1024 {
        return u64::MAX;
    }

    match shmem_create(size as usize, ShmemPerms::ReadWrite) {
        Ok(shmem_id) => shmem_id.get() as u64,
        Err(_) => u64::MAX,
    }
}

pub(super) fn syscall_shmem_map(shmem_id: u64, virt_addr: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_map;
    use core::num::NonZeroU32;

    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX,
    };

    if virt_addr == 0 {
        return u64::MAX;
    }

    match shmem_map(id, virt_addr as usize) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

pub(super) fn syscall_shmem_grant(shmem_id: u64, target_task: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_grant;
    use core::num::NonZeroU32;

    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX,
    };

    if target_task == 0 || target_task > u32::MAX as u64 {
        return u64::MAX;
    }

    match shmem_grant(id, target_task as u32) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

pub(super) fn syscall_shmem_unmap(shmem_id: u64, virt_addr: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_unmap;
    use core::num::NonZeroU32;

    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX,
    };

    if virt_addr == 0 {
        return u64::MAX;
    }

    match shmem_unmap(id, virt_addr as usize) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

pub(super) fn syscall_shmem_destroy(shmem_id: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_destroy;
    use core::num::NonZeroU32;

    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX,
    };

    match shmem_destroy(id) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

// ── Anonymous Memory Mapping ───────────────────────────────────────────

pub(super) fn syscall_mmap(hint_addr: u64, size: u64, flags: u64) -> u64 {
    use crate::memory::physical::alloc_page;
    use crate::memory::paging::map_page_in_table;
    use x86_64::structures::paging::PageTableFlags;

    const PAGE_SIZE: u64 = 4096;
    const MAX_MMAP_SIZE: u64 = 16 * 1024 * 1024;
    const MMAP_BASE: u64 = 0x4000_0000;

    if size == 0 || size > MAX_MMAP_SIZE {
        return u64::MAX;
    }

    let num_pages = ((size + PAGE_SIZE - 1) / PAGE_SIZE) as usize;

    let task_pml4 = crate::task::task::current_task().lock().page_table_phys;
    if task_pml4 == 0 {
        return u64::MAX;
    }

    let virt_base = if hint_addr != 0 {
        if hint_addr % PAGE_SIZE != 0 || hint_addr < MMAP_BASE {
            return u64::MAX;
        }
        hint_addr
    } else {
        use core::sync::atomic::{AtomicU64, Ordering};
        static NEXT_MMAP_ADDR: AtomicU64 = AtomicU64::new(MMAP_BASE);
        let addr = NEXT_MMAP_ADDR.fetch_add(num_pages as u64 * PAGE_SIZE, Ordering::Relaxed);
        if addr + (num_pages as u64 * PAGE_SIZE) > 0x7FFF_0000_0000 {
            return u64::MAX;
        }
        addr
    };

    let mut pt_flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if flags & 0x2 != 0 {
        pt_flags |= PageTableFlags::WRITABLE;
    }
    if flags & 0x4 == 0 {
        pt_flags |= PageTableFlags::NO_EXECUTE;
    }

    for i in 0..num_pages {
        let phys = match alloc_page() {
            Some(p) => p,
            None => {
                return u64::MAX;
            }
        };

        let virt = virt_base + (i as u64 * PAGE_SIZE);
        if map_page_in_table(task_pml4, virt as usize, phys, pt_flags).is_err() {
            return u64::MAX;
        }

        let hhdm_ptr = crate::phys_to_virt(phys) as *mut u8;
        unsafe {
            core::ptr::write_bytes(hhdm_ptr, 0, PAGE_SIZE as usize);
        }
    }

    virt_base
}

pub(super) fn syscall_munmap(virt_addr: u64, size: u64) -> u64 {
    use crate::memory::paging::unmap_page_in_table;
    use crate::memory::physical::free_pages;

    const PAGE_SIZE: u64 = 4096;
    const MMAP_BASE: u64 = 0x4000_0000;

    if size == 0 || virt_addr % PAGE_SIZE != 0 || virt_addr < MMAP_BASE {
        return u64::MAX;
    }

    let num_pages = ((size + PAGE_SIZE - 1) / PAGE_SIZE) as usize;

    let task_pml4 = crate::task::task::current_task().lock().page_table_phys;
    if task_pml4 == 0 {
        return u64::MAX;
    }

    let mut freed = 0usize;
    for i in 0..num_pages {
        let virt = virt_addr + (i as u64 * PAGE_SIZE);
        match unmap_page_in_table(task_pml4, virt as usize) {
            Ok(phys_addr) => {
                free_pages(phys_addr, 0);
                freed += 1;
            }
            Err(_) => {
            }
        }
    }

    if freed > 0 {
        crate::serial_println!("[MUNMAP] Freed {} pages at {:#x}", freed, virt_addr);
    }

    0
}

// ── Block Device ───────────────────────────────────────────────────────

pub(super) fn syscall_block_read(sector: u64, buf_ptr: u64, count: u64) -> u64 {
    use crate::drivers::virtio_blk;

    if !virtio_blk::is_initialized() {
        return u64::MAX;
    }

    if buf_ptr == 0 || count == 0 || count > 128 {
        return u64::MAX;
    }

    if buf_ptr >= 0x0000_8000_0000_0000 {
        return u64::MAX;
    }

    let count = count as usize;
    let mut offset = 0usize;
    let mut sec = sector;
    let mut remaining = count;

    while remaining > 0 {
        let burst = remaining.min(virtio_blk::MAX_BURST_SECTORS);
        let data_len = burst * 512;

        if burst > 1 {
            let dst = unsafe {
                core::slice::from_raw_parts_mut(
                    (buf_ptr as usize + offset) as *mut u8,
                    data_len,
                )
            };

            match virtio_blk::block_read_multi(sec, dst, burst) {
                Ok(()) => {
                    offset += data_len;
                    sec += burst as u64;
                    remaining -= burst;
                }
                Err(_) => return u64::MAX,
            }
        } else {
            let mut sector_buf = [0u8; 512];
            match virtio_blk::block_read(sec, &mut sector_buf) {
                Ok(()) => {
                    let dst = (buf_ptr as usize + offset) as *mut u8;
                    unsafe {
                        core::ptr::copy_nonoverlapping(sector_buf.as_ptr(), dst, 512);
                    }
                    offset += 512;
                    sec += 1;
                    remaining -= 1;
                }
                Err(_) => return u64::MAX,
            }
        }
    }
    0
}

pub(super) fn syscall_block_write(sector: u64, buf_ptr: u64, count: u64) -> u64 {
    use crate::drivers::virtio_blk;

    if !virtio_blk::is_initialized() {
        return u64::MAX;
    }

    if buf_ptr == 0 || count == 0 || count > 128 {
        return u64::MAX;
    }

    let buf_len = (count as usize) * virtio_blk::SECTOR_SIZE;

    if buf_ptr >= 0x0000_8000_0000_0000 {
        return u64::MAX;
    }

    let current_task = crate::task::task::get_current_task();
    let _ = virtio_blk::write_journal_entry(current_task, 1, sector, count);

    let buf = unsafe {
        core::slice::from_raw_parts(buf_ptr as *const u8, buf_len)
    };

    match virtio_blk::write_sectors(sector, buf, count as usize) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

// ── Network ────────────────────────────────────────────────────────────

pub(super) fn syscall_ping(ip_packed: u64) -> u64 {
    let a = (ip_packed & 0xFF) as u8;
    let b = ((ip_packed >> 8) & 0xFF) as u8;
    let c = ((ip_packed >> 16) & 0xFF) as u8;
    let d = ((ip_packed >> 24) & 0xFF) as u8;
    crate::net::send_ping(a, b, c, d);
    0
}

pub(super) fn syscall_dns_lookup(name_ptr: u64, name_len: u64) -> u64 {
    if name_ptr == 0 || name_len == 0 || name_len > 255 {
        return 0;
    }

    let name_bytes = unsafe {
        core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize)
    };

    let name = match core::str::from_utf8(name_bytes) {
        Ok(s) => s,
        Err(_) => return 0,
    };

    crate::net::dns_lookup(name)
}

pub(super) fn syscall_get_time() -> u64 {
    crate::drivers::cmos::unix_timestamp()
}

pub(super) fn syscall_get_random(buf_ptr: u64, buf_len: u64) -> u64 {
    if buf_ptr == 0 || buf_len == 0 || buf_len > 4096 {
        return u64::MAX;
    }
    let buf = unsafe {
        core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_len as usize)
    };
    crate::drivers::rng::fill_bytes(buf);
    0
}

pub(super) fn syscall_github_fetch(user_ptr: u64, user_len: u64, repo_ptr: u64, repo_len: u64) -> u64 {
    if user_ptr == 0 || user_len == 0 || user_len > 64 || repo_ptr == 0 || repo_len == 0 || repo_len > 64 {
        return u64::MAX;
    }
    let user = unsafe { core::slice::from_raw_parts(user_ptr as *const u8, user_len as usize) };
    let repo = unsafe { core::slice::from_raw_parts(repo_ptr as *const u8, repo_len as usize) };
    let user_str = match core::str::from_utf8(user) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };
    let repo_str = match core::str::from_utf8(repo) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    match crate::net::github::fetch_repo_info(user_str, repo_str) {
        Ok(info) => {
            crate::net::github::print_repo_info(&info);
            0
        }
        Err(e) => {
            crate::drivers::serial::write_str("[GITHUB] Error: ");
            crate::drivers::serial::write_str(e);
            crate::drivers::serial::write_newline();
            u64::MAX
        }
    }
}

pub(super) fn syscall_github_clone(user_ptr: u64, user_len: u64, repo_ptr: u64, repo_len: u64) -> u64 {
    if user_ptr == 0 || user_len == 0 || user_len > 64 || repo_ptr == 0 || repo_len == 0 || repo_len > 64 {
        return u64::MAX;
    }
    let user = unsafe { core::slice::from_raw_parts(user_ptr as *const u8, user_len as usize) };
    let repo = unsafe { core::slice::from_raw_parts(repo_ptr as *const u8, repo_len as usize) };
    let user_str = match core::str::from_utf8(user) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };
    let repo_str = match core::str::from_utf8(repo) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    let data = match crate::net::github::clone_repo(user_str, repo_str) {
        Ok(d) => d,
        Err(e) => {
            crate::drivers::serial::write_str("[CLONE] Error: ");
            crate::drivers::serial::write_str(e);
            crate::drivers::serial::write_newline();
            return u64::MAX;
        }
    };

    if data.is_empty() {
        return u64::MAX;
    }

    let size = data.len();
    let shmem_size = ((size + 4095) / 4096) * 4096;
    let shmem_size = if shmem_size == 0 { 4096 } else { shmem_size };

    use crate::ipc::shared_memory::{shmem_create, shmem_grant, ShmemPerms, SHMEM_TABLE};
    let id = match shmem_create(shmem_size, ShmemPerms::ReadWrite) {
        Ok(id) => id,
        Err(_) => return u64::MAX,
    };

    for tid in 2..=8u32 {
        let _ = shmem_grant(id, tid);
    }

    {
        let table = SHMEM_TABLE.lock();
        if let Some(shmem) = table.get(&id.get()) {
            let mut offset = 0;
            for &phys_page in &shmem.phys_pages {
                let virt = crate::phys_to_virt(phys_page);
                let chunk = (size - offset).min(4096);
                if chunk == 0 { break; }
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        data.as_ptr().add(offset),
                        virt as *mut u8,
                        chunk,
                    );
                }
                offset += chunk;
            }
        }
    }

    let handle = id.get();

    crate::serial_str!("[CLONE] Data in shmem handle=");
    crate::drivers::serial::write_dec(handle);
    crate::serial_str!(", size=");
    crate::drivers::serial::write_dec(size as u32);
    crate::serial_strln!(" bytes");

    ((size as u64) << 32) | (handle as u64)
}

pub(super) fn syscall_https_test(ip_packed: u64) -> u64 {
    let ip = if ip_packed != 0 {
        [
            (ip_packed >> 24) as u8,
            (ip_packed >> 16) as u8,
            (ip_packed >> 8) as u8,
            ip_packed as u8,
        ]
    } else {
        [93, 184, 215, 14] // example.com fallback
    };
    crate::serial_str!("[TLS] HTTPS GET to example.com...");
    match crate::net::tls::https_get(ip, "example.com", "/") {
        Ok(()) => {
            crate::serial_strln!("[TLS] HTTPS SUCCESS!");
            0
        }
        Err(e) => {
            crate::serial_str!("[TLS] HTTPS failed: ");
            crate::serial_strln!(e);
            u64::MAX
        }
    }
}

pub(super) fn syscall_http_fetch(url_ptr: u64, url_len: u64, buf_ptr: u64, buf_len: u64) -> u64 {
    if url_len == 0 || url_len > 512 || buf_len == 0 || buf_len > 65536 {
        return u64::MAX;
    }

    let url = unsafe {
        let slice = core::slice::from_raw_parts(url_ptr as *const u8, url_len as usize);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return u64::MAX,
        }
    };

    let stripped = url.strip_prefix("https://").unwrap_or(
        url.strip_prefix("http://").unwrap_or(url));
    let (host, path) = match stripped.find('/') {
        Some(i) => (&stripped[..i], &stripped[i..]),
        None => (stripped, "/"),
    };

    crate::serial_str!("[HTTP_FETCH] ");
    crate::serial_str!(host);
    crate::serial_str!(path);
    crate::serial_str!("\n");

    let ip_packed = crate::net::dns_lookup(host);
    if ip_packed == 0 || ip_packed == u64::MAX {
        crate::serial_strln!("[HTTP_FETCH] DNS failed");
        return u64::MAX;
    }
    let ip = [
        (ip_packed >> 24) as u8,
        (ip_packed >> 16) as u8,
        (ip_packed >> 8) as u8,
        ip_packed as u8,
    ];

    let mut request = alloc::vec::Vec::with_capacity(256 + host.len() + path.len());
    request.extend_from_slice(b"GET ");
    request.extend_from_slice(path.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(b"\r\nUser-Agent: FolkeringOS/1.0\r\nAccept: text/html,*/*\r\nConnection: close\r\n\r\n");

    let response = match crate::net::tls::https_get_raw(ip, host, &request) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[HTTP_FETCH] TLS failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    let body_start = response.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(0);

    let body = &response[body_start..];
    let copy_len = body.len().min(buf_len as usize);

    unsafe {
        let dst = core::slice::from_raw_parts_mut(buf_ptr as *mut u8, copy_len);
        dst.copy_from_slice(&body[..copy_len]);
    }

    crate::serial_str!("[HTTP_FETCH] OK, ");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_str!(" bytes body\n");

    copy_len as u64
}

pub(super) fn syscall_ntp_query(server_ip_packed: u64) -> u64 {
    let ip = [
        ((server_ip_packed >> 24) & 0xFF) as u8,
        ((server_ip_packed >> 16) & 0xFF) as u8,
        ((server_ip_packed >> 8) & 0xFF) as u8,
        (server_ip_packed & 0xFF) as u8,
    ];
    crate::net::ntp_query(ip)
}

pub(super) fn syscall_udp_send(target_packed: u64, port: u64, data_ptr: u64, data_len: u64) -> u64 {
    if data_len == 0 || data_len > 1472 { return u64::MAX; }
    let ip = [
        ((target_packed >> 24) & 0xFF) as u8,
        ((target_packed >> 16) & 0xFF) as u8,
        ((target_packed >> 8) & 0xFF) as u8,
        (target_packed & 0xFF) as u8,
    ];
    let data = unsafe {
        core::slice::from_raw_parts(data_ptr as *const u8, data_len as usize)
    };
    if crate::net::udp_send(ip, port as u16, data) { 0 } else { u64::MAX }
}

pub(super) fn syscall_udp_send_recv(
    target_packed: u64, port: u64,
    data_ptr: u64, data_len: u64,
    resp_ptr: u64, resp_len_and_timeout: u64,
) -> u64 {
    let resp_len = (resp_len_and_timeout & 0xFFFF_FFFF) as usize;
    let timeout_ms = (resp_len_and_timeout >> 32) as u32;
    if data_len == 0 || data_len > 1472 || resp_len == 0 || resp_len > 4096 {
        return u64::MAX;
    }
    let ip = [
        ((target_packed >> 24) & 0xFF) as u8,
        ((target_packed >> 16) & 0xFF) as u8,
        ((target_packed >> 8) & 0xFF) as u8,
        (target_packed & 0xFF) as u8,
    ];
    let data = unsafe {
        core::slice::from_raw_parts(data_ptr as *const u8, data_len as usize)
    };
    let response = unsafe {
        core::slice::from_raw_parts_mut(resp_ptr as *mut u8, resp_len)
    };
    crate::net::udp_send_recv(ip, port as u16, data, response, timeout_ms) as u64
}

// ── Audio ──────────────────────────────────────────────────────────────

pub(super) fn syscall_audio_play(samples_ptr: u64, samples_count: u64) -> u64 {
    if samples_count == 0 || samples_count > 1_000_000 { return u64::MAX; }
    let samples = unsafe {
        core::slice::from_raw_parts(samples_ptr as *const i16, samples_count as usize)
    };
    if crate::drivers::ac97::play_pcm(samples) { 0 } else { u64::MAX }
}

pub(super) fn syscall_audio_beep(duration_ms: u64) -> u64 {
    if crate::drivers::ac97::beep(duration_ms as u32) { 0 } else { u64::MAX }
}

// ── Tasks ──────────────────────────────────────────────────────────────

pub(super) fn syscall_spawn(binary_ptr: u64, binary_len: u64) -> u64 {
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

pub(super) fn syscall_exit(exit_code: u64) -> u64 {
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

pub(super) fn syscall_yield() -> u64 {
    // This should never be called - yield is handled directly in syscall_entry
    crate::serial_println!("[SYSCALL] ERROR: yield handler called (should be handled in assembly!)");
    0
}

pub(super) fn syscall_get_pid() -> u64 {
    crate::task::task::get_current_task() as u64
}

pub(super) fn syscall_task_list() -> u64 {
    use crate::task::task::TASK_TABLE;

    let table = TASK_TABLE.lock();
    let count = table.len();
    count as u64
}

pub(super) fn syscall_task_list_detailed(buf_ptr: u64, buf_size: u64) -> u64 {
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

pub(super) fn syscall_uptime() -> u64 {
    crate::timer::uptime_ms()
}

// ── Input/Output ───────────────────────────────────────────────────────

pub(super) fn syscall_read_key() -> u64 {
    if let Some(key) = crate::drivers::keyboard::read_key() {
        crate::drivers::iqe::record(
            crate::drivers::iqe::IqeEventType::KeyboardRead,
            crate::drivers::iqe::rdtsc(),
            key as u64,
        );
        if key == 0x03 {
            set_current_task_interrupt();
            return 0x03;
        }
        return key as u64;
    }

    if let Some(byte) = crate::drivers::serial::read_byte() {
        if byte == 0x03 {
            set_current_task_interrupt();
            return 0x03;
        }
        if byte == b'\r' {
            return b'\n' as u64;
        }
        return byte as u64;
    }

    0
}

pub(super) fn syscall_read_mouse() -> u64 {
    if let Some(event) = crate::drivers::mouse::read_event() {
        crate::drivers::iqe::record(
            crate::drivers::iqe::IqeEventType::MouseRead,
            crate::drivers::iqe::rdtsc(),
            0,
        );
        let buttons = event.buttons as u64;
        let dx = (event.dx as u16) as u64;
        let dy = (event.dy as u16) as u64;

        (1u64 << 63) | (dy << 24) | (dx << 8) | buttons
    } else {
        0
    }
}

/// Set interrupt flag on current task
fn set_current_task_interrupt() {
    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        task_arc.lock().interrupt_pending = true;
    }
}

pub(super) fn syscall_write_char(char_code: u64) -> u64 {
    let ch = (char_code & 0xFF) as u8;
    crate::drivers::serial::write_byte(ch);
    0
}

pub(super) fn syscall_poweroff() -> u64 {
    crate::serial_println!("\n[KERNEL] System poweroff requested");
    crate::serial_println!("[KERNEL] Goodbye!");

    unsafe {
        x86_64::instructions::port::Port::<u32>::new(0xf4).write(0x10);
    }

    unsafe {
        x86_64::instructions::port::Port::<u16>::new(0x604).write(0x2000);
    }

    loop {
        x86_64::instructions::hlt();
    }
}

pub(super) fn syscall_check_interrupt() -> u64 {
    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        if task_arc.lock().interrupt_pending {
            return 1;
        }
    }
    0
}

pub(super) fn syscall_clear_interrupt() -> u64 {
    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        task_arc.lock().interrupt_pending = false;
    }
    0
}

// ── Filesystem ─────────────────────────────────────────────────────────

pub(super) fn syscall_fs_read_dir(buf_ptr: u64, buf_size: u64) -> u64 {
    use crate::fs::format::DirEntry;

    if buf_ptr == 0 || buf_size == 0 {
        return u64::MAX;
    }

    let rd = match crate::fs::ramdisk() {
        Some(rd) => rd,
        None => return 0,
    };

    let entry_size = core::mem::size_of::<DirEntry>();
    let max_entries = buf_size as usize / entry_size;
    let entries = rd.entries();
    let count = entries.len().min(max_entries);

    for i in 0..count {
        let fpk = &entries[i];

        // CRITICAL: Use volatile reads to prevent LLVM from generating SSE instructions
        // that may cause GPF due to alignment assumptions in syscall context.
        let fpk_ptr = fpk as *const _ as *const u8;

        let id = unsafe { core::ptr::read_volatile(fpk_ptr as *const u16) };
        let entry_type = unsafe { core::ptr::read_volatile(fpk_ptr.add(2) as *const u16) };

        let mut name = [0u8; 32];
        for j in 0..32 {
            name[j] = unsafe { core::ptr::read_volatile(fpk_ptr.add(4 + j)) };
        }

        let size = unsafe { core::ptr::read_volatile(fpk_ptr.add(48) as *const u64) };

        let dir_entry = DirEntry {
            id,
            entry_type,
            name,
            size,
        };

        let dst = (buf_ptr as *mut u8).wrapping_add(i * entry_size);
        unsafe {
            let src = &dir_entry as *const DirEntry as *const u8;
            core::ptr::copy_nonoverlapping(src, dst, entry_size);
        }
    }

    count as u64
}

pub(super) fn syscall_fs_read_file(name_ptr: u64, buf_ptr: u64, buf_size: u64) -> u64 {
    if name_ptr == 0 || buf_ptr == 0 || buf_size == 0 {
        return u64::MAX;
    }

    let mut name_buf = [0u8; 32];
    let name_src = name_ptr as *const u8;
    let mut name_len = 0;
    for i in 0..32 {
        let b = unsafe { core::ptr::read(name_src.add(i)) };
        if b == 0 { break; }
        name_buf[i] = b;
        name_len = i + 1;
    }

    let name = match core::str::from_utf8(&name_buf[..name_len]) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    let rd = match crate::fs::ramdisk() {
        Some(rd) => rd,
        None => return u64::MAX,
    };

    let entry = match rd.find(name) {
        Some(e) => e,
        None => return u64::MAX,
    };

    let data = rd.read(entry);
    let copy_len = data.len().min(buf_size as usize);

    unsafe {
        core::ptr::copy_nonoverlapping(
            data.as_ptr(),
            buf_ptr as *mut u8,
            copy_len,
        );
    }

    copy_len as u64
}

// ── Compute ────────────────────────────────────────────────────────────

pub(super) fn syscall_parallel_gemm(
    input_ptr: u64,
    weight_ptr: u64,
    output_ptr: u64,
    k: u64,
    n: u64,
    quant_type: u64,
) -> u64 {
    crate::serial_str!("[PGEMM] syscall entry k=");
    crate::drivers::serial::write_dec(k as u32);
    crate::serial_str!(" n=");
    crate::drivers::serial::write_dec(n as u32);
    crate::drivers::serial::write_newline();

    let task_id = crate::task::task::get_current_task();
    let cr3 = match crate::task::task::get_task(task_id) {
        Some(t) => t.lock().page_table_phys,
        None => return u64::MAX,
    };

    crate::serial_str!("[PGEMM] task CR3=");
    crate::drivers::serial::write_hex(cr3);
    crate::serial_str!(" APs=");
    crate::drivers::serial::write_dec(super::super::smp::ap_count() as u32);
    crate::drivers::serial::write_newline();

    let result = super::super::smp::dispatch_parallel_gemm(
        input_ptr,
        weight_ptr,
        output_ptr,
        k as u32,
        n as u32,
        quant_type as u8,
        cr3,
    );

    if result == 0 { 0 } else { u64::MAX }
}

pub(super) fn syscall_ask_gemini(prompt_ptr: u64, prompt_len: u64, response_buf_ptr: u64) -> u64 {
    let prompt_len = prompt_len as usize;

    if prompt_len == 0 || prompt_len > 8192 {
        return u64::MAX;
    }

    let prompt_bytes = unsafe {
        core::slice::from_raw_parts(prompt_ptr as *const u8, prompt_len)
    };
    let prompt = match core::str::from_utf8(prompt_bytes) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    crate::serial_str!("[SYS_GEMINI] Prompt: ");
    let preview = &prompt[..prompt.len().min(80)];
    crate::drivers::serial::write_str(preview);
    crate::drivers::serial::write_newline();

    let result = crate::net::gemini::ask_gemini(prompt);

    let response_bytes = match result {
        Ok(bytes) => bytes,
        Err(e) => {
            crate::serial_str!("[SYS_GEMINI] Error: ");
            crate::drivers::serial::write_str(e);
            crate::drivers::serial::write_newline();
            let msg = alloc::format!("Error: {}", e);
            msg.into_bytes()
        }
    };

    let max_write = response_bytes.len().min(131072);
    unsafe {
        core::ptr::copy_nonoverlapping(
            response_bytes.as_ptr(),
            response_buf_ptr as *mut u8,
            max_write,
        );
    }

    max_write as u64
}

// ── GPU ────────────────────────────────────────────────────────────────

pub(super) fn syscall_gpu_flush(x: u64, y: u64, w: u64, h: u64) -> u64 {
    crate::drivers::virtio_gpu::flush_rect(x as u32, y as u32, w as u32, h as u32);
    0
}

pub(super) fn syscall_gpu_info(virt_addr: u64) -> u64 {
    use crate::drivers::virtio_gpu;

    if !virtio_gpu::GPU_ACTIVE.load(core::sync::atomic::Ordering::Relaxed) {
        return u64::MAX;
    }

    let (width, height) = match virtio_gpu::display_size() {
        Some(wh) => wh,
        None => return u64::MAX,
    };

    let pages = match virtio_gpu::framebuffer_pages() {
        Some(p) => p,
        None => return u64::MAX,
    };

    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        let pml4_phys = task_arc.lock().page_table_phys;
        let flags = x86_64::structures::paging::PageTableFlags::PRESENT
            | x86_64::structures::paging::PageTableFlags::WRITABLE
            | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE
            | x86_64::structures::paging::PageTableFlags::NO_EXECUTE
            | x86_64::structures::paging::PageTableFlags::WRITE_THROUGH;

        for (i, &phys_page) in pages.iter().enumerate() {
            let virt = virt_addr as usize + i * 4096;
            let _ = crate::memory::paging::map_page_in_table(
                pml4_phys, virt, phys_page, flags
            );
        }
    }

    ((width as u64) << 32) | (height as u64)
}

// ── Physical Memory Mapping (Phase 6.2) ────────────────────────────────

/// Map physical memory flags
pub mod map_flags {
    /// Allow reading from mapped memory
    pub const MAP_READ: u64 = 0x01;
    /// Allow writing to mapped memory
    pub const MAP_WRITE: u64 = 0x02;
    /// Allow executing from mapped memory (usually not used for MMIO)
    pub const MAP_EXEC: u64 = 0x04;
    /// Use Write-Combining caching (PAT index 4) - for framebuffer
    pub const MAP_CACHE_WC: u64 = 0x10;
    /// Use Uncached mode - for MMIO devices
    pub const MAP_CACHE_UC: u64 = 0x20;
}

pub(super) fn syscall_map_physical(phys_addr: u64, virt_addr: u64, size: u64, flags: u64, _reserved: u64) -> u64 {
    use crate::capability;
    use crate::memory::paging;
    use crate::task::task::{get_current_task, get_task};
    use x86_64::structures::paging::PageTableFlags as PTF;

    let task_id = get_current_task();

    if phys_addr & 0xFFF != 0 || virt_addr & 0xFFF != 0 {
        crate::serial_println!("[MAP_PHYSICAL] Error: Address not page-aligned");
        return u64::MAX;
    }

    if virt_addr >= 0x8000_0000_0000_0000 {
        crate::serial_println!("[MAP_PHYSICAL] Error: Virtual address in kernel space");
        return u64::MAX;
    }

    if size == 0 || size > 256 * 1024 * 1024 {
        crate::serial_println!("[MAP_PHYSICAL] Error: Invalid size");
        return u64::MAX;
    }

    // PCI MMIO BARs are typically above 0xF0000000 (MMIO hole)
    let is_pci_mmio = phys_addr >= 0xF000_0000 && size <= 1024 * 1024;
    if !is_pci_mmio && !capability::has_framebuffer_access(task_id, phys_addr, size) {
        crate::serial_str!("[MAP_PHYSICAL] Error: No capability for task ");
        crate::drivers::serial::write_dec(task_id);
        crate::serial_str!(" phys=");
        crate::drivers::serial::write_hex(phys_addr);
        crate::drivers::serial::write_newline();
        return u64::MAX;
    }

    let pml4_phys = match get_task(task_id) {
        Some(task) => task.lock().page_table_phys,
        None => {
            crate::serial_println!("[MAP_PHYSICAL] Error: Task not found");
            return u64::MAX;
        }
    };

    let mut ptf = PTF::PRESENT.union(PTF::USER_ACCESSIBLE).union(PTF::NO_EXECUTE);

    if flags & map_flags::MAP_WRITE != 0 {
        ptf = ptf.union(PTF::WRITABLE);
    }

    if flags & map_flags::MAP_CACHE_WC != 0 {
        ptf = ptf.union(PTF::NO_CACHE);
        crate::serial_println!("[MAP_PHYSICAL] Note: WC requested but using UC (PAT not supported by crate)");
    } else if flags & map_flags::MAP_CACHE_UC != 0 {
        ptf = ptf.union(PTF::NO_CACHE).union(PTF::WRITE_THROUGH);
    }

    let num_pages = ((size + 0xFFF) / 0x1000) as usize;

    crate::serial_println!("[MAP_PHYSICAL] Mapping {} pages from phys {:#x} to virt {:#x}",
                          num_pages, phys_addr, virt_addr);

    for i in 0..num_pages {
        let phys = phys_addr as usize + i * 0x1000;
        let virt = virt_addr as usize + i * 0x1000;

        if let Err(_) = paging::map_page_in_table(pml4_phys, virt, phys, ptf) {
            crate::serial_println!("[MAP_PHYSICAL] Error: Failed to map page at {:#x}", virt);
            return u64::MAX;
        }
    }

    crate::serial_println!("[MAP_PHYSICAL] Successfully mapped {} pages", num_pages);
    0
}

// ── PCI / Port I/O / IRQ (Phase 10) ────────────────────────────────────

/// Compact PCI device info for userspace (64 bytes, C-repr)
#[repr(C)]
#[derive(Clone, Copy)]
struct PciDeviceUserInfo {
    vendor_id: u16,       // 0
    device_id: u16,       // 2
    class_code: u8,       // 4
    subclass: u8,         // 5
    prog_if: u8,          // 6
    revision: u8,         // 7
    header_type: u8,      // 8
    interrupt_line: u8,   // 9
    interrupt_pin: u8,    // 10
    bus: u8,              // 11
    device: u8,           // 12
    function: u8,         // 13
    capabilities_ptr: u8, // 14
    _pad: u8,             // 15
    bar_addrs: [u64; 3],  // 16-39: BAR physical addresses (MMIO base, decoded)
    bar_sizes: [u32; 6],  // 40-63: BAR sizes in bytes
}

pub(super) fn syscall_pci_enumerate(buf_ptr: u64, buf_size: u64) -> u64 {
    let entry_size = core::mem::size_of::<PciDeviceUserInfo>();
    let max_entries = (buf_size as usize) / entry_size;

    if buf_ptr < 0x200000 || buf_ptr >= 0xFFFF_8000_0000_0000 || max_entries == 0 {
        return u64::MAX;
    }

    let list = crate::drivers::pci::PCI_DEVICES.lock();
    let mut written = 0usize;

    for i in 0..list.count.min(max_entries) {
        if let Some(ref dev) = list.devices[i] {
            let mut bar_addrs = [0u64; 3];
            let mut bar_sizes = [0u32; 6];

            for b in 0..6 {
                bar_sizes[b] = crate::drivers::pci::bar_size(dev.bus, dev.device, dev.function, b as u8);
                match crate::drivers::pci::decode_bar(dev, b) {
                    crate::drivers::pci::BarType::Mmio32 { base, .. } => {
                        if b < 3 { bar_addrs[b] = base as u64; }
                    }
                    crate::drivers::pci::BarType::Mmio64 { base, .. } => {
                        if b < 3 { bar_addrs[b] = base; }
                    }
                    crate::drivers::pci::BarType::Io { base } => {
                        if b < 3 { bar_addrs[b] = base as u64 | 0x1_0000_0000; }
                    }
                    crate::drivers::pci::BarType::None => {}
                }
            }

            let info = PciDeviceUserInfo {
                vendor_id: dev.vendor_id,
                device_id: dev.device_id,
                class_code: dev.class_code,
                subclass: dev.subclass,
                prog_if: dev.prog_if,
                revision: dev.revision,
                header_type: dev.header_type,
                interrupt_line: dev.interrupt_line,
                interrupt_pin: dev.interrupt_pin,
                bus: dev.bus,
                device: dev.device,
                function: dev.function,
                capabilities_ptr: dev.capabilities_ptr,
                _pad: 0,
                bar_addrs,
                bar_sizes,
            };

            let dest = (buf_ptr as usize) + written * entry_size;
            unsafe {
                let src = &info as *const PciDeviceUserInfo as *const u8;
                let dst = dest as *mut u8;
                core::ptr::copy_nonoverlapping(src, dst, entry_size);
            }
            written += 1;
        }
    }

    crate::serial_str!("[PCI] Enumerated ");
    crate::drivers::serial::write_dec(written as u32);
    crate::serial_strln!(" devices to userspace");

    written as u64
}

/// Check if a port is within a known PCI device's I/O BAR range.
fn port_io_allowed(port: u16) -> bool {
    // Blocklist: kernel-critical ports
    match port {
        0x0020..=0x0021 => return false, // PIC1
        0x00A0..=0x00A1 => return false, // PIC2
        0x0040..=0x0043 => return false, // PIT
        0x0060 | 0x0064 => return false, // PS/2
        0x0070..=0x0071 => return false, // CMOS
        0x03F8..=0x03FF => return false, // COM1
        0x02F8..=0x02FF => return false, // COM2
        0x03E8..=0x03EF => return false, // COM3
        0x0CF8..=0x0CFF => return false, // PCI config
        _ => {}
    }

    // Allowlist: check PCI device I/O BARs
    let list = crate::drivers::pci::PCI_DEVICES.lock();
    for i in 0..list.count {
        if let Some(ref dev) = list.devices[i] {
            for bar_idx in 0..6u8 {
                let bar_val = dev.bars[bar_idx as usize];
                if bar_val & 1 != 0 {
                    let base = (bar_val & 0xFFFC) as u16;
                    let size = crate::drivers::pci::bar_size(
                        dev.bus, dev.device, dev.function, bar_idx
                    ) as u16;
                    if size > 0 && port >= base && port < base.saturating_add(size) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

// ── IRQ Routing (Phase 10) ─────────────────────────────────────────────

const MAX_IRQ_BINDINGS: usize = 24;
const WASM_IRQ_BASE_VECTOR: u8 = 46;

struct IrqBinding {
    vector: u8,
    task_id: u32,
    pending: bool,
    active: bool,
}

static IRQ_BINDINGS: spin::Mutex<[IrqBinding; MAX_IRQ_BINDINGS]> = spin::Mutex::new({
    const EMPTY: IrqBinding = IrqBinding { vector: 0, task_id: 0, pending: false, active: false };
    [EMPTY; MAX_IRQ_BINDINGS]
});

/// Called from IDT handlers to signal a bound IRQ.
pub fn signal_irq(vector: u8) {
    let idx = vector.wrapping_sub(WASM_IRQ_BASE_VECTOR) as usize;
    if idx < MAX_IRQ_BINDINGS {
        if let Some(mut bindings) = IRQ_BINDINGS.try_lock() {
            if bindings[idx].active && bindings[idx].vector == vector {
                bindings[idx].pending = true;
            }
        }
    }
}

pub(super) fn syscall_bind_irq(irq_line: u64, _reserved: u64) -> u64 {
    let irq = irq_line as u8;
    let task_id = crate::task::task::get_current_task();

    if irq >= MAX_IRQ_BINDINGS as u8 {
        crate::serial_strln!("[IRQ] Bind failed: IRQ line out of range");
        return u64::MAX;
    }

    let vector = WASM_IRQ_BASE_VECTOR + irq;
    let idx = irq as usize;

    {
        let mut bindings = IRQ_BINDINGS.lock();
        bindings[idx] = IrqBinding {
            vector,
            task_id,
            pending: false,
            active: true,
        };
    }

    super::super::ioapic::enable_irq_level(irq, vector);

    crate::serial_str!("[IRQ] Bound IRQ");
    crate::drivers::serial::write_dec(irq as u32);
    crate::serial_str!(" -> vector ");
    crate::drivers::serial::write_dec(vector as u32);
    crate::serial_str!(" for task ");
    crate::drivers::serial::write_dec(task_id);
    crate::serial_strln!("");

    vector as u64
}

pub(super) fn syscall_ack_irq(irq_line: u64) -> u64 {
    let irq = irq_line as u8;
    let idx = irq as usize;

    if idx >= MAX_IRQ_BINDINGS { return u64::MAX; }

    {
        let mut bindings = IRQ_BINDINGS.lock();
        if bindings[idx].active {
            bindings[idx].pending = false;
        }
    }

    let vector = WASM_IRQ_BASE_VECTOR + irq;
    super::super::ioapic::enable_irq_level(irq, vector);

    0
}

pub(super) fn syscall_check_irq(irq_line: u64) -> u64 {
    let idx = irq_line as usize;
    if idx >= MAX_IRQ_BINDINGS { return u64::MAX; }

    let bindings = IRQ_BINDINGS.lock();
    if !bindings[idx].active { return u64::MAX; }
    if bindings[idx].pending { 1 } else { 0 }
}

pub(super) fn syscall_port_inb(port: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u8>::new(port);
        p.read() as u64
    }
}

pub(super) fn syscall_port_inw(port: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u16>::new(port);
        p.read() as u64
    }
}

pub(super) fn syscall_port_inl(port: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u32>::new(port);
        p.read() as u64
    }
}

pub(super) fn syscall_port_outb(port: u64, value: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u8>::new(port);
        p.write(value as u8);
    }
    0
}

pub(super) fn syscall_port_outw(port: u64, value: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u16>::new(port);
        p.write(value as u16);
    }
    0
}

pub(super) fn syscall_port_outl(port: u64, value: u64) -> u64 {
    let port = port as u16;
    if !port_io_allowed(port) {
        return u64::MAX;
    }
    unsafe {
        let mut p = x86_64::instructions::port::Port::<u32>::new(port);
        p.write(value as u32);
    }
    0
}

// ── DMA / IOMMU / WASM Net Bridge ──────────────────────────────────────

pub(super) fn syscall_dma_alloc(size: u64, vaddr: u64) -> u64 {
    let num_pages = ((size as usize) + 4095) / 4096;
    if num_pages == 0 || num_pages > 256 {
        return u64::MAX;
    }
    if vaddr < 0x200000 || vaddr >= 0xFFFF_8000_0000_0000 {
        return u64::MAX;
    }

    let phys_addr = match crate::memory::physical::alloc_contiguous(num_pages) {
        Some(addr) => addr,
        None => {
            crate::serial_strln!("[DMA] Failed to allocate contiguous memory");
            return u64::MAX;
        }
    };

    use crate::memory::paging;
    use crate::task::task::{get_current_task, get_task};
    use x86_64::structures::paging::PageTableFlags as Ptf;
    let task_id = get_current_task();
    let pml4_phys = match get_task(task_id) {
        Some(task) => task.lock().page_table_phys,
        None => return u64::MAX,
    };

    let ptf = Ptf::PRESENT | Ptf::WRITABLE | Ptf::USER_ACCESSIBLE | Ptf::NO_EXECUTE
        | Ptf::WRITE_THROUGH | Ptf::NO_CACHE;

    for i in 0..num_pages {
        let virt = vaddr as usize + i * 4096;
        let phys = phys_addr + i * 4096;
        if paging::map_page_in_table(pml4_phys, virt, phys, ptf).is_err() {
            crate::serial_strln!("[DMA] Page mapping failed");
            return u64::MAX;
        }
    }

    let iommu = super::super::acpi::iommu_available();

    crate::serial_str!("[DMA] Allocated ");
    crate::drivers::serial::write_dec(num_pages as u32);
    crate::serial_str!(" pages at phys=");
    crate::drivers::serial::write_hex(phys_addr as u64);
    crate::serial_str!(" vaddr=");
    crate::drivers::serial::write_hex(vaddr);
    if iommu {
        crate::serial_str!(" (IOMMU available)");
    }
    crate::drivers::serial::write_newline();

    phys_addr as u64
}

pub(super) fn syscall_iommu_status() -> u64 {
    let available = super::super::acpi::iommu_available();
    let base = super::super::acpi::iommu_base();
    if available {
        (base & 0xFFFFFFFF_00000000) | 1
    } else {
        0
    }
}

pub(super) fn syscall_net_register(mac_lo: u64, mac_hi: u64) -> u64 {
    let mac = [
        (mac_lo & 0xFF) as u8,
        ((mac_lo >> 8) & 0xFF) as u8,
        ((mac_lo >> 16) & 0xFF) as u8,
        ((mac_lo >> 24) & 0xFF) as u8,
        (mac_hi & 0xFF) as u8,
        ((mac_hi >> 8) & 0xFF) as u8,
    ];
    crate::net::init_wasm_net(mac);
    0
}

pub(super) fn syscall_net_submit_rx(vaddr: u64, length: u64) -> u64 {
    let len = length as usize;
    if len == 0 || len > 1514 || vaddr < 0x200000 {
        return u64::MAX;
    }
    let data = unsafe {
        core::slice::from_raw_parts(vaddr as *const u8, len)
    };
    if crate::net::wasm_net_submit_rx(data) {
        0
    } else {
        u64::MAX
    }
}

pub(super) fn syscall_net_poll_tx(vaddr: u64, max_len: u64) -> u64 {
    let max = max_len as usize;
    if max == 0 || max > 2048 || vaddr < 0x200000 {
        return u64::MAX;
    }
    let buf = unsafe {
        core::slice::from_raw_parts_mut(vaddr as *mut u8, max)
    };
    match crate::net::wasm_net_poll_tx(buf) {
        Some(len) => {
            static TX_POP_LOG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            let c = TX_POP_LOG.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if c < 5 {
                crate::serial_str!("[NET-POP] ");
                crate::drivers::serial::write_dec(len as u32);
                crate::serial_strln!("B popped from TX ring");
            }
            len as u64
        }
        None => 0,
    }
}

pub(super) fn syscall_dma_sync_read(phys_addr: u64, dest_and_len: u64) -> u64 {
    if phys_addr == 0 { return u64::MAX; }

    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let src_virt = hhdm + phys_addr as usize;

    let len = ((dest_and_len >> 32) & 0xFFFF) as usize;

    if len == 0 {
        // Mode 2: read u64 directly — flush cache line first to see DMA writes
        unsafe {
            core::arch::asm!("clflush [{}]", in(reg) src_virt, options(nostack));
            core::arch::asm!("mfence", options(nostack));
        }
        let val = unsafe { core::ptr::read_volatile(src_virt as *const u64) };
        return val;
    }

    // Mode 1: bulk copy
    let dest_vaddr = (dest_and_len & 0xFFFFFFFF) as usize;
    if len > 4096 || dest_vaddr < 0x200000 {
        return u64::MAX;
    }

    let src = src_virt as *const u8;
    let dst = dest_vaddr as *mut u8;
    unsafe {
        let mut addr = src_virt;
        while addr < src_virt + len {
            core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack));
            addr += 64;
        }
        core::arch::asm!("mfence", options(nostack));

        for i in 0..len {
            let byte = core::ptr::read_volatile(src.add(i));
            core::ptr::write_volatile(dst.add(i), byte);
        }
    }

    len as u64
}

pub(super) fn syscall_net_dma_rx(ring_and_idx: u64, buf_and_size: u64) -> u64 {
    let ring_phys = ring_and_idx & 0x0000_FFFF_FFFF_FFFF;
    let desc_idx = ((ring_and_idx >> 48) & 0xFFFF) as usize;
    let buf_phys = buf_and_size & 0x0000_FFFF_FFFF_FFFF;
    let buf_size = ((buf_and_size >> 48) & 0xFFFF) as usize;

    if ring_phys == 0 || buf_phys == 0 || buf_size == 0 || desc_idx > 7 {
        return u64::MAX;
    }

    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);

    let desc_phys = ring_phys + (desc_idx as u64 * 16);
    let desc_virt = hhdm + desc_phys as usize;

    unsafe {
        core::arch::asm!("clflush [{}]", in(reg) desc_virt, options(nostack));
        core::arch::asm!("mfence", options(nostack));
    }

    let len_status = unsafe { core::ptr::read_volatile((desc_virt + 8) as *const u64) };
    let pkt_len = (len_status & 0xFFFF) as usize;

    if pkt_len == 0 || pkt_len > 2048 {
        return 0;
    }

    let pkt_phys = buf_phys + (desc_idx as u64 * buf_size as u64);
    let pkt_virt = hhdm + pkt_phys as usize;

    unsafe {
        let mut addr = pkt_virt;
        let end = pkt_virt + pkt_len;
        while addr < end {
            core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack));
            addr += 64;
        }
        core::arch::asm!("mfence", options(nostack));
    }

    let mut pkt_buf = [0u8; 2048];
    unsafe {
        let src = pkt_virt as *const u8;
        for i in 0..pkt_len {
            pkt_buf[i] = core::ptr::read_volatile(src.add(i));
        }
    }

    if crate::net::wasm_net_submit_rx(&pkt_buf[..pkt_len]) {
        pkt_len as u64
    } else {
        0
    }
}

pub(super) fn syscall_dma_sync_write(phys_addr: u64, src_and_len: u64) -> u64 {
    let src_vaddr = (src_and_len & 0xFFFFFFFF) as usize;
    let len = ((src_and_len >> 32) & 0xFFFF) as usize;

    if len == 0 || len > 4096 || phys_addr == 0 || src_vaddr < 0x200000 {
        return u64::MAX;
    }

    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let dst_virt = hhdm + phys_addr as usize;
    let src = src_vaddr as *const u8;
    let dst = dst_virt as *mut u8;

    unsafe {
        for i in 0..len {
            let byte = core::ptr::read_volatile(src.add(i));
            core::ptr::write_volatile(dst.add(i), byte);
        }
        let mut addr = dst_virt;
        while addr < dst_virt + len {
            core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack));
            addr += 64;
        }
        core::arch::asm!("mfence", options(nostack));
    }

    len as u64
}

pub(super) fn syscall_net_metrics(metric_id: u64, _reserved: u64) -> u64 {
    match metric_id {
        0 => {
            // Network: has_ip(1) | ip_a(8) | ip_b(8) | ip_c(8) | ip_d(8)
            let has_ip = if crate::net::has_ip() { 1u64 } else { 0u64 };
            let guard = crate::net::NET_STATE.lock();
            if let Some(ref state) = *guard {
                let addrs = state.iface.ip_addrs();
                if let Some(cidr) = addrs.first() {
                    if let smoltcp::wire::IpAddress::Ipv4(v4) = cidr.address() {
                        let o = v4.octets();
                        drop(guard);
                        return has_ip
                            | ((o[0] as u64) << 8)
                            | ((o[1] as u64) << 16)
                            | ((o[2] as u64) << 24)
                            | ((o[3] as u64) << 32);
                    }
                }
            }
            drop(guard);
            has_ip
        }
        1 => {
            // Firewall: allows(32) | drops(32)
            let allows = crate::net::firewall::ALLOWS.load(core::sync::atomic::Ordering::Relaxed) as u64;
            let drops = crate::net::firewall::DROPS.load(core::sync::atomic::Ordering::Relaxed) as u64;
            allows | (drops << 32)
        }
        2 => crate::timer::uptime_ms(),
        3 => crate::net::firewall::SUSPICIOUS.count.load(core::sync::atomic::Ordering::Relaxed) as u64,
        4 => {
            // Anomaly detection stats: blocked_ips(16) | total_syn_attempts(16)
            let (blocked, attempts) = crate::net::firewall::anomaly_stats();
            (blocked as u64) | ((attempts as u64) << 16)
        }
        _ => u64::MAX,
    }
}
