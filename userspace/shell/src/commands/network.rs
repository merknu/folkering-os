//! Network commands: ping, resolve, https, fetch, clone.

use libfolk::println;
use libfolk::sys::synapse::write_file;
use libfolk::sys::{shmem_destroy, shmem_map, shmem_unmap};

use crate::ui::SHELL_SHMEM_VADDR;

pub fn cmd_ping<'a>(mut args: impl Iterator<Item = &'a str>) {
    let target = match args.next() {
        Some(s) => s,
        None => { println!("usage: ping <ip or hostname>"); return; }
    };

    let octets = match parse_ipv4(target) {
        Some(o) => o,
        None => {
            println!("Resolving {}...", target);
            match libfolk::sys::dns::lookup(target) {
                Some(o) => {
                    println!("{} -> {}.{}.{}.{}", target, o.0, o.1, o.2, o.3);
                    [o.0, o.1, o.2, o.3]
                }
                None => {
                    println!("ping: could not resolve {}", target);
                    return;
                }
            }
        }
    };

    println!("PING {}.{}.{}.{} ...", octets[0], octets[1], octets[2], octets[3]);
    libfolk::sys::ping::ping(octets[0], octets[1], octets[2], octets[3]);
    println!("(check serial log for reply)");
}

pub fn cmd_resolve<'a>(mut args: impl Iterator<Item = &'a str>) {
    let hostname = match args.next() {
        Some(s) => s,
        None => { println!("usage: resolve <hostname>"); return; }
    };

    println!("Resolving {}...", hostname);
    match libfolk::sys::dns::lookup(hostname) {
        Some((a, b, c, d)) => println!("{} -> {}.{}.{}.{}", hostname, a, b, c, d),
        None => println!("resolve: failed to resolve {}", hostname),
    }
}

/// Try to parse "a.b.c.d" as IPv4. Returns None if not a valid IP.
pub fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut idx = 0;
    for part in s.split('.') {
        if idx >= 4 { return None; }
        let mut val: u16 = 0;
        if part.is_empty() { return None; }
        for &b in part.as_bytes() {
            if b < b'0' || b > b'9' { return None; }
            val = val * 10 + (b - b'0') as u16;
            if val > 255 { return None; }
        }
        octets[idx] = val as u8;
        idx += 1;
    }
    if idx == 4 { Some(octets) } else { None }
}

pub fn cmd_https_test() {
    println!("Testing HTTPS connection to Google...");
    println!("(output on serial log)");
    let result = unsafe {
        libfolk::syscall::syscall0(libfolk::syscall::SYS_HTTPS_TEST)
    };
    if result == 0 {
        println!("HTTPS test completed successfully!");
    } else {
        println!("HTTPS test failed (check serial log)");
    }
}

pub fn cmd_fetch<'a>(mut args: impl Iterator<Item = &'a str>) {
    let user = match args.next() {
        Some(s) => s,
        None => { println!("usage: fetch <user> <repo>"); return; }
    };
    let repo = match args.next() {
        Some(s) => s,
        None => { println!("usage: fetch <user> <repo>"); return; }
    };

    println!("Fetching {}/{}...", user, repo);
    println!("(results on serial log)");
    let result = unsafe {
        libfolk::syscall::syscall4(
            libfolk::syscall::SYS_GITHUB_FETCH,
            user.as_ptr() as u64, user.len() as u64,
            repo.as_ptr() as u64, repo.len() as u64,
        )
    };
    if result == 0 {
        println!("Fetch completed!");
    } else {
        println!("Fetch failed (check serial log)");
    }
}

pub fn cmd_clone<'a>(mut args: impl Iterator<Item = &'a str>) {
    let user = match args.next() {
        Some(s) => s,
        None => { println!("usage: clone <user> <repo>"); return; }
    };
    let repo = match args.next() {
        Some(s) => s,
        None => { println!("usage: clone <user> <repo>"); return; }
    };

    println!("Cloning {}/{}...", user, repo);

    let result = unsafe {
        libfolk::syscall::syscall4(
            libfolk::syscall::SYS_GITHUB_CLONE,
            user.as_ptr() as u64, user.len() as u64,
            repo.as_ptr() as u64, repo.len() as u64,
        )
    };

    if result == u64::MAX {
        println!("Clone failed (check serial log)");
        return;
    }

    let data_size = (result >> 32) as usize;
    let shmem_handle = (result & 0xFFFFFFFF) as u32;

    println!("Downloaded {} bytes", data_size);

    if shmem_map(shmem_handle, SHELL_SHMEM_VADDR).is_err() {
        println!("Failed to map download buffer");
        let _ = shmem_destroy(shmem_handle);
        return;
    }

    let data = unsafe {
        core::slice::from_raw_parts(SHELL_SHMEM_VADDR as *const u8, data_size)
    };

    // Build filename: "{user}_{repo}.json"
    let mut filename = [0u8; 64];
    let mut flen = 0;
    for &b in user.as_bytes() {
        if flen < 60 { filename[flen] = b; flen += 1; }
    }
    if flen < 60 { filename[flen] = b'_'; flen += 1; }
    for &b in repo.as_bytes() {
        if flen < 58 { filename[flen] = b; flen += 1; }
    }
    let suffix = b".json";
    for &b in suffix {
        if flen < 63 { filename[flen] = b; flen += 1; }
    }
    let fname = unsafe { core::str::from_utf8_unchecked(&filename[..flen]) };

    let _ = shmem_unmap(shmem_handle, SHELL_SHMEM_VADDR);

    match write_file(fname, data) {
        Ok(()) => {
            println!("[VFS] Saved '{}' ({} bytes) to SQLite", fname, data_size);
            println!("Clone complete! Use 'cat {}' to view.", fname);
        }
        Err(e) => println!("VFS write failed: {:?}", e),
    }

    let _ = shmem_destroy(shmem_handle);
}
