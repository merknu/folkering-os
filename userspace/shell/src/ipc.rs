//! IPC command dispatcher — handles compositor → shell IPC for the UI apps
//! (calculator, greeter, folkpad) and the SHELL_OP_* opcodes.

use libfolk::println;
use libfolk::sys::shell::{
    SHELL_OP_LIST_FILES, SHELL_OP_CAT_FILE, SHELL_OP_SEARCH,
    SHELL_OP_PS, SHELL_OP_UPTIME, SHELL_OP_EXEC, SHELL_OP_OPEN_APP,
    SHELL_OP_INJECT_STATE,
    SHELL_STATUS_OK, SHELL_STATUS_NOT_FOUND, SHELL_STATUS_ERROR,
    hash_name as shell_hash_name,
};
use libfolk::sys::synapse::{read_file_shmem, SYNAPSE_TASK_ID, write_file};
use libfolk::sys::{shmem_create, shmem_destroy, shmem_grant, shmem_map, shmem_unmap, uptime};

use crate::commands::system::cmd_poweroff;
use crate::state::{get_app_state, AppState, APP_STATE_ENTRY_SIZE};
use crate::ui::{build_calc_ui, build_folkpad_ui, build_greeting_ui, SHELL_SHMEM_VADDR};
use crate::wasm;

/// Case-insensitive substring match (no_std).
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

/// Handle an IPC command from compositor or other tasks.
pub fn handle_ipc_command(payload0: u64) -> u64 {
    // Check for text submit event (TextInput Enter from compositor)
    let marker = (payload0 & 0xFFFF) as u16;
    if marker == 0xAC11 {
        return handle_text_submit(payload0);
    }

    // Check for UI action event (button click from compositor)
    if marker == 0xAC10 {
        return handle_ui_action(payload0);
    }

    let opcode = payload0 & 0xFF;
    match opcode {
        x if x == SHELL_OP_LIST_FILES => op_list_files(),
        x if x == SHELL_OP_CAT_FILE => op_cat_file(payload0),
        x if x == SHELL_OP_SEARCH => op_search(payload0),
        x if x == SHELL_OP_PS => op_ps(),
        x if x == SHELL_OP_UPTIME => uptime(),
        x if x == SHELL_OP_EXEC => op_exec(payload0),
        x if x == SHELL_OP_OPEN_APP => op_open_app(payload0),
        x if x == SHELL_OP_INJECT_STATE => op_inject_state(payload0),
        _ => SHELL_STATUS_ERROR,
    }
}

// ── 0xAC11: TextInput submit ─────────────────────────────────────────

fn handle_text_submit(payload0: u64) -> u64 {
    let action_id = ((payload0 >> 16) & 0xFFFF) as u32;
    let text_handle = ((payload0 >> 32) & 0xFFFF) as u32;
    let win_id = (payload0 >> 48) as u16;

    // Read text from shmem
    let mut text = [0u8; 64];
    let mut text_len = 0usize;
    if shmem_map(text_handle, SHELL_SHMEM_VADDR).is_ok() {
        let src = unsafe { core::slice::from_raw_parts(SHELL_SHMEM_VADDR as *const u8, 66) };
        text_len = u16::from_le_bytes([src[0], src[1]]) as usize;
        text_len = text_len.min(63);
        text[..text_len].copy_from_slice(&src[2..2+text_len]);
        let _ = shmem_unmap(text_handle, SHELL_SHMEM_VADDR);
    }

    let text_str = unsafe { core::str::from_utf8_unchecked(&text[..text_len]) };

    if let Some(state) = get_app_state(win_id as u32) {
        // Infer app_type from action_id
        if state.app_type == 0 && action_id >= 200 {
            state.app_type = 2; // folkpad
        } else if state.app_type == 0 && action_id >= 100 {
            state.app_type = 1; // greeter
        }

        if state.app_type == 2 {
            // Folkpad: add line
            if state.pad_line_count < 10 && text_len > 0 {
                let len = text_len.min(64);
                state.pad_lines[state.pad_line_count][..len].copy_from_slice(&text[..len]);
                state.pad_line_lens[state.pad_line_count] = len;
                state.pad_line_count += 1;
                state.pad_saved = false;
            }
            return build_folkpad_ui(state);
        }
        // Greeter: store name
        let nlen = text_len.min(31);
        state.greet_name[..nlen].copy_from_slice(&text[..nlen]);
        state.greet_name_len = nlen;
    }

    build_greeting_ui(text_str)
}

// ── 0xAC10: button click ──────────────────────────────────────────────

fn handle_ui_action(payload0: u64) -> u64 {
    let action_id = ((payload0 >> 16) & 0xFFFFFFFF) as u32;
    let win_id = (payload0 >> 48) as u16;

    let state = match get_app_state(win_id as u32) {
        Some(s) => s,
        None => return SHELL_STATUS_ERROR,
    };

    // M14: One-time WASM load from VFS
    if !wasm::is_loaded() {
        if let Ok(resp) = read_file_shmem("calc.wasm") {
            if resp.size > 0 {
                if shmem_map(resp.shmem_handle, SHELL_SHMEM_VADDR).is_ok() {
                    let wasm_data = unsafe {
                        core::slice::from_raw_parts(SHELL_SHMEM_VADDR as *const u8, resp.size as usize)
                    };
                    if wasm::parse(wasm_data) {
                        println!("[Shell] WASM loaded: calc.wasm ({} bytes)", resp.size);
                    }
                    let _ = shmem_unmap(resp.shmem_handle, SHELL_SHMEM_VADDR);
                }
                let _ = shmem_destroy(resp.shmem_handle);
            }
        }
    }

    // Folkpad app: action_ids 200-299
    if action_id >= 200 && action_id < 300 {
        return handle_folkpad_action(state, action_id);
    }

    // Greeter app: action_ids 100+
    if action_id >= 100 {
        state.app_type = 1;
        let name_str = unsafe {
            core::str::from_utf8_unchecked(&state.greet_name[..state.greet_name_len])
        };
        return build_greeting_ui(name_str);
    }

    // Calculator: try WASM first, fallback to hardcoded
    let wasm_ok = wasm::call_handle_event(state, action_id);
    if !wasm_ok {
        handle_calc_action(state, action_id);
    }

    let display_val = state.display;
    build_calc_ui(display_val)
}

fn handle_folkpad_action(state: &mut AppState, action_id: u32) -> u64 {
    state.app_type = 2;
    match action_id {
        201 => {
            // Save: concatenate lines with \n, write to VFS
            let mut content = [0u8; 700];
            let mut pos = 0;
            for i in 0..state.pad_line_count {
                if i > 0 && pos < content.len() { content[pos] = b'\n'; pos += 1; }
                let len = state.pad_line_lens[i].min(content.len() - pos);
                content[pos..pos+len].copy_from_slice(&state.pad_lines[i][..len]);
                pos += len;
            }
            let _ = write_file("note.txt", &content[..pos]);
            state.pad_saved = true;
        }
        202 => { state.pad_line_count = 0; state.pad_saved = false; }
        203 => {
            // Load: read note.txt from VFS, split on \n
            state.pad_line_count = 0;
            state.pad_saved = false;
            if let Ok(resp) = read_file_shmem("note.txt") {
                if shmem_map(resp.shmem_handle, SHELL_SHMEM_VADDR).is_ok() {
                    let src = unsafe {
                        core::slice::from_raw_parts(SHELL_SHMEM_VADDR as *const u8, resp.size as usize)
                    };
                    let mut line_start = 0;
                    let total = resp.size as usize;
                    for p in 0..total {
                        if src[p] == b'\n' || p == total - 1 {
                            let end = if src[p] == b'\n' { p } else { p + 1 };
                            if end > line_start && state.pad_line_count < 10 {
                                let len = (end - line_start).min(64);
                                state.pad_lines[state.pad_line_count][..len]
                                    .copy_from_slice(&src[line_start..line_start+len]);
                                state.pad_line_lens[state.pad_line_count] = len;
                                state.pad_line_count += 1;
                            }
                            line_start = p + 1;
                        }
                    }
                    let _ = shmem_unmap(resp.shmem_handle, SHELL_SHMEM_VADDR);
                }
                let _ = shmem_destroy(resp.shmem_handle);
            }
        }
        _ => {}
    }
    build_folkpad_ui(state)
}

fn handle_calc_action(state: &mut AppState, action_id: u32) {
    match action_id {
        0..=9 => {
            if state.fresh_digit {
                state.display = action_id as i64;
                state.fresh_digit = false;
            } else {
                state.display = state.display.saturating_mul(10).saturating_add(action_id as i64);
            }
        }
        10 => { // +
            state.accumulator = state.display;
            state.operator = 1;
            state.fresh_digit = true;
        }
        11 => { // -
            state.accumulator = state.display;
            state.operator = 2;
            state.fresh_digit = true;
        }
        12 => { // *
            state.accumulator = state.display;
            state.operator = 3;
            state.fresh_digit = true;
        }
        13 => { // /
            state.accumulator = state.display;
            state.operator = 4;
            state.fresh_digit = true;
        }
        14 => { // =
            state.display = match state.operator {
                1 => state.accumulator.saturating_add(state.display),
                2 => state.accumulator.saturating_sub(state.display),
                3 => state.accumulator.saturating_mul(state.display),
                4 => if state.display != 0 { state.accumulator / state.display } else { 0 },
                _ => state.display,
            };
            state.operator = 0;
            state.fresh_digit = true;
        }
        15 => { // C (clear)
            state.display = 0;
            state.accumulator = 0;
            state.operator = 0;
            state.fresh_digit = true;
        }
        _ => {}
    }
}

// ── SHELL_OP_* dispatch ───────────────────────────────────────────────

fn op_list_files() -> u64 {
    // Synapse already grants shmem to tasks 2-8; just forward the response
    unsafe {
        libfolk::syscall::syscall3(
            libfolk::syscall::SYS_IPC_SEND,
            SYNAPSE_TASK_ID as u64,
            libfolk::sys::synapse::SYN_OP_LIST_FILES,
            0,
        )
    }
}

fn op_cat_file(payload0: u64) -> u64 {
    let name_hash = ((payload0 >> 8) & 0xFFFFFFFF) as u32;
    let syn_request = libfolk::sys::synapse::SYN_OP_READ_FILE_SHMEM | ((name_hash as u64) << 16);
    let result = unsafe {
        libfolk::syscall::syscall3(
            libfolk::syscall::SYS_IPC_SEND,
            SYNAPSE_TASK_ID as u64,
            syn_request, 0,
        )
    };
    if result == u64::MAX {
        SHELL_STATUS_ERROR
    } else if result == libfolk::sys::synapse::SYN_STATUS_NOT_FOUND {
        SHELL_STATUS_NOT_FOUND
    } else {
        result
    }
}

fn op_search(payload0: u64) -> u64 {
    let query_handle = ((payload0 >> 8) & 0xFFFFFFFF) as u32;
    let query_len = ((payload0 >> 40) & 0xFF) as usize;

    if query_handle == 0 || query_len == 0 {
        return 0;
    }

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

    let syn_result = unsafe {
        libfolk::syscall::syscall3(
            libfolk::syscall::SYS_IPC_SEND,
            SYNAPSE_TASK_ID as u64,
            libfolk::sys::synapse::SYN_OP_LIST_FILES,
            0,
        )
    };
    if syn_result == u64::MAX { return 0; }
    let file_count = (syn_result >> 32) as usize;
    let files_handle = (syn_result & 0xFFFFFFFF) as u32;
    if files_handle == 0 || file_count == 0 { return 0; }

    if shmem_map(files_handle, SHELL_SHMEM_VADDR).is_err() {
        let _ = shmem_destroy(files_handle);
        return 0;
    }
    let file_buf = unsafe {
        core::slice::from_raw_parts(SHELL_SHMEM_VADDR as *const u8, file_count * 32)
    };

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
        if contains_ignore_case(name, query_str) {
            matches[match_count].0[..name_end].copy_from_slice(&file_buf[offset..offset+name_end]);
            matches[match_count].1 = u32::from_le_bytes([
                file_buf[offset+24], file_buf[offset+25],
                file_buf[offset+26], file_buf[offset+27],
            ]);
            matches[match_count].2 = u32::from_le_bytes([
                file_buf[offset+28], file_buf[offset+29],
                file_buf[offset+30], file_buf[offset+31],
            ]);
            match_count += 1;
        }
    }
    let _ = shmem_unmap(files_handle, SHELL_SHMEM_VADDR);
    let _ = shmem_destroy(files_handle);

    if match_count == 0 { return 0; }

    let results_size = match_count * 32;
    let results_handle = match shmem_create(results_size) {
        Ok(h) => h,
        Err(_) => return 0,
    };
    for tid in 2..=8 { let _ = shmem_grant(results_handle, tid); }
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

fn op_ps() -> u64 {
    let mut task_buf = [0u8; 512];
    let count = libfolk::sys::system::task_list_detailed(&mut task_buf) as usize;
    if count == 0 { return 0; }

    let shmem_size = count * 32;
    let handle = match shmem_create(shmem_size) {
        Ok(h) => h,
        Err(_) => return (count as u64) << 32,
    };

    if shmem_map(handle, SHELL_SHMEM_VADDR).is_err() {
        let _ = shmem_destroy(handle);
        return (count as u64) << 32;
    }

    let buf = unsafe {
        core::slice::from_raw_parts_mut(SHELL_SHMEM_VADDR as *mut u8, shmem_size)
    };
    buf.copy_from_slice(&task_buf[..shmem_size]);
    for tid in 2..=8 { let _ = shmem_grant(handle, tid); }
    let _ = shmem_unmap(handle, SHELL_SHMEM_VADDR);

    ((count as u64) << 32) | (handle as u64)
}

fn op_exec(payload0: u64) -> u64 {
    let cmd_hash = ((payload0 >> 8) & 0xFFFFFFFF) as u32;

    if cmd_hash == shell_hash_name("ui_test") {
        // Build a test UI widget tree
        let mut ui_buf = [0u8; 512];
        let mut w = libfolk::ui::UiWriter::new(&mut ui_buf);
        w.header("Folkering App", 280, 160);
        w.vstack_begin(6, 5);
          w.label("Hello from Shell!", 0x00CCFF);
          w.spacer(4);
          w.label("This UI was built by", 0xCCCCCC);
          w.label("Shell and sent via IPC", 0xCCCCCC);
          w.hstack_begin(8, 2);
            w.button("OK", 1, 0x226644, 0xFFFFFF);
            w.button("Cancel", 2, 0x664422, 0xFFFFFF);
        let ui_len = w.len();

        let handle = match shmem_create(ui_len) {
            Ok(h) => h,
            Err(_) => return SHELL_STATUS_ERROR,
        };
        for tid in 2..=8 { let _ = shmem_grant(handle, tid); }
        if shmem_map(handle, SHELL_SHMEM_VADDR).is_err() {
            let _ = shmem_destroy(handle);
            return SHELL_STATUS_ERROR;
        }
        let dst = unsafe {
            core::slice::from_raw_parts_mut(SHELL_SHMEM_VADDR as *mut u8, ui_len)
        };
        dst.copy_from_slice(&ui_buf[..ui_len]);
        let _ = shmem_unmap(handle, SHELL_SHMEM_VADDR);

        (0x5549_u64 << 48) | (ui_len as u64) << 32 | (handle as u64)
    } else if cmd_hash == shell_hash_name("poweroff") || cmd_hash == shell_hash_name("shutdown") {
        cmd_poweroff();
        SHELL_STATUS_OK // unreachable
    } else {
        SHELL_STATUS_OK
    }
}

fn op_open_app(payload0: u64) -> u64 {
    let cmd_hash = ((payload0 >> 8) & 0xFFFFFFFF) as u32;
    let calc_hash = shell_hash_name("calc");
    let greet_hash = shell_hash_name("greet");
    let folkpad_hash = shell_hash_name("folkpad");

    if cmd_hash == calc_hash {
        build_calc_ui(0)
    } else if cmd_hash == greet_hash {
        build_greeting_ui("")
    } else if cmd_hash == folkpad_hash {
        build_folkpad_ui(&AppState::new_folkpad(0))
    } else {
        SHELL_STATUS_ERROR
    }
}

fn op_inject_state(payload0: u64) -> u64 {
    let inject_handle = ((payload0 >> 16) & 0xFFFF) as u32;
    if inject_handle == 0 { return SHELL_STATUS_ERROR; }
    if shmem_map(inject_handle, SHELL_SHMEM_VADDR).is_err() {
        return SHELL_STATUS_ERROR;
    }
    let buf = unsafe {
        core::slice::from_raw_parts(SHELL_SHMEM_VADDR as *const u8, APP_STATE_ENTRY_SIZE)
    };
    let win_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let display = i64::from_le_bytes([buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11]]);
    let accumulator = i64::from_le_bytes([buf[12], buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19]]);
    let operator = buf[20];
    let fresh_digit = buf[21] != 0;
    let _ = shmem_unmap(inject_handle, SHELL_SHMEM_VADDR);

    if let Some(state) = get_app_state(win_id) {
        state.display = display;
        state.accumulator = accumulator;
        state.operator = operator;
        state.fresh_digit = fresh_digit;
    }

    build_calc_ui(display)
}
