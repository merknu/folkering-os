//! Network syscalls: ICMP ping, DNS, HTTPS/HTTP fetch, GitHub helpers,
//! UDP send/recv, NTP query.

pub fn syscall_ping(ip_packed: u64) -> u64 {
    let a = (ip_packed & 0xFF) as u8;
    let b = ((ip_packed >> 8) & 0xFF) as u8;
    let c = ((ip_packed >> 16) & 0xFF) as u8;
    let d = ((ip_packed >> 24) & 0xFF) as u8;
    crate::net::send_ping(a, b, c, d);
    0
}

pub fn syscall_dns_lookup(name_ptr: u64, name_len: u64) -> u64 {
    if name_ptr < 0x200000 || name_ptr >= 0x0000_8000_0000_0000 || name_len == 0 || name_len > 255 {
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

pub fn syscall_github_fetch(user_ptr: u64, user_len: u64, repo_ptr: u64, repo_len: u64) -> u64 {
    if user_ptr < 0x200000 || user_ptr >= 0x0000_8000_0000_0000
        || user_len == 0 || user_len > 64
        || repo_ptr < 0x200000 || repo_ptr >= 0x0000_8000_0000_0000
        || repo_len == 0 || repo_len > 64 {
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

pub fn syscall_github_clone(user_ptr: u64, user_len: u64, repo_ptr: u64, repo_len: u64) -> u64 {
    if user_ptr < 0x200000 || user_ptr >= 0x0000_8000_0000_0000
        || user_len == 0 || user_len > 64
        || repo_ptr < 0x200000 || repo_ptr >= 0x0000_8000_0000_0000
        || repo_len == 0 || repo_len > 64 {
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

pub fn syscall_https_test(ip_packed: u64) -> u64 {
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

pub fn syscall_http_fetch(url_ptr: u64, url_len: u64, buf_ptr: u64, buf_len: u64) -> u64 {
    if url_len == 0 || url_len > 512 || buf_len == 0 || buf_len > 65536 {
        return u64::MAX;
    }
    // User-pointer validation — both must point into the lower-half
    // userspace window. The syscall runs in ring 0 and would happily
    // read/write kernel memory via a hostile pointer.
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    if url_ptr < 0x200000 || url_ptr >= USERSPACE_TOP { return u64::MAX; }
    if buf_ptr < 0x200000 || buf_ptr >= USERSPACE_TOP { return u64::MAX; }
    let buf_end = match buf_ptr.checked_add(buf_len) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if buf_end > USERSPACE_TOP { return u64::MAX; }

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

/// HTTP POST: take a URL + form-encoded body, build a POST request,
/// fire it through the existing `https_get_raw` TLS pipeline, and copy
/// the response body back into a userspace buffer.
///
/// Args (6):
///   url_ptr/url_len   — UTF-8 URL (https:// is stripped if present)
///   body_ptr/body_len — request body bytes (typically `key=value&...`)
///   resp_ptr/resp_max — output buffer for the response body
/// Returns: number of body bytes copied, or `u64::MAX` on failure.
pub fn syscall_http_post(
    url_ptr: u64,
    url_len: u64,
    body_ptr: u64,
    body_len: u64,
    resp_ptr: u64,
    resp_max: u64,
) -> u64 {
    if url_len == 0 || url_len > 512 || resp_max == 0 || resp_max > 65536 {
        return u64::MAX;
    }
    if body_len > 4096 {
        return u64::MAX;
    }
    // All three pointers must land in userspace.
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    if url_ptr < 0x200000 || url_ptr >= USERSPACE_TOP { return u64::MAX; }
    if resp_ptr < 0x200000 || resp_ptr >= USERSPACE_TOP { return u64::MAX; }
    let resp_end = match resp_ptr.checked_add(resp_max) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if resp_end > USERSPACE_TOP { return u64::MAX; }
    if body_len > 0 {
        if body_ptr < 0x200000 || body_ptr >= USERSPACE_TOP { return u64::MAX; }
        let body_end = match body_ptr.checked_add(body_len) {
            Some(e) => e,
            None => return u64::MAX,
        };
        if body_end > USERSPACE_TOP { return u64::MAX; }
    }

    let url = unsafe {
        let slice = core::slice::from_raw_parts(url_ptr as *const u8, url_len as usize);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => return u64::MAX,
        }
    };
    let body: &[u8] = if body_len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(body_ptr as *const u8, body_len as usize) }
    };

    let stripped = url
        .strip_prefix("https://")
        .unwrap_or(url.strip_prefix("http://").unwrap_or(url));
    let (host, path) = match stripped.find('/') {
        Some(i) => (&stripped[..i], &stripped[i..]),
        None => (stripped, "/"),
    };

    crate::serial_str!("[HTTP_POST] ");
    crate::serial_str!(host);
    crate::serial_str!(path);
    crate::serial_str!(" body=");
    crate::drivers::serial::write_dec(body.len() as u32);
    crate::serial_str!("B\n");

    let ip_packed = crate::net::dns_lookup(host);
    if ip_packed == 0 || ip_packed == u64::MAX {
        crate::serial_strln!("[HTTP_POST] DNS failed");
        return u64::MAX;
    }
    let ip = [
        (ip_packed >> 24) as u8,
        (ip_packed >> 16) as u8,
        (ip_packed >> 8) as u8,
        ip_packed as u8,
    ];

    // Format Content-Length as ASCII so we can stream it directly into
    // the request without alloc::format!.
    let mut clen_buf = [0u8; 12];
    let clen_str = {
        let mut n = body.len();
        let mut tmp = [0u8; 12];
        let mut idx = 0;
        if n == 0 {
            tmp[0] = b'0';
            idx = 1;
        } else {
            while n > 0 {
                tmp[idx] = b'0' + (n % 10) as u8;
                n /= 10;
                idx += 1;
            }
        }
        for i in 0..idx { clen_buf[i] = tmp[idx - 1 - i]; }
        &clen_buf[..idx]
    };

    let mut request = alloc::vec::Vec::with_capacity(
        256 + host.len() + path.len() + body.len()
    );
    request.extend_from_slice(b"POST ");
    request.extend_from_slice(path.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(b"\r\nUser-Agent: FolkeringOS/1.0\r\nAccept: text/html,*/*\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: ");
    request.extend_from_slice(clen_str);
    request.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    request.extend_from_slice(body);

    let response = match crate::net::tls::https_get_raw(ip, host, &request) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[HTTP_POST] TLS failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    let body_start = response.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(0);

    let resp_body = &response[body_start..];
    let copy_len = resp_body.len().min(resp_max as usize);

    unsafe {
        let dst = core::slice::from_raw_parts_mut(resp_ptr as *mut u8, copy_len);
        dst.copy_from_slice(&resp_body[..copy_len]);
    }

    crate::serial_str!("[HTTP_POST] OK, ");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_str!(" bytes body\n");

    copy_len as u64
}

/// FBP request: talk to the host-side folkering-proxy over plain TCP.
///
/// The proxy runs on the host at `127.0.0.1:14711`. From inside the
/// QEMU guest, the host's loopback is reachable via the SLIRP gateway
/// at `10.0.2.2` (QEMU user-mode networking default).
///
/// Protocol (outbound):  `NAVIGATE <url>\n`
/// Protocol (inbound):   `[u32 length LE][FBP payload bytes]`
///
/// Args:
///   url_ptr/url_len   — URL to fetch
///   buf_ptr/buf_max   — output buffer for the raw FBP payload bytes
///                       (the 4-byte length prefix is stripped here)
///
/// Returns the number of FBP bytes written to `buf_ptr`, or `u64::MAX`
/// on any error.
pub fn syscall_fbp_request(
    url_ptr: u64,
    url_len: u64,
    buf_ptr: u64,
    buf_max: u64,
) -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;
    const MAX_REQUEST: usize = 1024;

    if url_len == 0 || url_len > 512 || buf_max == 0 || buf_max > 262144 {
        return u64::MAX;
    }
    // Validate pointers
    if url_ptr < 0x200000 || url_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }
    if buf_ptr < 0x200000 || buf_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }

    let url_bytes = unsafe {
        core::slice::from_raw_parts(url_ptr as *const u8, url_len as usize)
    };
    let url = match core::str::from_utf8(url_bytes) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    crate::serial_str!("[FBP_REQ] ");
    crate::serial_str!(url);
    crate::serial_str!("\n");

    // Build request line: "NAVIGATE <url>\n"
    let mut req: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(
        MAX_REQUEST.min(url.len() + 16),
    );
    req.extend_from_slice(b"NAVIGATE ");
    req.extend_from_slice(url.as_bytes());
    req.push(b'\n');

    // Reserve headroom for the 4-byte length prefix the proxy sends
    let max_total_response = (buf_max as usize).saturating_add(4).min(262144);

    // Phase 13.4: fast-fail KHunt so the overnight refactor loop can
    // start even if the Wikipedia extraction stalls. 60_000 tsc_ms is
    // ~20-40 s of wall clock on a 3-5 GHz host.
    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        &req,
        max_total_response,
        60_000,
    ) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[FBP_REQ] TCP failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    if response.len() < 4 {
        crate::serial_strln!("[FBP_REQ] short response (no length prefix)");
        return u64::MAX;
    }
    let payload_len = u32::from_le_bytes([
        response[0], response[1], response[2], response[3],
    ]) as usize;

    if payload_len == 0 {
        crate::serial_strln!("[FBP_REQ] proxy returned zero-length (error)");
        return u64::MAX;
    }

    let available = response.len() - 4;
    let copy_len = payload_len.min(available).min(buf_max as usize);

    unsafe {
        let dst = core::slice::from_raw_parts_mut(buf_ptr as *mut u8, copy_len);
        dst.copy_from_slice(&response[4..4 + copy_len]);
    }

    crate::serial_str!("[FBP_REQ] OK, ");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_str!(" bytes\n");

    copy_len as u64
}

/// FBP interact: send an INTERACTION_EVENT to the host-side proxy,
/// then read a fresh DOM_STATE_UPDATE back.
///
/// Because each TCP connection = one Chromium session on the proxy
/// side, this syscall MUST batch the navigate + interact into a
/// single connection so the click lands on the right tab:
///
/// 1. Open TCP to 10.0.2.2:14711
/// 2. Send `NAVIGATE <url>\n`
/// 3. Read and discard the first `[u32 length][FBP bytes]` (we
///    already have that DOM on the client — we just needed to
///    establish the session).
/// 4. Send a 12-byte FBP INTERACTION_EVENT frame:
///    `[0x02][action][pad:2][node_id:u32 LE][data_len=0:u32 LE]`
/// 5. Read the new `[u32 length][FBP bytes]` response.
/// 6. Copy the FBP payload (length prefix stripped) into `buf_ptr`
///    and return the byte count written.
///
/// Args:
///   url_ptr/url_len   — current page URL (kept so the proxy can
///                       resync if the session tab got reopened)
///   buf_ptr/buf_max   — destination for the post-interaction FBP
///   action_and_node   — low 8 bits = action (ACTION_CLICK = 0x01),
///                       high 32 bits = node_id (u32 LE)
pub fn syscall_fbp_interact(
    url_ptr: u64,
    url_len: u64,
    buf_ptr: u64,
    buf_max: u64,
    action_and_node: u64,
) -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;

    if url_len == 0 || url_len > 512 || buf_max == 0 || buf_max > 262144 {
        return u64::MAX;
    }
    // Pointer sanity — mirror `syscall_fbp_request`: must point into
    // the userspace range (above 2 MiB) and below the kernel half
    // (below 0x0000_8000_0000_0000). Blocks null-ptr kernel panics
    // and accidental writes into the kernel mapping.
    if url_ptr < 0x200000 || url_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }
    if buf_ptr < 0x200000 || buf_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }

    let action = (action_and_node & 0xFF) as u8;
    let node_id = ((action_and_node >> 8) & 0xFFFF_FFFF) as u32;

    let url_bytes = unsafe {
        core::slice::from_raw_parts(url_ptr as *const u8, url_len as usize)
    };
    let url = match core::str::from_utf8(url_bytes) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    crate::serial_str!("[FBP_INTERACT] url=");
    crate::serial_str!(url);
    crate::serial_str!(" action=");
    crate::drivers::serial::write_dec(action as u32);
    crate::serial_str!(" node=");
    crate::drivers::serial::write_dec(node_id);
    crate::serial_str!("\n");

    // Build the combined request stream: NAVIGATE first so the proxy
    // spins up the tab, then the binary INTERACTION_EVENT frame right
    // after. We send them as one contiguous buffer so tcp_plain only
    // needs one round-trip-capable send.
    let mut req: alloc::vec::Vec<u8> =
        alloc::vec::Vec::with_capacity(url.len() + 16 + 12);
    req.extend_from_slice(b"NAVIGATE ");
    req.extend_from_slice(url.as_bytes());
    req.push(b'\n');
    // 12-byte FBP INTERACTION_EVENT header (data_len=0 for ACTION_CLICK)
    req.push(0x02); // MSG_INTERACTION_EVENT
    req.push(action);
    req.push(0); // pad
    req.push(0); // pad
    req.extend_from_slice(&node_id.to_le_bytes());
    req.extend_from_slice(&0u32.to_le_bytes()); // data_len = 0

    // The proxy sends TWO responses back (one per request message):
    //   (a) [u32 len][FBP N1 bytes]  — response to NAVIGATE
    //   (b) [u32 len][FBP N2 bytes]  — response to INTERACTION_EVENT
    //
    // We care about (b). Reserve enough headroom to hold both.
    let max_total = ((buf_max as usize) * 2 + 16).min(524288);

    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        &req,
        max_total,
        120_000,
    ) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[FBP_INTERACT] TCP failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    if response.len() < 4 {
        crate::serial_strln!("[FBP_INTERACT] short first frame");
        return u64::MAX;
    }
    let first_len = u32::from_le_bytes([
        response[0], response[1], response[2], response[3],
    ]) as usize;

    // Skip the first frame (NAVIGATE response): 4 byte header + N bytes
    let second_start = 4usize.saturating_add(first_len);
    if second_start + 4 > response.len() {
        crate::serial_strln!("[FBP_INTERACT] missing second frame header");
        return u64::MAX;
    }

    let second_len = u32::from_le_bytes([
        response[second_start],
        response[second_start + 1],
        response[second_start + 2],
        response[second_start + 3],
    ]) as usize;
    if second_len == 0 {
        crate::serial_strln!("[FBP_INTERACT] interact returned zero-length");
        return u64::MAX;
    }

    let payload_start = second_start + 4;
    let available = response.len().saturating_sub(payload_start);
    let copy_len = second_len.min(available).min(buf_max as usize);

    unsafe {
        let dst = core::slice::from_raw_parts_mut(buf_ptr as *mut u8, copy_len);
        dst.copy_from_slice(&response[payload_start..payload_start + copy_len]);
    }

    crate::serial_str!("[FBP_INTERACT] OK, ");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_str!(" bytes\n");

    copy_len as u64
}

/// Phase 11 — Draug source-patch syscall.
///
/// Ships a Rust source file from the bare-metal OS to the host-side
/// folkering-proxy, which writes it into the `draug-sandbox` crate
/// and runs `cargo check` to validate it. The same TCP connection
/// that serves Wikipedia articles now also carries outgoing `.rs`
/// patches.
///
/// Args:
///   filename_ptr/filename_len — sandbox filename (must start with
///     `draug_` and end in `.rs`; proxy enforces the full allowlist)
///   content_ptr/content_len   — raw Rust source bytes
///   result_ptr/result_max     — output buffer for cargo's stderr
///
/// Wire protocol sent to the proxy:
///   `PATCH <filename>\n<byte_len>\n<content bytes>`
///
/// Reply frame: `[u32 status LE][u32 output_len LE][output bytes]`
///
/// Returns `(status << 32) | output_bytes_written`, or `u64::MAX`
/// on TCP/transport failure. Status codes match `PATCH_STATUS_*`
/// in the proxy's patch.rs (0 = OK, 1 = BUILD_FAILED, ...).
pub fn syscall_fbp_patch(
    filename_ptr: u64,
    content_ptr: u64,
    result_ptr: u64,
    packed_lens: u64,
) -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;

    // Unpack: 21 bits each for filename_len / content_len / result_max.
    // See `libfolk::sys::fbp_patch` for the matching pack step.
    let filename_len = (packed_lens & 0x1F_FFFF) as usize;
    let content_len = ((packed_lens >> 21) & 0x1F_FFFF) as usize;
    let result_max = ((packed_lens >> 42) & 0x1F_FFFF) as usize;

    if filename_len == 0 || filename_len > 64
        || content_len == 0 || content_len > 65_536
        || result_max == 0 || result_max > 262_144
    {
        crate::serial_str!("[FBP_PATCH] bad lens: fn=");
        crate::drivers::serial::write_dec(filename_len as u32);
        crate::serial_str!(" ct=");
        crate::drivers::serial::write_dec(content_len as u32);
        crate::serial_str!(" rm=");
        crate::drivers::serial::write_dec(result_max as u32);
        crate::serial_strln!("");
        return u64::MAX;
    }
    // Validate pointers
    if filename_ptr < 0x200000 || filename_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }
    if content_ptr < 0x200000 || content_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }
    if result_ptr < 0x200000 || result_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }

    let filename_bytes = unsafe {
        core::slice::from_raw_parts(filename_ptr as *const u8, filename_len as usize)
    };
    let filename = match core::str::from_utf8(filename_bytes) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };
    let content_bytes = unsafe {
        core::slice::from_raw_parts(content_ptr as *const u8, content_len as usize)
    };

    crate::serial_str!("[FBP_PATCH] ");
    crate::serial_str!(filename);
    crate::serial_str!(" (");
    crate::drivers::serial::write_dec(content_len as u32);
    crate::serial_strln!(" bytes)");

    // Build the outbound protocol frame:
    //   PATCH <filename>\n<content_len>\n<content>
    let mut req: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(
        content_len as usize + filename.len() + 32,
    );
    req.extend_from_slice(b"PATCH ");
    req.extend_from_slice(filename.as_bytes());
    req.push(b'\n');

    // Decimal encode content_len without pulling in fmt.
    let mut tmp = [0u8; 12];
    let mut n = content_len as usize;
    let mut idx = 0;
    if n == 0 {
        tmp[0] = b'0';
        idx = 1;
    } else {
        while n > 0 {
            tmp[idx] = b'0' + (n % 10) as u8;
            n /= 10;
            idx += 1;
        }
    }
    // Reverse the digit buffer.
    for i in 0..idx / 2 {
        tmp.swap(i, idx - 1 - i);
    }
    req.extend_from_slice(&tmp[..idx]);
    req.push(b'\n');
    req.extend_from_slice(content_bytes);

    // Reserve room for the 8-byte reply header + user buffer.
    let max_total = (result_max as usize).saturating_add(8).min(262_144);

    // Reduced timeout: cargo test typically 0.5-2s. 120K tsc_ms ≈ 40-80s
    // wall — prevents 8-minute compositor freeze on timeout.
    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        &req,
        max_total,
        120_000,
    ) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[FBP_PATCH] TCP failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    if response.len() < 8 {
        crate::serial_strln!("[FBP_PATCH] short reply header");
        return u64::MAX;
    }
    let status = u32::from_le_bytes([
        response[0], response[1], response[2], response[3],
    ]);
    let output_len = u32::from_le_bytes([
        response[4], response[5], response[6], response[7],
    ]) as usize;

    let available = response.len() - 8;
    let copy_len = output_len.min(available).min(result_max as usize);
    unsafe {
        let dst = core::slice::from_raw_parts_mut(result_ptr as *mut u8, copy_len);
        dst.copy_from_slice(&response[8..8 + copy_len]);
    }

    crate::serial_str!("[FBP_PATCH] status=");
    crate::drivers::serial::write_dec(status);
    crate::serial_str!(" output=");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_strln!(" bytes");

    ((status as u64) << 32) | (copy_len as u64)
}

/// Phase 12 — Generative LLM gateway syscall.
///
/// Ships a prompt to the host-side folkering-proxy's `LLM <model>`
/// command. The proxy POSTs to `http://127.0.0.1:11434/api/generate`
/// with `stream:false` and returns the raw response text to the OS.
///
/// Args (4, packed — see `sys_fbp_patch` for why we avoid 6-arg
/// syscalls):
///   model_ptr    — UTF-8 Ollama model name (e.g. `gemma4:31b-cloud`)
///   prompt_ptr   — UTF-8 prompt bytes
///   result_ptr   — output buffer for the response text
///   packed_lens  — (model_len | (prompt_len<<21) | (result_max<<42))
///
/// Returns `(status << 32) | response_bytes_written`, or `u64::MAX`
/// on TCP failure. Status codes match `LLM_STATUS_*` in the proxy
/// (0 = OK, 1 = HTTP_ERROR, 2 = NON_2XX, 3 = BAD_JSON, ...).
pub fn syscall_llm_generate(
    model_ptr: u64,
    prompt_ptr: u64,
    result_ptr: u64,
    packed_lens: u64,
) -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;

    let model_len = (packed_lens & 0x1F_FFFF) as usize;
    let prompt_len = ((packed_lens >> 21) & 0x1F_FFFF) as usize;
    let result_max = ((packed_lens >> 42) & 0x1F_FFFF) as usize;

    if model_len == 0 || model_len > 64
        || prompt_len == 0 || prompt_len > 32_768
        || result_max == 0 || result_max > 262_144
    {
        crate::serial_str!("[LLM] bad lens: m=");
        crate::drivers::serial::write_dec(model_len as u32);
        crate::serial_str!(" p=");
        crate::drivers::serial::write_dec(prompt_len as u32);
        crate::serial_str!(" r=");
        crate::drivers::serial::write_dec(result_max as u32);
        crate::serial_strln!("");
        return u64::MAX;
    }
    // Validate pointers
    if model_ptr < 0x200000 || model_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }
    if prompt_ptr < 0x200000 || prompt_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }
    if result_ptr < 0x200000 || result_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }

    let model_bytes = unsafe {
        core::slice::from_raw_parts(model_ptr as *const u8, model_len)
    };
    let model = match core::str::from_utf8(model_bytes) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };
    let prompt_bytes = unsafe {
        core::slice::from_raw_parts(prompt_ptr as *const u8, prompt_len)
    };

    crate::serial_str!("[LLM] model=");
    crate::serial_str!(model);
    crate::serial_str!(" prompt=");
    crate::drivers::serial::write_dec(prompt_len as u32);
    crate::serial_strln!(" bytes");

    // Build the wire frame:  LLM <model>\n<byte_len>\n<prompt>
    let mut req: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(
        prompt_len + model_len + 32,
    );
    req.extend_from_slice(b"LLM ");
    req.extend_from_slice(model.as_bytes());
    req.push(b'\n');

    // Decimal-encode prompt_len without pulling in fmt
    let mut tmp = [0u8; 12];
    let mut n = prompt_len;
    let mut idx = 0;
    if n == 0 {
        tmp[0] = b'0';
        idx = 1;
    } else {
        while n > 0 {
            tmp[idx] = b'0' + (n % 10) as u8;
            n /= 10;
            idx += 1;
        }
    }
    for i in 0..idx / 2 {
        tmp.swap(i, idx - 1 - i);
    }
    req.extend_from_slice(&tmp[..idx]);
    req.push(b'\n');
    req.extend_from_slice(prompt_bytes);

    let max_total = (result_max.saturating_add(8)).min(262_144);

    // Stability Fix 9: reduced timeout (120_000 tsc_ms ≈ 40-80s wall)
    // instead of default 900_000 (5-8 min). Enough for cold-start but
    // prevents 5+ minute UI freeze on failed calls.
    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        &req,
        max_total,
        120_000,
    ) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[LLM] TCP failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    if response.len() < 8 {
        crate::serial_strln!("[LLM] short reply header");
        return u64::MAX;
    }
    let status = u32::from_le_bytes([
        response[0], response[1], response[2], response[3],
    ]);
    let output_len = u32::from_le_bytes([
        response[4], response[5], response[6], response[7],
    ]) as usize;

    let available = response.len() - 8;
    let copy_len = output_len.min(available).min(result_max);
    unsafe {
        let dst = core::slice::from_raw_parts_mut(result_ptr as *mut u8, copy_len);
        dst.copy_from_slice(&response[8..8 + copy_len]);
    }

    crate::serial_str!("[LLM] status=");
    crate::drivers::serial::write_dec(status);
    crate::serial_str!(" output=");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_strln!(" bytes");

    ((status as u64) << 32) | (copy_len as u64)
}

/// Kernel-internal helper: do the GRAPH_CALLERS proxy round-trip
/// without any pointer validation. Used by both the userspace
/// syscall path (which validates first) and the in-kernel
/// `tcp_shell` command (which passes kernel-space buffers, where
/// the userspace ptr-range check would always reject).
///
/// Returns Some((status, bytes_written)) on a successful proxy
/// round-trip (any status code), None on TCP failure.
pub fn graph_callers_inner(name: &str, result: &mut [u8]) -> Option<(u32, usize)> {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;

    crate::serial_str!("[GRAPH] callers of ");
    crate::serial_strln!(name);

    // Wire frame: GRAPH_CALLERS <name>\n
    let mut req: alloc::vec::Vec<u8> =
        alloc::vec::Vec::with_capacity(name.len() + 16);
    req.extend_from_slice(b"GRAPH_CALLERS ");
    req.extend_from_slice(name.as_bytes());
    req.push(b'\n');

    let max_total = (result.len().saturating_add(8)).min(65_544);

    // 5s timeout — graph lookup is microseconds; anything longer
    // means TCP/proxy trouble, not a slow query.
    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        &req,
        max_total,
        15_000, // ~5s wall
    ) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[GRAPH] TCP failed: ");
            crate::serial_strln!(e);
            return None;
        }
    };

    if response.len() < 8 {
        crate::serial_strln!("[GRAPH] short reply header");
        return None;
    }
    let status = u32::from_le_bytes([
        response[0], response[1], response[2], response[3],
    ]);
    let output_len = u32::from_le_bytes([
        response[4], response[5], response[6], response[7],
    ]) as usize;

    let available = response.len() - 8;
    let copy_len = output_len.min(available).min(result.len());
    result[..copy_len].copy_from_slice(&response[8..8 + copy_len]);

    crate::serial_str!("[GRAPH] status=");
    crate::drivers::serial::write_dec(status);
    crate::serial_str!(" output=");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_strln!(" bytes");

    Some((status, copy_len))
}

/// Folkering CodeGraph — query callers of a fn name via the proxy's
/// `GRAPH_CALLERS <name>\n` command. Userspace syscall entry that
/// validates pointers (must be in user-space range) before calling
/// [`graph_callers_inner`].
///
/// Returns `(status << 32) | output_bytes_written`, or `u64::MAX`
/// on validation failure / TCP failure. Status codes:
///   0 = OK, 1 = NOT_FOUND, 2 = NOT_LOADED.
///
/// Packed-lengths ABI matches `syscall_llm_generate`:
///   arg1 = name_ptr, arg2 = result_ptr, arg3 = unused,
///   arg4 = (name_len & 0x1F_FFFF) | ((result_max & 0x1F_FFFF) << 21)
pub fn syscall_graph_callers(
    name_ptr: u64,
    result_ptr: u64,
    _arg3: u64,
    packed_lens: u64,
) -> u64 {
    let name_len = (packed_lens & 0x1F_FFFF) as usize;
    let result_max = ((packed_lens >> 21) & 0x1F_FFFF) as usize;

    if name_len == 0 || name_len > 256
        || result_max == 0 || result_max > 65_536
    {
        crate::serial_str!("[GRAPH] bad lens: n=");
        crate::drivers::serial::write_dec(name_len as u32);
        crate::serial_str!(" r=");
        crate::drivers::serial::write_dec(result_max as u32);
        crate::serial_strln!("");
        return u64::MAX;
    }
    // User-space pointer range only (kernel-side callers should use
    // `graph_callers_inner` directly).
    if name_ptr < 0x200000 || name_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }
    if result_ptr < 0x200000 || result_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }

    let name_bytes = unsafe {
        core::slice::from_raw_parts(name_ptr as *const u8, name_len)
    };
    let name = match core::str::from_utf8(name_bytes) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };
    let result = unsafe {
        core::slice::from_raw_parts_mut(result_ptr as *mut u8, result_max)
    };

    match graph_callers_inner(name, result) {
        Some((status, copy_len)) => ((status as u64) << 32) | (copy_len as u64),
        None => u64::MAX,
    }
}

/// Stability Fix 7 — Proxy health check.
///
/// Sends `PING\n` to the proxy. Worst-case wall time = 15s connect
/// timeout (in `tcp_plain::tcp_request_with_timeout`'s `may_send`
/// loop) + the 5_000 `tsc_ms` read budget passed below (≈2s on a
/// well-calibrated TSC). If the proxy is reachable but slow, return
/// is bounded by the read budget; if it's wedged at the TCP level,
/// return is bounded by the connect cap.
/// Returns 1 if proxy responds with PONG, 0 otherwise.
/// Used before expensive LLM calls to fail fast when proxy is down.
pub fn syscall_proxy_ping() -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;

    // Issue #58 instrumentation: log every ping attempt + outcome so we
    // can see whether hibernation's wakeup path is itself broken under
    // the post-flood TCP wedge.
    crate::serial_strln!("[PROXY_PING] requesting");

    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        b"PING\n",
        64,
        5_000, // ~2s wall clock
    ) {
        Ok(data) => {
            crate::serial_str!("[PROXY_PING] tcp_request OK, ");
            crate::drivers::serial::write_dec(data.len() as u32);
            crate::serial_strln!(" bytes");
            data
        }
        Err(e) => {
            crate::serial_str!("[PROXY_PING] tcp_request failed: ");
            crate::serial_strln!(e);
            return 0;
        }
    };

    // Proxy returns "PONG\n"
    if response.len() >= 4 && &response[..4] == b"PONG" {
        crate::serial_strln!("[PROXY_PING] PONG → result=1 (waking Draug)");
        1
    } else {
        crate::serial_strln!("[PROXY_PING] no PONG in response → result=0");
        0
    }
}

/// Issue #55 — query the proxy for the most recent cached verdict
/// for our source IP. Returns u64-packed `(status << 32) | output_len`
/// on cache hit, or `u64::MAX` on transport failure / cache miss.
///
/// Output bytes are written into `buf_ptr`. Caller must allocate
/// at least 16 KB. Cache miss is signalled by a server-side sentinel
/// (status = 0xDEADBEEF, output_len = 0) which we surface as
/// `u64::MAX` so the userspace branch is unambiguous.
pub fn syscall_proxy_last_verdict(buf_ptr: u64, buf_max: u64) -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;

    if buf_max == 0 || buf_max > 65_536 {
        return u64::MAX;
    }
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    if buf_ptr < 0x200000 || buf_ptr >= USERSPACE_TOP { return u64::MAX; }
    let buf_end = match buf_ptr.checked_add(buf_max) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if buf_end > USERSPACE_TOP { return u64::MAX; }

    crate::serial_strln!("[LAST_VERDICT] requesting from proxy");

    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        b"LAST_VERDICT\n",
        16 * 1024 + 8, // header + body
        5_000,
    ) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[LAST_VERDICT] tcp_request failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    if response.len() < 8 {
        crate::serial_strln!("[LAST_VERDICT] short reply");
        return u64::MAX;
    }
    let status = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
    let output_len = u32::from_le_bytes([response[4], response[5], response[6], response[7]]);

    if status == 0xDEADBEEF {
        crate::serial_strln!("[LAST_VERDICT] cache miss");
        return u64::MAX;
    }

    let body_len = (output_len as usize).min(response.len().saturating_sub(8));
    let copy_len = body_len.min(buf_max as usize);
    if copy_len > 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(
                response.as_ptr().add(8),
                buf_ptr as *mut u8,
                copy_len,
            );
        }
    }

    crate::serial_str!("[LAST_VERDICT] cache HIT status=");
    crate::drivers::serial::write_dec(status);
    crate::serial_str!(" output=");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_strln!(" bytes");

    ((status as u64) << 32) | (copy_len as u64)
}

/// PATCH_DEDUP — content-addressed verdict cache lookup.
///
/// Daemon computes SHA-256 of the source it's about to ship and
/// passes the hex string to this syscall. Kernel forwards to the
/// proxy as `PATCH_DEDUP <hex>\n`. Proxy responds with either:
///   * cached verdict bytes `[u32 status][u32 output_len][output]`, or
///   * miss sentinel `[u32 status=0xCACED15D][u32 output_len=0]`.
///
/// On hit: copies the output bytes into `buf_ptr` and returns the
/// usual packed `(status << 32) | output_len`.
/// On miss / transport failure / old proxy that doesn't know the
/// command: returns `u64::MAX` so the userspace path falls through
/// to the regular PATCH flow.
pub fn syscall_proxy_patch_dedup(
    hash_ptr: u64, hash_len: u64,
    buf_ptr: u64, buf_max: u64,
) -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;
    const MISS_SENTINEL: u32 = 0xCACE_D15D;

    // Pointer + length sanity. Hash is fixed at 64 hex chars; reject
    // anything else loudly so callers that mis-encode get u64::MAX
    // instead of an opaque "miss".
    if hash_len != 64 || buf_max == 0 || buf_max > 65_536 {
        return u64::MAX;
    }
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    if hash_ptr < 0x200000 || hash_ptr >= USERSPACE_TOP { return u64::MAX; }
    if buf_ptr < 0x200000 || buf_ptr >= USERSPACE_TOP { return u64::MAX; }
    let hash_end = match hash_ptr.checked_add(hash_len) {
        Some(e) => e,
        None => return u64::MAX,
    };
    let buf_end = match buf_ptr.checked_add(buf_max) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if hash_end > USERSPACE_TOP || buf_end > USERSPACE_TOP {
        return u64::MAX;
    }

    // Build `PATCH_DEDUP <hex>\n` on the kernel stack — small,
    // deterministic, no heap allocation needed for the request.
    let hash_bytes = unsafe {
        core::slice::from_raw_parts(hash_ptr as *const u8, 64)
    };
    // Validate the hex chars upfront so a malformed daemon-side
    // build can't smuggle garbage into the proxy's parser.
    for &b in hash_bytes {
        let ok = matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F');
        if !ok { return u64::MAX; }
    }

    let mut req = [0u8; 12 + 64 + 1];
    req[..12].copy_from_slice(b"PATCH_DEDUP ");
    req[12..76].copy_from_slice(hash_bytes);
    req[76] = b'\n';

    crate::serial_strln!("[PATCH_DEDUP] querying proxy");

    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        &req,
        16 * 1024 + 8, // header + body, matches LAST_VERDICT
        5_000,
    ) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[PATCH_DEDUP] tcp_request failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    if response.len() < 8 {
        // Old proxy that doesn't know PATCH_DEDUP returns a 4-byte
        // error frame. Treat as miss — caller falls back to PATCH.
        crate::serial_strln!("[PATCH_DEDUP] short reply (proxy lacks command?) → MISS");
        return u64::MAX;
    }
    let status = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
    let output_len = u32::from_le_bytes([response[4], response[5], response[6], response[7]]);

    if status == MISS_SENTINEL {
        crate::serial_strln!("[PATCH_DEDUP] cache miss");
        return u64::MAX;
    }

    let body_len = (output_len as usize).min(response.len().saturating_sub(8));
    let copy_len = body_len.min(buf_max as usize);
    if copy_len > 0 {
        unsafe {
            core::ptr::copy_nonoverlapping(
                response.as_ptr().add(8),
                buf_ptr as *mut u8,
                copy_len,
            );
        }
    }

    crate::serial_str!("[PATCH_DEDUP] cache HIT status=");
    crate::drivers::serial::write_dec(status);
    crate::serial_str!(" output=");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_strln!(" bytes");

    ((status as u64) << 32) | (copy_len as u64)
}

/// Issue #55 — application-level acknowledgement of a verdict.
///
/// PR #65 added the `LAST_VERDICT` recovery path: if the VM times
/// out before reading a PATCH/CARGO_CHECK reply, it can ask the
/// proxy "what did you last archive for me?". That recovery is
/// gated only by a 30-day TTL on the proxy side. Once the daemon
/// has actually persisted the verdict to Synapse, it should tell
/// the proxy to drop its cached entry — the daemon no longer needs
/// the safety net for that task. This is the "ACK_VERDICT" half of
/// the explicit-ack pattern from the Dora-rs analysis.
///
/// Wire shape:
///   VM → proxy: ACK_VERDICT\n
///   proxy → VM: OK\n  (always returns OK, even if cache was empty)
///
/// Cache is per-source-IP (one entry deep). The implicit ACK target
/// is whatever verdict the proxy last archived for our IP. No body
/// or task_id needed on the wire — if a future bug needs that, we'll
/// extend with a payload.
///
/// Returns 1 on successful ACK (proxy acknowledged or cache was
/// already empty), 0 on transport failure.
pub fn syscall_proxy_ack_verdict() -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;

    crate::serial_strln!("[ACK_VERDICT] sending to proxy");

    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        b"ACK_VERDICT\n",
        16, // expect "OK\n" or similar — small reply
        5_000,
    ) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[ACK_VERDICT] tcp_request failed: ");
            crate::serial_strln!(e);
            // Soft-fail: the cache will eventually GC via the 30-day
            // backstop. Logging the failure is enough — the daemon
            // doesn't need to retry, since by the time it's calling
            // this, the verdict is already persisted locally.
            return 0;
        }
    };

    let ok = response.starts_with(b"OK");
    if ok {
        crate::serial_strln!("[ACK_VERDICT] proxy ACK'd");
    } else {
        crate::serial_strln!("[ACK_VERDICT] unexpected reply (treated as no-op)");
    }
    if ok { 1 } else { 0 }
}

/// Issue #58 — UDP variant of proxy_ping.
///
/// Sends a 4-byte "PING" UDP datagram to the proxy and awaits "PONG".
/// Uses smoltcp's UDP socket type, which is a different code path
/// than `tcp_plain` — so this can succeed even when the TCP-side
/// state is wedged (post-flood scenario from #58).
///
/// Timeout is 1000 in `tsc_ms` units — `udp_send_recv` polls
/// `tls::tsc_ms()`, which is calibrated TSC ticks divided down to
/// milliseconds (≈1s wall-clock when the IQE TSC calibration
/// succeeded; on a fallback-to-3 GHz host the value can drift
/// proportionally to the real CPU frequency).
///
/// Returns 1 on PONG received, 0 otherwise.
pub fn syscall_proxy_ping_udp() -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;

    crate::serial_strln!("[PROXY_PING_UDP] requesting");

    let mut response = [0u8; 64];
    let n = crate::net::udp::udp_send_recv(
        PROXY_IP, PROXY_PORT,
        b"PING",
        &mut response,
        1000, // tsc_ms — see fn-doc above for the wall-clock caveat
    );

    if n == 0 {
        crate::serial_strln!("[PROXY_PING_UDP] no UDP response");
        return 0;
    }

    crate::serial_str!("[PROXY_PING_UDP] got ");
    crate::drivers::serial::write_dec(n as u32);
    crate::serial_strln!(" bytes");

    if n >= 4 && &response[..4] == b"PONG" {
        crate::serial_strln!("[PROXY_PING_UDP] PONG → result=1 (waking Draug)");
        1
    } else {
        crate::serial_strln!("[PROXY_PING_UDP] no PONG in response → result=0");
        0
    }
}

/// Phase 16 — WASM compilation.
///
/// Sends `WASM_COMPILE\n` to the proxy, which compiles the current
/// `draug_latest.rs` to `wasm32-unknown-unknown` and returns the
/// binary. The .wasm bytes are written to `buf_ptr`.
///
/// Returns `(status << 32) | wasm_bytes_written` or `u64::MAX` on failure.
pub fn syscall_wasm_compile(buf_ptr: u64, buf_max: u64) -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;

    if buf_max == 0 || buf_max > 262_144 {
        return u64::MAX;
    }
    // Pointer sanity — must point into the userspace window. Guards
    // against a user task handing in 0 or a kernel-half address that
    // `copy_from_slice` would otherwise write into blindly.
    if buf_ptr < 0x200000 || buf_ptr >= 0x0000_8000_0000_0000 {
        return u64::MAX;
    }

    crate::serial_strln!("[WASM_COMPILE] requesting wasm32 build from proxy");

    let req = b"WASM_COMPILE\n";
    let max_total = (buf_max as usize).saturating_add(8).min(262_144);

    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        req,
        max_total,
        120_000, // same reduced timeout as LLM/PATCH
    ) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[WASM_COMPILE] TCP failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    if response.len() < 8 {
        crate::serial_strln!("[WASM_COMPILE] short reply header");
        return u64::MAX;
    }

    let status = u32::from_le_bytes([
        response[0], response[1], response[2], response[3],
    ]);
    let payload_len = u32::from_le_bytes([
        response[4], response[5], response[6], response[7],
    ]) as usize;

    let available = response.len() - 8;
    let copy_len = payload_len.min(available).min(buf_max as usize);

    if copy_len > 0 {
        unsafe {
            let dst = core::slice::from_raw_parts_mut(buf_ptr as *mut u8, copy_len);
            dst.copy_from_slice(&response[8..8 + copy_len]);
        }
    }

    crate::serial_str!("[WASM_COMPILE] status=");
    crate::drivers::serial::write_dec(status);
    crate::serial_str!(" wasm=");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_strln!(" bytes");

    ((status as u64) << 32) | (copy_len as u64)
}

pub fn syscall_ntp_query(server_ip_packed: u64) -> u64 {
    let ip = [
        ((server_ip_packed >> 24) & 0xFF) as u8,
        ((server_ip_packed >> 16) & 0xFF) as u8,
        ((server_ip_packed >> 8) & 0xFF) as u8,
        (server_ip_packed & 0xFF) as u8,
    ];
    crate::net::ntp_query(ip)
}

pub fn syscall_udp_send(target_packed: u64, port: u64, data_ptr: u64, data_len: u64) -> u64 {
    if data_len == 0 || data_len > 1472 { return u64::MAX; }
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    if data_ptr < 0x200000 || data_ptr >= USERSPACE_TOP { return u64::MAX; }
    let end = match data_ptr.checked_add(data_len) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if end > USERSPACE_TOP { return u64::MAX; }
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

pub fn syscall_udp_send_recv(
    target_packed: u64, port: u64,
    data_ptr: u64, data_len: u64,
    resp_ptr: u64, resp_len_and_timeout: u64,
) -> u64 {
    let resp_len = (resp_len_and_timeout & 0xFFFF_FFFF) as usize;
    let timeout_ms = (resp_len_and_timeout >> 32) as u32;
    if data_len == 0 || data_len > 1472 || resp_len == 0 || resp_len > 4096 {
        return u64::MAX;
    }
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    if data_ptr < 0x200000 || data_ptr >= USERSPACE_TOP { return u64::MAX; }
    if resp_ptr < 0x200000 || resp_ptr >= USERSPACE_TOP { return u64::MAX; }
    let data_end = match data_ptr.checked_add(data_len) {
        Some(e) => e,
        None => return u64::MAX,
    };
    let resp_end = match resp_ptr.checked_add(resp_len as u64) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if data_end > USERSPACE_TOP || resp_end > USERSPACE_TOP { return u64::MAX; }
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

/// Phase 17 — autonomous-refactor validation syscall.
///
/// Ships a real OS source file (target_file_rel + new bytes) to the
/// host-side folkering-proxy's `CARGO_CHECK <target>` command. The
/// proxy overwrites the file in the live tree, runs `cargo check`,
/// restores the original, and replies with status + a stderr excerpt.
///
/// Sister of `syscall_fbp_patch` — same wire shape, same 4-arg
/// packed-lengths ABI, but `CARGO_CHECK` operates on real file paths
/// (e.g. `kernel/src/memory/physical.rs`) instead of the draug-sandbox
/// crate, so Draug can verify a refactor against real callers.
///
/// Args (4):
///   target_ptr   — UTF-8 repo-relative path
///   content_ptr  — UTF-8 candidate Rust source
///   result_ptr   — output buffer for the stderr excerpt
///   packed_lens  — (target_len | (content_len<<21) | (result_max<<42))
///
/// Returns `(status << 32) | output_bytes_written`, or `u64::MAX` on
/// transport failure. Status codes match `CC_STATUS_*` in the proxy
/// (0 = OK, 1 = BUILD_FAILED, 2 = BAD_PATH, 3 = IO_ERROR,
/// 4 = CHECK_TIMEOUT, 5 = TOO_LARGE, 6 = NOT_CONFIGURED).
pub fn syscall_cargo_check(
    target_ptr: u64,
    content_ptr: u64,
    result_ptr: u64,
    packed_lens: u64,
) -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;

    let target_len = (packed_lens & 0x1F_FFFF) as usize;
    let content_len = ((packed_lens >> 21) & 0x1F_FFFF) as usize;
    let result_max = ((packed_lens >> 42) & 0x1F_FFFF) as usize;

    // Proxy currently caps content at 128 KB; keep the kernel-side
    // ceiling slightly under so a too-large request fails locally.
    if target_len == 0 || target_len > 256
        || content_len == 0 || content_len > 131_072
        || result_max == 0 || result_max > 262_144
    {
        crate::serial_str!("[CARGO_CHECK] bad lens: tg=");
        crate::drivers::serial::write_dec(target_len as u32);
        crate::serial_str!(" ct=");
        crate::drivers::serial::write_dec(content_len as u32);
        crate::serial_str!(" rm=");
        crate::drivers::serial::write_dec(result_max as u32);
        crate::serial_strln!("");
        return u64::MAX;
    }
    if target_ptr < 0x200000 || target_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }
    if content_ptr < 0x200000 || content_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }
    if result_ptr < 0x200000 || result_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }

    let target_bytes = unsafe {
        core::slice::from_raw_parts(target_ptr as *const u8, target_len)
    };
    let target = match core::str::from_utf8(target_bytes) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };
    let content_bytes = unsafe {
        core::slice::from_raw_parts(content_ptr as *const u8, content_len)
    };

    crate::serial_str!("[CARGO_CHECK] ");
    crate::serial_str!(target);
    crate::serial_str!(" (");
    crate::drivers::serial::write_dec(content_len as u32);
    crate::serial_strln!(" bytes)");

    // Build outbound frame: CARGO_CHECK <target>\n<len>\n<bytes>
    let mut req: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(
        content_len + target.len() + 32,
    );
    req.extend_from_slice(b"CARGO_CHECK ");
    req.extend_from_slice(target.as_bytes());
    req.push(b'\n');

    // Decimal encode without dragging in fmt.
    let mut tmp = [0u8; 12];
    let mut n = content_len;
    let mut idx = 0;
    if n == 0 {
        tmp[0] = b'0';
        idx = 1;
    } else {
        while n > 0 {
            tmp[idx] = b'0' + (n % 10) as u8;
            n /= 10;
            idx += 1;
        }
    }
    for i in 0..idx / 2 {
        tmp.swap(i, idx - 1 - i);
    }
    req.extend_from_slice(&tmp[..idx]);
    req.push(b'\n');
    req.extend_from_slice(content_bytes);

    let max_total = result_max.saturating_add(8).min(262_144);

    // cargo check on a cold workspace is up to 90s on the proxy side
    // (CHECK_TIMEOUT_SECS). Give ourselves another margin for the
    // overwrite/restore round-trip — 180s tsc_ms ≈ 60-120s wall.
    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        &req,
        max_total,
        180_000,
    ) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[CARGO_CHECK] TCP failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    if response.len() < 8 {
        crate::serial_strln!("[CARGO_CHECK] short reply header");
        return u64::MAX;
    }
    let status = u32::from_le_bytes([
        response[0], response[1], response[2], response[3],
    ]);
    let output_len = u32::from_le_bytes([
        response[4], response[5], response[6], response[7],
    ]) as usize;

    let available = response.len() - 8;
    let copy_len = output_len.min(available).min(result_max);
    unsafe {
        let dst = core::slice::from_raw_parts_mut(result_ptr as *mut u8, copy_len);
        dst.copy_from_slice(&response[8..8 + copy_len]);
    }

    crate::serial_str!("[CARGO_CHECK] status=");
    crate::drivers::serial::write_dec(status);
    crate::serial_str!(" output=");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_strln!(" bytes");

    ((status as u64) << 32) | (copy_len as u64)
}

/// Phase 17 — fetch the bytes of a real OS source file from the host.
///
/// Sends `FETCH_SOURCE <target>\n` to the proxy and copies the reply
/// body into `result_ptr`. Lighter than `cargo_check` — there is no
/// outbound body, so we use a 3-arg packed-lengths shape (target_len
/// + result_max only).
///
/// Args (4):
///   target_ptr   — UTF-8 repo-relative path
///   result_ptr   — output buffer for the file bytes
///   _unused      — kept 0 for ABI symmetry with graph_callers
///   packed_lens  — (target_len | (result_max<<21))
///
/// Returns `(status << 32) | output_bytes_written`, or `u64::MAX` on
/// transport failure. Status codes match `FS_STATUS_*` in the proxy
/// (0 = OK, 1 = BAD_PATH, 2 = NOT_FOUND, 3 = IO_ERROR, 4 = TOO_LARGE,
/// 5 = NOT_CONFIGURED).
pub fn syscall_fetch_source(
    target_ptr: u64,
    result_ptr: u64,
    _unused: u64,
    packed_lens: u64,
) -> u64 {
    const PROXY_IP: [u8; 4] = [192, 168, 68, 150];
    const PROXY_PORT: u16 = 14711;

    let target_len = (packed_lens & 0x1F_FFFF) as usize;
    let result_max = ((packed_lens >> 21) & 0x1F_FFFF) as usize;

    if target_len == 0 || target_len > 256
        || result_max == 0 || result_max > 262_144
    {
        crate::serial_str!("[FETCH_SOURCE] bad lens: tg=");
        crate::drivers::serial::write_dec(target_len as u32);
        crate::serial_str!(" rm=");
        crate::drivers::serial::write_dec(result_max as u32);
        crate::serial_strln!("");
        return u64::MAX;
    }
    if target_ptr < 0x200000 || target_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }
    if result_ptr < 0x200000 || result_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }

    let target_bytes = unsafe {
        core::slice::from_raw_parts(target_ptr as *const u8, target_len)
    };
    let target = match core::str::from_utf8(target_bytes) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    crate::serial_str!("[FETCH_SOURCE] ");
    crate::serial_strln!(target);

    // Outbound: "FETCH_SOURCE <target>\n"
    let mut req: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(target.len() + 16);
    req.extend_from_slice(b"FETCH_SOURCE ");
    req.extend_from_slice(target.as_bytes());
    req.push(b'\n');

    let max_total = result_max.saturating_add(8).min(262_144);

    // 90s wall is plenty for a single fs::read.
    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        &req,
        max_total,
        90_000,
    ) {
        Ok(data) => data,
        Err(e) => {
            crate::serial_str!("[FETCH_SOURCE] TCP failed: ");
            crate::serial_strln!(e);
            return u64::MAX;
        }
    };

    if response.len() < 8 {
        crate::serial_strln!("[FETCH_SOURCE] short reply header");
        return u64::MAX;
    }
    let status = u32::from_le_bytes([
        response[0], response[1], response[2], response[3],
    ]);
    let output_len = u32::from_le_bytes([
        response[4], response[5], response[6], response[7],
    ]) as usize;

    let available = response.len() - 8;
    let copy_len = output_len.min(available).min(result_max);
    unsafe {
        let dst = core::slice::from_raw_parts_mut(result_ptr as *mut u8, copy_len);
        dst.copy_from_slice(&response[8..8 + copy_len]);
    }

    crate::serial_str!("[FETCH_SOURCE] status=");
    crate::drivers::serial::write_dec(status);
    crate::serial_str!(" output=");
    crate::drivers::serial::write_dec(copy_len as u32);
    crate::serial_strln!(" bytes");

    ((status as u64) << 32) | (copy_len as u64)
}
