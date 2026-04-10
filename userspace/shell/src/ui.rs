//! UI builders for shell-hosted apps (calculator, greeter, folkpad).
//! Each function builds an FKUI widget tree, copies it into shmem, and
//! returns the encoded shmem handle as the IPC reply value.

use libfolk::sys::shell::SHELL_STATUS_ERROR;
use libfolk::sys::{shmem_create, shmem_destroy, shmem_grant, shmem_map, shmem_unmap};

use crate::state::AppState;

/// Virtual address for Shell's shared memory buffer mapping
pub const SHELL_SHMEM_VADDR: usize = 0x20000000;

/// Format an `i64` as decimal ASCII string. Returns number of bytes written.
pub fn format_i64(val: i64, buf: &mut [u8; 24]) -> usize {
    if val == 0 {
        buf[0] = b'0';
        return 1;
    }

    let negative = val < 0;
    let mut abs_val = if negative { (val as i128).wrapping_neg() as u64 } else { val as u64 };

    let mut tmp = [0u8; 20];
    let mut i = 0;
    while abs_val > 0 && i < 20 {
        tmp[i] = b'0' + (abs_val % 10) as u8;
        abs_val /= 10;
        i += 1;
    }

    let mut pos = 0;
    if negative {
        buf[0] = b'-';
        pos = 1;
    }
    for j in (0..i).rev() {
        buf[pos] = tmp[j];
        pos += 1;
    }
    pos
}

/// Build greeting demo UI with a `TextInput`.
pub fn build_greeting_ui(name: &str) -> u64 {
    let mut ui_buf = [0u8; 512];
    let mut w = libfolk::ui::UiWriter::new(&mut ui_buf);

    w.header("Greeter", 220, 120);
    if name.is_empty() {
        w.vstack_begin(6, 3);
          w.label("Type your name:", 0xFFFFFF);
          w.text_input("Name...", 100, 32);
          w.button("Greet", 101, 0x226644, 0xFFFFFF);
    } else {
        let mut hello = [0u8; 64];
        let prefix = b"Hello, ";
        let suffix = b"!";
        let nlen = name.len().min(50);
        hello[..prefix.len()].copy_from_slice(prefix);
        hello[prefix.len()..prefix.len()+nlen].copy_from_slice(&name.as_bytes()[..nlen]);
        hello[prefix.len()+nlen..prefix.len()+nlen+suffix.len()].copy_from_slice(suffix);
        let total = prefix.len() + nlen + suffix.len();
        let hello_str = unsafe { core::str::from_utf8_unchecked(&hello[..total]) };

        w.vstack_begin(6, 3);
          w.label(hello_str, 0x00FF88);
          w.text_input("Name...", 100, 32);
          w.button("Greet", 101, 0x226644, 0xFFFFFF);
    }

    let ui_len = w.len();
    publish_ui(&ui_buf, ui_len)
}

/// Build Folkpad UI with lines, text input, and Save/Load/Clear buttons.
pub fn build_folkpad_ui(state: &AppState) -> u64 {
    let mut ui_buf = [0u8; 1024];
    let mut w = libfolk::ui::UiWriter::new(&mut ui_buf);

    let child_count = (4 + state.pad_line_count).min(255) as u8;
    w.header("Folkpad", 280, 200);
    w.vstack_begin(4, child_count);

    if state.pad_saved {
        w.label("Folkpad - Saved!", 0x00FF88);
    } else {
        w.label("Folkpad - Simple Notes", 0x00CCFF);
    }

    for i in 0..state.pad_line_count {
        let s = core::str::from_utf8(&state.pad_lines[i][..state.pad_line_lens[i]])
            .unwrap_or("<invalid>");
        w.label(s, 0xCCCCCC);
    }

    w.text_input("Type a line...", 200, 60);
    w.hstack_begin(8, 3);
      w.button("Save", 201, 0x226644, 0xFFFFFF);
      w.button("Load", 203, 0x224466, 0xFFFFFF);
      w.button("Clear", 202, 0x664422, 0xFFFFFF);

    let ui_len = w.len();
    publish_ui(&ui_buf, ui_len)
}

/// Build calculator UI and return the encoded shmem handle.
pub fn build_calc_ui(display_value: i64) -> u64 {
    let mut display_buf = [0u8; 24];
    let display_len = format_i64(display_value, &mut display_buf);
    let display_str = unsafe { core::str::from_utf8_unchecked(&display_buf[..display_len]) };

    let mut ui_buf = [0u8; 1024];
    let mut w = libfolk::ui::UiWriter::new(&mut ui_buf);

    w.header("Calculator", 200, 260);
    w.vstack_begin(4, 6);
      w.label(display_str, 0xFFFFFF);
      w.spacer(4);
      w.hstack_begin(4, 4);
        w.button("7", 7, 0x334455, 0xFFFFFF);
        w.button("8", 8, 0x334455, 0xFFFFFF);
        w.button("9", 9, 0x334455, 0xFFFFFF);
        w.button("/", 13, 0x554433, 0xFFFFFF);
      w.hstack_begin(4, 4);
        w.button("4", 4, 0x334455, 0xFFFFFF);
        w.button("5", 5, 0x334455, 0xFFFFFF);
        w.button("6", 6, 0x334455, 0xFFFFFF);
        w.button("*", 12, 0x554433, 0xFFFFFF);
      w.hstack_begin(4, 4);
        w.button("1", 1, 0x334455, 0xFFFFFF);
        w.button("2", 2, 0x334455, 0xFFFFFF);
        w.button("3", 3, 0x334455, 0xFFFFFF);
        w.button("-", 11, 0x554433, 0xFFFFFF);
      w.hstack_begin(4, 4);
        w.button("0", 0, 0x334455, 0xFFFFFF);
        w.button("C", 15, 0x664422, 0xFFFFFF);
        w.button("=", 14, 0x226644, 0xFFFFFF);
        w.button("+", 10, 0x554433, 0xFFFFFF);

    let ui_len = w.len();
    publish_ui(&ui_buf, ui_len)
}

/// Allocate shmem, copy a UI buffer into it, grant tasks 2-8, and return
/// the encoded reply value `(0x5549 << 48) | (ui_len << 32) | handle`.
fn publish_ui(ui_buf: &[u8], ui_len: usize) -> u64 {
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

    (0x5549_u64 << 48) | ((ui_len as u64) << 32) | (handle as u64)
}
