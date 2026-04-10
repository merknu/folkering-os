//! AI inference commands: ai-status, ask, infer.

use libfolk::println;
use libfolk::sys::{shmem_create, shmem_destroy, shmem_grant, shmem_map, shmem_unmap};

const ASK_SHMEM_VADDR: usize = 0x30000000;

pub fn cmd_ai_status() {
    use libfolk::sys::inference;

    match inference::status() {
        Ok((has_model, arena_size)) => {
            println!("AI Inference Server (Task {}):", inference::INFERENCE_TASK_ID);
            println!("  Model: {}", if has_model { "loaded" } else { "not loaded (stub mode)" });
            println!("  Arena: {}KB", arena_size / 1024);
        }
        Err(e) => {
            println!("AI server unavailable: {:?}", e);
        }
    }
}

pub fn cmd_ask(full_cmd: &str) {
    use libfolk::sys::inference;

    let query = if full_cmd.starts_with("ask ") {
        &full_cmd[4..]
    } else if full_cmd.starts_with("infer ") {
        &full_cmd[6..]
    } else {
        println!("Usage: ask <question>");
        return;
    };

    let query = query.trim();
    if query.is_empty() {
        println!("Usage: ask <question>");
        return;
    }

    match inference::ping() {
        Ok(has_model) => {
            if !has_model {
                println!("[AI] No model loaded — inference server running in stub mode.");
                println!("[AI] Pack a GGUF model into virtio-data.img to enable inference.");
                return;
            }
        }
        Err(_) => {
            println!("[AI] Inference server not available.");
            return;
        }
    }

    println!("[AI] Processing: {}", query);

    let query_bytes = query.as_bytes();
    let shmem_size = ((query_bytes.len() + 4095) / 4096) * 4096;
    let shmem_size = if shmem_size == 0 { 4096 } else { shmem_size };

    let handle = match shmem_create(shmem_size) {
        Ok(h) => h,
        Err(_) => { println!("[AI] Failed to allocate shared memory"); return; }
    };

    let _ = shmem_grant(handle, inference::INFERENCE_TASK_ID);

    if shmem_map(handle, ASK_SHMEM_VADDR).is_err() {
        let _ = shmem_destroy(handle);
        println!("[AI] Failed to map shared memory");
        return;
    }

    unsafe {
        let ptr = ASK_SHMEM_VADDR as *mut u8;
        core::ptr::copy_nonoverlapping(query_bytes.as_ptr(), ptr, query_bytes.len());
    }

    let _ = shmem_unmap(handle, ASK_SHMEM_VADDR);

    let is_ask = full_cmd.starts_with("ask");
    let result = if is_ask {
        inference::ask(handle, query_bytes.len())
    } else {
        inference::generate(handle, query_bytes.len())
    };

    match result {
        Ok((out_shmem, out_len)) => {
            if out_len > 0 && out_shmem > 0 {
                if shmem_map(out_shmem, ASK_SHMEM_VADDR).is_ok() {
                    let text = unsafe {
                        let ptr = ASK_SHMEM_VADDR as *const u8;
                        let slice = core::slice::from_raw_parts(ptr, out_len);
                        core::str::from_utf8_unchecked(slice)
                    };
                    println!("{}", text);
                    let _ = shmem_unmap(out_shmem, ASK_SHMEM_VADDR);
                    let _ = shmem_destroy(out_shmem);
                } else {
                    println!("[AI] Failed to read response");
                }
            } else {
                println!("[AI] No response generated");
            }
        }
        Err(inference::InferError::NoModel) => {
            println!("[AI] No model loaded");
        }
        Err(e) => {
            println!("[AI] Error: {:?}", e);
        }
    }

    let _ = shmem_destroy(handle);
}
