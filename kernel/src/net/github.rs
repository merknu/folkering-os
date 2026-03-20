//! GitHub REST API client
//!
//! Provides `fetch_repo_info(user, repo)` to query GitHub's API.
//! Uses TLS 1.3 via embedded-tls for HTTPS.

extern crate alloc;

use alloc::vec;
use embedded_io::{Read, Write};

use super::json;
use super::tls;

/// GitHub API server IP (api.github.com)
/// Hardcoded to bypass DNS — resolve manually when DNS works:
///   dig api.github.com → 140.82.121.6 (may change)
const GITHUB_API_IP: [u8; 4] = [140, 82, 121, 6];

/// Maximum response size we'll read (64KB)
const MAX_RESPONSE: usize = 65536;

/// Result of a GitHub repo query
pub struct RepoInfo {
    /// HTTP status code
    pub status: u16,
    /// Response body (JSON or error)
    pub body: alloc::vec::Vec<u8>,
    /// Total bytes received
    pub total_bytes: usize,
}

/// Fetch repository information from GitHub API.
///
/// Calls: GET /repos/{user}/{repo}
/// Returns: RepoInfo with JSON body containing name, description, stars, etc.
pub fn fetch_repo_info(user: &str, repo: &str) -> Result<RepoInfo, &'static str> {
    crate::serial_str!("[GITHUB] Fetching ");
    crate::drivers::serial::write_str(user);
    crate::serial_str!("/");
    crate::drivers::serial::write_str(repo);
    crate::drivers::serial::write_newline();

    // Build the API path: /repos/{user}/{repo}
    let mut path_buf = [0u8; 128];
    let path_len = build_api_path(&mut path_buf, user, repo);
    let path = core::str::from_utf8(&path_buf[..path_len]).map_err(|_| "invalid path")?;

    // Build the full HTTP request with required GitHub headers
    let mut req_buf = [0u8; 512];
    let req_len = build_github_request(&mut req_buf, "api.github.com", path);

    // Perform HTTPS GET using our TLS stack
    let response = tls::https_get_raw(GITHUB_API_IP, "api.github.com", &req_buf[..req_len])?;

    // Parse HTTP status from response
    let status = parse_http_status(&response);

    // Find body (after \r\n\r\n)
    let body_start = find_body_start(&response).unwrap_or(response.len());
    let body = response[body_start..].to_vec();

    crate::serial_str!("[GITHUB] HTTP ");
    crate::drivers::serial::write_dec(status as u32);
    crate::serial_str!(", body=");
    crate::drivers::serial::write_dec(body.len() as u32);
    crate::serial_strln!(" bytes");

    Ok(RepoInfo {
        status,
        body,
        total_bytes: response.len(),
    })
}

/// Fetch latest release info for a repo.
/// Calls: GET /repos/{user}/{repo}/releases/latest
pub fn fetch_latest_release(user: &str, repo: &str) -> Result<RepoInfo, &'static str> {
    crate::serial_str!("[GITHUB] Fetching latest release: ");
    crate::drivers::serial::write_str(user);
    crate::serial_str!("/");
    crate::drivers::serial::write_str(repo);
    crate::drivers::serial::write_newline();

    // Build path: /repos/{user}/{repo}/releases/latest
    let mut path_buf = [0u8; 160];
    let mut pos = 0;
    let parts: &[&[u8]] = &[
        b"/repos/", user.as_bytes(), b"/", repo.as_bytes(), b"/releases/latest",
    ];
    for part in parts {
        let len = part.len().min(path_buf.len() - pos);
        path_buf[pos..pos + len].copy_from_slice(&part[..len]);
        pos += len;
    }
    let path = core::str::from_utf8(&path_buf[..pos]).map_err(|_| "invalid path")?;

    let mut req_buf = [0u8; 512];
    let req_len = build_github_request(&mut req_buf, "api.github.com", path);

    let response = tls::https_get_raw(GITHUB_API_IP, "api.github.com", &req_buf[..req_len])?;

    let status = parse_http_status(&response);
    let body_start = find_body_start(&response).unwrap_or(response.len());
    let body = response[body_start..].to_vec();

    Ok(RepoInfo {
        status,
        body,
        total_bytes: response.len(),
    })
}

/// Print parsed repo info to serial
pub fn print_repo_info(info: &RepoInfo) {
    if info.status != 200 {
        crate::serial_str!("[GITHUB] Error: HTTP ");
        crate::drivers::serial::write_dec(info.status as u32);
        crate::drivers::serial::write_newline();
        // Print first 200 bytes of body for error details
        let preview = info.body.len().min(200);
        for &b in &info.body[..preview] {
            if b >= 0x20 && b < 0x7F {
                crate::drivers::serial::write_byte(b);
            }
        }
        crate::drivers::serial::write_newline();
        return;
    }

    // Extract fields from JSON
    if let Some(name) = json::json_get_str(&info.body, "full_name") {
        crate::serial_str!("[GITHUB] Repo: ");
        crate::drivers::serial::write_str(name);
        crate::drivers::serial::write_newline();
    }

    if let Some(desc) = json::json_get_str(&info.body, "description") {
        crate::serial_str!("[GITHUB] Desc: ");
        crate::drivers::serial::write_str(desc);
        crate::drivers::serial::write_newline();
    }

    if let Some(lang) = json::json_get_str(&info.body, "language") {
        crate::serial_str!("[GITHUB] Lang: ");
        crate::drivers::serial::write_str(lang);
        crate::drivers::serial::write_newline();
    }

    if let Some(stars) = json::json_get_num(&info.body, "stargazers_count") {
        crate::serial_str!("[GITHUB] Stars: ");
        crate::drivers::serial::write_dec(stars as u32);
        crate::drivers::serial::write_newline();
    }

    if let Some(size) = json::json_get_num(&info.body, "size") {
        crate::serial_str!("[GITHUB] Size: ");
        crate::drivers::serial::write_dec(size as u32);
        crate::serial_strln!(" KB");
    }
}

/// Print parsed release info to serial
pub fn print_release_info(info: &RepoInfo) {
    if info.status != 200 {
        crate::serial_str!("[GITHUB] Error: HTTP ");
        crate::drivers::serial::write_dec(info.status as u32);
        crate::drivers::serial::write_newline();
        return;
    }

    if let Some(name) = json::json_get_str(&info.body, "name") {
        crate::serial_str!("[GITHUB] Release: ");
        crate::drivers::serial::write_str(name);
        crate::drivers::serial::write_newline();
    }

    if let Some(tag) = json::json_get_str(&info.body, "tag_name") {
        crate::serial_str!("[GITHUB] Tag: ");
        crate::drivers::serial::write_str(tag);
        crate::drivers::serial::write_newline();
    }

    if let Some(url) = json::json_get_str(&info.body, "zipball_url") {
        crate::serial_str!("[GITHUB] Zipball: ");
        crate::drivers::serial::write_str(url);
        crate::drivers::serial::write_newline();
    }

    if let Some(date) = json::json_get_str(&info.body, "published_at") {
        crate::serial_str!("[GITHUB] Published: ");
        crate::drivers::serial::write_str(date);
        crate::drivers::serial::write_newline();
    }
}

/// Clone a repo: fetch repo info JSON and return raw bytes.
/// Called from syscall — result stored in shmem for shell to write to VFS.
pub fn clone_repo(user: &str, repo: &str) -> Result<alloc::vec::Vec<u8>, &'static str> {
    crate::serial_str!("[GITHUB] Cloning ");
    crate::drivers::serial::write_str(user);
    crate::serial_str!("/");
    crate::drivers::serial::write_str(repo);
    crate::serial_strln!("...");

    // Step 1: Fetch repo info
    let info = fetch_repo_info(user, repo)?;

    if info.status != 200 {
        crate::serial_str!("[GITHUB] Clone failed: HTTP ");
        crate::drivers::serial::write_dec(info.status as u32);
        crate::drivers::serial::write_newline();
        return Err("GitHub API error");
    }

    // Step 2: Log what we got
    print_repo_info(&info);

    crate::serial_str!("[GITHUB] Clone: ");
    crate::drivers::serial::write_dec(info.body.len() as u32);
    crate::serial_strln!(" bytes of repo JSON ready for VFS");

    Ok(info.body)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build "/repos/{user}/{repo}"
fn build_api_path(buf: &mut [u8], user: &str, repo: &str) -> usize {
    let mut pos = 0;
    let parts: &[&[u8]] = &[b"/repos/", user.as_bytes(), b"/", repo.as_bytes()];
    for part in parts {
        let len = part.len().min(buf.len() - pos);
        buf[pos..pos + len].copy_from_slice(&part[..len]);
        pos += len;
    }
    pos
}

/// Build a GitHub API HTTP request with required headers
fn build_github_request(buf: &mut [u8], host: &str, path: &str) -> usize {
    let mut pos = 0;
    let parts: &[&[u8]] = &[
        b"GET ",
        path.as_bytes(),
        b" HTTP/1.1\r\nHost: ",
        host.as_bytes(),
        b"\r\nUser-Agent: FolkeringOS/0.1\r\nAccept: application/vnd.github+json\r\nConnection: close\r\n\r\n",
    ];
    for part in parts {
        let len = part.len().min(buf.len() - pos);
        buf[pos..pos + len].copy_from_slice(&part[..len]);
        pos += len;
    }
    pos
}

/// Parse HTTP status code from response (e.g. "HTTP/1.1 200 OK")
fn parse_http_status(response: &[u8]) -> u16 {
    // Find "HTTP/1.x NNN"
    if response.len() < 12 {
        return 0;
    }
    // Skip "HTTP/1.x " (9 bytes)
    if &response[..5] != b"HTTP/" {
        return 0;
    }
    // Find the space after version
    let mut i = 5;
    while i < response.len() && response[i] != b' ' {
        i += 1;
    }
    i += 1; // skip space

    // Parse 3-digit status
    let mut status: u16 = 0;
    for j in 0..3 {
        if i + j < response.len() && response[i + j] >= b'0' && response[i + j] <= b'9' {
            status = status * 10 + (response[i + j] - b'0') as u16;
        }
    }
    status
}

/// Find the start of HTTP body (after \r\n\r\n)
fn find_body_start(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}
