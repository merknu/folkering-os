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

pub fn syscall_github_fetch(user_ptr: u64, user_len: u64, repo_ptr: u64, repo_len: u64) -> u64 {
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

pub fn syscall_github_clone(user_ptr: u64, user_len: u64, repo_ptr: u64, repo_len: u64) -> u64 {
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
    const PROXY_IP: [u8; 4] = [10, 0, 2, 2];
    const PROXY_PORT: u16 = 14711;
    const MAX_REQUEST: usize = 1024;

    if url_len == 0 || url_len > 512 || buf_max == 0 || buf_max > 262144 {
        return u64::MAX;
    }

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
    const PROXY_IP: [u8; 4] = [10, 0, 2, 2];
    const PROXY_PORT: u16 = 14711;

    if url_len == 0 || url_len > 512 || buf_max == 0 || buf_max > 262144 {
        return u64::MAX;
    }

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

    let response = match crate::net::tcp_plain::tcp_request(
        PROXY_IP,
        PROXY_PORT,
        &req,
        max_total,
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
    const PROXY_IP: [u8; 4] = [10, 0, 2, 2];
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

    let response = match crate::net::tcp_plain::tcp_request(
        PROXY_IP,
        PROXY_PORT,
        &req,
        max_total,
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
    const PROXY_IP: [u8; 4] = [10, 0, 2, 2];
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

/// Stability Fix 7 — Proxy health check.
///
/// Sends `PING\n` to the proxy with a 5_000 tsc_ms timeout (~2s).
/// Returns 1 if proxy responds with PONG, 0 otherwise.
/// Used before expensive LLM calls to fail fast when proxy is down.
pub fn syscall_proxy_ping() -> u64 {
    const PROXY_IP: [u8; 4] = [10, 0, 2, 2];
    const PROXY_PORT: u16 = 14711;

    let response = match crate::net::tcp_plain::tcp_request_with_timeout(
        PROXY_IP,
        PROXY_PORT,
        b"PING\n",
        64,
        5_000, // ~2s wall clock
    ) {
        Ok(data) => data,
        Err(_) => return 0,
    };

    // Proxy returns "PONG\n"
    if response.len() >= 4 && &response[..4] == b"PONG" { 1 } else { 0 }
}

/// Phase 16 — WASM compilation.
///
/// Sends `WASM_COMPILE\n` to the proxy, which compiles the current
/// `draug_latest.rs` to `wasm32-unknown-unknown` and returns the
/// binary. The .wasm bytes are written to `buf_ptr`.
///
/// Returns `(status << 32) | wasm_bytes_written` or `u64::MAX` on failure.
pub fn syscall_wasm_compile(buf_ptr: u64, buf_max: u64) -> u64 {
    const PROXY_IP: [u8; 4] = [10, 0, 2, 2];
    const PROXY_PORT: u16 = 14711;

    if buf_max == 0 || buf_max > 262_144 {
        return u64::MAX;
    }

    crate::serial_strln!("[WASM_COMPILE] requesting wasm32 build from proxy");

    let req = b"WASM_COMPILE\n";
    let max_total = (buf_max as usize).saturating_add(8).min(262_144);

    let response = match crate::net::tcp_plain::tcp_request(
        PROXY_IP,
        PROXY_PORT,
        req,
        max_total,
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
