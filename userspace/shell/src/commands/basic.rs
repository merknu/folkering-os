//! Basic CLI commands: help, echo, ls, ps, uptime, pid, clear, exit.

use libfolk::{print, println};
use libfolk::sys::{exit, get_pid, task_list, uptime};

pub fn cmd_help() {
    println!("Available commands:");
    println!("  help              - Show this help message");
    println!("  echo              - Echo text back");
    println!("  ls                - List files in ramdisk");
    println!("  heap              - Kernel heap diagnostics (X-ray)");
    println!("  cat <file>        - Display file contents");
    println!("  sql \"...\"         - Execute SQL query on files database");
    println!("  search <keyword>  - Search files by keyword");
    println!("  search -s <file>  - Find files similar to <file>");
    println!("  search <kw> -s <f> - Hybrid search (keyword + semantic)");
    println!("  test-gui          - Test Semantic Mirror integration");
    println!("  ps                - List running tasks");
    println!("  uptime            - Show system uptime");
    println!("  pid               - Show current process ID");
    println!("  clear             - Clear the screen");
    println!("  ping <ip|host>    - Ping IP or hostname (DNS resolves automatically)");
    println!("  resolve <host>    - DNS lookup (e.g. resolve google.com)");
    println!("  time              - Show current date/time (RTC)");
    println!("  random            - Generate random numbers (RDRAND)");
    println!("  https             - Test HTTPS GET to Google (TLS 1.3)");
    println!("  fetch <user> <repo> - Fetch GitHub repo info via API");
    println!("  clone <user> <repo> - Download repo to VFS (SQLite)");
    println!("  save <file> <text> - Save text file to VFS (SQLite)");
    println!("  load              - Load text from persistent storage");
    println!("  mvfs <subcmd>     - Mutable tmpfs (ls/cat/write/rm, disk-persistent)");
    println!("  ask <question>    - Ask AI a question (RAG-enhanced)");
    println!("  infer <prompt>    - Generate text from prompt");
    println!("  ai-status         - Check AI inference server status");
    println!("  exit              - Exit the shell");
    println!("  poweroff          - Shut down the system");
}

pub fn cmd_echo<'a>(mut args: impl Iterator<Item = &'a str>) {
    let mut first = true;
    for arg in args.by_ref() {
        if !first { print!(" "); }
        print!("{}", arg);
        first = false;
    }
    println!();
}

pub fn cmd_ls() {
    let mut entries = [libfolk::sys::fs::DirEntry {
        id: 0, entry_type: 0, name: [0u8; 32], size: 0,
    }; 16];

    let count = libfolk::sys::fs::read_dir(&mut entries);
    if count == 0 {
        println!("(no files)");
        return;
    }

    println!();
    for i in 0..count {
        let e = entries[i];
        let kind = if e.is_elf() { "ELF " } else { "DATA" };
        let size = e.size;
        println!("  {} {:>8} {}", kind, size, e.name_str());
    }
    println!("\n{} file(s)", count);
}

pub fn cmd_ps() {
    let count = task_list();
    println!("\n{} task(s) total", count);
}

pub fn cmd_uptime() {
    let ms = uptime();
    let seconds = ms / 1000;
    let minutes = seconds / 60;
    let hours = minutes / 60;

    if hours > 0 {
        println!("Uptime: {}h {}m {}s", hours, minutes % 60, seconds % 60);
    } else if minutes > 0 {
        println!("Uptime: {}m {}s", minutes, seconds % 60);
    } else {
        println!("Uptime: {}s ({}ms)", seconds, ms);
    }
}

pub fn cmd_pid() {
    println!("PID: {}", get_pid());
}

/// `heap` — kernel-heap X-ray. Reads the syscall-0x85 stats struct
/// and prints both the inner-allocator view (with alignment padding)
/// and the requested-bytes tracker (raw user-code demand). The
/// high-water line is the load-bearing bit for #54-style "did the
/// heap ever grow this big" investigations after the system has
/// GC'd back to a smaller working set.
///
/// Format is plain integer KiB so we don't need `alloc::format!` —
/// shell binary has no global allocator. Number gymnastics happen
/// in the kernel's `KernelHeapStats` view.
pub fn cmd_heap() {
    let stats = match libfolk::sys::heap_walk() {
        Some(s) => s,
        None => {
            println!("[heap] syscall failed or layout-version mismatch");
            return;
        }
    };

    let total_kib = stats.total_bytes / 1024;
    let used_kib = stats.used_bytes / 1024;
    let free_kib = stats.free_bytes / 1024;
    let requested_kib = stats.requested_bytes / 1024;
    let high_water_kib = stats.high_water_bytes / 1024;
    let overhead_kib = stats.overhead_bytes() / 1024;
    let pmille = stats.used_per_mille();

    println!("");
    println!("Kernel heap (layout v{}):", stats.layout_version);
    println!("  Total              : {} KiB", total_kib);
    println!("  Used (alloc view)  : {} KiB  ({}.{}%)",
             used_kib, pmille / 10, pmille % 10);
    println!("  Free               : {} KiB", free_kib);
    println!("  Requested (live)   : {} KiB", requested_kib);
    println!("  High-water         : {} KiB   <-- peak since boot",
             high_water_kib);
    println!("  Overhead (padding) : {} KiB", overhead_kib);
    println!("");
    println!("  Allocs             : {}", stats.alloc_count);
    println!("  Deallocs           : {}", stats.dealloc_count);
    println!("  Live allocations   : {}", stats.live_allocs());
    println!("");
}

pub fn cmd_clear() {
    print!("\x1B[2J\x1B[H");
}

pub fn cmd_exit() {
    println!("Goodbye!");
    exit(0)
}
