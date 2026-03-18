//! Folkering Shell - Simple interactive shell for Folkering OS
//!
//! This is the first userspace application built using libfolk.

#![no_std]
#![no_main]

use libfolk::{entry, print, println};
use libfolk::sys::{yield_cpu, get_pid, exit, task_list, uptime, shmem_map, shmem_create, shmem_grant, shmem_unmap, shmem_destroy, poweroff, check_interrupt, clear_interrupt};
use libfolk::sys::synapse::{
    read_file_shmem, file_count, embedding_count,
    vector_search, get_embedding, SYNAPSE_TASK_ID,
};
use libfolk::sys::compositor::{
    create_window, update_node, find_node_by_hash, hash_name as comp_hash_name,
    role, CompError,
};
use libfolk::sys::fs::DirEntry;
use libfolk::sys::ipc::{recv_async, reply_with_token, IpcError};
use libfolk::sys::shell::{
    SHELL_OP_LIST_FILES, SHELL_OP_CAT_FILE, SHELL_OP_SEARCH,
    SHELL_OP_PS, SHELL_OP_UPTIME, SHELL_OP_EXEC,
    SHELL_STATUS_OK, SHELL_STATUS_NOT_FOUND, SHELL_STATUS_ERROR,
    hash_name as shell_hash_name,
};

/// Embedding size in bytes (384 dimensions × 4 bytes)
const EMBEDDING_SIZE: usize = 1536;

entry!(main);

/// Maximum command buffer size
const CMD_BUFFER_SIZE: usize = 256;

/// Command buffer for user input
static mut CMD_BUFFER: [u8; CMD_BUFFER_SIZE] = [0u8; CMD_BUFFER_SIZE];
static mut CMD_LEN: usize = 0;

/// Case-insensitive substring match (no_std)
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() { return true; }
    if needle.len() > haystack.len() { return false; }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    for i in 0..=(h.len() - n.len()) {
        let mut matched = true;
        for j in 0..n.len() {
            let a = if h[i+j] >= b'A' && h[i+j] <= b'Z' { h[i+j] + 32 } else { h[i+j] };
            let b = if n[j] >= b'A' && n[j] <= b'Z' { n[j] + 32 } else { n[j] };
            if a != b { matched = false; break; }
        }
        if matched { return true; }
    }
    false
}

// Helper functions for volatile access to prevent compiler optimizations
fn get_cmd_len() -> usize {
    unsafe { core::ptr::read_volatile(&CMD_LEN) }
}

fn set_cmd_len(len: usize) {
    unsafe { core::ptr::write_volatile(&mut CMD_LEN, len) }
}

fn get_cmd_byte(idx: usize) -> u8 {
    unsafe { core::ptr::read_volatile(&CMD_BUFFER[idx]) }
}

fn set_cmd_byte(idx: usize, val: u8) {
    unsafe { core::ptr::write_volatile(&mut CMD_BUFFER[idx], val) }
}

fn main() -> ! {
    let pid = get_pid();
    println!("Folkering Shell v0.1.0 (PID: {})", pid);
    println!("Type 'help' for available commands.\n");

    println!("[SHELL] Running (Task {})", pid);
    print_prompt();

    loop {
        // Process all pending async IPC messages before yielding.
        // The compositor sends commands here (ls, ps, uptime, exec, etc.)
        let mut did_work = false;
        loop {
            match recv_async() {
                Ok(msg) => {
                    did_work = true;
                    let response = handle_ipc_command(msg.payload0);
                    let _ = reply_with_token(msg.token, response, 0);
                }
                Err(IpcError::WouldBlock) => break,
                Err(_) => break,
            }
        }

        if !did_work {
            yield_cpu();
        }
    }
}

/// Handle IPC command from compositor or other tasks
fn handle_ipc_command(payload0: u64) -> u64 {
    let opcode = payload0 & 0xFF;

    match opcode {
        x if x == SHELL_OP_LIST_FILES => {
            // List files via Synapse (reads from SQLite)
            // Synapse returns (count << 32) | shmem_handle with file entries
            let result = unsafe {
                libfolk::syscall::syscall3(
                    libfolk::syscall::SYS_IPC_SEND,
                    SYNAPSE_TASK_ID as u64,
                    libfolk::sys::synapse::SYN_OP_LIST_FILES,
                    0
                )
            };
            // Forward Synapse's response directly to compositor
            // (Synapse already granted shmem to tasks 2-8)
            result
        }

        x if x == SHELL_OP_CAT_FILE => {
            // Cat file via Synapse (reads BLOB from SQLite, returns shmem)
            let name_hash = ((payload0 >> 8) & 0xFFFFFFFF) as u32;

            // Send to Synapse: SYN_OP_READ_FILE_SHMEM | (name_hash << 16)
            let syn_request = libfolk::sys::synapse::SYN_OP_READ_FILE_SHMEM
                | ((name_hash as u64) << 16);
            let result = unsafe {
                libfolk::syscall::syscall3(
                    libfolk::syscall::SYS_IPC_SEND,
                    SYNAPSE_TASK_ID as u64,
                    syn_request,
                    0
                )
            };

            // Forward Synapse's response directly to compositor
            // Synapse returns (size << 32) | shmem_handle, or SYN_STATUS_NOT_FOUND
            if result == u64::MAX {
                SHELL_STATUS_ERROR
            } else if result == libfolk::sys::synapse::SYN_STATUS_NOT_FOUND {
                SHELL_STATUS_NOT_FOUND
            } else {
                result // (size << 32) | shmem_handle
            }
        }

        x if x == SHELL_OP_SEARCH => {
            // Semantic search — query string in shmem from compositor
            let query_handle = ((payload0 >> 8) & 0xFFFFFFFF) as u32;
            let query_len = ((payload0 >> 40) & 0xFF) as usize;

            if query_handle == 0 || query_len == 0 {
                return 0;
            }

            // Map query shmem to read the search string
            let mut query_buf = [0u8; 64];
            if shmem_map(query_handle, SHELL_SHMEM_VADDR).is_ok() {
                let src = unsafe {
                    core::slice::from_raw_parts(SHELL_SHMEM_VADDR as *const u8, query_len.min(63))
                };
                query_buf[..src.len()].copy_from_slice(src);
                let _ = shmem_unmap(query_handle, SHELL_SHMEM_VADDR);
            } else {
                return 0;
            }
            let query_str = unsafe {
                core::str::from_utf8_unchecked(&query_buf[..query_len.min(63)])
            };

            // Get file list from Synapse
            let syn_result = unsafe {
                libfolk::syscall::syscall3(
                    libfolk::syscall::SYS_IPC_SEND,
                    SYNAPSE_TASK_ID as u64,
                    libfolk::sys::synapse::SYN_OP_LIST_FILES,
                    0
                )
            };
            if syn_result == u64::MAX {
                return 0;
            }
            let file_count = (syn_result >> 32) as usize;
            let files_handle = (syn_result & 0xFFFFFFFF) as u32;
            if files_handle == 0 || file_count == 0 {
                return 0;
            }

            // Map file list shmem and do substring match
            if shmem_map(files_handle, SHELL_SHMEM_VADDR).is_err() {
                let _ = shmem_destroy(files_handle);
                return 0;
            }
            let file_buf = unsafe {
                core::slice::from_raw_parts(SHELL_SHMEM_VADDR as *const u8, file_count * 32)
            };

            // Collect matching entries (max 10)
            let mut matches: [([u8; 24], u32, u32); 10] = [([0u8; 24], 0, 0); 10];
            let mut match_count = 0;

            for i in 0..file_count {
                if match_count >= 10 { break; }
                let offset = i * 32;
                let name_end = file_buf[offset..offset+24].iter()
                    .position(|&b| b == 0).unwrap_or(24);
                let name = unsafe {
                    core::str::from_utf8_unchecked(&file_buf[offset..offset+name_end])
                };
                // Case-insensitive substring match
                if contains_ignore_case(name, query_str) {
                    matches[match_count].0[..name_end].copy_from_slice(&file_buf[offset..offset+name_end]);
                    matches[match_count].1 = u32::from_le_bytes([
                        file_buf[offset+24], file_buf[offset+25],
                        file_buf[offset+26], file_buf[offset+27]
                    ]);
                    matches[match_count].2 = u32::from_le_bytes([
                        file_buf[offset+28], file_buf[offset+29],
                        file_buf[offset+30], file_buf[offset+31]
                    ]);
                    match_count += 1;
                }
            }
            let _ = shmem_unmap(files_handle, SHELL_SHMEM_VADDR);
            let _ = shmem_destroy(files_handle);

            if match_count == 0 {
                return 0;
            }

            // Create results shmem
            let results_size = match_count * 32;
            let results_handle = match shmem_create(results_size) {
                Ok(h) => h,
                Err(_) => return 0,
            };
            for tid in 2..=8 {
                let _ = shmem_grant(results_handle, tid);
            }
            if shmem_map(results_handle, SHELL_SHMEM_VADDR).is_err() {
                let _ = shmem_destroy(results_handle);
                return 0;
            }
            let result_buf = unsafe {
                core::slice::from_raw_parts_mut(SHELL_SHMEM_VADDR as *mut u8, results_size)
            };
            for i in 0..match_count {
                let offset = i * 32;
                result_buf[offset..offset+24].copy_from_slice(&matches[i].0);
                result_buf[offset+24..offset+28].copy_from_slice(&matches[i].1.to_le_bytes());
                result_buf[offset+28..offset+32].copy_from_slice(&matches[i].2.to_le_bytes());
            }
            let _ = shmem_unmap(results_handle, SHELL_SHMEM_VADDR);

            ((match_count as u64) << 32) | (results_handle as u64)
        }

        x if x == SHELL_OP_PS => {
            // Process list - return count + shmem with task details
            // Format per task (32 bytes): [task_id: u32][state: u32][name: [u8; 16]][cpu_time_ms: u64]
            let mut task_buf = [0u8; 512]; // max 16 tasks × 32 bytes
            let count = libfolk::sys::system::task_list_detailed(&mut task_buf) as usize;
            if count == 0 {
                return 0;
            }

            let shmem_size = count * 32;
            let handle = match shmem_create(shmem_size) {
                Ok(h) => h,
                Err(_) => return (count as u64) << 32,
            };

            if shmem_map(handle, SHELL_SHMEM_VADDR).is_err() {
                let _ = shmem_destroy(handle);
                return (count as u64) << 32;
            }

            // Copy task data to shmem
            let buf = unsafe {
                core::slice::from_raw_parts_mut(SHELL_SHMEM_VADDR as *mut u8, shmem_size)
            };
            buf.copy_from_slice(&task_buf[..shmem_size]);

            // Grant to potential callers
            for task_id in 2..=8 {
                let _ = shmem_grant(handle, task_id);
            }

            let _ = shmem_unmap(handle, SHELL_SHMEM_VADDR);

            // Return count in upper 32 bits, shmem handle in lower
            ((count as u64) << 32) | (handle as u64)
        }

        x if x == SHELL_OP_UPTIME => {
            // System uptime
            uptime()
        }

        x if x == SHELL_OP_EXEC => {
            // Execute a command sent as a hash in the payload.
            // High 32 bits: command identifier hash
            // Low  8  bits: opcode (0x85)
            // For now we return status OK — compositor renders output itself
            // for common commands; full shmem text output is a future milestone.
            SHELL_STATUS_OK
        }

        _ => {
            // Unknown opcode
            SHELL_STATUS_ERROR
        }
    }
}

fn print_prompt() {
    print!("folk> ");
}

fn handle_key(key: u8) {
    match key {
        // Ctrl+C - cancel current input
        0x03 => {
            println!("^C");
            clear_buffer();
            clear_interrupt(); // Clear the interrupt flag
            print_prompt();
        }
        // Enter - execute command
        b'\r' | b'\n' => {
            println!();
            execute_command();
            clear_buffer();
            clear_interrupt(); // Clear any interrupt that happened during command
            print_prompt();
        }
        // Backspace
        0x7F | 0x08 => {
            let len = get_cmd_len();
            if len > 0 {
                set_cmd_len(len - 1);
                // Erase character on screen: backspace, space, backspace
                print!("\x08 \x08");
            }
        }
        // Printable characters
        0x20..=0x7E => {
            let len = get_cmd_len();
            if len < CMD_BUFFER_SIZE - 1 {
                set_cmd_byte(len, key);
                set_cmd_len(len + 1);
                print!("{}", key as char);
            }
        }
        // Ignore other keys
        _ => {}
    }
}

fn clear_buffer() {
    set_cmd_len(0);
    for i in 0..CMD_BUFFER_SIZE {
        set_cmd_byte(i, 0);
    }
}

fn execute_command() {
    let len = get_cmd_len();
    if len == 0 {
        return;
    }

    // Copy buffer to local array to avoid volatile reads in loop
    let mut local_buf = [0u8; CMD_BUFFER_SIZE];
    for i in 0..len {
        local_buf[i] = get_cmd_byte(i);
    }

    let cmd = unsafe {
        core::str::from_utf8_unchecked(&local_buf[..len])
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
        "search" => cmd_search(parts),
        "test-gui" => cmd_test_gui(),
        "ps" => cmd_ps(),
        "uptime" => cmd_uptime(),
        "pid" => cmd_pid(),
        "clear" => cmd_clear(),
        "exit" => cmd_exit(),
        "poweroff" | "shutdown" => cmd_poweroff(),
        _ => {
            println!("Unknown command: {}", command);
            println!("Type 'help' for available commands.");
        }
    }
}

fn cmd_help() {
    println!("Available commands:");
    println!("  help              - Show this help message");
    println!("  echo              - Echo text back");
    println!("  ls                - List files in ramdisk");
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
    println!("  exit              - Exit the shell");
    println!("  poweroff          - Shut down the system");
}

fn cmd_poweroff() {
    println!("Shutting down...");
    poweroff();
}

/// Test Semantic Mirror integration.
///
/// Performs end-to-end verification:
/// 1. Creates a window via compositor IPC
/// 2. Sends a UI tree with a "Submit Form" button
/// 3. Queries for the button (simulates AI agent)
/// 4. Verifies the compositor correctly maintains and queries the WorldTree
fn cmd_test_gui() {
    println!("=== Semantic Mirror Integration Test ===\n");

    // Step 1: Create window
    println!("[1] Creating window...");
    let window_id = match create_window() {
        Ok(id) => {
            println!("    Window created: {}", id);
            id
        }
        Err(e) => {
            println!("    FAIL: {:?}", e);
            println!("\n    Hint: Is the compositor running?");
            return;
        }
    };

    // Step 2: Send "Submit Form" button (node 42, role=Button)
    println!("[2] Sending 'Submit Form' button...");
    let button_name = "Submit Form";
    let name_hash = comp_hash_name(button_name);
    let node_id: u64 = 42;

    match update_node(window_id, node_id, role::BUTTON, name_hash) {
        Ok(()) => {
            println!("    TreeUpdate sent OK");
        }
        Err(_) => {
            println!("    TreeUpdate FAIL");
            return;
        }
    }

    // Step 3: Query - simulate AI asking "where is Submit?"
    println!("[3] Querying...");
    match find_node_by_hash(name_hash) {
        Ok((true, found_node_id, found_window_id)) => {
            // Step 4: Verify
            if found_node_id == node_id && found_window_id == window_id {
                println!("[SUCCESS] Semantic Mirror verified!");
            } else {
                println!("[FAIL] Node/window mismatch");
            }
        }
        Ok((false, _, _)) => {
            println!("[FAIL] Node not found");
        }
        Err(_) => {
            println!("[FAIL] Query error");
        }
    }
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

/// Virtual address for vector search query embedding
const VECTOR_QUERY_VADDR: usize = 0x21000000;

/// Virtual address for vector search results
const VECTOR_RESULTS_VADDR: usize = 0x22000000;

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

    // Step 4: Cleanup - unmap the shared memory
    // Note: We don't destroy since Synapse is the owner
    let _ = shmem_unmap(response.shmem_handle, SHELL_SHMEM_VADDR);
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

/// Search for files by keyword, similarity, or hybrid
///
/// Usage:
///   search <keyword>           - Search filenames containing keyword
///   search -s <filename>       - Find files semantically similar to a file
///   search <keyword> -s <file> - Hybrid search (keyword + semantic RRF)
fn cmd_search<'a>(args: impl Iterator<Item = &'a str>) {
    // Parse arguments to find keyword and/or -s flag
    let mut keyword: Option<&str> = None;
    let mut similar_file: Option<&str> = None;
    let mut collected_args: [&str; 8] = [""; 8];
    let mut arg_count = 0;

    // Collect all arguments first
    for arg in args {
        if arg_count < 8 {
            collected_args[arg_count] = arg;
            arg_count += 1;
        }
    }

    if arg_count == 0 {
        println!("usage: search <keyword>");
        println!("       search -s <filename>  (semantic search)");
        println!("       search <keyword> -s <file>  (hybrid search)");
        return;
    }

    // Parse arguments
    let mut i = 0;
    while i < arg_count {
        let arg = collected_args[i];
        if arg == "-s" || arg == "--similar" {
            if i + 1 < arg_count {
                similar_file = Some(collected_args[i + 1]);
                i += 2;
            } else {
                println!("search: -s requires a filename");
                return;
            }
        } else {
            keyword = Some(arg);
            i += 1;
        }
    }

    // Dispatch to appropriate search mode
    match (keyword, similar_file) {
        (Some(kw), Some(sf)) => {
            // Hybrid search: keyword + semantic with RRF
            cmd_search_hybrid(kw, sf);
        }
        (None, Some(sf)) => {
            // Semantic-only search
            cmd_search_similar(sf);
        }
        (Some(kw), None) => {
            // Keyword-only search
            cmd_search_keyword(kw);
        }
        (None, None) => {
            println!("usage: search <keyword>");
            println!("       search -s <filename>  (semantic search)");
            println!("       search <keyword> -s <file>  (hybrid search)");
        }
    }
}

/// Keyword-only search
fn cmd_search_keyword(query: &str) {
    // Convert query to lowercase
    let mut query_lower = [0u8; 64];
    let mut query_len = 0;
    for &b in query.as_bytes() {
        if query_len < query_lower.len() - 1 {
            query_lower[query_len] = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
            query_len += 1;
        }
    }
    let query_str = unsafe { core::str::from_utf8_unchecked(&query_lower[..query_len]) };

    // Get file list
    let mut entries = [DirEntry {
        id: 0, entry_type: 0, name: [0u8; 32], size: 0
    }; 16];
    let dir_count = libfolk::sys::fs::read_dir(&mut entries);

    if dir_count == 0 {
        println!("No files available.");
        return;
    }

    // Find matches
    let mut found = 0;
    println!();

    for i in 0..dir_count {
        let entry = &entries[i];
        let name = entry.name_str();
        let name_lower = name_to_lowercase(name);

        if contains_substring(&name_lower, query_str) {
            let kind = if entry.is_elf() { "ELF " } else { "DATA" };
            println!("  {} {:>8} {} (keyword match)", kind, entry.size, name);
            found += 1;
        }
    }

    if found == 0 {
        println!("  No files matching '{}'", query);

        // Suggest semantic/hybrid search if embeddings available
        if let Ok(emb_count) = embedding_count() {
            if emb_count > 0 {
                println!("\n  Tip: Try 'search {} -s <file>' for hybrid search", query);
                println!("       ({} files have embeddings)", emb_count);
            }
        }
    } else {
        println!("\n{} file(s) found", found);
    }
}

/// RRF constant (standard value from literature)
const RRF_K: u32 = 60;

/// Maximum results for hybrid search
const MAX_HYBRID_RESULTS: usize = 16;

/// Hybrid search result entry
#[derive(Clone, Copy)]
struct HybridResult {
    file_id: u32,
    keyword_rank: u32,    // 0 = not in keyword results, 1+ = rank
    semantic_rank: u32,   // 0 = not in semantic results, 1+ = rank
    semantic_sim: f32,    // Raw similarity score for display
    rrf_score: u32,       // RRF score × 1000 for integer comparison
}

impl Default for HybridResult {
    fn default() -> Self {
        Self {
            file_id: 0,
            keyword_rank: 0,
            semantic_rank: 0,
            semantic_sim: 0.0,
            rrf_score: 0,
        }
    }
}

/// Hybrid search: combines keyword matching with semantic similarity using RRF
fn cmd_search_hybrid(keyword: &str, similar_file: &str) {
    // Track handles for cleanup
    let mut embedding_handle: Option<u32> = None;
    let mut query_handle: Option<u32> = None;
    let mut results_handle: Option<u32> = None;

    // Check if semantic search is available
    let emb_count = match embedding_count() {
        Ok(c) => c,
        Err(_) => {
            println!("search: Synapse not available");
            return;
        }
    };

    if emb_count == 0 {
        println!("search: No embeddings for hybrid search, falling back to keyword");
        cmd_search_keyword(keyword);
        return;
    }

    // Get file list
    let mut entries = [DirEntry {
        id: 0, entry_type: 0, name: [0u8; 32], size: 0
    }; 16];
    let dir_count = libfolk::sys::fs::read_dir(&mut entries);

    if dir_count == 0 {
        println!("No files available.");
        return;
    }

    // Find the reference file for semantic search
    let source_entry = entries[..dir_count]
        .iter()
        .find(|e| e.name_str() == similar_file);

    let source_file_id = match source_entry {
        Some(e) => e.id as u32,
        None => {
            println!("search: reference file '{}' not found", similar_file);
            return;
        }
    };

    // === STEP 1: Keyword Search ===
    let mut results: [HybridResult; MAX_HYBRID_RESULTS] = [HybridResult::default(); MAX_HYBRID_RESULTS];
    let mut result_count = 0;

    // Convert keyword to lowercase
    let mut keyword_lower = [0u8; 64];
    let mut keyword_len = 0;
    for &b in keyword.as_bytes() {
        if keyword_len < keyword_lower.len() - 1 {
            keyword_lower[keyword_len] = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
            keyword_len += 1;
        }
    }
    let keyword_str = unsafe { core::str::from_utf8_unchecked(&keyword_lower[..keyword_len]) };

    let mut keyword_rank = 1u32;
    for i in 0..dir_count {
        let entry = &entries[i];
        let name_lower = name_to_lowercase(entry.name_str());

        if contains_substring(&name_lower, keyword_str) {
            if result_count < MAX_HYBRID_RESULTS {
                results[result_count].file_id = entry.id as u32;
                results[result_count].keyword_rank = keyword_rank;
                result_count += 1;
                keyword_rank += 1;
            }
        }
    }

    // === STEP 2: Semantic Search ===
    // Get embedding for reference file
    let embedding_response = match get_embedding(source_file_id) {
        Ok(r) => r,
        Err(_) => {
            println!("search: reference file '{}' has no embedding", similar_file);
            println!("Falling back to keyword-only search.\n");
            cmd_search_keyword(keyword);
            return;
        }
    };

    // Map the embedding
    if shmem_map(embedding_response.shmem_handle, VECTOR_QUERY_VADDR).is_err() {
        println!("search: failed to map embedding");
        return;
    }
    embedding_handle = Some(embedding_response.shmem_handle);

    // Create query buffer for Synapse
    let query_shmem = match shmem_create(4096) {
        Ok(h) => h,
        Err(_) => {
            println!("search: failed to create query buffer");
            cleanup_shmem(embedding_handle, None, None);
            return;
        }
    };
    query_handle = Some(query_shmem);

    if shmem_grant(query_shmem, SYNAPSE_TASK_ID).is_err() {
        println!("search: failed to grant query buffer");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }

    if shmem_map(query_shmem, VECTOR_RESULTS_VADDR).is_err() {
        println!("search: failed to map query buffer");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }

    // Copy embedding to query buffer
    unsafe {
        let src = VECTOR_QUERY_VADDR as *const u8;
        let dst = VECTOR_RESULTS_VADDR as *mut u8;
        core::ptr::copy_nonoverlapping(src, dst, EMBEDDING_SIZE);
    }

    // Perform vector search (get more results for better RRF fusion)
    let k = 10;
    let search_response = match vector_search(query_shmem, k) {
        Ok(r) => r,
        Err(_) => {
            println!("search: vector search failed, falling back to keyword");
            cleanup_shmem(embedding_handle, query_handle, None);
            cmd_search_keyword(keyword);
            return;
        }
    };

    // Map results
    if search_response.count > 0 {
        if shmem_map(search_response.shmem_handle, VECTOR_RESULTS_VADDR).is_err() {
            println!("search: failed to map results");
            cleanup_shmem(embedding_handle, query_handle, None);
            return;
        }
        results_handle = Some(search_response.shmem_handle);

        // Process semantic results
        let results_ptr = VECTOR_RESULTS_VADDR as *const u8;
        let mut semantic_rank = 1u32;

        for i in 0..search_response.count {
            let offset = i * 8;
            let file_id = unsafe {
                let ptr = results_ptr.add(offset) as *const u32;
                *ptr
            };
            let similarity = unsafe {
                let ptr = results_ptr.add(offset + 4) as *const f32;
                *ptr
            };

            // Skip the reference file itself
            if file_id == source_file_id {
                continue;
            }

            // Check if this file is already in results (from keyword search)
            let existing = results[..result_count]
                .iter_mut()
                .find(|r| r.file_id == file_id);

            if let Some(result) = existing {
                result.semantic_rank = semantic_rank;
                result.semantic_sim = similarity;
            } else if result_count < MAX_HYBRID_RESULTS {
                // Add new result (semantic-only)
                results[result_count].file_id = file_id;
                results[result_count].semantic_rank = semantic_rank;
                results[result_count].semantic_sim = similarity;
                result_count += 1;
            }

            semantic_rank += 1;
        }
    }

    // === STEP 3: Calculate RRF Scores ===
    for result in results[..result_count].iter_mut() {
        let mut score = 0u32;

        // Keyword contribution: 1/(k + rank)
        if result.keyword_rank > 0 {
            score += 1000 / (RRF_K + result.keyword_rank);
        }

        // Semantic contribution: 1/(k + rank)
        if result.semantic_rank > 0 {
            score += 1000 / (RRF_K + result.semantic_rank);
        }

        result.rrf_score = score;
    }

    // === STEP 4: Sort by RRF Score (descending) ===
    // Simple bubble sort (small array)
    for i in 0..result_count {
        for j in (i + 1)..result_count {
            if results[j].rrf_score > results[i].rrf_score {
                let tmp = results[i];
                results[i] = results[j];
                results[j] = tmp;
            }
        }
    }

    // === STEP 5: Display Results ===
    if result_count == 0 {
        println!("\nNo files match '{}' or are similar to '{}'", keyword, similar_file);
        cleanup_shmem(embedding_handle, query_handle, results_handle);
        return;
    }

    println!("\nHybrid search: '{}' + similar to '{}':\n", keyword, similar_file);

    let display_count = result_count.min(8);
    for result in results[..display_count].iter() {
        // Find filename
        let name = entries[..dir_count]
            .iter()
            .find(|e| e.id as u32 == result.file_id)
            .map(|e| e.name_str())
            .unwrap_or("???");

        // Build match type indicator
        let match_type = match (result.keyword_rank > 0, result.semantic_rank > 0) {
            (true, true) => "K+S",   // Both keyword and semantic
            (true, false) => "K  ",  // Keyword only
            (false, true) => "  S",  // Semantic only
            (false, false) => "   ", // Shouldn't happen
        };

        // Show similarity if available
        if result.semantic_rank > 0 {
            let sim_pct = (result.semantic_sim * 100.0) as u32;
            println!("  [{}] {:<16} {:>3}% sim  (RRF: {})",
                     match_type, name, sim_pct, result.rrf_score);
        } else {
            println!("  [{}] {:<16}          (RRF: {})",
                     match_type, name, result.rrf_score);
        }
    }

    println!("\n{} result(s) - [K]=keyword [S]=semantic", display_count);

    // === STEP 6: Cleanup ===
    cleanup_shmem(embedding_handle, query_handle, results_handle);
}

/// Search for files semantically similar to a given file
fn cmd_search_similar(filename: &str) {
    // Track handles for cleanup
    let mut embedding_handle: Option<u32> = None;
    let mut query_handle: Option<u32> = None;
    let mut results_handle: Option<u32> = None;

    // Step 1: Check if semantic search is available
    let emb_count = match embedding_count() {
        Ok(c) => c,
        Err(_) => {
            println!("search: Synapse not available");
            return;
        }
    };

    if emb_count == 0 {
        println!("search: No embeddings available");
        println!("        Build with 'folk-pack create-sqlite --embed'");
        return;
    }

    // Step 2: Find the file ID for the given filename
    let mut entries = [DirEntry {
        id: 0, entry_type: 0, name: [0u8; 32], size: 0
    }; 16];
    let dir_count = libfolk::sys::fs::read_dir(&mut entries);

    let source_file = entries[..dir_count]
        .iter()
        .find(|e| e.name_str() == filename);

    let source_entry = match source_file {
        Some(e) => e,
        None => {
            println!("search: '{}' not found", filename);
            return;
        }
    };

    let file_id = source_entry.id as u32;

    // Step 3: Get the embedding for this file
    let embedding_response = match get_embedding(file_id) {
        Ok(r) => r,
        Err(_) => {
            println!("search: '{}' has no embedding", filename);
            return;
        }
    };

    // Step 4: Map the embedding to our address space
    if shmem_map(embedding_response.shmem_handle, VECTOR_QUERY_VADDR).is_err() {
        println!("search: failed to map embedding");
        return;
    }
    embedding_handle = Some(embedding_response.shmem_handle);

    // Step 5: Create shared memory for the query (Synapse needs to read from it)
    let query_shmem = match shmem_create(4096) {
        Ok(h) => h,
        Err(_) => {
            println!("search: failed to create query buffer");
            cleanup_shmem(embedding_handle, None, None);
            return;
        }
    };
    query_handle = Some(query_shmem);

    // Grant Synapse access to read the query
    if shmem_grant(query_shmem, SYNAPSE_TASK_ID).is_err() {
        println!("search: failed to grant query buffer");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }

    // Map query buffer and copy the embedding
    if shmem_map(query_shmem, VECTOR_RESULTS_VADDR).is_err() {
        println!("search: failed to map query buffer");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }

    // Copy embedding from source to query buffer
    unsafe {
        let src = VECTOR_QUERY_VADDR as *const u8;
        let dst = VECTOR_RESULTS_VADDR as *mut u8;
        core::ptr::copy_nonoverlapping(src, dst, EMBEDDING_SIZE);
    }

    // Step 6: Perform vector search
    let k = 5; // Get top 5 results
    let search_response = match vector_search(query_shmem, k) {
        Ok(r) => r,
        Err(_) => {
            println!("search: vector search failed");
            cleanup_shmem(embedding_handle, query_handle, None);
            return;
        }
    };

    if search_response.count == 0 {
        println!("No similar files found.");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }

    // Step 7: Map results and display
    if shmem_map(search_response.shmem_handle, VECTOR_RESULTS_VADDR).is_err() {
        println!("search: failed to map results");
        cleanup_shmem(embedding_handle, query_handle, None);
        return;
    }
    results_handle = Some(search_response.shmem_handle);

    println!("\nFiles similar to '{}':\n", filename);

    // Read results from shared memory
    let results_ptr = VECTOR_RESULTS_VADDR as *const u8;
    for i in 0..search_response.count {
        let offset = i * 8;

        // Read file_id (4 bytes, little-endian)
        let result_file_id = unsafe {
            let ptr = results_ptr.add(offset) as *const u32;
            *ptr
        };

        // Read similarity (4 bytes, little-endian f32)
        let similarity = unsafe {
            let ptr = results_ptr.add(offset + 4) as *const f32;
            *ptr
        };

        // Skip the source file itself
        if result_file_id == file_id {
            continue;
        }

        // Find the filename for this file_id
        let result_name = entries[..dir_count]
            .iter()
            .find(|e| e.id as u32 == result_file_id)
            .map(|e| e.name_str())
            .unwrap_or("???");

        // Display with similarity score (as percentage)
        let sim_pct = (similarity * 100.0) as u32;
        println!("  {:<16} ({:>3}% similar)", result_name, sim_pct);
    }
    println!();

    // Step 8: Cleanup - unmap all shared memory, destroy what we own
    cleanup_shmem(embedding_handle, query_handle, results_handle);
}

/// Helper to clean up shared memory after search operations
fn cleanup_shmem(embedding: Option<u32>, query: Option<u32>, results: Option<u32>) {
    // Unmap embedding (owned by Synapse, just unmap)
    if let Some(h) = embedding {
        let _ = shmem_unmap(h, VECTOR_QUERY_VADDR);
    }

    // Unmap and destroy query buffer (owned by shell)
    if let Some(h) = query {
        let _ = shmem_unmap(h, VECTOR_RESULTS_VADDR);
        let _ = shmem_destroy(h); // Shell created this, so shell can destroy
    }

    // Unmap results (owned by Synapse, just unmap)
    if let Some(h) = results {
        let _ = shmem_unmap(h, VECTOR_RESULTS_VADDR);
    }
}

/// Convert filename to lowercase (in-place buffer)
fn name_to_lowercase(name: &str) -> [u8; 32] {
    let mut lower = [0u8; 32];
    for (i, &b) in name.as_bytes().iter().enumerate() {
        if i >= 32 {
            break;
        }
        lower[i] = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
    }
    lower
}

/// Check if haystack contains needle (case-insensitive)
fn contains_substring(haystack: &[u8; 32], needle: &str) -> bool {
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() {
        return true;
    }

    // Find the actual length of haystack (stop at null)
    let mut haystack_len = 0;
    for &b in haystack.iter() {
        if b == 0 {
            break;
        }
        haystack_len += 1;
    }

    if haystack_len < needle_bytes.len() {
        return false;
    }

    for i in 0..=(haystack_len - needle_bytes.len()) {
        let mut matches = true;
        for (j, &needle_byte) in needle_bytes.iter().enumerate() {
            if haystack[i + j] != needle_byte {
                matches = false;
                break;
            }
        }
        if matches {
            return true;
        }
    }
    false
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
