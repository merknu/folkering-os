//! Folkering Shell — binary entry point.
//!
//! Phase C4 reduced this file from 2486 lines to ~50 by extracting state,
//! UI builders, IPC dispatch, input handling and individual commands into
//! the `shell` library crate. This binary is now just:
//!
//! 1. `entry!()` declaration
//! 2. Boot banner
//! 3. IPC poll loop (recv_async → handle_ipc_command → reply)

#![no_std]
#![no_main]

use libfolk::{entry, println};
use libfolk::sys::ipc::{recv_async, reply_with_token, IpcError};
use libfolk::sys::{get_pid, yield_cpu};

use shell::input::print_prompt;
use shell::ipc::handle_ipc_command;

entry!(main);

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
