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
