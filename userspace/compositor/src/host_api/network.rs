//! Network host functions for WASM apps
//! HTTP requests via gemini proxy, WebSocket connections.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use wasmi::*;
use super::HostState;

pub fn register(linker: &mut Linker<HostState>) {
    // folk_http_get(url_ptr, url_len, buf_ptr, max_len) -> i32
    // Makes an HTTP GET request. Tries kernel TLS first, falls back to proxy.
    // Returns bytes written to buf, or -1 on error.
    let _ = linker.func_wrap("env", "folk_http_get",
        |mut caller: Caller<HostState>, url_ptr: i32, url_len: i32, buf_ptr: i32, max_len: i32| -> i32 {
            if url_len <= 0 || url_len > 512 || max_len <= 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut url_buf = alloc::vec![0u8; url_len as usize];
            if mem.read(&caller, url_ptr as usize, &mut url_buf).is_err() { return -1; }
            let url = match alloc::str::from_utf8(&url_buf) {
                Ok(s) => s,
                Err(_) => return -1,
            };

            // Try 1: Direct kernel HTTPS fetch (no proxy dependency)
            let mut direct_buf = alloc::vec![0u8; max_len as usize];
            let direct_bytes = libfolk::sys::http_fetch(url, &mut direct_buf);
            if direct_bytes > 0 {
                let copy_len = direct_bytes.min(max_len as usize);
                if mem.write(&mut caller, buf_ptr as usize, &direct_buf[..copy_len]).is_ok() {
                    return copy_len as i32;
                }
            }

            // Try 2: Fallback to serial proxy (handles sites that fail direct TLS)
            let prompt = alloc::format!("__HTTP_GET__{}", url);
            let gemini_buf_size = 8192usize;
            let mut response = alloc::vec![0u8; gemini_buf_size];
            let bytes = libfolk::sys::ask_gemini(&prompt, &mut response);
            if bytes == 0 { return -1; }
            let copy_len = bytes.min(max_len as usize).min(gemini_buf_size);
            if mem.write(&mut caller, buf_ptr as usize, &response[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // folk_http_get_large(url_ptr, url_len, buf_ptr, max_len) -> i32
    // Same as folk_http_get but uses a larger buffer (up to 256KB).
    // Returns bytes loaded, or -1 on error.
    let _ = linker.func_wrap("env", "folk_http_get_large",
        |mut caller: Caller<HostState>, url_ptr: i32, url_len: i32, buf_ptr: i32, max_len: i32| -> i32 {
            if url_len <= 0 || url_len > 512 || max_len <= 0 || max_len > 262144 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            let mut url_buf = alloc::vec![0u8; url_len as usize];
            if mem.read(&caller, url_ptr as usize, &mut url_buf).is_err() { return -1; }
            let url = match alloc::str::from_utf8(&url_buf) {
                Ok(s) => String::from(s),
                Err(_) => return -1,
            };

            // Route via proxy with __HTTP_GET__ prefix (proxy now returns up to 8KB)
            // For truly large files, we'd need direct kernel TCP — for now, use proxy
            let full_prompt = alloc::format!("__HTTP_GET__{}", url);
            let gemini_buf_size = (max_len as usize).max(8192);
            let mut response = alloc::vec![0u8; gemini_buf_size];
            let bytes = libfolk::sys::ask_gemini(&full_prompt, &mut response);
            if bytes == 0 { return -1; }
            let copy_len = bytes.min(max_len as usize).min(gemini_buf_size);
            if mem.write(&mut caller, buf_ptr as usize, &response[..copy_len]).is_ok() {
                copy_len as i32
            } else { -1 }
        },
    );

    // Phase 14: WebSocket — Persistent streaming connections
    // folk_ws_connect(url_ptr, url_len) -> i32 (slot_id or -1)
    // URL format: "ws://host:port/path"
    let _ = linker.func_wrap("env", "folk_ws_connect",
        |mut caller: Caller<HostState>, url_ptr: i32, url_len: i32| -> i32 {
            if url_len <= 0 || url_len > 256 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut url_buf = alloc::vec![0u8; url_len as usize];
            if mem.read(&caller, url_ptr as usize, &mut url_buf).is_err() { return -1; }
            let url = match alloc::str::from_utf8(&url_buf) {
                Ok(s) => s,
                Err(_) => return -1,
            };

            // Parse "ws://host:port/path" or just "host:port/path"
            let stripped = url.strip_prefix("ws://").unwrap_or(
                url.strip_prefix("wss://").unwrap_or(url));
            let (host_port, path) = match stripped.find('/') {
                Some(i) => (&stripped[..i], &stripped[i..]),
                None => (stripped, "/"),
            };
            let (host, port) = match host_port.rfind(':') {
                Some(i) => {
                    let p = host_port[i+1..].parse::<u16>().unwrap_or(80);
                    (&host_port[..i], p)
                }
                None => (host_port, 80),
            };

            // Resolve to IP — for local proxy, use 127.0.0.1
            // For production, would need DNS. Using loopback for now.
            let ip = if host == "localhost" || host == "127.0.0.1" {
                [127, 0, 0, 1]
            } else {
                // Try to parse as dotted quad
                let parts: alloc::vec::Vec<&str> = host.split('.').collect();
                if parts.len() == 4 {
                    [
                        parts[0].parse().unwrap_or(10),
                        parts[1].parse().unwrap_or(0),
                        parts[2].parse().unwrap_or(2),
                        parts[3].parse().unwrap_or(2),
                    ]
                } else {
                    [10, 0, 2, 2] // QEMU gateway default
                }
            };

            match libfolk::sys::ws_connect(ip, port, host, path) {
                Some(id) => id as i32,
                None => -1,
            }
        },
    );

    // folk_ws_send(socket_id, data_ptr, data_len) -> i32 (0=ok, -1=error)
    let _ = linker.func_wrap("env", "folk_ws_send",
        |mut caller: Caller<HostState>, socket_id: i32, data_ptr: i32, data_len: i32| -> i32 {
            if data_len <= 0 || data_len > 8192 || socket_id < 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut data = alloc::vec![0u8; data_len as usize];
            if mem.read(&caller, data_ptr as usize, &mut data).is_err() { return -1; }
            if libfolk::sys::ws_send(socket_id as u8, &data) { 0 } else { -1 }
        },
    );

    // folk_ws_poll_recv(socket_id, buf_ptr, max_len) -> i32
    // Returns: bytes read (>0), 0 (nothing yet), -1 (closed/error)
    let _ = linker.func_wrap("env", "folk_ws_poll_recv",
        |mut caller: Caller<HostState>, socket_id: i32, buf_ptr: i32, max_len: i32| -> i32 {
            if max_len <= 0 || socket_id < 0 { return -1; }
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };
            let mut buf = alloc::vec![0u8; max_len as usize];
            match libfolk::sys::ws_poll_recv(socket_id as u8, &mut buf) {
                None => -1, // Connection closed/error
                Some(0) => 0, // Nothing yet
                Some(n) => {
                    if mem.write(&mut caller, buf_ptr as usize, &buf[..n]).is_ok() {
                        n as i32
                    } else { -1 }
                }
            }
        },
    );
}
