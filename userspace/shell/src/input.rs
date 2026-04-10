//! Stdin input handling: keystroke processing and command dispatch.

use libfolk::{print, println};
use libfolk::sys::clear_interrupt;

use crate::commands;
use crate::state::{
    clear_buffer, get_cmd_byte, get_cmd_len, set_cmd_byte, set_cmd_len, CMD_BUFFER_SIZE,
};

pub fn print_prompt() {
    print!("folk> ");
}

/// Process a single keystroke from stdin.
pub fn handle_key(key: u8) {
    match key {
        // Ctrl+C - cancel current input
        0x03 => {
            println!("^C");
            clear_buffer();
            clear_interrupt();
            print_prompt();
        }
        // Enter - execute command
        b'\r' | b'\n' => {
            println!();
            execute_command();
            clear_buffer();
            clear_interrupt();
            print_prompt();
        }
        // Backspace
        0x7F | 0x08 => {
            let len = get_cmd_len();
            if len > 0 {
                set_cmd_len(len - 1);
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
        _ => {}
    }
}

/// Parse the command buffer and dispatch to the appropriate `cmd_*` handler.
pub fn execute_command() {
    let len = get_cmd_len();
    if len == 0 {
        return;
    }

    let mut local_buf = [0u8; CMD_BUFFER_SIZE];
    for i in 0..len {
        local_buf[i] = get_cmd_byte(i);
    }

    let cmd = unsafe { core::str::from_utf8_unchecked(&local_buf[..len]) };
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return;
    }

    let mut parts = cmd.split_whitespace();
    let command = parts.next().unwrap_or("");

    match command {
        "help" => commands::basic::cmd_help(),
        "echo" => commands::basic::cmd_echo(parts),
        "ls" => commands::basic::cmd_ls(),
        "cat" => commands::vfs::cmd_cat(parts),
        "sql" => commands::vfs::cmd_sql(cmd),
        "search" => commands::search::cmd_search(parts),
        "test-gui" => commands::system::cmd_test_gui(),
        "ps" => commands::basic::cmd_ps(),
        "uptime" => commands::basic::cmd_uptime(),
        "pid" => commands::basic::cmd_pid(),
        "clear" => commands::basic::cmd_clear(),
        "exit" => commands::basic::cmd_exit(),
        "poweroff" | "shutdown" => commands::system::cmd_poweroff(),
        "ping" => commands::network::cmd_ping(parts),
        "resolve" | "nslookup" => commands::network::cmd_resolve(parts),
        "time" | "date" => commands::system::cmd_time(),
        "random" | "rand" => commands::system::cmd_random(),
        "https" => commands::network::cmd_https_test(),
        "fetch" => commands::network::cmd_fetch(parts),
        "clone" => commands::network::cmd_clone(parts),
        "save" => commands::vfs::cmd_save(parts),
        "load" => commands::vfs::cmd_load(),
        "ask" | "infer" => commands::ai::cmd_ask(cmd),
        "ai-status" => commands::ai::cmd_ai_status(),
        _ => {
            println!("Unknown command: {}", command);
            println!("Type 'help' for available commands.");
        }
    }
}
