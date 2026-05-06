//! Compute syscalls: parallel GEMM via SMP and Gemini cloud query.

pub fn syscall_parallel_gemm(
    input_ptr: u64,
    weight_ptr: u64,
    output_ptr: u64,
    k_seq: u64,
    n_qt: u64,
) -> u64 {
    // Packed ABI (see `libfolk::sys::parallel_gemm`):
    //   k_seq lower 32 = k (in_dim), upper 32 = seq (batch size)
    //   n_qt  lower 32 = n (out_dim), top byte = quant_type
    // The syscall entry shuffle drops C-ABI arg6, so we live within
    // 5 args and pack two pairs of fields.
    let k = (k_seq & 0xFFFF_FFFF) as u32;
    let seq = ((k_seq >> 32) & 0xFFFF_FFFF) as u32;
    let n = (n_qt & 0xFFFF_FFFF) as u32;
    let quant_type = ((n_qt >> 56) & 0xFF) as u8;

    let task_id = crate::task::task::get_current_task();
    let cr3 = match crate::task::task::get_task(task_id) {
        Some(t) => t.lock().page_table_phys,
        None => return u64::MAX,
    };

    let result = crate::arch::x86_64::smp::dispatch_parallel_gemm(
        input_ptr,
        weight_ptr,
        output_ptr,
        k,
        n,
        seq,
        quant_type,
        cr3,
    );

    if result == 0 { 0 } else { u64::MAX }
}

pub fn syscall_ask_gemini(prompt_ptr: u64, prompt_len: u64, response_buf_ptr: u64) -> u64 {
    let prompt_len = prompt_len as usize;

    if prompt_len == 0 || prompt_len > 8192 {
        return u64::MAX;
    }
    // Validate both pointers
    if prompt_ptr < 0x200000 || prompt_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }
    if response_buf_ptr < 0x200000 || response_buf_ptr >= 0x0000_8000_0000_0000 { return u64::MAX; }

    let prompt_bytes = unsafe {
        core::slice::from_raw_parts(prompt_ptr as *const u8, prompt_len)
    };
    let prompt = match core::str::from_utf8(prompt_bytes) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    crate::serial_str!("[SYS_GEMINI] Prompt: ");
    let preview = &prompt[..prompt.len().min(80)];
    crate::drivers::serial::write_str(preview);
    crate::drivers::serial::write_newline();

    let result = crate::net::gemini::ask_gemini(prompt);

    let response_bytes = match result {
        Ok(bytes) => bytes,
        Err(e) => {
            crate::serial_str!("[SYS_GEMINI] Error: ");
            crate::drivers::serial::write_str(e);
            crate::drivers::serial::write_newline();
            let msg = alloc::format!("Error: {}", e);
            msg.into_bytes()
        }
    };

    let max_write = response_bytes.len().min(131072);
    unsafe {
        core::ptr::copy_nonoverlapping(
            response_bytes.as_ptr(),
            response_buf_ptr as *mut u8,
            max_write,
        );
    }

    max_write as u64
}
