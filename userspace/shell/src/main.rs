//! Folkering Shell - Simple interactive shell for Folkering OS
//!
//! This is the first userspace application built using libfolk.

#![no_std]
#![no_main]

use libfolk::{entry, print, println};
use libfolk::sys::{read_key, yield_cpu, get_pid, exit, task_list, uptime, shmem_map};
use libfolk::sys::synapse::{read_file_shmem, file_count};
use libfolk::sys::fs::DirEntry;

entry!(main);

/// Maximum command buffer size
const CMD_BUFFER_SIZE: usize = 256;

/// Command buffer for user input
static mut CMD_BUFFER: [u8; CMD_BUFFER_SIZE] = [0u8; CMD_BUFFER_SIZE];
static mut CMD_LEN: usize = 0;

fn main() -> ! {
    let pid = get_pid();
    println!("Folkering Shell v0.1.0 (PID: {})", pid);
    println!("Type 'help' for available commands.\n");

    print_prompt();

    loop {
        match read_key() {
            Some(key) => handle_key(key),
            None => yield_cpu(),
        }
    }
}

fn print_prompt() {
    print!("folk> ");
}

fn handle_key(key: u8) {
    match key {
        // Enter - execute command
        b'\r' | b'\n' => {
            println!();
            execute_command();
            clear_buffer();
            print_prompt();
        }
        // Backspace
        0x7F | 0x08 => {
            unsafe {
                if CMD_LEN > 0 {
                    CMD_LEN -= 1;
                    // Erase character on screen: backspace, space, backspace
                    print!("\x08 \x08");
                }
            }
        }
        // Printable characters
        0x20..=0x7E => {
            unsafe {
                if CMD_LEN < CMD_BUFFER_SIZE - 1 {
                    CMD_BUFFER[CMD_LEN] = key;
                    CMD_LEN += 1;
                    print!("{}", key as char);
                }
            }
        }
        // Ignore other keys
        _ => {}
    }
}

fn clear_buffer() {
    unsafe {
        CMD_LEN = 0;
        CMD_BUFFER = [0u8; CMD_BUFFER_SIZE];
    }
}

fn execute_command() {
    let cmd = unsafe {
        core::str::from_utf8_unchecked(&CMD_BUFFER[..CMD_LEN])
    };

    let cmd = cmd.trim();
    if cmd.is_empty() {
        return;
    }

    // Parse command and arguments
    let mut parts = cmd.split_whitespace();
    let command = parts.next().unwrap_or("");

    match command {
        "help" => cmd_help(),
        "echo" => cmd_echo(parts),
        "ls" => cmd_ls(),
        "cat" => cmd_cat(parts),
        "sql" => cmd_sql(cmd),
        "ps" => cmd_ps(),
        "uptime" => cmd_uptime(),
        "pid" => cmd_pid(),
        "clear" => cmd_clear(),
        "exit" => cmd_exit(),
        _ => {
            println!("Unknown command: {}", command);
            println!("Type 'help' for available commands.");
        }
    }
}

fn cmd_help() {
    println!("Available commands:");
    println!("  help       - Show this help message");
    println!("  echo       - Echo text back");
    println!("  ls         - List files in ramdisk");
    println!("  cat <file> - Display file contents");
    println!("  sql \"...\"  - Execute SQL query on files database");
    println!("  ps         - List running tasks");
    println!("  uptime     - Show system uptime");
    println!("  pid        - Show current process ID");
    println!("  clear      - Clear the screen");
    println!("  exit       - Exit the shell");
}

fn cmd_ls() {
    let mut entries = [libfolk::sys::fs::DirEntry {
        id: 0, entry_type: 0, name: [0u8; 32], size: 0
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

/// Virtual address for Shell's shared memory buffer mapping
/// Using a fixed address that won't conflict with code/stack
const SHELL_SHMEM_VADDR: usize = 0x20000000;

fn cmd_cat<'a>(mut args: impl Iterator<Item = &'a str>) {
    let filename = match args.next() {
        Some(f) => f,
        None => {
            println!("usage: cat <filename>");
            return;
        }
    };

    // Step 1: Request file via Synapse IPC (zero-copy)
    // Synapse will create shared memory, load the file, and grant us access
    let response = match read_file_shmem(filename) {
        Ok(r) => r,
        Err(_) => {
            println!("cat: {}: not found", filename);
            return;
        }
    };

    if response.size == 0 {
        println!("cat: {}: empty file", filename);
        return;
    }

    // Step 2: Map the shared memory into our address space
    if shmem_map(response.shmem_handle, SHELL_SHMEM_VADDR).is_err() {
        println!("cat: failed to map file buffer");
        return;
    }

    // Step 3: Read directly from mapped memory (ZERO-COPY!)
    let buffer = unsafe {
        core::slice::from_raw_parts(SHELL_SHMEM_VADDR as *const u8, response.size as usize)
    };

    // Print the file contents
    for &b in buffer {
        if b == b'\n' || b == b'\r' || b == b'\t' || (b >= 0x20 && b < 0x7F) {
            print!("{}", b as char);
        } else if b == 0 {
            // Stop at null terminator for text files
            break;
        } else {
            print!(".");
        }
    }
    println!();
}

/// Execute SQL query on files database
/// Supports simple SELECT queries:
/// - SELECT name FROM files
/// - SELECT name, size FROM files
/// - SELECT * FROM files
fn cmd_sql(full_cmd: &str) {
    // Extract the query from quotes: sql "SELECT ..."
    let query = if let Some(start) = full_cmd.find('"') {
        if let Some(end) = full_cmd[start + 1..].find('"') {
            &full_cmd[start + 1..start + 1 + end]
        } else {
            println!("sql: missing closing quote");
            return;
        }
    } else {
        // Try without quotes: sql SELECT ...
        let trimmed = full_cmd.strip_prefix("sql ").unwrap_or("");
        if trimmed.is_empty() {
            println!("usage: sql \"SELECT ... FROM files\"");
            return;
        }
        trimmed
    };

    let query_upper = query.to_uppercase_simple();

    // Parse the SELECT query
    if !query_upper.starts_with("SELECT ") {
        println!("sql: only SELECT queries are supported");
        return;
    }

    // Check if it's a query on 'files' table
    if !query_upper.contains(" FROM FILES") {
        println!("sql: only 'files' table is available");
        return;
    }

    // Determine which columns to show
    let columns_part = &query[7..]; // Skip "SELECT "
    let from_pos = columns_part.to_uppercase_simple().find(" FROM");
    let columns_str = match from_pos {
        Some(pos) => columns_part[..pos].trim(),
        None => {
            println!("sql: invalid query syntax");
            return;
        }
    };

    // Parse column names
    let show_name = columns_str == "*" ||
                   columns_str.to_uppercase_simple().contains("NAME");
    let show_size = columns_str == "*" ||
                   columns_str.to_uppercase_simple().contains("SIZE");
    let show_kind = columns_str == "*" ||
                   columns_str.to_uppercase_simple().contains("KIND") ||
                   columns_str.to_uppercase_simple().contains("TYPE");

    // Get file count from Synapse
    let count = match file_count() {
        Ok(c) => c,
        Err(_) => {
            println!("sql: Synapse not available");
            return;
        }
    };

    if count == 0 {
        println!("(0 rows)");
        return;
    }

    // Fetch and display each file
    // We need to get file names from ls since Synapse only returns metadata
    let mut entries = [DirEntry {
        id: 0, entry_type: 0, name: [0u8; 32], size: 0
    }; 16];
    let dir_count = libfolk::sys::fs::read_dir(&mut entries);

    println!();
    for i in 0..dir_count.min(count) {
        let entry = &entries[i];
        let name = entry.name_str();

        if show_name && show_size && show_kind {
            let kind = if entry.is_elf() { "elf" } else { "data" };
            println!("{:<16} {:>8} {}", name, entry.size, kind);
        } else if show_name && show_size {
            println!("{:<16} {:>8}", name, entry.size);
        } else if show_name && show_kind {
            let kind = if entry.is_elf() { "elf" } else { "data" };
            println!("{:<16} {}", name, kind);
        } else if show_name {
            println!("{}", name);
        } else if show_size {
            println!("{}", entry.size);
        }
    }
    println!("\n({} rows)", dir_count.min(count));
}

/// Simple uppercase conversion for ASCII strings
trait ToUppercaseSimple {
    fn to_uppercase_simple(&self) -> SimpleUpper;
}

impl ToUppercaseSimple for &str {
    fn to_uppercase_simple(&self) -> SimpleUpper {
        SimpleUpper { s: self }
    }
}

struct SimpleUpper<'a> {
    s: &'a str,
}

impl<'a> SimpleUpper<'a> {
    fn starts_with(&self, prefix: &str) -> bool {
        if self.s.len() < prefix.len() {
            return false;
        }
        for (a, b) in self.s.bytes().zip(prefix.bytes()) {
            let a_upper = if a >= b'a' && a <= b'z' { a - 32 } else { a };
            if a_upper != b {
                return false;
            }
        }
        true
    }

    fn contains(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        for i in 0..=self.s.len().saturating_sub(needle.len()) {
            let slice = &self.s[i..];
            if (SimpleUpper { s: slice }).starts_with(needle) {
                return true;
            }
        }
        false
    }

    fn find(&self, needle: &str) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        for i in 0..=self.s.len().saturating_sub(needle.len()) {
            let slice = &self.s[i..];
            if (SimpleUpper { s: slice }).starts_with(needle) {
                return Some(i);
            }
        }
        None
    }
}

fn cmd_echo<'a>(mut args: impl Iterator<Item = &'a str>) {
    let mut first = true;
    for arg in args.by_ref() {
        if !first {
            print!(" ");
        }
        print!("{}", arg);
        first = false;
    }
    println!();
}

fn cmd_ps() {
    let count = task_list();
    println!("\n{} task(s) total", count);
}

fn cmd_uptime() {
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

fn cmd_pid() {
    println!("PID: {}", get_pid());
}

fn cmd_clear() {
    // Send ANSI escape sequence to clear screen
    print!("\x1B[2J\x1B[H");
}

fn cmd_exit() {
    println!("Goodbye!");
    exit(0)
}
