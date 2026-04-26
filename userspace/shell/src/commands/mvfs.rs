//! Mutable VFS (tmpfs) commands: ls, cat, write, rm.
//!
//! The `mvfs` namespace sits parallel to the read-only ramdisk `ls` /
//! `cat` and to Synapse's SQL-backed `save` / `load`. It's the
//! "just a disk-persistent scratch pad" layer — fast, no IPC, 16 ×
//! 4 KiB max, survives reboot (see `kernel/src/fs/mvfs.rs`).

use libfolk::println;
use libfolk::sys::fs::{mvfs_write, mvfs_read, mvfs_delete, mvfs_list, MVFS_MAX_FILE_SIZE};

/// Dispatcher for `mvfs <subcmd> [args...]`. Keeps the top-level
/// shell namespace clean.
pub fn cmd_mvfs<'a>(mut args: impl Iterator<Item = &'a str>) {
    let sub = args.next().unwrap_or("");
    match sub {
        "ls"    => cmd_ls(args),
        "cat"   => cmd_cat(args),
        "write" => cmd_write(args),
        "rm"    => cmd_rm(args),
        "" | "help" => print_help(),
        other => {
            println!("mvfs: unknown subcommand '{}'", other);
            print_help();
        }
    }
}

fn print_help() {
    println!("mvfs commands:");
    println!("  mvfs ls [prefix]        list entries (optional prefix filter)");
    println!("  mvfs cat <name>         print file contents");
    println!("  mvfs write <name> <text...>");
    println!("                          write text to <name>");
    println!("  mvfs rm <name>          delete <name>");
}

fn cmd_ls<'a>(mut args: impl Iterator<Item = &'a str>) {
    let prefix = args.next().unwrap_or("");
    let mut buf = [0u8; 1024];
    let n = mvfs_list(prefix, &mut buf);
    if n == 0 {
        if prefix.is_empty() {
            println!("mvfs: (empty)");
        } else {
            println!("mvfs: no entries matching '{}'", prefix);
        }
        return;
    }

    // Parse the flat [name_len:u8][name bytes] stream.
    let mut pos = 0usize;
    let mut count = 0u32;
    while pos < n {
        let len = buf[pos] as usize;
        pos += 1;
        if pos + len > n { break; }
        let name = unsafe { core::str::from_utf8_unchecked(&buf[pos..pos + len]) };
        println!("  {}", name);
        pos += len;
        count += 1;
    }
    println!("({} entr{})", count, if count == 1 { "y" } else { "ies" });
}

fn cmd_cat<'a>(mut args: impl Iterator<Item = &'a str>) {
    let name = match args.next() {
        Some(n) => n,
        None => { println!("usage: mvfs cat <name>"); return; }
    };
    let mut buf = [0u8; MVFS_MAX_FILE_SIZE];
    match mvfs_read(name, &mut buf) {
        Some(n) if n > 0 => {
            // Print as UTF-8 if valid, hex byte-by-byte otherwise.
            // Shell is strict no_std with no heap allocator in scope,
            // so we stream bytes directly via `libfolk::print!`.
            match core::str::from_utf8(&buf[..n]) {
                Ok(s) => println!("{}", s),
                Err(_) => {
                    println!("(binary, {} bytes — hex dump first 64):", n);
                    let show = n.min(64);
                    for &b in &buf[..show] {
                        libfolk::print!("{:02x} ", b);
                    }
                    println!();
                }
            }
        }
        Some(_) => println!("mvfs: {}: empty", name),
        None => println!("mvfs: {}: not found", name),
    }
}

fn cmd_write<'a>(mut args: impl Iterator<Item = &'a str>) {
    let name = match args.next() {
        Some(n) => n,
        None => { println!("usage: mvfs write <name> <text...>"); return; }
    };

    // Rejoin the remaining args with single spaces into a fixed
    // stack buffer — mirrors the shell's other no-alloc command
    // patterns. Capped at MVFS's on-disk slot size.
    let mut payload = [0u8; MVFS_MAX_FILE_SIZE];
    let mut written = 0usize;
    let mut first = true;
    for word in args {
        if !first && written < payload.len() {
            payload[written] = b' ';
            written += 1;
        }
        let wb = word.as_bytes();
        let n = wb.len().min(payload.len() - written);
        payload[written..written + n].copy_from_slice(&wb[..n]);
        written += n;
        first = false;
        if written >= payload.len() { break; }
    }
    if written == 0 {
        println!("usage: mvfs write <name> <text...>");
        return;
    }

    if mvfs_write(name, &payload[..written]) {
        println!("mvfs: wrote {} ({} bytes)", name, written);
    } else {
        println!("mvfs: write failed (name too long, data too large, or table full)");
    }
}

fn cmd_rm<'a>(mut args: impl Iterator<Item = &'a str>) {
    let name = match args.next() {
        Some(n) => n,
        None => { println!("usage: mvfs rm <name>"); return; }
    };
    if mvfs_delete(name) {
        println!("mvfs: removed {}", name);
    } else {
        println!("mvfs: {}: not found", name);
    }
}
