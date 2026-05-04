//! Input/output syscalls: keyboard, mouse, console write, interrupt flag,
//! time, RNG, and poweroff. (Combined "io + system control" domain.)

// ── Input ──────────────────────────────────────────────────────────────

pub fn syscall_read_key() -> u64 {
    if let Some(key) = crate::drivers::keyboard::read_key() {
        crate::drivers::iqe::record(
            crate::drivers::iqe::IqeEventType::KeyboardRead,
            crate::drivers::iqe::rdtsc(),
            key as u64,
        );
        if key == 0x03 {
            set_current_task_interrupt();
            return 0x03;
        }
        return key as u64;
    }

    if let Some(byte) = crate::drivers::serial::read_byte() {
        if byte == 0x03 {
            set_current_task_interrupt();
            return 0x03;
        }
        if byte == b'\r' {
            return b'\n' as u64;
        }
        return byte as u64;
    }

    0
}

pub fn syscall_read_mouse() -> u64 {
    if let Some(event) = crate::drivers::mouse::read_event() {
        crate::drivers::iqe::record(
            crate::drivers::iqe::IqeEventType::MouseRead,
            crate::drivers::iqe::rdtsc(),
            0,
        );
        let buttons = event.buttons as u64;
        let dx = (event.dx as u16) as u64;
        let dy = (event.dy as u16) as u64;

        (1u64 << 63) | (dy << 24) | (dx << 8) | buttons
    } else {
        0
    }
}

/// Absolute pointer read. Pumps the virtio-input eventq and returns
/// the latest scaled `(x, y, buttons)` frame, or 0 when nothing is
/// queued / the driver didn't attach.
///
/// Args: `fb_w`, `fb_h` — pixel dimensions to scale device coords into.
/// We clamp `fb_w`, `fb_h` at 16 bits since they ride in 16-bit slots
/// in the return packing; 65535 is well above any framebuffer the OS
/// targets today.
///
/// Return packing (when frame present):
///   bit 63    = 1 (presence flag, mirrors `syscall_read_mouse`)
///   bits 32-47 = y
///   bits 16-31 = x
///   bits 8-15  = buttons (bit 0 = left, 1 = right, 2 = middle)
///   bits 0-7   = reserved (always 0)
pub fn syscall_read_mouse_abs(fb_w: u64, fb_h: u64) -> u64 {
    let w = (fb_w & 0xFFFF) as u32;
    let h = (fb_h & 0xFFFF) as u32;
    if w == 0 || h == 0 { return 0; }
    match crate::drivers::virtio_input::read_frame_scaled(w, h) {
        Some((x, y, buttons)) => {
            let xb = (x & 0xFFFF) as u64;
            let yb = (y & 0xFFFF) as u64;
            let btn = (buttons as u64) & 0xFF;
            (1u64 << 63) | (yb << 32) | (xb << 16) | (btn << 8)
        }
        None => 0,
    }
}

/// Set interrupt flag on current task (private helper for read_key/read_mouse)
fn set_current_task_interrupt() {
    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        task_arc.lock().interrupt_pending = true;
    }
}

// ── Output ─────────────────────────────────────────────────────────────

pub fn syscall_write_char(char_code: u64) -> u64 {
    let ch = (char_code & 0xFF) as u8;
    crate::drivers::serial::write_byte(ch);
    0
}

// ── System Control ─────────────────────────────────────────────────────

pub fn syscall_poweroff() -> u64 {
    crate::serial_println!("\n[KERNEL] System poweroff requested");
    crate::serial_println!("[KERNEL] Goodbye!");

    unsafe {
        x86_64::instructions::port::Port::<u32>::new(0xf4).write(0x10);
    }

    unsafe {
        x86_64::instructions::port::Port::<u16>::new(0x604).write(0x2000);
    }

    loop {
        x86_64::instructions::hlt();
    }
}

pub fn syscall_check_interrupt() -> u64 {
    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        if task_arc.lock().interrupt_pending {
            return 1;
        }
    }
    0
}

pub fn syscall_clear_interrupt() -> u64 {
    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        task_arc.lock().interrupt_pending = false;
    }
    0
}

// ── Time / RNG ─────────────────────────────────────────────────────────

pub fn syscall_get_time() -> u64 {
    crate::drivers::cmos::unix_timestamp()
}

pub fn syscall_get_random(buf_ptr: u64, buf_len: u64) -> u64 {
    if buf_ptr == 0 || buf_len == 0 || buf_len > 4096 {
        return u64::MAX;
    }
    // Userspace-only — otherwise a task could `get_random(kernel_vaddr, 4096)`
    // and spray 4 KiB of pseudo-random bytes into the kernel image.
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    let end = match buf_ptr.checked_add(buf_len) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if buf_ptr < 0x200000 || buf_ptr >= USERSPACE_TOP || end > USERSPACE_TOP {
        return u64::MAX;
    }
    let buf = unsafe {
        core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_len as usize)
    };
    crate::drivers::rng::fill_bytes(buf);
    0
}
